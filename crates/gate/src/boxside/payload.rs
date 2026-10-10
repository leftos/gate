//! The payload of a `gate box` call: one base64 word (standard alphabet, padded) of UTF-8 JSON holding an object, as
//! the remote client sends it. Keys are read by their exact name; keys no verb reads are ignored.

use std::ffi::OsStr;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value};

/// The decoded JSON object.
pub(crate) struct Payload(Map<String, Value>);

impl Payload {
    /// Decodes the payload word; else the usage line saying why it cannot be read.
    pub(crate) fn decode(word: &OsStr) -> Result<Payload, String> {
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

    /// The run's `Snapshot`: the payload's commit, exactly 40 lowercase hex digits, as the client sends it; else the
    /// line saying it is not one. A missing key and a JSON null are shown as the empty string, any other non-string
    /// value as its compact JSON text, each with its control characters escaped, so a value cannot break or forge the
    /// line.
    pub(crate) fn snapshot(&self) -> Result<&str, String> {
        match self.0.get("Snapshot") {
            Some(Value::String(text)) if is_commit_sha(text) => Ok(text),
            Some(Value::String(text)) => Err(not_a_snapshot(text)),
            Some(Value::Null) | None => Err(not_a_snapshot("")),
            Some(other) => Err(not_a_snapshot(&other.to_string())),
        }
    }
}

/// The client's work-folder checks: the key, the work folder named for it, and the run's job and files beside it.
impl Payload {
    /// The work-folder key: ASCII letters, digits, `-` and `_`, as the client builds it from the repo's name; else the
    /// usage line, the key shown with its control characters escaped.
    pub(crate) fn key(&self) -> Result<&str, String> {
        let key = self.text("Key")?;
        if key
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        {
            Ok(key)
        } else {
            Err(not_a_key(key))
        }
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
        let full = |path: &str| std::path::absolute(path).map(|full| ordinal_upper(&full.to_string_lossy())).ok();
        match (full(value), full(&expected)) {
            (Some(value_full), Some(expected_full)) if value_full == expected_full => Ok(value),
            _ => Err(format!("gate: box payload {key} must be {expected}")),
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

fn not_a_key(key: &str) -> String {
    format!("gate: box payload Key is not a work-folder key: {}", escaped(key))
}

/// The line refusing a run whose payload named no 40-hex commit as its snapshot, showing `value` as it is.
fn not_a_snapshot(value: &str) -> String {
    format!("gate: remote run payload's Snapshot '{}' is not a 40-hex commit sha", escaped(value))
}

/// Whether `value` is exactly 40 lowercase hex digits: PowerShell's `-cmatch '^[0-9a-f]{40}$'`, but without .NET's
/// allowance for a trailing newline, so 40 hex digits followed by a line end are not a snapshot.
fn is_commit_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

/// `text` with each control character escaped, so a value cannot break or forge a line.
pub(crate) fn escaped(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                character.escape_debug().to_string()
            } else {
                character.to_string()
            }
        })
        .collect()
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

fn object(word: &OsStr) -> Result<Map<String, Value>, String> {
    let text = word.to_str().ok_or_else(|| "the word is not valid Unicode".to_owned())?;
    let bytes = STANDARD.decode(text).map_err(|error| error.to_string())?;
    let json = String::from_utf8(bytes).map_err(|error| format!("the decoded bytes are not UTF-8: {error}"))?;
    match serde_json::from_str(&json).map_err(|error| error.to_string())? {
        Value::Object(map) => Ok(map),
        _ => Err("the JSON is not an object".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde_json::json;

    use super::Payload;

    fn decoded(json: &str) -> Result<Payload, String> {
        Payload::decode(OsStr::new(&STANDARD.encode(json)))
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
        assert_names_a_reason(Payload::decode(OsStr::new("not base64")), "not base64");
        assert_names_a_reason(Payload::decode(OsStr::new(&STANDARD.encode([0xFF, 0xFE]))), "not UTF-8");
        assert_names_a_reason(decoded("{\"Work\":"), "not JSON");
        let unpadded = STANDARD.encode("{}").trim_end_matches('=').to_owned();
        assert_names_a_reason(Payload::decode(OsStr::new(&unpadded)), "the standard engine wants the padding");
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
}
