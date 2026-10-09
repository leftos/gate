//! No case derives a path or a pattern from where the repo is checked out: the suite's sources never read the
//! manifest directory or the working directory, except this scan's own root.

use std::fs;
use std::path::{Path, PathBuf};

/// The needles, built so this file's own text does not match them.
const MANIFEST_DIR: &str = concat!("CARGO_", "MANIFEST_DIR");
const CURRENT_DIR: &str = concat!("current_", "dir()");

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
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut files = Vec::new();
    sources(&tests, &mut files);
    assert!(files.len() > 1, "the scan finds the suite's sources under {}", tests.display());

    let mut uses = Vec::new();
    for file in &files {
        let text = fs::read_to_string(file).expect("a test source can be read");
        for needle in [MANIFEST_DIR, CURRENT_DIR] {
            uses.extend(
                text.match_indices(needle)
                    .map(|_| (file.file_name().map(|name| name.to_string_lossy().into_owned()), needle)),
            );
        }
    }

    assert_eq!(
        uses,
        [(Some("checkout.rs".to_owned()), MANIFEST_DIR)],
        "only this scan's root reads the manifest directory, and nothing reads the working directory"
    );
}
