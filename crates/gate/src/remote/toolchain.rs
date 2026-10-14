//! The repo's toolchain manifest, `.gate/toolchain.toml`, and the pins a repo already declares in other files, read as
//! data: what a routed run's builder must have beyond the .NET SDK. Nothing here runs a process or reads a file; the
//! caller hands over each file's text.
//!
//! The manifest is a `[tools]` table of `name = "version"`, where a version is a minimum (`major[.minor[.patch]]`, its
//! missing parts 0) and `""` means present at any version, and a top-level `workloads` array of .NET workload ids. A
//! file that does not parse is unreadable whole; a bad entry is skipped alone, with a problem naming it. The pins read
//! beside it are each local dotnet tool's version in `.config/dotnet-tools.json` and the `Godot.NET.Sdk` version a
//! project file names.

use serde_json::Value as Json;
use toml::{Table, Value};

use crate::remote::parity::number;

/// The tool names the manifest knows, as its `[tools]` keys spell them.
pub(crate) const KNOWN_TOOLS: [&str; 7] = ["rust", "cargo-nextest", "node", "pnpm", "pwsh", "godot", "ffmpeg"];

/// The SDK a Godot .NET project names, its letter case aside as package ids are.
const GODOT_SDK: &str = "Godot.NET.Sdk";

/// The markup that holds no element, by the text that opens and closes it: a comment, a CDATA section, a processing
/// instruction (the XML declaration among them) and a declaration such as a DOCTYPE.
const NOT_ELEMENTS: [(&str, &str); 4] = [("<!--", "-->"), ("<![CDATA[", "]]>"), ("<?", "?>"), ("<!", ">")];

/// A version as the manifest spells a minimum, `major[.minor[.patch]]`, ordered by its parts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    /// The version `text` spells: one to three dot-separated numbers of ASCII digits, the missing ones 0; `None` for
    /// anything else, such as `>=1.2`, `1.2.x`, `latest`, `v24`, four parts or an empty part.
    pub(crate) fn parse(text: &str) -> Option<Version> {
        let parts = numbers(text)?;
        if parts.len() > 3 {
            return None;
        }
        Version::of(&parts)
    }

    /// The version whose leading parts are `parts`, the first three of them, the missing ones 0; `None` for no part.
    fn of(parts: &[u64]) -> Option<Version> {
        let (&major, rest) = parts.split_first()?;
        Some(Version {
            major,
            minor: rest.first().copied().unwrap_or(0),
            patch: rest.get(1).copied().unwrap_or(0),
        })
    }

    /// The release a package version `text` names: its numbers before any `-` prerelease label or `+` build metadata,
    /// the first three of them, as a package version may have four; `None` when that part is not dot-separated numbers.
    fn release_of(text: &str) -> Option<Version> {
        let release = text.find(['-', '+']).map_or(text, |at| &text[..at]);
        Version::of(&numbers(release)?)
    }
}

/// The version as `major.minor.patch`, every part written.
impl std::fmt::Display for Version {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Whether a tool found at the version `found` spells is at or past the minimum `min`; `None` when `found` is no
/// version [`Version::parse`] reads, so it cannot be compared.
pub(crate) fn meets(min: Version, found: &str) -> Option<bool> {
    Version::parse(found).map(|found| found >= min)
}

/// The dot-separated numbers `text` spells, each one or more ASCII digits; `None` when a part is not one.
fn numbers(text: &str) -> Option<Vec<u64>> {
    text.split('.').map(number).collect()
}

/// One `[tools]` entry of the manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ToolNeed {
    /// The tool's name, as its key spells it.
    pub name: String,
    /// The version it must be at or past; `None` when any version will do.
    pub min: Option<Version>,
    /// Whether the name is one of [`KNOWN_TOOLS`]; the caller skips an unknown one with a line.
    pub known: bool,
}

/// What a `.gate/toolchain.toml` declares.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Manifest {
    /// The `[tools]` entries that read, in file order.
    pub tools: Vec<ToolNeed>,
    /// The `workloads` ids that read, in file order.
    pub workloads: Vec<String>,
    /// One text per entry skipped as bad, in file order, such as `tools.node is not a version: ">=24"`.
    pub problems: Vec<String>,
}

/// The line said when the manifest cannot be read, `why` being the reason [`read`] or [`read_bytes`] gives.
pub(crate) fn unreadable_line(why: &str) -> String {
    format!("gate: cannot read .gate/toolchain.toml: {why}; skipping the manifest's checks")
}

/// The manifest whose file holds `bytes`; `Err` with the reason when they are not UTF-8 or do not parse.
pub(crate) fn read_bytes(bytes: &[u8]) -> Result<Manifest, String> {
    let text = std::str::from_utf8(bytes).map_err(|error| format!("not valid UTF-8 ({error})"))?;
    read(text)
}

/// The manifest whose text is `text`, a leading byte order mark skipped as the TOML parser skips it; `Err` with the
/// line, column and message of the failure when it is not TOML. A bad entry is skipped and named in
/// [`Manifest::problems`]: a tool whose value is not a string or not a version, a `tools` that is not a table, a
/// `workloads` that is not an array, a workload that is not a string or is empty, and a top-level key other than
/// `tools` and `workloads`.
pub(crate) fn read(text: &str) -> Result<Manifest, String> {
    let table = text.parse::<Table>().map_err(|error| parse_failure(text, &error))?;
    let mut manifest = Manifest::default();
    for (key, value) in table {
        match key.as_str() {
            "tools" => read_tools(value, &mut manifest),
            "workloads" => read_workloads(value, &mut manifest),
            _ => manifest
                .problems
                .push(format!("{} is not a key of the manifest (tools, workloads)", spelled(&key))),
        }
    }
    Ok(manifest)
}

/// `text` without a leading byte order mark.
fn without_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// The key `key` as TOML spells it: bare when it is one or more ASCII letters, digits, `_` and `-`, else quoted, its
/// line ends and other control characters escaped, so it is always one line.
pub(crate) fn spelled(key: &str) -> String {
    let bare = !key.is_empty() && key.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if bare { key.to_owned() } else { format!("{key:?}") }
}

/// `error`, a failure to parse `text`, as the line and column it starts at and its message; a leading byte order mark
/// takes no column.
fn parse_failure(text: &str, error: &toml::de::Error) -> String {
    let message = error.message().trim_end();
    let Some(span) = error.span() else {
        return message.to_owned();
    };
    let before = without_bom(text.get(..span.start).unwrap_or(text));
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().unwrap_or(before).chars().count() + 1;
    format!("line {line}, column {column}: {message}")
}

/// Reads the `tools` value `value` into `manifest`, each entry a tool or a problem.
fn read_tools(value: Value, manifest: &mut Manifest) {
    let tools = match value {
        Value::Table(tools) => tools,
        other => {
            manifest.problems.push(format!("tools is not a table (TOML {})", other.type_str()));
            return;
        }
    };
    for (name, value) in tools {
        match tool_need(&name, &value) {
            Ok(tool) => manifest.tools.push(tool),
            Err(problem) => manifest.problems.push(problem),
        }
    }
}

/// The tool `name` whose value is `value`; `Err` with its problem when the value is not a string or not a version.
fn tool_need(name: &str, value: &Value) -> Result<ToolNeed, String> {
    let Value::String(text) = value else {
        return Err(format!("tools.{} is not a string (TOML {})", spelled(name), value.type_str()));
    };
    let min = if text.is_empty() {
        None
    } else {
        Some(Version::parse(text).ok_or_else(|| format!("tools.{} is not a version: {text:?}", spelled(name)))?)
    };
    Ok(ToolNeed {
        name: name.to_owned(),
        min,
        known: KNOWN_TOOLS.contains(&name),
    })
}

/// Reads the `workloads` value `value` into `manifest`, each entry a workload id or a problem.
fn read_workloads(value: Value, manifest: &mut Manifest) {
    let ids = match value {
        Value::Array(ids) => ids,
        other => {
            manifest.problems.push(format!("workloads is not an array (TOML {})", other.type_str()));
            return;
        }
    };
    for (index, id) in ids.into_iter().enumerate() {
        match id {
            Value::String(id) if id.is_empty() => manifest.problems.push(format!("workloads[{index}] is empty")),
            Value::String(id) => manifest.workloads.push(id),
            other => manifest
                .problems
                .push(format!("workloads[{index}] is not a string (TOML {})", other.type_str())),
        }
    }
}

/// Each local tool a `.config/dotnet-tools.json` whose text is `text` pins, in file order: its id and the release of
/// its `version`, so a prerelease `1.0.0-beta.1` is `1.0.0` and a four-part `1.2.3.4` is `1.2.3`. The file is strict
/// JSON, as dotnet parses it: a comment or a trailing comma does not parse. A leading byte order mark is skipped,
/// member names are spelled exactly, a file with no `tools` pins none, and a tool id given twice is one entry, at its
/// first place with its last value, as the JSON map keeps it. `Err` with the reason for a file dotnet could not restore
/// from: one that does not parse, is not a JSON object, whose `tools` is not an object, or with a tool whose `version`
/// is not a string or names no release.
pub(crate) fn dotnet_tools(text: &str) -> Result<Vec<(String, Version)>, String> {
    let parsed: Json = serde_json::from_str(without_bom(text)).map_err(|error| error.to_string())?;
    let Some(root) = parsed.as_object() else {
        return Err("the file is not a JSON object".to_owned());
    };
    let Some(tools) = root.get("tools") else {
        return Ok(Vec::new());
    };
    let tools = tools.as_object().ok_or_else(|| "tools is not an object".to_owned())?;
    tools.iter().map(|(id, tool)| tool_pin(id, tool)).collect()
}

/// The id and release of the dotnet tool `id` whose manifest entry is `tool`.
fn tool_pin(id: &str, tool: &Json) -> Result<(String, Version), String> {
    let version = tool
        .get("version")
        .and_then(Json::as_str)
        .ok_or_else(|| format!("tools.{id}.version is not a string"))?;
    let release = Version::release_of(version).ok_or_else(|| format!("tools.{id}.version is not a version: {version:?}"))?;
    Ok((id.to_owned(), release))
}

/// The `Godot.NET.Sdk` versions a project file whose text is `csproj_text` names, in document order: from the `Sdk`
/// attribute of its root `<Project>` element (a `;`-separated list of `name/version`), and from the `Version` of each
/// `<Sdk Name="Godot.NET.Sdk" Version="..."/>` element that is a direct child of that root. Each is the release its
/// version names, as [`dotnet_tools`] reads one. Nothing else is read: not an `<Import Sdk="..."/>`, nor an `<Sdk>`
/// element nested deeper. The scan skips comments, CDATA, processing instructions and declarations, reads attribute
/// values in either quote as written (entities are not decoded), spells element and attribute names exactly, and the
/// SDK name in any letter case. An SDK with no version gives no pin, nor does one whose version names no release, such
/// as the `Godot.NET.Sdk/min=4.7.2` form or an `<Sdk>` element with only a `MinimumVersion`.
pub(crate) fn godot_sdk_pins(csproj_text: &str) -> Vec<Version> {
    let tags = start_tags(csproj_text);
    let Some(root) = tags.first().filter(|tag| tag.name == "Project" && tag.depth == 0) else {
        return Vec::new();
    };
    // A project with no `Sdk` attribute names no SDK there, as an empty list does.
    let from_root = root
        .attribute("Sdk")
        .unwrap_or_default()
        .split(';')
        .filter_map(|sdk| sdk.trim().split_once('/'))
        .filter(|(name, _)| is_godot(name))
        .map(|(_, version)| version.trim());
    let from_elements = tags
        .iter()
        .filter(|tag| tag.depth == 1 && tag.name == "Sdk" && tag.attribute("Name").is_some_and(is_godot))
        .filter_map(|tag| tag.attribute("Version"));
    from_root.chain(from_elements).filter_map(Version::release_of).collect()
}

/// Whether `name` is the Godot .NET SDK's.
fn is_godot(name: &str) -> bool {
    name.trim().eq_ignore_ascii_case(GODOT_SDK)
}

/// A start tag of an XML text: its name, how many elements it is nested in (0 for the root), and its attributes as
/// written.
struct Tag<'t> {
    name: &'t str,
    depth: usize,
    attributes: Vec<(&'t str, &'t str)>,
}

impl<'t> Tag<'t> {
    /// The value of its attribute `name`; `None` when it has none.
    fn attribute(&self, name: &str) -> Option<&'t str> {
        self.attributes.iter().find(|(key, _)| *key == name).map(|(_, value)| *value)
    }
}

/// The start tags of the XML text `text`, in document order, outside the markup that holds no element, each with the
/// depth its end tags and self-closing tags leave it at.
fn start_tags(text: &str) -> Vec<Tag<'_>> {
    let mut tags = Vec::new();
    let mut depth = 0_usize;
    let mut rest = text;
    while let Some(at) = rest.find('<') {
        rest = &rest[at..];
        if let Some((open, close)) = NOT_ELEMENTS.iter().find(|(open, _)| rest.starts_with(open)) {
            let inner = &rest[open.len()..];
            rest = inner.find(close).map_or("", |end| &inner[end + close.len()..]);
            continue;
        }
        let Some(end) = tag_end(rest) else {
            break;
        };
        let body = &rest[1..end];
        if body.starts_with('/') {
            depth = depth.saturating_sub(1);
        } else if let Some(tag) = start_tag(body, depth) {
            if !body.ends_with('/') {
                depth += 1;
            }
            tags.push(tag);
        }
        rest = &rest[end + 1..];
    }
    tags
}

/// The byte offset in `text`, which starts with a tag, of the `>` that ends that tag, a `>` inside a quoted attribute
/// value aside; `None` when the tag never ends.
fn tag_end(text: &str) -> Option<usize> {
    let mut quote = None;
    for (at, character) in text.char_indices() {
        match (quote, character) {
            (None, '"' | '\'') => quote = Some(character),
            (Some(open), _) if character == open => quote = None,
            (None, '>') => return Some(at),
            _ => {}
        }
    }
    None
}

/// The start tag at `depth` whose text between `<` and `>` is `body`; `None` for one with no name.
fn start_tag(body: &str, depth: usize) -> Option<Tag<'_>> {
    let end = body
        .find(|character: char| character.is_whitespace() || character == '/')
        .unwrap_or(body.len());
    let (name, rest) = body.split_at(end);
    (!name.is_empty()).then(|| Tag {
        name,
        depth,
        attributes: attributes(rest),
    })
}

/// The `name="value"` or `name='value'` attributes of `text`, in order, up to the first that is not one.
fn attributes(text: &str) -> Vec<(&str, &str)> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some((name, after)) = rest.split_once('=') {
        let after = after.trim_start();
        let Some(quote) = after.chars().next().filter(|character| *character == '"' || *character == '\'') else {
            break;
        };
        let Some((value, next)) = after[1..].split_once(quote) else {
            break;
        };
        found.push((name.trim(), value));
        rest = next;
    }
    found
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a fixture that does not read must fail loudly")]
mod tests {
    use super::{Manifest, ToolNeed, Version, dotnet_tools, godot_sdk_pins, meets, read, read_bytes, unreadable_line};

    fn version(major: u64, minor: u64, patch: u64) -> Version {
        Version { major, minor, patch }
    }

    #[test]
    fn meets_is_true_at_or_past_the_minimum_and_false_below_it() {
        let min = version(1, 89, 0);
        assert_eq!(meets(min, "1.89.0"), Some(true), "at the minimum");
        assert_eq!(meets(min, "1.89"), Some(true), "missing parts are 0");
        assert_eq!(meets(min, "1.99.0"), Some(true), "past it");
        assert_eq!(meets(min, "2.0.0"), Some(true), "a later major");
        assert_eq!(meets(min, "1.88.9"), Some(false), "below it");
        assert_eq!(meets(min, "0.100.0"), Some(false), "an earlier major whatever its minor");
        assert_eq!(meets(version(0, 9, 100), "0.9.99"), Some(false), "parts compare as numbers, not text");
    }

    #[test]
    fn meets_cannot_compare_a_found_text_that_is_no_version() {
        let min = version(24, 0, 0);
        for found in ["", "v24.1.0", "24.1.0-beta", "1.2.3.4", "present"] {
            assert_eq!(meets(min, found), None, "{found:?}");
        }
    }

    #[test]
    fn a_version_is_written_with_every_part() {
        assert_eq!(version(1, 89, 0).to_string(), "1.89.0");
        assert_eq!(Version::parse("24").map(|found| found.to_string()).as_deref(), Some("24.0.0"));
    }

    fn manifest(text: &str) -> Manifest {
        read(text).expect("the manifest parses")
    }

    fn tool(name: &str, min: Option<Version>, known: bool) -> ToolNeed {
        ToolNeed {
            name: name.to_owned(),
            min,
            known,
        }
    }

    fn owned(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| (*text).to_owned()).collect()
    }

    #[test]
    fn a_bare_major_is_that_major_point_zero_point_zero() {
        assert_eq!(Version::parse("24"), Some(version(24, 0, 0)));
        assert_eq!(Version::parse("1.89"), Some(version(1, 89, 0)));
        assert_eq!(Version::parse("0.9.100"), Some(version(0, 9, 100)));
        assert_eq!(manifest("[tools]\nnode = \"24\"\n").tools, [tool("node", Some(version(24, 0, 0)), true)]);
    }

    #[test]
    fn versions_order_by_their_parts() {
        let parse = |text| Version::parse(text).expect("a version");
        assert!(parse("1.10") > parse("1.9"), "parts by value, not as text");
        assert!(parse("2") > parse("1.99.99"));
        assert!(parse("1.2.3") > parse("1.2"));
        assert_eq!(parse("1.2"), parse("1.2.0"), "a missing part is 0");
    }

    #[test]
    fn a_version_is_one_to_three_numbers_and_nothing_else() {
        for text in [">=1.2", "1.2.x", "latest", "v24", "1.2.3.4", "1..2", "", "24 ", " 24", "-1", "1.2.", "+3"] {
            assert_eq!(Version::parse(text), None, "{text:?} is no version");
        }
    }

    #[test]
    fn a_leading_zero_is_the_same_number() {
        assert_eq!(Version::parse("024"), Some(version(24, 0, 0)));
        assert_eq!(Version::parse("1.09.007"), Some(version(1, 9, 7)));
    }

    #[test]
    fn a_key_that_needs_quoting_is_named_quoted() {
        let read = manifest("\"a b\" = 1\n[tools]\n\"my tool\" = 24\n\"x.y\" = \"latest\"\nnode = true\n");
        assert_eq!(
            read.problems,
            [
                "\"a b\" is not a key of the manifest (tools, workloads)",
                "tools.\"my tool\" is not a string (TOML integer)",
                "tools.\"x.y\" is not a version: \"latest\"",
                "tools.node is not a string (TOML boolean)",
            ]
        );
    }

    #[test]
    fn an_empty_version_means_present_at_any_version() {
        assert_eq!(manifest("[tools]\npwsh = \"\"\n").tools, [tool("pwsh", None, true)]);
    }

    #[test]
    fn every_known_tool_is_read_in_file_order() {
        let text = "[tools]\nrust = \"1.89\"\ncargo-nextest = \"0.9.100\"\nnode = \"24\"\npnpm = \"10\"\npwsh = \"7.5\"\n\
                    godot = \"4.7.2\"\nffmpeg = \"\"\n";
        assert_eq!(
            manifest(text).tools,
            [
                tool("rust", Some(version(1, 89, 0)), true),
                tool("cargo-nextest", Some(version(0, 9, 100)), true),
                tool("node", Some(version(24, 0, 0)), true),
                tool("pnpm", Some(version(10, 0, 0)), true),
                tool("pwsh", Some(version(7, 5, 0)), true),
                tool("godot", Some(version(4, 7, 2)), true),
                tool("ffmpeg", None, true),
            ]
        );
    }

    #[test]
    fn an_unknown_tool_is_kept_and_flagged_unknown() {
        let read = manifest("[tools]\nnodejs = \"24\"\nNode = \"\"\nnode = \"24\"\n");
        assert_eq!(
            read.tools,
            [
                tool("nodejs", Some(version(24, 0, 0)), false),
                tool("Node", None, false),
                tool("node", Some(version(24, 0, 0)), true),
            ],
            "names are known in lowercase only"
        );
        assert_eq!(read.problems, Vec::<String>::new(), "an unknown tool is no problem");
    }

    #[test]
    fn a_tool_version_that_is_not_a_version_is_skipped_with_its_problem() {
        for bad in [">=24", "1.2.x", "latest", "v24", "1.2.3.4", "1..2", "24 "] {
            let read = manifest(&format!("[tools]\nrust = \"1.89\"\nnode = {bad:?}\npwsh = \"\"\n"));
            assert_eq!(
                read.tools,
                [tool("rust", Some(version(1, 89, 0)), true), tool("pwsh", None, true)],
                "{bad:?}"
            );
            assert_eq!(read.problems, [format!("tools.node is not a version: {bad:?}")]);
        }
    }

    #[test]
    fn a_tool_value_that_is_not_a_string_is_skipped_with_its_problem() {
        for (value, kind) in [
            ("24", "integer"),
            ("24.1", "float"),
            ("true", "boolean"),
            ("[\"24\"]", "array"),
            ("{ min = \"24\" }", "table"),
            ("2026-10-13", "datetime"),
        ] {
            let read = manifest(&format!("[tools]\nnode = {value}\nrust = \"1.89\"\n"));
            assert_eq!(read.tools, [tool("rust", Some(version(1, 89, 0)), true)], "{value}");
            assert_eq!(read.problems, [format!("tools.node is not a string (TOML {kind})")]);
        }
    }

    #[test]
    fn a_tools_that_is_not_a_table_is_skipped_with_its_problem() {
        let read = manifest("tools = \"node\"\nworkloads = [\"wasm-tools\"]\n");
        assert_eq!(read.tools, []);
        assert_eq!(read.workloads, owned(&["wasm-tools"]));
        assert_eq!(read.problems, ["tools is not a table (TOML string)"]);
    }

    #[test]
    fn workloads_are_read_in_order() {
        let read = manifest("workloads = [\"wasm-tools\", \"android\", \"ios\"]\n");
        assert_eq!(read.workloads, owned(&["wasm-tools", "android", "ios"]));
        assert_eq!(read.problems, Vec::<String>::new());
    }

    #[test]
    fn a_bad_workloads_entry_is_skipped_with_its_problem() {
        let read = manifest("workloads = [\"wasm-tools\", 1, \"\", \"android\"]\n");
        assert_eq!(read.workloads, owned(&["wasm-tools", "android"]));
        assert_eq!(read.problems, ["workloads[1] is not a string (TOML integer)", "workloads[2] is empty"]);
        let read = manifest("workloads = \"wasm-tools\"\n[tools]\nnode = \"24\"\n");
        assert_eq!(read.workloads, Vec::<String>::new());
        assert_eq!(read.tools, [tool("node", Some(version(24, 0, 0)), true)]);
        assert_eq!(read.problems, ["workloads is not an array (TOML string)"]);
    }

    #[test]
    fn a_top_level_key_other_than_tools_and_workloads_is_skipped_with_its_problem() {
        let read = manifest("siblings = [\"../other\"]\n[tools]\nnode = \"24\"\n");
        assert_eq!(read.tools, [tool("node", Some(version(24, 0, 0)), true)]);
        assert_eq!(read.problems, ["siblings is not a key of the manifest (tools, workloads)"]);
    }

    #[test]
    fn a_file_that_is_not_toml_is_unreadable_whole() {
        let why = read("[tools]\nnode = \"24\nrust = \"1.89\"\n").expect_err("an unterminated string is no TOML");
        assert!(why.starts_with("line 2, column "), "{why}");
        let why = read("[tools]\nnode = \"24\"\nnode = \"25\"\n").expect_err("a duplicate key is no TOML");
        assert!(why.starts_with("line 3, column "), "{why}");
    }

    #[test]
    fn a_parse_failure_names_its_exact_column_a_byte_order_mark_aside() {
        let why = read("node = \"24\" x\n").expect_err("text after a value is no TOML");
        assert!(why.starts_with("line 1, column 13: "), "{why}");
        let why = read("\u{feff}node = \"24\" x\n").expect_err("text after a value is no TOML");
        assert!(why.starts_with("line 1, column 13: "), "{why}");
    }

    #[test]
    fn bytes_that_are_not_utf8_are_unreadable_whole() {
        let why = read_bytes(b"[tools]\nnode = \"\xff\"\n").expect_err("no UTF-8");
        assert!(why.starts_with("not valid UTF-8 ("), "{why}");
        assert_eq!(
            read_bytes(b"[tools]\nnode = \"24\"\n").expect("UTF-8 reads").tools,
            [tool("node", Some(version(24, 0, 0)), true)]
        );
    }

    #[test]
    fn an_empty_file_is_an_empty_manifest() {
        assert_eq!(manifest(""), Manifest::default());
        assert_eq!(manifest("# nothing declared yet\n\n"), Manifest::default());
        assert_eq!(manifest("[tools]\n"), Manifest::default());
    }

    #[test]
    fn a_byte_order_mark_is_skipped() {
        assert_eq!(
            manifest("\u{feff}[tools]\nnode = \"24\"\n").tools,
            [tool("node", Some(version(24, 0, 0)), true)]
        );
    }

    #[test]
    fn the_unreadable_line_names_the_file_and_why_and_skips_its_checks() {
        assert_eq!(
            unreadable_line("line 2, column 8: invalid string"),
            "gate: cannot read .gate/toolchain.toml: line 2, column 8: invalid string; skipping the manifest's checks"
        );
    }

    const DOTNET_TOOLS: &str = r#"{
  "version": 1,
  "isRoot": true,
  "tools": {
    "csharpier": {
      "version": "1.3.0",
      "commands": [
        "csharpier"
      ],
      "rollForward": false
    },
    "dotnet-reportgenerator-globaltool": {
      "version": "5.4.0-beta.1",
      "commands": ["reportgenerator"]
    },
    "four": { "version": "1.2.3.4", "commands": ["four"] },
    "built": { "version": "2.0+sha.1234", "commands": ["built"] }
  }
}"#;

    #[test]
    fn dotnet_tools_pins_each_tool_s_release_in_file_order() {
        assert_eq!(
            dotnet_tools(DOTNET_TOOLS).expect("the manifest reads"),
            [
                ("csharpier".to_owned(), version(1, 3, 0)),
                ("dotnet-reportgenerator-globaltool".to_owned(), version(5, 4, 0)),
                ("four".to_owned(), version(1, 2, 3)),
                ("built".to_owned(), version(2, 0, 0)),
            ]
        );
        assert_eq!(
            dotnet_tools("\u{feff}{ \"version\": 1, \"isRoot\": true, \"tools\": {} }").expect("an empty manifest reads"),
            []
        );
        assert_eq!(dotnet_tools("{ \"version\": 1 }").expect("no tools reads"), []);
    }

    #[test]
    fn a_dotnet_tool_given_twice_is_one_entry_at_its_first_place_with_its_last_version() {
        let text = r#"{ "tools": { "x": { "version": "1.0.0" }, "y": { "version": "3.0.0" }, "x": { "version": "2.0.0" } } }"#;
        assert_eq!(
            dotnet_tools(text).expect("the manifest reads"),
            [("x".to_owned(), version(2, 0, 0)), ("y".to_owned(), version(3, 0, 0))]
        );
    }

    #[test]
    fn a_comment_or_a_trailing_comma_in_dotnet_tools_is_err() {
        for text in [
            "// the tools\n{ \"tools\": {} }",
            "{ /* none */ \"tools\": {} }",
            "{ \"tools\": { \"x\": { \"version\": \"1.0.0\" }, } }",
        ] {
            let error = dotnet_tools(text).expect_err(text);
            assert!(!error.is_empty(), "{text}");
        }
    }

    #[test]
    fn a_dotnet_tools_file_dotnet_could_not_restore_from_is_err() {
        for (text, why) in [
            (r#"{ "tools": { "x": { "version": "1.0.0" } }"#, "EOF while parsing an object"),
            ("[1]", "the file is not a JSON object"),
            (r#"{ "tools": [] }"#, "tools is not an object"),
            (r#"{ "tools": { "x": { "commands": ["x"] } } }"#, "tools.x.version is not a string"),
            (r#"{ "tools": { "x": { "version": 1 } } }"#, "tools.x.version is not a string"),
            (
                r#"{ "tools": { "x": { "version": "latest" } } }"#,
                "tools.x.version is not a version: \"latest\"",
            ),
            (r#"{ "tools": { "x": "1.0.0" } }"#, "tools.x.version is not a string"),
        ] {
            let error = dotnet_tools(text).expect_err(text);
            assert!(error.starts_with(why), "{text}: {error}");
        }
    }

    #[test]
    fn a_csproj_s_root_sdk_attribute_names_its_godot_sdk() {
        let csproj = "<Project Sdk=\"Godot.NET.Sdk/4.7.2\">\n  <PropertyGroup>\n    <TargetFramework>net10.0</TargetFramework>\n    \
                      <EnableDynamicLoading>true</EnableDynamicLoading>\n  </PropertyGroup>\n</Project>\n";
        assert_eq!(godot_sdk_pins(csproj), [version(4, 7, 2)]);
        let declared = "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!-- the client -->\n\
                        <Project Sdk='Microsoft.NET.Sdk; godot.net.sdk/4.4.0-beta.1'>\n</Project>\n";
        assert_eq!(godot_sdk_pins(declared), [version(4, 4, 0)], "a list, a prerelease, either quote");
    }

    #[test]
    fn a_csproj_s_sdk_element_names_its_godot_sdk() {
        let csproj = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!-- <Sdk Name=\"Godot.NET.Sdk\" Version=\"9.9.9\" /> -->\n\
                      <Project>\n  <Sdk Name=\"Godot.NET.Sdk\" Version=\"4.7.2\" />\n  \
                      <Sdk Name=\"Microsoft.NET.Sdk\" Version=\"10.0.100\" />\n  <Sdk Name=\"Godot.NET.Sdk\" />\n  \
                      <PropertyGroup><TargetFramework>net10.0</TargetFramework></PropertyGroup>\n</Project>\n";
        assert_eq!(
            godot_sdk_pins(csproj),
            [version(4, 7, 2)],
            "a comment and an SDK with no version name none"
        );
    }

    #[test]
    fn a_csproj_naming_no_godot_sdk_pins_none() {
        let csproj = "<Project Sdk=\"Microsoft.NET.Sdk\">\n  <Import Project=\"Sdk.props\" Sdk=\"Godot.NET.Sdk/4.7.2\" />\n  \
                      <PropertyGroup><TargetFramework>net10.0</TargetFramework></PropertyGroup>\n</Project>\n";
        assert_eq!(
            godot_sdk_pins(csproj),
            [],
            "only the root Project's Sdk attribute and its Sdk elements are read, not an Import's"
        );
        assert_eq!(godot_sdk_pins(""), []);
        assert_eq!(godot_sdk_pins("<Project Sdk=\"Godot.NET.Sdk/next\"></Project>"), [], "no release");
        assert_eq!(godot_sdk_pins("<Project Sdk=\"Godot.NET.Sdk/min=4.7.2\"></Project>"), [], "a minimum");
        assert_eq!(
            godot_sdk_pins("<Project><Sdk Name=\"Godot.NET.Sdk\" MinimumVersion=\"4.7.2\" /></Project>"),
            [],
            "a minimum"
        );
        let nested = "<Project>\n  <ItemGroup><Sdk Name=\"Godot.NET.Sdk\" Version=\"4.7.2\"/></ItemGroup>\n  \
                      <Sdk Name=\"Godot.NET.Sdk\" Version=\"4.6.0\"/>\n</Project>\n";
        assert_eq!(
            godot_sdk_pins(nested),
            [version(4, 6, 0)],
            "an Sdk element is read only as the root's child"
        );
        assert_eq!(
            godot_sdk_pins("<Other Sdk=\"Godot.NET.Sdk/4.7.2\"><Sdk Name=\"Godot.NET.Sdk\" Version=\"4.7.2\"/></Other>"),
            [],
            "a root that is not a Project"
        );
    }

    #[test]
    fn a_quoted_greater_than_sign_does_not_end_the_tag() {
        let csproj = "<Project Condition=\"'$(A)' > '1'\" Sdk=\"Godot.NET.Sdk/4.7.2\">\n</Project>\n";
        assert_eq!(godot_sdk_pins(csproj), [version(4, 7, 2)]);
    }

    #[test]
    fn a_cdata_section_holds_no_element() {
        let csproj = "<Project>\n  <![CDATA[<Sdk Name=\"Godot.NET.Sdk\" Version=\"9.9.9\"/>]]>\n  \
                      <Sdk Name=\"Godot.NET.Sdk\" Version=\"4.7.2\"/>\n</Project>\n";
        assert_eq!(godot_sdk_pins(csproj), [version(4, 7, 2)]);
    }

    #[test]
    fn whitespace_around_an_attribute_s_equals_sign_is_read() {
        assert_eq!(godot_sdk_pins("<Project Sdk = 'Godot.NET.Sdk/4.7.2'>\n</Project>\n"), [version(4, 7, 2)]);
    }
}
