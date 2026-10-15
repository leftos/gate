//! The payload of a `gate box` call: one base64 word (standard alphabet, padded) of UTF-8 JSON holding an object, as
//! the remote client sends it as the first line of the call's standard input. Keys are read by their exact name; keys
//! no verb reads are ignored.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use gate_args::MAX_WHOLE_NUMBER;
pub(crate) use gate_router::{escaped, is_reordering_mark};
use gate_router::{is_layout_name, repo_name};
use serde_json::{Map, Value};

use crate::outputs::archive::SUFFIX;
use crate::outputs::hash::{SHA256_DIGITS, is_lower_hex};
use crate::remote::siblings::MOST_SIBLINGS;

/// The decoded JSON object.
pub(crate) struct Payload(Map<String, Value>);

impl Payload {
    /// Decodes the payload word; else the usage line saying why it cannot be read.
    pub(crate) fn decode(word: &str) -> Result<Payload, String> {
        object(word)
            .map(Payload)
            .map_err(|reason| format!("gate: box payload is not base64 JSON: {reason}"))
    }

    /// The text of `key`; else the usage line saying the payload lacks it. A key that is absent, empty or not a string
    /// is lacking.
    pub(crate) fn text(&self, key: &str) -> Result<&str, String> {
        self.0
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| format!("gate: box payload lacks {key}"))
    }

    /// The path of `key`: absolute on a drive (`X:\…` or `X:/…`), with no character below U+0020; else the usage line
    /// saying why, the value shown with its control characters escaped. A relative, UNC or `\\?\` path, or one that
    /// starts with `-`, is refused, so no such value reaches git as a path or an option.
    pub(crate) fn path(&self, key: &str) -> Result<&str, String> {
        let value = self.text(key)?;
        if is_drive_path(value) {
            Ok(value)
        } else {
            Err(format!("gate: box payload {key} is not an absolute path on a drive: {value:?}"))
        }
    }

    /// The words of `key`, a list of texts; else the usage line saying the payload lacks it (absent or empty) or that
    /// it is not a list of texts.
    pub(crate) fn words(&self, key: &str) -> Result<Vec<String>, String> {
        let not_words = || format!("gate: box payload {key} is not a list of texts");
        let list = match self.0.get(key) {
            None | Some(Value::Null) => return Err(format!("gate: box payload lacks {key}")),
            Some(value) => value.as_array().ok_or_else(not_words)?,
        };
        if list.is_empty() {
            return Err(format!("gate: box payload lacks {key}"));
        }
        list.iter().map(|word| word.as_str().map(str::to_owned).ok_or_else(not_words)).collect()
    }

    /// The text of `key`, empty when the key is absent or null; else the usage line saying it is not a text.
    pub(crate) fn optional_text(&self, key: &str) -> Result<&str, String> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(""),
            Some(value) => value.as_str().ok_or_else(|| format!("gate: box payload {key} is not a text")),
        }
    }

    /// The name and text of each entry of the object at `key`, none when the key is absent or null; else the usage
    /// line saying it is not an object of texts.
    pub(crate) fn pairs(&self, key: &str) -> Result<Vec<(String, String)>, String> {
        let not_pairs = || format!("gate: box payload {key} is not an object of texts");
        let object = match self.0.get(key) {
            None | Some(Value::Null) => return Ok(Vec::new()),
            Some(value) => value.as_object().ok_or_else(not_pairs)?,
        };
        object
            .iter()
            .map(|(name, value)| value.as_str().map(|text| (name.clone(), text.to_owned())).ok_or_else(not_pairs))
            .collect()
    }

    /// The run's output globs, `Outputs`: a list of non-empty texts holding no `[` or `]`, as the routing file allows
    /// them; none when the key is absent or null. Else the usage line, a glob shown with its control characters escaped.
    pub(crate) fn output_globs(&self) -> Result<Vec<String>, String> {
        let not_globs = || "gate: box payload Outputs is not a list of texts".to_owned();
        let list = match self.0.get("Outputs") {
            None | Some(Value::Null) => return Ok(Vec::new()),
            Some(value) => value.as_array().ok_or_else(not_globs)?,
        };
        list.iter()
            .map(|glob| match glob.as_str() {
                None => Err(not_globs()),
                Some("") => Err("gate: box payload Outputs holds an empty glob".to_owned()),
                Some(glob) if glob.contains(['[', ']']) => Err(format!("gate: box payload Outputs glob '{}' holds '[' or ']'", escaped(glob))),
                Some(glob) => Ok(glob.to_owned()),
            })
            .collect()
    }

    /// Whether `OutputsFull` is set, as the client sets it when the PC's last outputs step did not complete; false when
    /// the key is absent or null. Else the usage line saying it is not true or false.
    pub(crate) fn outputs_full(&self) -> Result<bool, String> {
        match self.0.get("OutputsFull") {
            None | Some(Value::Null) => Ok(false),
            Some(Value::Bool(full)) => Ok(*full),
            Some(_) => Err("gate: box payload OutputsFull is not true or false".to_owned()),
        }
    }

    /// The run's outputs `Nonce`: 32 lower-case hex digits, as the client draws it; else the usage line, the value
    /// shown with its control characters escaped.
    pub(crate) fn nonce(&self) -> Result<&str, String> {
        let nonce = self.text("Nonce")?;
        if is_lower_hex(nonce, OUTPUTS_NONCE_DIGITS) {
            Ok(nonce)
        } else {
            Err(format!("gate: box payload Nonce is not 32 lower-case hex digits: {}", escaped(nonce)))
        }
    }

    /// The run's ceiling, `Timeout`: whole seconds from 1 to 2147483647, the load-adjusted time the box gives the
    /// command; `None` when the key is absent or null, a run with no ceiling on the box. Else the usage line.
    pub(crate) fn timeout(&self) -> Result<Option<u64>, String> {
        match self.0.get("Timeout") {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_u64()
                .filter(|seconds| (1..=MAX_WHOLE_NUMBER).contains(seconds))
                .map(Some)
                .ok_or_else(|| "gate: box payload Timeout is not a whole number of seconds".to_owned()),
        }
    }

    /// The run's `InputsDigest`, the SHA-256 of the inputs manifest line the client sent the work folder's inputs
    /// with: 64 lower-case hex digits; `None` when the key is absent or null. Else the usage line, the value shown with
    /// its control characters escaped.
    pub(crate) fn inputs_digest(&self) -> Result<Option<&str>, String> {
        match self.0.get("InputsDigest") {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(digest)) if is_lower_hex(digest, SHA256_DIGITS) => Ok(Some(digest)),
            Some(other) => Err(format!(
                "gate: box payload InputsDigest is not 64 lower-case hex digits: {}",
                escaped(&other.as_str().map_or_else(|| other.to_string(), str::to_owned))
            )),
        }
    }

    /// Whether the payload sets `key` to anything but null.
    pub(crate) fn sets(&self, key: &str) -> bool {
        !matches!(self.0.get(key), None | Some(Value::Null))
    }

    /// The run's `Snapshot`: the payload's commit, exactly 40 lowercase hex digits, as the client sends it; else the
    /// line saying it is not one. This is PowerShell's `-cmatch '^[0-9a-f]{40}$'` without .NET's allowance for a
    /// trailing newline. A missing key and a JSON null are shown as the empty string, any other non-string value as its
    /// compact JSON text, each with its control characters escaped, so a value cannot break or forge the line.
    pub(crate) fn snapshot(&self) -> Result<&str, String> {
        commit(self.0.get("Snapshot")).map_err(|shown| not_a_snapshot("Snapshot", &shown))
    }
}

/// A sibling pair as the box lays it out: the main checkout's folder name, `Leaf`, and each sibling with the bare
/// repository its snapshot is pushed to.
pub(crate) struct Pair {
    leaf: String,
    siblings: Vec<PairSibling>,
}

impl Pair {
    /// The main checkout's folder name.
    pub(crate) fn leaf(&self) -> &str {
        &self.leaf
    }

    /// The siblings, in the payload's order.
    pub(crate) fn siblings(&self) -> &[PairSibling] {
        &self.siblings
    }
}

/// One sibling of a [`Pair`]: its folder name and its bare repository, `{root}\{repo_name(name)}.git` beside the main's,
/// derived on the box and never read from the payload.
pub(crate) struct PairSibling {
    name: String,
    bare: String,
}

impl PairSibling {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn bare(&self) -> &str {
        &self.bare
    }
}

/// The sibling pair checks: the names the box lays out, the key they share and the bare repositories beside `Work`.
impl Payload {
    /// The run's sibling `Pair`, `{"Leaf": "<name>", "Siblings": ["<name>", …]}`; `None` when the key is absent or null,
    /// a run of one repo. Else the usage line: a name the box cannot lay out ([`is_layout_name`]), no sibling or more
    /// than [`MOST_SIBLINGS`], two names (the leaf included) equal ignoring ASCII case, a `Key` that is not the leaf's
    /// (`<repo_name(leaf)>-<8 hex>`), a `Bare` that is not `<root>\<name>.git` for the work folder `<root>\work\<key>`,
    /// or two bare repositories (the main's and each sibling's `<repo_name(name)>.git`) equal ignoring ASCII case.
    pub(crate) fn pair(&self) -> Result<Option<Pair>, String> {
        let (leaf, names) = match self.0.get("Pair") {
            None | Some(Value::Null) => return Ok(None),
            Some(value) => pair_names(value)?,
        };
        check_pair_names(&leaf, &names)?;
        let (key, work) = self.work_folder()?;
        if !is_key_of(key, &leaf) {
            return Err(format!(
                "gate: box payload Key {key} is not the work-folder key of the Pair's Leaf {leaf}"
            ));
        }
        let bare = self.path("Bare")?;
        let Some(root) = grandparent(work) else {
            return Err(format!(
                r"gate: box payload Work must be <root>\work\<key> with a Pair: {}",
                escaped(work)
            ));
        };
        let Some(main_stem) = bare_stem(bare, root) else {
            return Err(format!(
                r"gate: box payload Bare must be {root}\<name>.git with a Pair: {}",
                escaped(bare)
            ));
        };
        let mut stems = vec![main_stem.to_owned()];
        let mut siblings = Vec::with_capacity(names.len());
        for name in names {
            let stem = repo_name(&name);
            if stems.iter().any(|taken| taken.eq_ignore_ascii_case(&stem)) {
                return Err(format!(
                    "gate: box payload Pair sibling {name} would share the bare repository {stem}.git"
                ));
            }
            siblings.push(PairSibling {
                bare: format!(r"{root}\{stem}.git"),
                name,
            });
            stems.push(stem);
        }
        Ok(Some(Pair { leaf, siblings }))
    }

    /// The commit of each sibling of `pair`, in its order, from the run's `SiblingSnapshots`, an object whose names are
    /// exactly the siblings' and whose values are each 40 lowercase hex digits, as [`Payload::snapshot`] reads them;
    /// else the line saying why.
    pub(crate) fn sibling_snapshots(&self, pair: &Pair) -> Result<Vec<&str>, String> {
        let object = match self.0.get("SiblingSnapshots") {
            None | Some(Value::Null) => None,
            Some(Value::Object(object)) => Some(object),
            Some(_) => return Err("gate: box payload SiblingSnapshots is not an object".to_owned()),
        };
        // A pair has a sibling, so no SiblingSnapshots never names them all.
        let named =
            object.filter(|object| object.len() == pair.siblings.len() && pair.siblings.iter().all(|sibling| object.contains_key(&sibling.name)));
        let Some(object) = named else {
            let names: Vec<&str> = pair.siblings.iter().map(PairSibling::name).collect();
            return Err(format!(
                "gate: box payload SiblingSnapshots must name exactly the Pair's siblings: {}",
                names.join(", ")
            ));
        };
        pair.siblings
            .iter()
            .map(|sibling| commit(object.get(&sibling.name)).map_err(|shown| not_a_snapshot(&format!("SiblingSnapshots {}", sibling.name), &shown)))
            .collect()
    }
}

/// The leaf and the sibling names of the `Pair` value `value`; else the usage line saying it is not one.
fn pair_names(value: &Value) -> Result<(String, Vec<String>), String> {
    let not_a_pair = || "gate: box payload Pair is not an object of a Leaf text and a Siblings list of texts".to_owned();
    let object = value.as_object().ok_or_else(not_a_pair)?;
    let leaf = object.get("Leaf").and_then(Value::as_str).ok_or_else(not_a_pair)?;
    let names = object.get("Siblings").and_then(Value::as_array).ok_or_else(not_a_pair)?;
    let names = names
        .iter()
        .map(|name| name.as_str().map(str::to_owned).ok_or_else(not_a_pair))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((leaf.to_owned(), names))
}

/// Checks the pair's names: each one the box can lay out, 1 to [`MOST_SIBLINGS`] siblings, and no two, the leaf
/// included, equal ignoring ASCII case; else the usage line.
fn check_pair_names(leaf: &str, names: &[String]) -> Result<(), String> {
    if let Some(name) = std::iter::once(leaf)
        .chain(names.iter().map(String::as_str))
        .find(|name| !is_layout_name(name))
    {
        return Err(format!(
            "gate: box payload Pair name '{}' is not a folder name of letters, digits, '.', '_' and '-' not starting or ending \
             with a dot, not a device name",
            escaped(name)
        ));
    }
    if names.is_empty() {
        return Err("gate: box payload Pair names no sibling".to_owned());
    }
    if names.len() > MOST_SIBLINGS {
        return Err(format!("gate: box payload Pair names more than {MOST_SIBLINGS} siblings"));
    }
    for (index, name) in names.iter().enumerate() {
        let mut earlier = std::iter::once(leaf).chain(names[..index].iter().map(String::as_str));
        if let Some(earlier) = earlier.find(|earlier| earlier.eq_ignore_ascii_case(name)) {
            return Err(format!("gate: box payload Pair names {name} and {earlier}, equal ignoring case"));
        }
    }
    Ok(())
}

/// Whether `key` is the work-folder key of a root whose folder name is `leaf`: `<repo_name(leaf)>-<8 lower-case hex>`.
fn is_key_of(key: &str, leaf: &str) -> bool {
    let name = repo_name(leaf);
    key.strip_prefix(name.as_str())
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|digits| is_lower_hex(digits, KEY_HASH_DIGITS))
}

/// The stem of the bare repository `bare` when it is `<root>\<stem>.git`, `<stem>` one or more ASCII letters, digits,
/// `-` and `_`, its folder compared with `root` as full paths without regard to case; else `None`.
pub(super) fn bare_stem<'a>(bare: &'a str, root: &str) -> Option<&'a str> {
    let split = bare.rfind(['\\', '/'])?;
    let (folder, file) = (&bare[..split], &bare[split + 1..]);
    let dot = file.len().checked_sub(".git".len())?;
    let (stem, suffix) = (file.get(..dot)?, file.get(dot..)?);
    (suffix.eq_ignore_ascii_case(".git") && is_key(stem) && same_path(folder, root)).then_some(stem)
}

/// The commit `value` names: exactly 40 lowercase hex digits; else the value as a refusal line shows it, a missing key
/// and a JSON null as the empty string and any other non-string value as its compact JSON text.
fn commit(value: Option<&Value>) -> Result<&str, String> {
    match value {
        Some(Value::String(text)) if is_lower_hex(text, COMMIT_SHA_DIGITS) => Ok(text),
        Some(Value::String(text)) => Err(text.clone()),
        Some(Value::Null) | None => Err(String::new()),
        Some(other) => Err(other.to_string()),
    }
}

/// The client's work-folder checks: the key, the work folder named for it, and the run's job and files beside it.
impl Payload {
    /// The work-folder key: ASCII letters, digits, `-` and `_`, as the client builds it from the repo's name; else the
    /// usage line, the key shown with its control characters escaped.
    pub(crate) fn key(&self) -> Result<&str, String> {
        let key = self.text("Key")?;
        if is_key(key) { Ok(key) } else { Err(not_a_key(key)) }
    }

    /// The key and the work folder: `Work` an absolute path on a drive, with no trailing separator, whose last name is
    /// the key; else the usage line.
    pub(crate) fn work_folder(&self) -> Result<(&str, &str), String> {
        let work = self.path("Work")?;
        if work.ends_with(['\\', '/']) {
            return Err(format!("gate: box payload Work must not end with a separator: {}", escaped(work)));
        }
        let key = self.key()?;
        let last = work.rsplit(['\\', '/']).next();
        if last.is_some_and(|last| last.eq_ignore_ascii_case(key)) {
            Ok((key, work))
        } else {
            Err(not_a_key(key))
        }
    }

    /// The path of `key`, which must be `<work>.<extension>`, as the client builds the lock and the index; compared as
    /// full paths without regard to case. Else the usage line.
    pub(crate) fn beside_work(&self, key: &str, work: &str, extension: &str) -> Result<&str, String> {
        let value = self.path(key)?;
        let expected = format!("{work}.{extension}");
        if same_path(value, &expected) {
            Ok(value)
        } else {
            Err(format!("gate: box payload {key} must be {expected}"))
        }
    }

    /// The `OutputFolder`, which must be `<root>\outputs` for the work folder `work`, `<root>\work\<key>`, as the client
    /// builds it; compared as full paths without regard to case. Else the usage line.
    pub(crate) fn output_folder(&self, work: &str) -> Result<&str, String> {
        let value = self.path("OutputFolder")?;
        let Some(root) = grandparent(work) else {
            return Err(format!(
                r"gate: box payload Work must be <root>\work\<key> to place OutputFolder: {}",
                escaped(work)
            ));
        };
        let expected = format!(r"{root}\outputs");
        if same_path(value, &expected) {
            Ok(value)
        } else {
            Err(format!("gate: box payload OutputFolder must be {expected}"))
        }
    }

    /// The outputs archive's `Name`: `<key>-<nonce>.tar.zst` with the payload's own key and an outputs nonce, as the box
    /// names a run's archive; else the line refusing it, the name shown with its control characters escaped, or the
    /// usage line saying the payload lacks it. The outputs verb can so reach no file in `OutputFolder` but an archive of
    /// this work folder.
    pub(crate) fn archive_name(&self) -> Result<&str, String> {
        let key = self.key()?;
        let name = self.text("Name")?;
        if archive_key(name) == Some(key) {
            Ok(name)
        } else {
            Err(format!(
                "gate: remote outputs archive name '{}' is not an outputs archive of this work folder",
                escaped(name)
            ))
        }
    }

    /// The run's job name, `Local\gate-remote-<key>-<nonce>` with the payload's own key and a nonce of 12 hex digits,
    /// as the client names it; else the usage line. A stop can so reach no job but a run's of this work folder.
    pub(crate) fn job(&self, key: &str) -> Result<&str, String> {
        let job = self.text("Job")?;
        let nonce = job
            .strip_prefix(r"Local\gate-remote-")
            .and_then(|rest| rest.strip_prefix(key))
            .and_then(|rest| rest.strip_prefix('-'));
        if nonce.is_some_and(|nonce| nonce.len() == NONCE_DIGITS && nonce.chars().all(|digit| digit.is_ascii_hexdigit())) {
            Ok(job)
        } else {
            Err(format!("gate: box payload Job is not this work folder's job name: {}", escaped(job)))
        }
    }
}

/// The hex digits of a run's nonce, as the client draws it.
const NONCE_DIGITS: usize = 12;
/// The hex digits of a run's outputs nonce, as the client draws it.
pub(crate) const OUTPUTS_NONCE_DIGITS: usize = 32;
/// The hex digits of a commit's sha.
const COMMIT_SHA_DIGITS: usize = 40;
/// The hex digits that end a work-folder key, after the repo name and a `-`.
const KEY_HASH_DIGITS: usize = 8;

/// Whether `text` is a work-folder key: one or more ASCII letters, digits, `-` and `_`.
fn is_key(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

/// The name of the outputs archive of the work folder `key` for the outputs nonce `nonce`: `<key>-<nonce>.tar.zst`.
/// [`archive_key`] reads it back.
pub(crate) fn archive_file_name(key: &str, nonce: &str) -> String {
    format!("{key}-{nonce}{SUFFIX}")
}

/// The work-folder key of `name` when it is an outputs archive's name as [`archive_file_name`] makes it, with a
/// work-folder key and an outputs nonce of 32 lower-case hex digits; else `None`.
pub(crate) fn archive_key(name: &str) -> Option<&str> {
    let stem = name.strip_suffix(SUFFIX)?;
    let split = stem.len().checked_sub(OUTPUTS_NONCE_DIGITS + 1)?;
    let key = stem.get(..split)?;
    let nonce = stem.get(split..)?.strip_prefix('-')?;
    (is_key(key) && is_lower_hex(nonce, OUTPUTS_NONCE_DIGITS)).then_some(key)
}

/// The `<root>` of the work folder `path`, `<root>\work\<key>` with `work` in any case, without a trailing separator;
/// `None` when `path` is not one.
pub(crate) fn grandparent(path: &str) -> Option<&str> {
    let parent = &path[..path.rfind(['\\', '/'])?];
    let split = parent.rfind(['\\', '/'])?;
    let (root, middle) = (&parent[..split], &parent[split + 1..]);
    (!root.is_empty() && middle.eq_ignore_ascii_case("work")).then_some(root)
}

/// Whether `first` and `second` are the same path once made full, without regard to case. Never when a part of
/// `first` other than `.` or `..` ends in `.` or a space, which making the path full would drop, so the path that
/// passes is the one opened.
fn same_path(first: &str, second: &str) -> bool {
    let renamed = first
        .split(['\\', '/'])
        .any(|part| !matches!(part, "." | "..") && part.ends_with(['.', ' ']));
    let full = |path: &str| std::path::absolute(path).map(|full| ordinal_upper(&full.to_string_lossy())).ok();
    !renamed && matches!((full(first), full(second)), (Some(first_full), Some(second_full)) if first_full == second_full)
}

fn not_a_key(key: &str) -> String {
    format!("gate: box payload Key is not a work-folder key: {}", escaped(key))
}

/// The line refusing a run whose payload named no 40-hex commit as the snapshot `what`, showing `value` as it is.
fn not_a_snapshot(what: &str, value: &str) -> String {
    format!("gate: remote run payload's {what} '{}' is not a 40-hex commit sha", escaped(value))
}

/// `text` with each character mapped to its simple uppercase, as .NET's `OrdinalIgnoreCase` compares: a character
/// whose uppercase is more than one character is kept.
pub(crate) fn ordinal_upper(text: &str) -> String {
    text.chars()
        .map(|character| {
            let mut upper = character.to_uppercase();
            match (upper.next(), upper.next()) {
                (Some(single), None) => single,
                _ => character,
            }
        })
        .collect()
}

fn is_drive_path(value: &str) -> bool {
    let rooted = matches!(value.as_bytes(), [drive, b':', b'\\' | b'/', ..] if drive.is_ascii_alphabetic());
    rooted && !value.chars().any(|character| character < ' ')
}

fn object(word: &str) -> Result<Map<String, Value>, String> {
    let bytes = STANDARD.decode(word).map_err(|error| error.to_string())?;
    let json = String::from_utf8(bytes).map_err(|error| format!("the decoded bytes are not UTF-8: {error}"))?;
    match serde_json::from_str(&json).map_err(|error| error.to_string())? {
        Value::Object(map) => Ok(map),
        _ => Err("the JSON is not an object".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde_json::json;

    use super::{Payload, escaped};

    fn decoded(json: &str) -> Result<Payload, String> {
        Payload::decode(&STANDARD.encode(json))
    }

    fn text(json: &str, key: &str) -> Result<String, String> {
        decoded(json).and_then(|payload| payload.text(key).map(str::to_owned))
    }

    const NOT_BASE64_JSON: &str = "gate: box payload is not base64 JSON: ";

    /// Checks `refused` is the not-base64-JSON line with a reason after its prefix; the reason's wording is the
    /// decoder's own.
    fn assert_names_a_reason(refused: Result<Payload, String>, what: &str) {
        let line = refused.err();
        assert!(
            line.as_deref()
                .is_some_and(|line| line.len() > NOT_BASE64_JSON.len() && line.starts_with(NOT_BASE64_JSON)),
            "{what}: a reason follows the prefix, got {line:?}"
        );
    }

    #[test]
    fn a_word_that_is_not_base64_json_names_why() {
        assert_names_a_reason(Payload::decode("not base64"), "not base64");
        assert_names_a_reason(Payload::decode(&STANDARD.encode([0xFF, 0xFE])), "not UTF-8");
        assert_names_a_reason(decoded("{\"Work\":"), "not JSON");
        let unpadded = STANDARD.encode("{}").trim_end_matches('=').to_owned();
        assert_names_a_reason(Payload::decode(&unpadded), "the standard engine wants the padding");
        assert_eq!(
            decoded("[\"Work\"]").err(),
            Some("gate: box payload is not base64 JSON: the JSON is not an object".to_owned())
        );
    }

    fn path(value: &str) -> Result<String, String> {
        decoded(&json!({ "Work": value }).to_string()).and_then(|payload| payload.path("Work").map(str::to_owned))
    }

    #[test]
    fn a_path_is_absolute_on_a_drive_with_no_control_character() {
        for good in [r"C:\w", "c:/w", r"X:\dev\a b\k.git"] {
            assert_eq!(path(good), Ok(good.to_owned()), "{good}");
        }
        for bad in [
            "w",
            r"remote\k",
            r"\w",
            "C:w",
            "C:",
            r"\\server\share\w",
            r"\\?\C:\w",
            "//server/share/w",
            "-bare",
            "--upload-pack=C:/x",
            "C:\\w\nx",
            "C:\\w\u{1}",
            "C:\\w\u{1f}",
            "1:\\w",
        ] {
            assert_eq!(
                path(bad),
                Err(format!("gate: box payload Work is not an absolute path on a drive: {bad:?}")),
                "{bad:?}"
            );
        }
        assert_eq!(path(""), Err("gate: box payload lacks Work".to_owned()), "an empty path is lacking");
    }

    #[test]
    fn a_missing_empty_or_non_text_key_is_lacking_and_other_keys_are_ignored() {
        let json = r#"{"Bare":"C:\\b.git","Work":"C:\\w","Job":"j","Unknown":[1]}"#;
        assert_eq!(text(json, "Bare"), Ok(r"C:\b.git".to_owned()));
        assert_eq!(text(json, "Work"), Ok(r"C:\w".to_owned()));
        assert_eq!(text(r#"{"Bare":"b"}"#, "Work"), Err("gate: box payload lacks Work".to_owned()));
        assert_eq!(text(r#"{"Work":"w"}"#, "Bare"), Err("gate: box payload lacks Bare".to_owned()));
        assert_eq!(text(r#"{"Work":""}"#, "Work"), Err("gate: box payload lacks Work".to_owned()));
        assert_eq!(text(r#"{"Work":3}"#, "Work"), Err("gate: box payload lacks Work".to_owned()));
        assert_eq!(
            text(r#"{"work":"w"}"#, "Work"),
            Err("gate: box payload lacks Work".to_owned()),
            "names are exact"
        );
    }

    /// The run's snapshot of the JSON `json`: its text, or the line refusing it.
    fn snapshot(json: &str) -> Result<String, String> {
        decoded(json).and_then(|payload| payload.snapshot().map(str::to_owned))
    }

    /// The refusal line a snapshot value is shown in.
    fn refusing(value: &str) -> String {
        format!("gate: remote run payload's Snapshot '{value}' is not a 40-hex commit sha")
    }

    #[test]
    fn a_snapshot_is_exactly_40_lowercase_hex_digits() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            snapshot(&format!(r#"{{"Snapshot":"{sha}"}}"#)),
            Ok(sha.to_owned()),
            "40 lowercase hex digits"
        );
        assert_eq!(
            snapshot(&format!(r#"{{"Snapshot":"{sha}\n"}}"#)),
            Err(refusing(&format!("{sha}\\n"))),
            "a line end after the sha is not allowed"
        );
        assert_eq!(
            snapshot(&format!(r#"{{"Snapshot":"{sha}0"}}"#)),
            Err(refusing(&format!("{sha}0"))),
            "41 of them"
        );
        let dashes = "-".repeat(40);
        assert_eq!(snapshot(&format!(r#"{{"Snapshot":"{dashes}"}}"#)), Err(refusing(&dashes)), "40 non-hex");
        let gees = "g".repeat(40);
        assert_eq!(snapshot(&format!(r#"{{"Snapshot":"{gees}"}}"#)), Err(refusing(&gees)), "a non-hex letter");
        assert_eq!(snapshot(r#"{"Snapshot":""}"#), Err(refusing("")), "the empty string");
        assert_eq!(snapshot(r#"{"Snapshot":null}"#), Err(refusing("")), "null shows nothing");
    }

    #[test]
    fn a_snapshot_that_is_not_a_string_is_shown_as_its_compact_json() {
        assert_eq!(snapshot(r#"{"Snapshot":3}"#), Err(refusing("3")), "a number");
        assert_eq!(snapshot(r#"{"Snapshot":true}"#), Err(refusing("true")), "a boolean");
        assert_eq!(snapshot(r#"{"Snapshot":["x"]}"#), Err(refusing(r#"["x"]"#)), "an array");
        assert_eq!(snapshot(r#"{"Snapshot":{"a":1}}"#), Err(refusing(r#"{"a":1}"#)), "an object");
    }

    #[test]
    fn a_snapshot_line_escapes_every_control_character() {
        // ESC, CR, LF, DEL and U+009B, in a string value and in a non-string one.
        let controls = "\u{1b}\r\n\u{7f}\u{9b}";
        assert_eq!(
            snapshot(&json!({ "Snapshot": controls }).to_string()),
            Err(refusing(r"\u{1b}\r\n\u{7f}\u{9b}")),
            "a string value"
        );
        assert_eq!(
            snapshot(&json!({ "Snapshot": ["\u{1b}[31m"] }).to_string()),
            Err(refusing(r#"["\u001b[31m"]"#)),
            "a non-string value is shown as its compact JSON text"
        );
    }

    fn output_globs(json: &serde_json::Value) -> Result<Vec<String>, String> {
        decoded(&json.to_string()).and_then(|payload| payload.output_globs())
    }

    #[test]
    fn output_globs_are_a_list_of_texts_none_when_absent() {
        assert_eq!(output_globs(&json!({})), Ok(vec![]), "absent");
        assert_eq!(output_globs(&json!({ "Outputs": null })), Ok(vec![]), "null");
        assert_eq!(output_globs(&json!({ "Outputs": [] })), Ok(vec![]), "an empty list");
        assert_eq!(
            output_globs(&json!({ "Outputs": ["**/bin/**", "*.dll"] })),
            Ok(vec!["**/bin/**".to_owned(), "*.dll".to_owned()])
        );
        let not_a_list = Err("gate: box payload Outputs is not a list of texts".to_owned());
        assert_eq!(output_globs(&json!({ "Outputs": "**/bin/**" })), not_a_list, "a text");
        assert_eq!(output_globs(&json!({ "Outputs": ["a", 1] })), not_a_list, "a number in the list");
        assert_eq!(
            output_globs(&json!({ "Outputs": ["a", ""] })),
            Err("gate: box payload Outputs holds an empty glob".to_owned())
        );
        assert_eq!(
            output_globs(&json!({ "Outputs": ["bin/[ab].dll"] })),
            Err("gate: box payload Outputs glob 'bin/[ab].dll' holds '[' or ']'".to_owned())
        );
        assert_eq!(
            output_globs(&json!({ "Outputs": ["\u{1b}]"] })),
            Err(r"gate: box payload Outputs glob '\u{1b}]' holds '[' or ']'".to_owned()),
            "a glob shown with its control characters escaped"
        );
    }

    #[test]
    fn outputs_full_is_true_or_false_and_false_when_absent() {
        let full = |json: serde_json::Value| decoded(&json.to_string()).and_then(|payload| payload.outputs_full());
        assert_eq!(full(json!({})), Ok(false), "absent");
        assert_eq!(full(json!({ "OutputsFull": null })), Ok(false), "null");
        assert_eq!(full(json!({ "OutputsFull": true })), Ok(true));
        assert_eq!(full(json!({ "OutputsFull": false })), Ok(false));
        for bad in [json!("true"), json!(1), json!([true])] {
            assert_eq!(
                full(json!({ "OutputsFull": bad })),
                Err("gate: box payload OutputsFull is not true or false".to_owned()),
                "{bad}"
            );
        }
    }

    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn an_outputs_nonce_is_32_lower_case_hex_digits() {
        let nonce = |json: serde_json::Value| decoded(&json.to_string()).and_then(|payload| payload.nonce().map(str::to_owned));
        assert_eq!(nonce(json!({ "Nonce": NONCE })), Ok(NONCE.to_owned()));
        assert_eq!(nonce(json!({})), Err("gate: box payload lacks Nonce".to_owned()));
        for bad in [
            NONCE.to_uppercase(),
            NONCE[1..].to_owned(),
            format!("{NONCE}0"),
            format!("{NONCE}\n"),
            "n1".to_owned(),
        ] {
            assert_eq!(
                nonce(json!({ "Nonce": bad })),
                Err(format!("gate: box payload Nonce is not 32 lower-case hex digits: {}", escaped(&bad))),
                "{bad:?}"
            );
        }
    }

    const WORK: &str = r"D:\gate-remote\work\app-1a2b3c4d";

    fn output_folder(work: &str, folder: &str) -> Result<String, String> {
        decoded(&json!({ "OutputFolder": folder }).to_string()).and_then(|payload| payload.output_folder(work).map(str::to_owned))
    }

    #[test]
    fn the_output_folder_is_outputs_beside_the_work_folders_parent() {
        for good in [r"D:\gate-remote\outputs", "d:/GATE-REMOTE/outputs", r"D:\gate-remote\work\..\outputs"] {
            assert_eq!(output_folder(WORK, good), Ok(good.to_owned()), "{good}");
        }
        assert_eq!(
            output_folder(r"C:\work\k", r"C:\outputs"),
            Ok(r"C:\outputs".to_owned()),
            "a root on the drive"
        );
        let refused = Err(r"gate: box payload OutputFolder must be D:\gate-remote\outputs".to_owned());
        for bad in [
            r"D:\gate-remote\work\outputs",
            r"D:\other\outputs",
            r"D:\gate-remote\outputs\x",
            r"D:\gate-remote",
        ] {
            assert_eq!(output_folder(WORK, bad), refused, "{bad}");
        }
        assert_eq!(
            output_folder(WORK, "outputs"),
            Err("gate: box payload OutputFolder is not an absolute path on a drive: \"outputs\"".to_owned())
        );
        assert_eq!(output_folder(WORK, ""), Err("gate: box payload lacks OutputFolder".to_owned()));
        assert_eq!(
            output_folder(r"D:\app-1a2b3c4d", r"D:\outputs"),
            Err(r"gate: box payload Work must be <root>\work\<key> to place OutputFolder: D:\app-1a2b3c4d".to_owned())
        );
    }

    fn archive_name(name: &str) -> Result<String, String> {
        decoded(&json!({ "Key": "app-1a2b3c4d", "Name": name }).to_string()).and_then(|payload| payload.archive_name().map(str::to_owned))
    }

    #[test]
    fn grandparent_needs_a_work_folder() {
        let outputs = r"D:\gate-remote\outputs";
        assert_eq!(
            output_folder(r"D:\gate-remote\WORK\app-1a2b3c4d", outputs),
            Ok(outputs.to_owned()),
            "work in any case"
        );
        for work in [
            r"D:\gate-remote\other\app-1a2b3c4d",
            r"D:\gate-remote\works\app-1a2b3c4d",
            r"D:\gate-remote\work.\app-1a2b3c4d",
        ] {
            assert_eq!(
                output_folder(work, outputs),
                Err(format!(r"gate: box payload Work must be <root>\work\<key> to place OutputFolder: {work}")),
                "{work}"
            );
        }
    }

    #[test]
    fn a_path_with_a_part_ending_in_a_dot_or_space_is_not_the_one_expected() {
        let refused = Err(r"gate: box payload OutputFolder must be D:\gate-remote\outputs".to_owned());
        for bad in [
            r"D:\gate-remote\outputs.",
            r"D:\gate-remote\outputs ",
            r"D:\gate-remote.\outputs",
            r"D:\gate-remote \outputs",
            "D:/gate-remote/outputs..",
        ] {
            assert_eq!(output_folder(WORK, bad), refused, "{bad:?}");
        }
    }

    #[test]
    fn an_archive_name_needs_a_work_folder_key() {
        let name = format!("app-1a2b3c4d-{NONCE}.tar.zst");
        let named = |json: serde_json::Value| decoded(&json.to_string()).and_then(|payload| payload.archive_name().map(str::to_owned));
        assert_eq!(named(json!({ "Name": name })), Err("gate: box payload lacks Key".to_owned()));
        assert_eq!(
            named(json!({ "Key": "app 1", "Name": name })),
            Err("gate: box payload Key is not a work-folder key: app 1".to_owned())
        );
    }

    #[test]
    fn an_archive_name_is_the_work_folders_key_and_an_outputs_nonce() {
        let good = format!("app-1a2b3c4d-{NONCE}.tar.zst");
        assert_eq!(archive_name(&good), Ok(good.clone()));
        for bad in [
            format!("app-1a2b3c4d-{NONCE}.tar.xz"),
            format!("other-{NONCE}.tar.zst"),
            format!("APP-1A2B3C4D-{NONCE}.tar.zst"),
            format!("app-1a2b3c4d-{}.tar.zst", NONCE.to_uppercase()),
            format!("app-1a2b3c4d-{}.tar.zst", &NONCE[1..]),
            format!(r"..\app-1a2b3c4d-{NONCE}.tar.zst"),
            format!("app-1a2b3c4d-{NONCE}.tar.zst\n"),
            format!("app-1a2b3c4d{NONCE}.tar.zst"),
        ] {
            assert_eq!(
                archive_name(&bad),
                Err(format!(
                    "gate: remote outputs archive name '{}' is not an outputs archive of this work folder",
                    escaped(&bad)
                )),
                "{bad:?}"
            );
        }
        assert_eq!(archive_name(""), Err("gate: box payload lacks Name".to_owned()));
    }

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    /// A run payload of the work folder `app-1a2b3c4d` under `D:\gate-remote`, its bare repository `app.git`, with
    /// `pair` as its `Pair`.
    fn with_pair(pair: serde_json::Value) -> serde_json::Value {
        let mut payload = json!({
            "Key": "app-1a2b3c4d",
            "Work": WORK,
            "Bare": r"D:\gate-remote\app.git",
        });
        payload["Pair"] = pair;
        payload
    }

    /// A pair's leaf, and each sibling's name and bare repository.
    type LaidOut = (String, Vec<(String, String)>);

    /// The leaf and each sibling's name and bare repository the payload `json` lays out, `None` for no pair; else the
    /// line refusing it.
    fn laid_out(json: &serde_json::Value) -> Result<Option<LaidOut>, String> {
        let pair = decoded(&json.to_string())?.pair()?;
        Ok(pair.map(|pair| {
            let siblings = pair
                .siblings()
                .iter()
                .map(|sibling| (sibling.name().to_owned(), sibling.bare().to_owned()))
                .collect();
            (pair.leaf().to_owned(), siblings)
        }))
    }

    /// The refusal line of the payload `json`'s pair.
    fn refused_pair(json: &serde_json::Value) -> String {
        laid_out(json).err().unwrap_or_else(|| format!("not refused: {json}"))
    }

    #[test]
    fn a_pair_absent_or_null_is_a_run_of_one_repo() {
        let mut payload = with_pair(json!(null));
        assert_eq!(laid_out(&payload), Ok(None), "null");
        if let Some(object) = payload.as_object_mut() {
            object.remove("Pair");
        }
        assert_eq!(laid_out(&payload), Ok(None), "absent");
    }

    #[test]
    fn a_pair_puts_each_siblings_bare_repository_beside_the_main_ones() {
        let payload = with_pair(json!({"Leaf": "app", "Siblings": ["yaat-server", "a.b", "Lib_2"]}));
        let siblings = [
            ("yaat-server", r"D:\gate-remote\yaat-server.git"),
            ("a.b", r"D:\gate-remote\a-b.git"),
            ("Lib_2", r"D:\gate-remote\Lib_2.git"),
        ]
        .map(|(name, bare)| (name.to_owned(), bare.to_owned()));
        assert_eq!(laid_out(&payload), Ok(Some(("app".to_owned(), siblings.to_vec()))));
    }

    #[test]
    fn a_pair_that_is_not_an_object_of_a_leaf_and_a_list_of_siblings_is_refused() {
        let refused = "gate: box payload Pair is not an object of a Leaf text and a Siblings list of texts";
        for bad in [
            json!("app"),
            json!(["app"]),
            json!({"Siblings": ["lib"]}),
            json!({"Leaf": 1, "Siblings": ["lib"]}),
            json!({"Leaf": "app"}),
            json!({"Leaf": "app", "Siblings": "lib"}),
            json!({"Leaf": "app", "Siblings": ["lib", 1]}),
        ] {
            assert_eq!(refused_pair(&with_pair(bad.clone())), refused, "{bad}");
        }
    }

    #[test]
    fn a_pair_refuses_a_name_the_box_cannot_lay_out() {
        for bad in ["..", r"a\b", "nul.txt", ".x", "x.", ""] {
            let line = format!(
                "gate: box payload Pair name '{}' is not a folder name of letters, digits, '.', '_' and '-' not starting or \
                 ending with a dot, not a device name",
                escaped(bad)
            );
            assert_eq!(
                refused_pair(&with_pair(json!({"Leaf": "app", "Siblings": ["lib", bad]}))),
                line,
                "sibling {bad:?}"
            );
            assert_eq!(refused_pair(&with_pair(json!({"Leaf": bad, "Siblings": ["lib"]}))), line, "leaf {bad:?}");
        }
    }

    #[test]
    fn a_pair_names_one_to_eight_siblings() {
        let names = |count: usize| (1..=count).map(|number| format!("s{number}")).collect::<Vec<_>>();
        assert_eq!(
            refused_pair(&with_pair(json!({"Leaf": "app", "Siblings": []}))),
            "gate: box payload Pair names no sibling"
        );
        assert!(
            laid_out(&with_pair(json!({"Leaf": "app", "Siblings": names(8)}))).is_ok_and(|pair| pair.is_some()),
            "eight"
        );
        assert_eq!(
            refused_pair(&with_pair(json!({"Leaf": "app", "Siblings": names(9)}))),
            "gate: box payload Pair names more than 8 siblings"
        );
    }

    #[test]
    fn a_pair_refuses_names_equal_ignoring_case() {
        assert_eq!(
            refused_pair(&with_pair(json!({"Leaf": "main", "Siblings": ["App", "app"]}))),
            "gate: box payload Pair names app and App, equal ignoring case"
        );
        assert_eq!(
            refused_pair(&with_pair(json!({"Leaf": "app", "Siblings": ["lib", "APP"]}))),
            "gate: box payload Pair names APP and app, equal ignoring case",
            "a sibling named as the leaf"
        );
    }

    #[test]
    fn a_pair_refuses_siblings_sharing_a_bare_repository() {
        assert_eq!(
            refused_pair(&with_pair(json!({"Leaf": "app", "Siblings": ["a.b", "a-b"]}))),
            "gate: box payload Pair sibling a-b would share the bare repository a-b.git"
        );
        let mut worktree = with_pair(json!({"Leaf": "app-wt", "Siblings": ["App"]}));
        worktree["Key"] = json!("app-wt-1a2b3c4d");
        worktree["Work"] = json!(r"D:\gate-remote\work\app-wt-1a2b3c4d");
        assert_eq!(
            refused_pair(&worktree),
            "gate: box payload Pair sibling App would share the bare repository App.git",
            "a sibling named as the main checkout, whose bare is app.git"
        );
    }

    #[test]
    fn a_pair_needs_the_key_of_its_leaf() {
        assert!(laid_out(&with_pair(json!({"Leaf": "app", "Siblings": ["lib"]}))).is_ok_and(|pair| pair.is_some()));
        let refusing = |key: &str, leaf: &str| format!("gate: box payload Key {key} is not the work-folder key of the Pair's Leaf {leaf}");
        assert_eq!(
            refused_pair(&with_pair(json!({"Leaf": "other", "Siblings": ["lib"]}))),
            refusing("app-1a2b3c4d", "other")
        );
        assert_eq!(
            refused_pair(&with_pair(json!({"Leaf": "App", "Siblings": ["lib"]}))),
            refusing("app-1a2b3c4d", "App"),
            "the key keeps the leaf's case"
        );
        for key in ["app-1A2B3C4D", "app-1a2b3c4", "app-1a2b3c4d0", "app1a2b3c4d"] {
            let mut payload = with_pair(json!({"Leaf": "app", "Siblings": ["lib"]}));
            payload["Key"] = json!(key);
            payload["Work"] = json!(format!(r"D:\gate-remote\work\{key}"));
            assert_eq!(refused_pair(&payload), refusing(key, "app"), "{key}");
        }
    }

    #[test]
    fn a_pair_needs_a_bare_repository_beside_the_work_folders() {
        assert!(
            laid_out(&json!({"Key": "app-1a2b3c4d", "Work": WORK, "Bare": "d:/GATE-REMOTE/App.GIT", "Pair": {"Leaf": "app", "Siblings": ["lib"]}}))
                .is_ok_and(|pair| pair.is_some()),
            "compared without regard to case"
        );
        for bad in [
            r"D:\other\app.git",
            r"D:\gate-remote\app",
            r"D:\gate-remote\a b.git",
            r"D:\gate-remote\work\app.git",
            r"D:\gate-remote\.git",
            r"D:\gate-remote\a.b.git",
        ] {
            let mut payload = with_pair(json!({"Leaf": "app", "Siblings": ["lib"]}));
            payload["Bare"] = json!(bad);
            assert_eq!(
                refused_pair(&payload),
                format!(r"gate: box payload Bare must be D:\gate-remote\<name>.git with a Pair: {bad}"),
                "{bad}"
            );
        }
        let mut outside = with_pair(json!({"Leaf": "app", "Siblings": ["lib"]}));
        outside["Work"] = json!(r"D:\gate-remote\app-1a2b3c4d");
        assert_eq!(
            refused_pair(&outside),
            r"gate: box payload Work must be <root>\work\<key> with a Pair: D:\gate-remote\app-1a2b3c4d"
        );
    }

    /// The sibling commits of `snapshots` for a pair of `lib` and `tools`; else the line refusing them.
    fn sibling_snapshots(snapshots: &serde_json::Value) -> Result<Vec<String>, String> {
        let mut json = with_pair(json!({"Leaf": "app", "Siblings": ["lib", "tools"]}));
        json["SiblingSnapshots"] = snapshots.clone();
        let payload = decoded(&json.to_string())?;
        let pair = payload.pair()?.ok_or("a pair is given")?;
        payload
            .sibling_snapshots(&pair)
            .map(|commits| commits.into_iter().map(str::to_owned).collect())
    }

    #[test]
    fn sibling_snapshots_name_exactly_the_pairs_siblings() {
        let other = "fedcba9876543210fedcba9876543210fedcba98";
        assert_eq!(
            sibling_snapshots(&json!({"tools": other, "lib": SHA})),
            Ok(vec![SHA.to_owned(), other.to_owned()]),
            "in the pair's order"
        );
        let refused = Err("gate: box payload SiblingSnapshots must name exactly the Pair's siblings: lib, tools".to_owned());
        for bad in [
            json!(null),
            json!({}),
            json!({"lib": SHA}),
            json!({"lib": SHA, "tools": SHA, "app": SHA}),
            json!({"lib": SHA, "Tools": SHA}),
            json!({"lib": SHA, "other": SHA}),
        ] {
            assert_eq!(sibling_snapshots(&bad), refused, "{bad}");
        }
        assert_eq!(
            sibling_snapshots(&json!([SHA, SHA])),
            Err("gate: box payload SiblingSnapshots is not an object".to_owned())
        );
    }

    #[test]
    fn a_sibling_snapshot_is_exactly_40_lowercase_hex_digits() {
        let refusing = |value: &str| {
            Err(format!(
                "gate: remote run payload's SiblingSnapshots tools '{value}' is not a 40-hex commit sha"
            ))
        };
        let upper = SHA.to_uppercase();
        assert_eq!(sibling_snapshots(&json!({"lib": SHA, "tools": &upper})), refusing(&upper));
        assert_eq!(sibling_snapshots(&json!({"lib": SHA, "tools": &SHA[1..]})), refusing(&SHA[1..]));
        assert_eq!(sibling_snapshots(&json!({"lib": SHA, "tools": null})), refusing(""));
        assert_eq!(sibling_snapshots(&json!({"lib": SHA, "tools": 3})), refusing("3"));
        assert_eq!(sibling_snapshots(&json!({"lib": SHA, "tools": "\u{1b}"})), refusing(r"\u{1b}"));
    }
}
