//! The PC's outputs manifest, the box's pre-command listing, and the rule choosing which of the box's files to pack, as
//! the PowerShell gate's `Select-OutputEntry` chooses them.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;

use serde_json::{Map, Value, json};

use crate::boxside::payload::{escaped, ordinal_upper};
use crate::outputs::hash::{SHA256_DIGITS, is_lower_hex};
use crate::outputs::walk::{Found, Stamp};

#[cfg(test)]
use crate::outputs::hash::sha256_of;
#[cfg(test)]
use crate::outputs::walk::in_context;
#[cfg(test)]
use std::fs::File;

/// The most a manifest line may hold, its newline included: 64 MiB, what the box reads and the client sends at most.
pub(crate) const MANIFEST_LIMIT: usize = 64 * 1024 * 1024;

/// One file of the PC's manifest: its stamp and its SHA-256.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Held {
    pub stamp: Stamp,
    pub sha256: String,
}

/// Each SHA-256 a manifest holds, with every path holding it, in the order of the paths' simple uppercase, and what the
/// PC holds there.
pub(crate) type HeldBy<'m> = HashMap<&'m str, Vec<(&'m str, &'m Held)>>;

/// The PC's outputs manifest: each of its files under the run's globs, by `/`-separated path relative to the worktree
/// root, with its stamp and SHA-256. Paths are found without regard to case, as Windows finds files. Sent as one line
/// of JSON and a newline: `{"files":{"<path>":{"size":<bytes>,"modified":<file time>,"sha256":"<hex>"}}}`.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Manifest {
    /// Each file by its path's simple uppercase: its path as given, and what the PC holds there.
    files: BTreeMap<String, (String, Held)>,
}

impl Manifest {
    /// The manifest of the files a walk `found`, each hashed as a stream; else why the first that cannot be read failed.
    /// No production caller: the run's own manifest is built by [`build_with`], which stops at the cap, so this serves
    /// tests only.
    #[cfg(test)]
    pub(crate) fn build(found: &[Found]) -> io::Result<Manifest> {
        Manifest::build_with(found, usize::MAX, |file| {
            let opened = File::open(&file.full).map_err(|error| in_context(&file.full, &error))?;
            let metadata = opened.metadata().map_err(|error| in_context(&file.full, &error))?;
            let sha256 = sha256_of(opened)
                .map(|(sha256, _)| sha256)
                .map_err(|error| in_context(&file.full, &error))?;
            Ok((sha256, Stamp::of(&metadata)))
        })
    }

    /// The manifest of the files a walk `found`, each hashed by `hash` and stamped by the stamp it returns, as [`select`]
    /// takes it, so a caller can stop the build between two reads of a file. Before a file is hashed, the bytes its entry
    /// adds to the manifest's line are counted from the stamp the walk found it at, the line's outer braces and newline
    /// left out: once they pass `limit`, the build fails as [`io::ErrorKind::FileTooLarge`], no further file hashed. The
    /// count is then corrected to the entry actually written, whose stamp is the hash's own and can carry a digit the
    /// walk's does not, so the next file is weighed against the entry the line will hold. Else the error of the first
    /// hash that fails.
    pub(crate) fn build_with(found: &[Found], limit: usize, mut hash: impl FnMut(&Found) -> io::Result<(String, Stamp)>) -> io::Result<Manifest> {
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

    /// Holds `held` at `path`, in place of any file whose path differs from it only in case.
    pub(crate) fn insert(&mut self, path: String, held: Held) {
        self.files.insert(ordinal_upper(&path), (path, held));
    }

    /// What the PC holds at `path`, found without regard to case.
    pub(crate) fn get(&self, path: &str) -> Option<&Held> {
        self.files.get(&ordinal_upper(path)).map(|(_, held)| held)
    }

    /// The manifest's files by their SHA-256 ([`HeldBy`]), each path of `skip` left out, found without regard to case.
    pub(crate) fn by_hash(&self, skip: &[String]) -> HeldBy<'_> {
        let skip: HashSet<String> = skip.iter().map(|path| ordinal_upper(path)).collect();
        let mut index = HeldBy::new();
        for (key, (path, held)) in &self.files {
            if !skip.contains(key) {
                index.entry(held.sha256.as_str()).or_default().push((path.as_str(), held));
            }
        }
        index
    }

    /// The manifest as it is sent: one line of JSON, and a newline.
    pub(crate) fn line(&self) -> String {
        let files: Map<String, Value> = self
            .files
            .values()
            .map(|(path, held)| {
                let file = json!({ "size": held.stamp.size, "modified": held.stamp.modified, "sha256": held.sha256 });
                (path.clone(), file)
            })
            .collect();
        format!("{}\n", json!({ "files": files }))
    }

    /// The manifest `line` holds, its newline included or not; a blank line is an empty manifest. Else why it is not
    /// one.
    pub(crate) fn parse(line: &str) -> Result<Manifest, String> {
        if line.trim().is_empty() {
            return Ok(Manifest::default());
        }
        let value: Value = serde_json::from_str(line).map_err(|error| format!("the outputs manifest is not JSON: {error}"))?;
        let files = value
            .get("files")
            .and_then(Value::as_object)
            .ok_or_else(|| "the outputs manifest holds no object of files".to_owned())?;
        let mut manifest = Manifest::default();
        for (path, file) in files {
            manifest.insert(path.clone(), held(path, file)?);
        }
        Ok(manifest)
    }
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

/// What the manifest's entry `file` for `path` holds; else why it is not a file.
fn held(path: &str, file: &Value) -> Result<Held, String> {
    let size = file.get("size").and_then(Value::as_u64);
    let modified = file.get("modified").and_then(Value::as_u64);
    let sha256 = file
        .get("sha256")
        .and_then(Value::as_str)
        .filter(|sha256| is_lower_hex(sha256, SHA256_DIGITS));
    match (size, modified, sha256) {
        (Some(size), Some(modified), Some(sha256)) => Ok(Held {
            stamp: Stamp { size, modified },
            sha256: sha256.to_owned(),
        }),
        _ => Err(format!(
            "the outputs manifest's entry for '{}' is not a size, a last write and a SHA-256",
            escaped(path)
        )),
    }
}

/// The box's stat listing taken just before the command: the stamp of each file under the run's globs, by path found
/// without regard to case.
#[derive(Debug, Default)]
pub(crate) struct Listing {
    stamps: HashMap<String, Stamp>,
}

impl Listing {
    /// The listing of the files a walk `found`.
    pub(crate) fn of(found: &[Found]) -> Listing {
        let stamps = found.iter().map(|file| (ordinal_upper(&file.path), file.stamp)).collect();
        Listing { stamps }
    }

    fn get(&self, path: &str) -> Option<Stamp> {
        self.stamps.get(&ordinal_upper(path)).copied()
    }
}

/// A file [`select`] chose to pack, with its SHA-256 when it was hashed before the pack.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Chosen<'found> {
    pub file: &'found Found,
    pub sha256: Option<String>,
}

impl<'found> From<&'found Found> for Chosen<'found> {
    /// The file `file`, not hashed.
    fn from(file: &'found Found) -> Chosen<'found> {
        Chosen { file, sha256: None }
    }
}

/// The files of `found`, the box's walk after the command, to pack, in their order: each outside `skip` (the paths of
/// the run's back commit, which the sync-back carries) that the PC's `manifest` lacks, and each it holds that the run
/// created or changed (`listing` lacks it or holds another stamp) whose SHA-256 (`hash`) differs from the manifest's.
/// Only a file the manifest holds and the run changed is hashed, and a chosen one keeps its hash.
///
/// `listing` is `None` under `OutputsFull` (the PC's last outputs step did not complete): every file then counts as
/// changed, so each that differs from the PC's comes back. Paths compare without regard to case. Else the error of the
/// first hash that fails.
pub(crate) fn select<'found>(
    found: &'found [Found],
    manifest: &Manifest,
    listing: Option<&Listing>,
    skip: &[String],
    mut hash: impl FnMut(&Found) -> io::Result<String>,
) -> io::Result<Vec<Chosen<'found>>> {
    let skip: HashSet<String> = skip.iter().map(|path| ordinal_upper(path)).collect();
    let mut chosen = Vec::new();
    for file in found {
        if skip.contains(&ordinal_upper(&file.path)) {
            continue;
        }
        let Some(held) = manifest.get(&file.path) else {
            chosen.push(Chosen::from(file));
            continue;
        };
        let changed = listing.is_none_or(|listing| listing.get(&file.path) != Some(file.stamp));
        if !changed {
            continue;
        }
        let sha256 = hash(file)?;
        if sha256 != held.sha256 {
            chosen.push(Chosen { file, sha256: Some(sha256) });
        }
    }
    Ok(chosen)
}

/// Hashes each of `chosen` not hashed yet whose size, as the walk found it, another of `chosen` or a file of `manifest`
/// has: only such a file can be a copy of another, which the pack then lists without packing its bytes. Every other file
/// is hashed by the pack's own read. Else the error of the first hash that fails.
pub(crate) fn hash_possible_copies(
    chosen: &mut [Chosen<'_>],
    manifest: &Manifest,
    mut hash: impl FnMut(&Found) -> io::Result<String>,
) -> io::Result<()> {
    let mut sizes: HashMap<u64, usize> = HashMap::new();
    for item in chosen.iter() {
        *sizes.entry(item.file.stamp.size).or_default() += 1;
    }
    let held: HashSet<u64> = manifest.files.values().map(|(_, held)| held.stamp.size).collect();
    for item in chosen.iter_mut().filter(|item| item.sha256.is_none()) {
        let size = item.file.stamp.size;
        if held.contains(&size) || sizes.get(&size).is_some_and(|count| *count > 1) {
            item.sha256 = Some(hash(item.file)?);
        }
    }
    Ok(())
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its files or read its manifest must fail loudly")]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::fs;
    use std::io;
    use std::path::PathBuf;

    use super::{Held, Listing, Manifest, hash_possible_copies, select};
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

    /// What `select` chooses when each file hashes to its `hashes` entry: the chosen paths and the paths it hashed.
    fn chosen(files: &[Found], manifest: &Manifest, listing: Option<&Listing>, skip: &[&str], hashes: &[(&str, char)]) -> (Vec<String>, Vec<String>) {
        let hashes: HashMap<&str, String> = hashes.iter().map(|(path, digit)| (*path, sha(*digit))).collect();
        let hashed = RefCell::new(Vec::new());
        let skip: Vec<String> = skip.iter().map(|path| (*path).to_owned()).collect();
        let picked = select(files, manifest, listing, &skip, |file| {
            hashed.borrow_mut().push(file.path.clone());
            Ok(hashes.get(file.path.as_str()).cloned().expect("every hashed file has a hash"))
        })
        .expect("every hash succeeds");
        let picked = picked.iter().map(|chosen| chosen.file.path.clone()).collect();
        (picked, hashed.into_inner())
    }

    #[test]
    fn only_files_whose_size_matches_another_are_hashed_before_the_pack() {
        let files = [
            found("bin/a.dll", stamp(5, 10)),
            found("bin/b.dll", stamp(5, 10)),
            found("bin/c.dll", stamp(7, 10)),
            found("bin/d.dll", stamp(9, 10)),
            found("bin/e.dll", stamp(3, 10)),
        ];
        let pc = manifest(&[("bin/e.dll", stamp(3, 1), 'e'), ("lib/x.dll", stamp(9, 1), 'x')]);
        let hashes: HashMap<&str, String> = [
            ("bin/a.dll", 'a'),
            ("bin/b.dll", 'b'),
            ("bin/c.dll", 'c'),
            ("bin/d.dll", 'd'),
            ("bin/e.dll", 'f'),
        ]
        .into_iter()
        .map(|(path, digit)| (path, sha(digit)))
        .collect();
        let hashed = RefCell::new(Vec::new());
        let hash = |file: &Found| {
            hashed.borrow_mut().push(file.path.clone());
            Ok(hashes.get(file.path.as_str()).cloned().expect("every hashed file has a hash"))
        };
        let mut chosen = select(&files, &pc, None, &[], hash).expect("every hash succeeds");
        assert_eq!(hashed.take(), ["bin/e.dll"], "the selection hashes only the file the PC holds");

        hash_possible_copies(&mut chosen, &pc, hash).expect("every hash succeeds");

        assert_eq!(
            hashed.take(),
            ["bin/a.dll", "bin/b.dll", "bin/d.dll"],
            "two of one size, and one of a size the PC holds; not one of a size no other file has, nor one hashed already"
        );
        let kept: Vec<(&str, Option<String>)> = chosen.iter().map(|item| (item.file.path.as_str(), item.sha256.clone())).collect();
        assert_eq!(
            kept,
            [
                ("bin/a.dll", Some(sha('a'))),
                ("bin/b.dll", Some(sha('b'))),
                ("bin/c.dll", None),
                ("bin/d.dll", Some(sha('d'))),
                ("bin/e.dll", Some(sha('f')))
            ]
        );
    }

    #[test]
    fn a_manifest_goes_out_as_one_line_and_comes_back_the_same() {
        let sent = manifest(&[
            ("bin/a.dll", stamp(3, 133_000_000_000_000_000), 'a'),
            ("bin/Ünïcødé ✓.dll", stamp(0, 1), 'b'),
        ]);
        let line = sent.line();
        assert!(line.ends_with('\n'), "{line}");
        assert_eq!(line.matches('\n').count(), 1, "{line}");
        assert_eq!(Manifest::parse(&line), Ok(sent), "{line}");
        assert_eq!(
            manifest(&[("a", stamp(1, 2), 'c')]).line(),
            format!("{{\"files\":{{\"a\":{{\"size\":1,\"modified\":2,\"sha256\":\"{}\"}}}}}}\n", sha('c'))
        );
    }

    #[test]
    fn a_blank_line_is_an_empty_manifest() {
        for blank in ["", "\n", " \r\n"] {
            assert_eq!(Manifest::parse(blank), Ok(Manifest::default()), "{blank:?}");
        }
        assert_eq!(Manifest::default().line(), "{\"files\":{}}\n");
    }

    #[test]
    fn a_line_that_is_not_a_manifest_says_why() {
        let reason = |line: &str| Manifest::parse(line).expect_err(line);
        assert!(reason("{\"files\":").starts_with("the outputs manifest is not JSON: "));
        assert_eq!(reason("[]"), "the outputs manifest holds no object of files");
        assert_eq!(reason("{\"files\":[]}"), "the outputs manifest holds no object of files");
        let entry = |file: &str| reason(&format!("{{\"files\":{{\"a\\u001b\":{file}}}}}"));
        let refused = "the outputs manifest's entry for 'a\\u{1b}' is not a size, a last write and a SHA-256";
        let good = sha('d');
        for file in [
            format!("\"{good}\""),
            format!("{{\"modified\":1,\"sha256\":\"{good}\"}}"),
            format!("{{\"size\":-1,\"modified\":1,\"sha256\":\"{good}\"}}"),
            format!("{{\"size\":1,\"sha256\":\"{good}\"}}"),
            "{\"size\":1,\"modified\":1,\"sha256\":\"abc\"}".to_owned(),
            format!("{{\"size\":1,\"modified\":1,\"sha256\":\"{}\"}}", good.to_uppercase()),
        ] {
            assert_eq!(entry(&file), refused, "{file}");
        }
    }

    #[test]
    fn a_manifest_finds_a_path_without_regard_to_case() {
        let held = manifest(&[("Bin/A.dll", stamp(1, 1), 'a')]);
        assert_eq!(held.get("bin/a.DLL").map(|file| file.sha256.clone()), Some(sha('a')));
        assert_eq!(held.get("bin/b.dll"), None);
    }

    #[test]
    fn a_manifest_built_from_a_walk_holds_each_files_stamp_and_hash() {
        let root = std::env::temp_dir().join(format!("gate-manifest-{}-build", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("bin")).expect("the folder can be made");
        fs::write(root.join("bin").join("ü.dll"), "abc").expect("the file can be written");
        let files = walk(&root, &Globs::new(&["**".to_owned()]).expect("a glob")).expect("the walk");
        let built = Manifest::build(&files).expect("the manifest can be built");
        let held = built.get("bin/ü.dll").expect("the file is held");
        assert_eq!(held.sha256, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(held.stamp, files[0].stamp);
        assert_eq!(
            Manifest::build(&[found("absent", stamp(0, 0))]).map(|_| ()).map_err(|error| error.kind()),
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
            let built = Manifest::build_with(&files, limit, |file| {
                hashed.push(file.path.clone());
                Ok((sha('a'), file.stamp))
            });
            assert_eq!(hashed.len(), hashed_before, "limit {limit}");
            let refused = built.err().map(|error| error.kind());
            let expected = (hashed_before < 2).then_some(io::ErrorKind::FileTooLarge);
            assert_eq!(refused, expected, "limit {limit}");
        }
    }

    #[test]
    fn a_file_the_pc_holds_that_the_run_left_unchanged_is_skipped_unhashed() {
        let files = [found("bin/a.dll", stamp(1, 10))];
        let listing = Listing::of(&files);
        let pc = manifest(&[("bin/a.dll", stamp(1, 5), 'f')]);
        assert_eq!(chosen(&files, &pc, Some(&listing), &[], &[]), (vec![], vec![]), "even with another hash");
    }

    #[test]
    fn a_file_the_pc_lacks_comes_back_even_if_the_run_left_it_unchanged() {
        let files = [found("bin/a.dll", stamp(1, 10))];
        let listing = Listing::of(&files);
        let picked = chosen(&files, &Manifest::default(), Some(&listing), &[], &[]);
        assert_eq!(picked, (vec!["bin/a.dll".to_owned()], vec![]), "chosen without being hashed");
    }

    #[test]
    fn a_file_the_run_changed_or_created_comes_back_only_when_its_hash_differs_from_the_pcs() {
        let before = Listing::of(&[found("bin/same.dll", stamp(1, 10)), found("bin/other.dll", stamp(1, 10))]);
        let files = [
            found("bin/new.dll", stamp(2, 20)),
            found("bin/new-same.dll", stamp(2, 20)),
            found("bin/other.dll", stamp(1, 11)),
            found("bin/same.dll", stamp(2, 10)),
        ];
        let pc = manifest(&[
            ("bin/new-same.dll", stamp(2, 1), 'n'),
            ("bin/other.dll", stamp(1, 1), 'o'),
            ("bin/same.dll", stamp(1, 1), 's'),
        ]);
        let hashes = [
            ("bin/new.dll", 'a'),
            ("bin/new-same.dll", 'n'),
            ("bin/other.dll", 'b'),
            ("bin/same.dll", 's'),
        ];
        let (picked, hashed) = chosen(&files, &pc, Some(&before), &[], &hashes);
        assert_eq!(picked, ["bin/new.dll", "bin/other.dll"], "created, and changed to another hash");
        assert_eq!(
            hashed,
            ["bin/new-same.dll", "bin/other.dll", "bin/same.dll"],
            "only the files the PC holds"
        );
    }

    #[test]
    fn outputs_full_drops_the_listing_so_every_file_that_differs_from_the_pcs_comes_back() {
        let files = [found("bin/a.dll", stamp(1, 10)), found("bin/b.dll", stamp(1, 10))];
        let pc = manifest(&[("bin/a.dll", stamp(1, 10), 'a'), ("bin/b.dll", stamp(1, 10), 'x')]);
        let hashes = [("bin/a.dll", 'a'), ("bin/b.dll", 'b')];
        let listing = Listing::of(&files);
        assert_eq!(chosen(&files, &pc, Some(&listing), &[], &hashes), (vec![], vec![]), "without OutputsFull");
        let (picked, hashed) = chosen(&files, &pc, None, &[], &hashes);
        assert_eq!(picked, ["bin/b.dll"], "the untouched file the PC holds with another hash");
        assert_eq!(hashed, ["bin/a.dll", "bin/b.dll"]);
    }

    #[test]
    fn a_path_of_the_back_commit_is_never_packed_nor_hashed() {
        let files = [found("bin/a.dll", stamp(1, 10)), found("bin/b.dll", stamp(1, 10))];
        let pc = manifest(&[("bin/b.dll", stamp(1, 1), 'x')]);
        let (picked, hashed) = chosen(&files, &pc, None, &["BIN/A.dll"], &[("bin/b.dll", 'b')]);
        assert_eq!(picked, ["bin/b.dll"]);
        assert_eq!(hashed, ["bin/b.dll"]);
    }

    #[test]
    fn the_listing_and_the_manifest_find_a_path_without_regard_to_case() {
        let files = [found("bin/A.dll", stamp(1, 10))];
        let listing = Listing::of(&[found("BIN/a.dll", stamp(1, 10))]);
        let pc = manifest(&[("Bin/a.DLL", stamp(1, 1), 'f')]);
        assert_eq!(chosen(&files, &pc, Some(&listing), &[], &[]), (vec![], vec![]));
    }

    #[test]
    fn a_hash_that_fails_fails_the_selection() {
        let files = [found("bin/a.dll", stamp(1, 10))];
        let pc = manifest(&[("bin/a.dll", stamp(1, 1), 'a')]);
        let failed = select(&files, &pc, None, &[], |_| Err(io::Error::other("denied")));
        assert_eq!(failed.map_err(|error| error.to_string()), Err("denied".to_owned()));
    }
}
