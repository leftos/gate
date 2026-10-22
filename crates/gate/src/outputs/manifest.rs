//! The PC's outputs manifest, the box's pre-command listing, and the rule choosing which of the box's files to pack, all
//! [`gate_inputs::manifest`]'s and re-exported here; the manifest's build from a walk is this PC's.

use std::io;

use gate_wire::SHA256_DIGITS;

pub(crate) use gate_inputs::manifest::{Chosen, Held, HeldBy, Listing, MANIFEST_LIMIT, Manifest, hash_possible_copies, select};

use crate::outputs::walk::{Found, Stamp};

#[cfg(test)]
use crate::outputs::hash::sha256_of;
#[cfg(test)]
use crate::outputs::walk::{in_context, stamp_of};
#[cfg(test)]
use std::fs::File;

/// The manifest of the files a walk `found`, each hashed as a stream; else why the first that cannot be read failed.
/// No production caller: the run's own manifest is built by [`build_manifest_with`], which stops at the cap, so this
/// serves tests only.
#[cfg(test)]
pub(crate) fn build_manifest(found: &[Found]) -> io::Result<Manifest> {
    build_manifest_with(found, usize::MAX, |file| {
        let opened = File::open(&file.full).map_err(|error| in_context(&file.full, &error))?;
        let metadata = opened.metadata().map_err(|error| in_context(&file.full, &error))?;
        let sha256 = sha256_of(opened)
            .map(|(sha256, _)| sha256)
            .map_err(|error| in_context(&file.full, &error))?;
        Ok((sha256, stamp_of(&metadata)))
    })
}

/// The manifest of the files a walk `found`, each hashed by `hash` and stamped by the stamp it returns, as [`select`]
/// takes it, so a caller can stop the build between two reads of a file. Before a file is hashed, the bytes its entry
/// adds to the manifest's line are counted from the stamp the walk found it at, the line's outer braces and newline
/// left out: once they pass `limit`, the build fails as [`io::ErrorKind::FileTooLarge`], no further file hashed. The
/// count is then corrected to the entry actually written, whose stamp is the hash's own and can carry a digit the
/// walk's does not, so the next file is weighed against the entry the line will hold. Else the error of the first
/// hash that fails.
pub(crate) fn build_manifest_with(
    found: &[Found],
    limit: usize,
    mut hash: impl FnMut(&Found) -> io::Result<(String, Stamp)>,
) -> io::Result<Manifest> {
    let mut manifest = Manifest::default();
    let mut length = 0_usize;
    for file in found {
        let listed = entry_length(&file.path, file.stamp)?;
        length = length.saturating_add(listed);
        if length > limit {
            let passed = format!("the outputs manifest passes {limit} bytes");
            return Err(io::Error::new(io::ErrorKind::FileTooLarge, passed));
        }
        let (sha256, stamp) = hash(file)?;
        length = length.saturating_sub(listed).saturating_add(entry_length(&file.path, stamp)?);
        manifest.insert(file.path.clone(), Held { stamp, sha256 });
    }
    Ok(manifest)
}

/// The characters of one manifest entry besides its quoted path, its digits and its hash:
/// `:{"size":`, `,"modified":`, `,"sha256":"`, `"}` and the comma after it.
const ENTRY_PUNCTUATION: usize = 35;

/// The bytes the entry of the file at `path`, stamped `stamp`, adds to the manifest's line:
/// `"<path>":{"size":<n>,"modified":<n>,"sha256":"<hex>"},`, its path escaped as JSON escapes it.
fn entry_length(path: &str, stamp: Stamp) -> io::Result<usize> {
    let quoted = serde_json::to_string(path).map_err(io::Error::other)?;
    let digits = stamp.size.to_string().len() + stamp.modified.to_string().len();
    Ok(quoted.len() + ENTRY_PUNCTUATION + SHA256_DIGITS + digits)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its files or read its manifest must fail loudly")]
mod tests {
    use std::fs;
    use std::io;
    use std::path::PathBuf;

    use super::{Held, Manifest, build_manifest, build_manifest_with};
    use crate::outputs::glob::Globs;
    use crate::outputs::walk::{Found, Stamp, walk};

    /// A SHA-256 of `digit` repeated.
    fn sha(digit: char) -> String {
        digit.to_string().repeat(64)
    }

    fn stamp(size: u64, modified: u64) -> Stamp {
        Stamp { size, modified }
    }

    fn found(path: &str, stamp: Stamp) -> Found {
        Found {
            path: path.to_owned(),
            full: PathBuf::from(format!(r"C:\work\{path}")),
            stamp,
        }
    }

    fn manifest(files: &[(&str, Stamp, char)]) -> Manifest {
        let mut manifest = Manifest::default();
        for (path, stamp, digit) in files {
            manifest.insert(
                (*path).to_owned(),
                Held {
                    stamp: *stamp,
                    sha256: sha(*digit),
                },
            );
        }
        manifest
    }

    #[test]
    fn a_manifest_built_from_a_walk_holds_each_files_stamp_and_hash() {
        let root = crate::test_support::temp_dir().join(format!("gate-manifest-{}-build", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("bin")).expect("the folder can be made");
        fs::write(root.join("bin").join("ü.dll"), "abc").expect("the file can be written");
        let files = walk(&root, &Globs::new(&["**".to_owned()]).expect("a glob")).expect("the walk");
        let built = build_manifest(&files).expect("the manifest can be built");
        let held = built.get("bin/ü.dll").expect("the file is held");
        assert_eq!(held.sha256, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(held.stamp, files[0].stamp);
        assert_eq!(
            build_manifest(&[found("absent", stamp(0, 0))]).map(|_| ()).map_err(|error| error.kind()),
            Err(io::ErrorKind::NotFound)
        );
    }

    #[test]
    fn a_build_past_its_limit_stops_before_hashing_the_rest() {
        // Each entry is 113 bytes: the quoted path's 11, 35 of punctuation, 64 of hash and the 3 digits of its stamp.
        let files = [found("bin/a.dll", stamp(1, 10)), found("bin/b.dll", stamp(1, 10))];
        let entry = files[0].path.len() + 2 + 35 + 64 + 3;
        assert_eq!(
            manifest(&[("bin/a.dll", stamp(1, 10), 'a')]).line().len(),
            entry - 1 + "{\"files\":{}}\n".len()
        );
        for (limit, hashed_before) in [(entry - 1, 0), (2 * entry - 1, 1), (2 * entry, 2)] {
            let mut hashed = Vec::new();
            let built = build_manifest_with(&files, limit, |file| {
                hashed.push(file.path.clone());
                Ok((sha('a'), file.stamp))
            });
            assert_eq!(hashed.len(), hashed_before, "limit {limit}");
            let refused = built.err().map(|error| error.kind());
            let expected = (hashed_before < 2).then_some(io::ErrorKind::FileTooLarge);
            assert_eq!(refused, expected, "limit {limit}");
        }
    }
}
