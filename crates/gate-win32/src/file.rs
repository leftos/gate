//! File calls the standard library does not make: a rename that never replaces the file its target names.

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

/// Renames the file `source` to `target` on the same volume, never replacing a file `target` already names. Both are
/// made absolute and given the verbatim prefix first, so a path past `MAX_PATH` is moved as well.
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

/// The backslash that separates a path's components.
const SEP: u16 = b'\\' as u16;
/// The colon after a drive letter.
const COLON: u16 = b':' as u16;
/// What a verbatim path starts with, `\\?\`, which the system takes as it is, past `MAX_PATH`.
const VERBATIM: [u16; 4] = [SEP, SEP, b'?' as u16, SEP];
/// What an NT object path starts with, `\??\`, which is left as it is.
const NT_OBJECT: [u16; 4] = [SEP, b'?' as u16, b'?' as u16, SEP];
/// What a verbatim UNC path starts with, `\\?\UNC\`.
const VERBATIM_UNC: [u16; 8] = [SEP, SEP, b'?' as u16, SEP, b'U' as u16, b'N' as u16, b'C' as u16, SEP];
/// What a device path starts with, `\\.\`.
const DEVICE: [u16; 4] = [SEP, SEP, b'.' as u16, SEP];

/// `path` made absolute and verbatim, as a NUL-terminated UTF-16 buffer the system takes past `MAX_PATH`; else the
/// error for a path that holds a NUL, which would cut it short, or one that cannot be made absolute.
fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    if path.as_os_str().encode_wide().any(|unit| unit == 0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("the path {} holds a NUL", path.display()),
        ));
    }
    let absolute: Vec<u16> = std::path::absolute(path)?.as_os_str().encode_wide().collect();
    let mut wide = verbatim(&absolute);
    wide.push(0);
    Ok(wide)
}

/// The absolute, normalized path `absolute` with the verbatim prefix, as the standard library gives one: `C:\…` becomes
/// `\\?\C:\…`, a device path `\\.\…` becomes `\\?\…` and a UNC path `\\server\…` becomes `\\?\UNC\server\…`; a path
/// already verbatim, or an NT object path, is left as it is, and so is any other.
fn verbatim(absolute: &[u16]) -> Vec<u16> {
    let (prefix, rest): (&[u16], &[u16]) = if let [_, COLON, SEP, ..] = absolute {
        (&VERBATIM, absolute)
    } else if absolute.starts_with(&VERBATIM) || absolute.starts_with(&NT_OBJECT) {
        (&[], absolute)
    } else if let Some(rest) = absolute.strip_prefix(&DEVICE) {
        (&VERBATIM, rest)
    } else if let Some(rest) = absolute.strip_prefix(&[SEP, SEP]) {
        (&VERBATIM_UNC, rest)
    } else {
        (&[], absolute)
    };
    [prefix, rest].concat()
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose files cannot be made must fail loudly")]
mod tests {
    use std::io;
    use std::path::{Path, PathBuf};

    use super::{move_no_replace, verbatim};

    /// [`verbatim`] of the absolute path `absolute`, read back as text.
    fn verbatim_of(absolute: &str) -> String {
        let wide: Vec<u16> = absolute.encode_utf16().collect();
        String::from_utf16(&verbatim(&wide)).expect("the path is UTF-16")
    }

    #[test]
    fn each_kind_of_absolute_path_gets_the_verbatim_prefix_once() {
        assert_eq!(verbatim_of(r"C:\a\b.txt"), r"\\?\C:\a\b.txt", "a drive path");
        assert_eq!(verbatim_of(r"\\server\share\a"), r"\\?\UNC\server\share\a", "a UNC path");
        assert_eq!(verbatim_of(r"\\.\C:\a"), r"\\?\C:\a", "a device path");
        assert_eq!(verbatim_of(r"\\?\C:\a"), r"\\?\C:\a", "a verbatim path");
        assert_eq!(verbatim_of(r"\\?\UNC\server\share"), r"\\?\UNC\server\share", "a verbatim UNC path");
        assert_eq!(verbatim_of(r"\??\C:\a"), r"\??\C:\a", "an NT object path");
        assert_eq!(verbatim_of(r"\a"), r"\a", "anything else");
    }

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

    /// A file whose source and target paths are both longer than the 260 characters of `MAX_PATH` is moved.
    #[test]
    fn a_file_whose_paths_pass_max_path_is_moved() {
        let folder = folder("long");
        let deep = folder.join("a".repeat(150)).join("b".repeat(150));
        std::fs::create_dir_all(&deep).expect("the deep folder can be made");
        let (source, target) = (deep.join("source.txt"), deep.join("target.txt"));
        assert!(source.as_os_str().len() > 260 && target.as_os_str().len() > 260, "{}", source.display());
        std::fs::write(&source, "a").expect("the file can be written");

        move_no_replace(&source, &target).expect("the rename goes through");

        assert!(!source.exists(), "the source is gone");
        assert_eq!(std::fs::read_to_string(&target).expect("the target is there"), "a");
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_path_holding_a_nul_is_refused() {
        let moved = move_no_replace(Path::new("a\0b"), Path::new("c"));

        assert_eq!(moved.map_err(|error| error.kind()), Err(io::ErrorKind::InvalidInput));
    }
}
