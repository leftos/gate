//! The PC's outputs manifest: the list of its files, each with its stamp and SHA-256, as it is sent and read.

use std::collections::{BTreeMap, HashMap, HashSet};

use gate_router::escaped;
use gate_wire::{SHA256_DIGITS, is_lower_hex, ordinal_upper};
use serde_json::{Map, Value, json};

use crate::hash::sha256_of;
use crate::names::{is_git_s, is_lfs_config, is_plain_path};

/// The most a manifest line may hold, its newline included: 64 MiB, what the box reads and the client sends at most.
pub const MANIFEST_LIMIT: usize = 64 * 1024 * 1024;

/// A file's stat stamp: its length and its last write as a Windows file time. A file whose stamp differs from the one
/// the pre-command listing holds is one the run created or changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub size: u64,
    /// the PC's Windows file time, opaque to the box
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

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot read its manifest must fail loudly")]
mod tests {
    use super::{Held, Manifest, Stamp, read_sent};
    use crate::hash::sha256_of;

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
}
