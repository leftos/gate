//! No case derives a path or a pattern from where the repo is checked out: the suite's sources never read the
//! manifest directory or the working directory, except this scan's own root. [`repo_file`] is the one sanctioned
//! exception, for a case that must run a checked-in file, never for a path or a pattern a case asserts on.

use std::fs;
use std::path::{Path, PathBuf};

/// The needles, built so this file's own text does not match them.
const MANIFEST_DIR: &str = concat!("CARGO_", "MANIFEST_DIR");
const CURRENT_DIR: &str = concat!("current_", "dir()");
const REPO_FILE: &str = concat!("repo_file", "(");

/// The directory this suite is compiled against: the single place a suite source reads where the repo is, shared by
/// [`repo_file`] and this scan's own root.
const CHECKOUT: &str = env!("CARGO_MANIFEST_DIR");

/// The absolute path of the checked-in file `relative`, which a case runs as a script or a program, such as
/// `tools/install.ps1`. A case must never assert on a path this returns, nor build a pattern from one: it names a file
/// in the checked-out repo, which the suite is otherwise not allowed to know about.
pub fn repo_file(relative: &str) -> PathBuf {
    let file = Path::new(CHECKOUT).join("..").join("..").join(relative);
    let canonical = fs::canonicalize(file).expect("the checked-in file a case runs is there");
    let text = canonical.to_string_lossy().into_owned();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    canonical
}

fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("a test folder can be listed") {
        let path = entry.expect("a test folder entry can be read").path();
        if path.is_dir() {
            sources(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

#[test]
fn no_test_derives_a_pattern_from_the_checkout_path() {
    let tests = Path::new(CHECKOUT).join("tests");
    let mut files = Vec::new();
    sources(&tests, &mut files);
    assert!(files.len() > 1, "the scan finds the suite's sources under {}", tests.display());

    let mut uses = Vec::new();
    let mut callers = Vec::new();
    for file in &files {
        let text = fs::read_to_string(file).expect("a test source can be read");
        let name = file.file_name().map(|name| name.to_string_lossy().into_owned());
        for needle in [MANIFEST_DIR, CURRENT_DIR] {
            uses.extend(text.match_indices(needle).map(|_| (name.clone(), needle)));
        }
        callers.extend(text.match_indices(REPO_FILE).map(|_| name.clone()));
    }
    callers.sort();

    assert_eq!(
        uses,
        [(Some("checkout.rs".to_owned()), MANIFEST_DIR)],
        "only this scan's root reads the manifest directory, and nothing reads the working directory"
    );
    assert_eq!(
        callers,
        [Some("checkout.rs".to_owned()), Some("install.rs".to_owned())],
        "the checkout route is defined in this scan's root and called only by the install cases"
    );
}
