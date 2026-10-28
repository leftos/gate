//! The PC's outputs manifest: the list of its files, each with its stamp and SHA-256, as it is sent and read; the box's
//! pre-command listing; and the rule choosing which of the box's files to pack, as the PowerShell gate's
//! `Select-OutputEntry` chooses them.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;

use gate_router::escaped;
use gate_wire::{SHA256_DIGITS, is_lower_hex, ordinal_upper};
use serde_json::{Map, Value, json};

use crate::hash::sha256_of;
use crate::names::{is_git_s, is_lfs_config, is_plain_path};
use crate::walk::Found;

/// The most a manifest line may hold, its newline included: 64 MiB, what the box reads and the client sends at most.
pub const MANIFEST_LIMIT: usize = 64 * 1024 * 1024;

/// A file's stat stamp: its length and its last write. A file whose stamp differs from the one the pre-command listing
/// holds is one the run created or changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub size: u64,
    /// The last write, a stamp compared box against box within one run: a Windows file time on the Windows box and a
    /// Unix mtime in nanoseconds on the Unix box. In the PC's manifest it is opaque to the box, never compared.
    pub modified: u64,
}

/// One file of the PC's manifest: its stamp and its SHA-256.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Held {
    pub stamp: Stamp,
    pub sha256: String,
}

/// Each SHA-256 a manifest holds, with every path holding it, in the order of the paths' simple uppercase, and what the
/// PC holds there.
pub type HeldBy<'m> = HashMap<&'m str, Vec<(&'m str, &'m Held)>>;

/// The PC's outputs manifest: each of its files under the run's globs, by `/`-separated path relative to the worktree
/// root, with its stamp and SHA-256. Paths are found without regard to case, as Windows finds files. Sent as one line
/// of JSON and a newline: `{"files":{"<path>":{"size":<bytes>,"modified":<file time>,"sha256":"<hex>"}}}`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    /// Each file by its path's simple uppercase: its path as given, and what the PC holds there.
    files: BTreeMap<String, (String, Held)>,
}

impl Manifest {
    /// Holds `held` at `path`, in place of any file whose path differs from it only in case.
    pub fn insert(&mut self, path: String, held: Held) {
        self.files.insert(ordinal_upper(&path), (path, held));
    }

    /// Drops the file at `path`, found without regard to case, when the manifest holds one.
    pub fn remove(&mut self, path: &str) {
        self.files.remove(&ordinal_upper(path));
    }

    /// What the PC holds at `path`, found without regard to case.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&Held> {
        self.files.get(&ordinal_upper(path)).map(|(_, held)| held)
    }

    /// Each file of the manifest, its path as given and what the PC holds there, in the order of the paths' simple
    /// uppercase.
    pub fn files(&self) -> impl Iterator<Item = (&str, &Held)> {
        self.files.values().map(|(path, held)| (path.as_str(), held))
    }

    /// The manifest's files by their SHA-256 ([`HeldBy`]), each path of `skip` left out, found without regard to case.
    #[must_use]
    pub fn by_hash(&self, skip: &[String]) -> HeldBy<'_> {
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
    #[must_use]
    pub fn line(&self) -> String {
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
    pub fn parse(line: &str) -> Result<Manifest, String> {
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

/// The PC's inputs manifest the `line` a box read holds, its `\n` or `\r\n` dropped, and the SHA-256 of the line as
/// received, without its line end; else why the box does not take it, a path in it the box does not take
/// ([`refused_path`]) among them.
pub fn read_sent(mut line: Vec<u8>) -> Result<(Manifest, String), String> {
    if line.ends_with(b"\n") {
        line.pop();
        if line.ends_with(b"\r") {
            line.pop();
        }
    }
    let (digest, _) = sha256_of(line.as_slice()).map_err(|error| format!("the inputs manifest could not be hashed: {error}"))?;
    let text = String::from_utf8(line).map_err(|_| "the inputs manifest is not UTF-8".to_owned())?;
    let manifest = Manifest::parse(&text).map_err(|why| why.replacen("the outputs manifest", "the inputs manifest", 1))?;
    if let Some((path, why)) = manifest.files().find_map(|(path, _)| refused_path(path).map(|why| (path, why))) {
        return Err(format!("the inputs manifest names '{}', {why}", escaped(path)));
    }
    Ok((manifest, digest))
}

/// Why the box does not take the manifest's `path`: one that is not a plain relative path, or one the PC never sends
/// (a `.git*` name, a `.lfsconfig`, or a `.env` or `*.env` file); `None` when it takes it.
#[must_use]
pub fn refused_path(path: &str) -> Option<&'static str> {
    if !is_plain_path(path) {
        return Some("which is not a plain relative path");
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    let env = name.to_ascii_lowercase().ends_with(".env");
    (is_git_s(path) || is_lfs_config(path) || env).then_some("which inputs never carry")
}

/// The box's stat listing taken just before the command: the stamp of each file under the run's output and failure
/// globs, by path found without regard to case, and whether every file outside a `.tmp` folder counts as changed
/// whatever its stamp (`OutputsFull`).
#[derive(Debug, Default)]
pub struct Listing {
    stamps: HashMap<String, Stamp>,
    full: bool,
}

impl Listing {
    /// The listing of the files a walk `found`.
    #[must_use]
    pub fn of(found: &[Found]) -> Listing {
        let stamps = found.iter().map(|file| (ordinal_upper(&file.path), file.stamp)).collect();
        Listing { stamps, full: false }
    }

    /// This listing under `OutputsFull` (the PC's last outputs step did not complete): every file outside a `.tmp`
    /// folder counts as changed, and one below a `.tmp` folder still only when its stamp says so.
    #[must_use]
    pub fn under_outputs_full(self) -> Listing {
        Listing { full: true, ..self }
    }

    /// Whether the run created or changed `file`: this listing lacks it or holds another stamp, or, under
    /// `OutputsFull`, it lies outside a `.tmp` folder (`below_tmp` false).
    fn changed(&self, file: &Found, below_tmp: bool) -> bool {
        (self.full && !below_tmp) || self.stamps.get(&ordinal_upper(&file.path)).copied() != Some(file.stamp)
    }
}

/// Whether the `/`-separated relative `path` lies below a `.tmp` folder, in any letter case, at any depth.
fn is_below_tmp(path: &str) -> bool {
    path.rsplit_once('/')
        .is_some_and(|(folders, _)| folders.split('/').any(|folder| folder.eq_ignore_ascii_case(".tmp")))
}

/// A file [`select`] chose to pack, with its SHA-256 when it was hashed before the pack.
#[derive(Debug, PartialEq, Eq)]
pub struct Chosen<'found> {
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
/// created or changed (`listing` lacks it or holds another stamp, [`Listing::changed`]) whose SHA-256 (`hash`) differs
/// from the manifest's. Only a file the manifest holds and the run changed is hashed, and a chosen one keeps its hash.
///
/// A file below a `.tmp` folder comes back only from the run that wrote it: it is chosen only when `listing` shows the
/// run created or changed it, whether or not the manifest holds it, so an old file the box kept is never sent again.
///
/// `listing` `None` counts every file outside a `.tmp` folder as changed (`OutputsFull` without a listing), so each
/// that differs from the PC's comes back, and chooses no file below one. Paths compare without regard to case. Else
/// the error of the first hash that fails.
pub fn select<'found>(
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
        let below_tmp = is_below_tmp(&file.path);
        let changed = listing.map_or(!below_tmp, |listing| listing.changed(file, below_tmp));
        let Some(held) = manifest.get(&file.path) else {
            if changed || !below_tmp {
                chosen.push(Chosen::from(file));
            }
            continue;
        };
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
pub fn hash_possible_copies(chosen: &mut [Chosen<'_>], manifest: &Manifest, mut hash: impl FnMut(&Found) -> io::Result<String>) -> io::Result<()> {
    let mut sizes: HashMap<u64, usize> = HashMap::new();
    for item in chosen.iter() {
        *sizes.entry(item.file.stamp.size).or_default() += 1;
    }
    let held: HashSet<u64> = manifest.files().map(|(_, held)| held.stamp.size).collect();
    for item in chosen.iter_mut().filter(|item| item.sha256.is_none()) {
        let size = item.file.stamp.size;
        if held.contains(&size) || sizes.get(&size).is_some_and(|count| *count > 1) {
            item.sha256 = Some(hash(item.file)?);
        }
    }
    Ok(())
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot read its manifest must fail loudly")]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::io;
    use std::path::PathBuf;

    use super::{Held, Listing, Manifest, Stamp, hash_possible_copies, read_sent, select};
    use crate::hash::sha256_of;
    use crate::walk::Found;

    /// A SHA-256 of `digit` repeated.
    fn sha(digit: char) -> String {
        digit.to_string().repeat(64)
    }

    fn stamp(size: u64, modified: u64) -> Stamp {
        Stamp { size, modified }
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

    /// Why `read_sent` refuses the manifest holding one file at `path`.
    fn refusal_of(path: &str) -> String {
        let line = manifest(&[(path, stamp(1, 1), 'a')]).line();
        read_sent(line.into_bytes()).expect_err(path)
    }

    #[test]
    fn a_sent_path_that_is_not_plain_is_refused() {
        assert_eq!(
            refusal_of("bin/a:b.dll"),
            "the inputs manifest names 'bin/a:b.dll', which is not a plain relative path"
        );
    }

    #[test]
    fn a_sent_git_name_in_any_form_is_refused() {
        for path in [".gitattributes", "sub/.GitIgnore", "sub/.git-blame-ignore-revs"] {
            assert_eq!(
                refusal_of(path),
                format!("the inputs manifest names '{path}', which inputs never carry"),
                "{path}"
            );
        }
    }

    #[test]
    fn a_sent_lfsconfig_is_refused() {
        assert_eq!(
            refusal_of("sub/.LfsConfig"),
            "the inputs manifest names 'sub/.LfsConfig', which inputs never carry"
        );
    }

    #[test]
    fn a_sent_env_file_is_refused() {
        for path in [".env", "app/prod.ENV"] {
            assert_eq!(
                refusal_of(path),
                format!("the inputs manifest names '{path}', which inputs never carry"),
                "{path}"
            );
        }
    }

    #[test]
    fn a_sent_line_that_is_not_utf8_is_refused() {
        assert_eq!(read_sent(b"\xff\n".to_vec()), Err("the inputs manifest is not UTF-8".to_owned()));
    }

    #[test]
    fn a_sent_line_that_does_not_parse_names_the_inputs_manifest() {
        let reason = |line: &str| read_sent(line.as_bytes().to_vec()).expect_err(line);
        assert!(reason("{\"files\":\n").starts_with("the inputs manifest is not JSON: "));
        assert_eq!(reason("[]\n"), "the inputs manifest holds no object of files");
        assert_eq!(
            reason("{\"files\":{\"a\":1}}\n"),
            "the inputs manifest's entry for 'a' is not a size, a last write and a SHA-256"
        );
    }

    #[test]
    fn a_good_sent_line_comes_back_with_the_sha256_of_its_line_without_its_end() {
        let sent = manifest(&[("bin/a.dll", stamp(3, 4), 'a')]);
        let line = sent.line();
        let (bare, _) = sha256_of(line.trim_end_matches('\n').as_bytes()).expect("the line hashes");
        let crlf = format!("{}\r\n", line.trim_end_matches('\n'));
        for received in [line.clone(), crlf, line.trim_end_matches('\n').to_owned()] {
            assert_eq!(
                read_sent(received.clone().into_bytes()),
                Ok((manifest(&[("bin/a.dll", stamp(3, 4), 'a')]), bare.clone())),
                "{received:?}"
            );
        }
        assert_eq!(
            read_sent(Vec::new()),
            Ok((Manifest::default(), sha256_of(&b""[..]).expect("nothing hashes").0)),
            "an empty line"
        );
    }

    fn found(path: &str, stamp: Stamp) -> Found {
        Found {
            path: path.to_owned(),
            full: PathBuf::from(format!(r"C:\work\{path}")),
            stamp,
        }
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
    fn a_tmp_file_the_run_did_not_write_is_not_chosen_though_the_manifest_lacks_it() {
        let files = [found(".tmp/old.json", stamp(1, 10)), found("src/.TMP/dumps/old.dmp", stamp(2, 10))];
        let listing = Listing::of(&files);
        assert_eq!(chosen(&files, &Manifest::default(), Some(&listing), &[], &[]), (vec![], vec![]));
        let full = Listing::of(&files).under_outputs_full();
        assert_eq!(
            chosen(&files, &Manifest::default(), Some(&full), &[], &[]),
            (vec![], vec![]),
            "nor under OutputsFull"
        );
        assert_eq!(
            chosen(&files, &Manifest::default(), None, &[], &[]),
            (vec![], vec![]),
            "nor without a listing"
        );
    }

    #[test]
    fn a_tmp_file_the_run_created_is_chosen() {
        let files = [found(".tmp/new.json", stamp(1, 10)), found(".tmp/old.json", stamp(1, 10))];
        let listing = Listing::of(&[found(".tmp/old.json", stamp(1, 10))]);
        let picked = chosen(&files, &Manifest::default(), Some(&listing), &[], &[]);
        assert_eq!(picked, (vec![".tmp/new.json".to_owned()], vec![]), "chosen without being hashed");
    }

    #[test]
    fn a_tmp_file_the_run_changed_is_chosen_when_its_hash_differs() {
        let before = Listing::of(&[
            found(".tmp/other.json", stamp(1, 10)),
            found(".tmp/same.json", stamp(1, 10)),
            found(".tmp/lacked.json", stamp(1, 10)),
        ]);
        let files = [
            found(".tmp/lacked.json", stamp(1, 11)),
            found(".tmp/other.json", stamp(1, 11)),
            found(".tmp/same.json", stamp(2, 10)),
        ];
        let pc = manifest(&[(".tmp/other.json", stamp(1, 1), 'o'), (".tmp/same.json", stamp(1, 1), 's')]);
        let hashes = [(".tmp/other.json", 'b'), (".tmp/same.json", 's')];
        let (picked, hashed) = chosen(&files, &pc, Some(&before), &[], &hashes);
        assert_eq!(
            picked,
            [".tmp/lacked.json", ".tmp/other.json"],
            "one the PC lacks, and one changed to another hash"
        );
        assert_eq!(hashed, [".tmp/other.json", ".tmp/same.json"], "only the files the PC holds");
    }

    #[test]
    fn under_outputs_full_a_non_tmp_file_is_still_chosen_without_a_listing() {
        let files = [
            found("bin/a.dll", stamp(1, 10)),
            found("bin/b.dll", stamp(1, 10)),
            found(".tmp/c.json", stamp(1, 10)),
        ];
        let pc = manifest(&[("bin/a.dll", stamp(1, 10), 'x')]);
        let (picked, hashed) = chosen(&files, &pc, None, &[], &[("bin/a.dll", 'a')]);
        assert_eq!(
            picked,
            ["bin/a.dll", "bin/b.dll"],
            "the untouched files outside .tmp, but not the one below it"
        );
        assert_eq!(hashed, ["bin/a.dll"]);
        let full = Listing::of(&files).under_outputs_full();
        let (picked, _) = chosen(&files, &pc, Some(&full), &[], &[("bin/a.dll", 'a')]);
        assert_eq!(picked, ["bin/a.dll", "bin/b.dll"], "the same with a listing under OutputsFull");
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
