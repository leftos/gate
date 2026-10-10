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
}
