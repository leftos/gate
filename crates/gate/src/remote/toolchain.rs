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

use crate::boxside::payload::escaped;
use crate::boxside::probe::Probed;
use crate::remote::git::{FAILURE_BYTES, first_bytes};
use crate::remote::parity::{Need, NeedKind, number};

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

/// The start of the line a builder's preflight says for each tool its `Needs` named: `gate: remote tool <name> <what>`.
const TOOL_LINE: &str = "gate: remote tool ";

/// The longest tool name a preflight sends.
const SENT_NAME_MAX: usize = 32;

/// The why of a tool the builder found but whose version it could not read, or whose version text reads as none.
const NOT_READABLE: &str = "found, version not readable";

/// The why of a tool the preflight asked about and the builder said nothing of.
pub(crate) const NO_ANSWER: &str = "the builder gave no answer";

/// Why `gate check --builder` skips a tool when the tools make the preflight call too long for cmd, so none is sent.
pub(crate) const NOT_SENT: &str = "not sent, as the tools make the preflight call too long for cmd";

/// What a probe of one tool found, as a builder's tool line words it or as `gate check`'s probe found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Reported {
    /// Found, at the version this text spells.
    Version(String),
    /// Found, its version not read.
    Present,
    /// Not found.
    Missing,
    /// Not probed, for this reason.
    Skipped(String),
}

impl From<&Probed> for Reported {
    fn from(probed: &Probed) -> Reported {
        match probed {
            Probed::Version(found) => Reported::Version(found.clone()),
            Probed::Present => Reported::Present,
            Probed::Missing => Reported::Missing,
            Probed::Skipped(why) => Reported::Skipped(why.clone()),
        }
    }
}

/// How a declared tool stands against what its probe found: the one verdict both a routed run and `gate check` give.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Standing {
    /// It serves the need.
    Met,
    /// Found below the minimum, at the version this text spells.
    TooOld(String),
    /// Not found.
    Missing,
    /// Found, but with no version that can be read against the minimum.
    NotReadable,
    /// Not probed, for this reason.
    Skipped(String),
}

/// How a tool whose minimum is `min` (`None` for any version) stands against `reported`, what its probe found.
pub(crate) fn standing(min: Option<Version>, reported: &Reported) -> Standing {
    match (reported, min) {
        (Reported::Skipped(why), _) => Standing::Skipped(why.clone()),
        (Reported::Missing, _) => Standing::Missing,
        (Reported::Present | Reported::Version(_), None) => Standing::Met,
        (Reported::Present, Some(_)) => Standing::NotReadable,
        (Reported::Version(found), Some(min)) => match meets(min, found) {
            Some(true) => Standing::Met,
            Some(false) => Standing::TooOld(found.clone()),
            None => Standing::NotReadable,
        },
    }
}

/// What the manifest declares that a builder lacks, and the lines that say what was not checked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Judged {
    /// One need per tool missing or too old, in the manifest's order.
    pub needs: Vec<Need>,
    /// One line per manifest problem, then one per tool skipped, in the manifest's order.
    pub skips: Vec<String>,
}

/// The line said, on a routed run, for a manifest `problem`: an entry skipped as bad, or a whole file that cannot be
/// read or is no regular file. The problem is escaped, as it may quote the file.
pub(crate) fn skip_line(problem: &str) -> String {
    format!("gate: .gate/toolchain.toml: {}; skipping it", escaped(problem))
}

/// A text the builder said, as a line shows it: its first [`FAILURE_BYTES`], escaped, so a builder can neither forge a
/// line nor flood the console.
fn shown(text: &str) -> String {
    escaped(first_bytes(text, FAILURE_BYTES))
}

/// A tool name the manifest gives, as a line shows it: spelled as its key ([`spelled`]) and escaped, so it is always
/// one line.
fn one_line(name: &str) -> String {
    escaped(&spelled(name))
}

/// The line said, on a routed run, when the manifest's tools would make the preflight call longer than cmd takes, so it
/// is sent without them.
pub(crate) fn over_cap_line(limit: usize) -> String {
    skip_line(&format!("its tools make the preflight call longer than the {limit} characters cmd takes"))
}

/// Whether a preflight sends `tool` to the builder: a known tool, whose name is 1 to 32 lower-case ASCII letters,
/// digits and `-`. Every name of [`KNOWN_TOOLS`] passes the charset test, so it is a belt-and-braces guard: a known
/// name that failed it would still never reach the payload.
fn is_sent(tool: &ToolNeed) -> bool {
    let name = &tool.name;
    tool.known
        && (1..=SENT_NAME_MAX).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// The `Needs` a preflight sends for `manifest`, in its order: each tool it sends ([`is_sent`]) by name, with the minimum
/// as `major.minor.patch`, or empty for any version.
pub(crate) fn needs_of(manifest: &Manifest) -> Vec<(String, String)> {
    manifest
        .tools
        .iter()
        .filter(|tool| is_sent(tool))
        .map(|tool| (tool.name.clone(), tool.min.map(|min| min.to_string()).unwrap_or_default()))
        .collect()
}

/// What the preflight `lines` say of each tool of `sent`, from the last tool line naming it exactly, in the order of
/// `sent`; a name no line names has no entry, and a line naming no sent tool is not read.
pub(crate) fn reported(lines: &[String], sent: &[String]) -> Vec<(String, Reported)> {
    sent.iter()
        .filter_map(|name| {
            let said = lines.iter().rev().find_map(|line| {
                let rest = line.strip_prefix(TOOL_LINE)?.strip_prefix(name.as_str())?;
                rest.strip_prefix(' ')
            })?;
            Some((name.clone(), reported_as(said)))
        })
        .collect()
}

/// What the part of a tool line after its name, `said`, reports: `present`, `missing`, a `skipped:` and the reason it
/// carries, which is empty when the box gave none, else the version text itself.
fn reported_as(said: &str) -> Reported {
    match said.trim() {
        "present" => Reported::Present,
        "missing" => Reported::Missing,
        other => other
            .strip_prefix("skipped:")
            .map_or_else(|| Reported::Version(other.to_owned()), |why| Reported::Skipped(why.trim().to_owned())),
    }
}

/// What the repo's manifest, as it was read (`None` when the repo has none, `Err` with why when it cannot be read or
/// is no regular file), declares that the builder `host` lacks, given the tools `sent` in the preflight's `Needs` and
/// what it `reported` of them: a need for each sent tool it found missing or below its minimum, and a skip line for a
/// manifest that cannot be read, then for each problem of the manifest, then, in the manifest's order, for each tool
/// not sent as unknown and each sent tool it did not probe, gave no answer for, or found at a version that cannot be
/// read against a minimum. A known tool left unsent, as when the preflight could not carry the needs, says nothing.
/// Every text from the manifest or the builder is escaped, and the builder's cut to [`FAILURE_BYTES`].
pub(crate) fn judge(manifest: Option<&Result<Manifest, String>>, sent: &[String], reported: &[(String, Reported)], host: &str) -> Judged {
    let manifest = match manifest {
        None => return Judged::default(),
        Some(Err(why)) => {
            return Judged {
                needs: Vec::new(),
                skips: vec![skip_line(why)],
            };
        }
        Some(Ok(manifest)) => manifest,
    };
    let mut judged = Judged {
        needs: Vec::new(),
        skips: manifest.problems.iter().map(|problem| skip_line(problem)).collect(),
    };
    for tool in &manifest.tools {
        if !is_sent(tool) {
            let name = one_line(&tool.name);
            judged
                .skips
                .push(format!("gate: .gate/toolchain.toml names unknown tool {name}; skipping it"));
            continue;
        }
        if !sent.contains(&tool.name) {
            continue;
        }
        let answer = reported.iter().find(|(name, _)| *name == tool.name).map(|(_, answer)| answer);
        let why = match answer.map(|answer| standing(tool.min, answer)) {
            Some(Standing::Met) => continue,
            Some(Standing::TooOld(found)) => {
                judged.needs.push(lacking(tool, shown(&found)));
                continue;
            }
            Some(Standing::Missing) => {
                judged.needs.push(lacking(tool, "none".to_owned()));
                continue;
            }
            None => NO_ANSWER.to_owned(),
            Some(Standing::NotReadable) => NOT_READABLE.to_owned(),
            Some(Standing::Skipped(why)) => shown(&why),
        };
        let name = one_line(&tool.name);
        judged.skips.push(format!("gate: {host} did not probe {name}: {why}; not checking it"));
    }
    judged
}

/// The need of `tool`, of which the builder has `found`: its name, and `<min> or later` when it has a minimum.
fn lacking(tool: &ToolNeed, found: String) -> Need {
    let name = one_line(&tool.name);
    let text = match tool.min {
        Some(min) => format!("{name} {min} or later"),
        None => name,
    };
    Need {
        kind: NeedKind::Tool { found },
        text,
    }
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
    use super::{
        Judged, Manifest, Reported, Standing, ToolNeed, Version, dotnet_tools, godot_sdk_pins, is_sent, judge, meets, needs_of, over_cap_line, read,
        read_bytes, reported, reported_as, skip_line, standing, unreadable_line,
    };
    use crate::remote::git::FAILURE_BYTES;
    use crate::remote::parity::{Need, NeedKind};

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

    fn lacks(text: &str, found: &str) -> Need {
        Need {
            kind: NeedKind::Tool { found: found.to_owned() },
            text: text.to_owned(),
        }
    }

    /// What `judge` makes of the manifest `text` when the builder said the lines `said` to a preflight sending its needs.
    fn judged(text: &str, said: &[&str]) -> Judged {
        let manifest = manifest(text);
        let sent: Vec<String> = needs_of(&manifest).into_iter().map(|(name, _)| name).collect();
        let said = owned(said);
        judge(Some(&Ok(manifest)), &sent, &reported(&said, &sent), "builder")
    }

    #[test]
    fn the_needs_sent_are_the_known_tools_in_file_order_with_their_minimums() {
        let read = manifest("[tools]\nnodejs = \"24\"\nnode = \"24\"\n\"my tool\" = \"1\"\ncargo-nextest = \"\"\nNode = \"1\"\npwsh = \"7.5\"\n");
        assert_eq!(
            needs_of(&read),
            [
                ("node".to_owned(), "24.0.0".to_owned()),
                ("cargo-nextest".to_owned(), String::new()),
                ("pwsh".to_owned(), "7.5.0".to_owned()),
            ],
            "an unknown name, one that needs quoting and one in another case are never sent"
        );
        assert_eq!(needs_of(&Manifest::default()), []);
    }

    #[test]
    fn a_tool_line_reports_a_version_present_missing_or_skipped() {
        let sent = owned(&["node", "pnpm", "pwsh", "ffmpeg", "rust"]);
        let said = owned(&[
            "gate: remote version 0.1.0",
            "gate: remote tool node 24.1.0",
            "gate: remote tool pnpm present",
            "gate: remote tool pwsh missing",
            "gate: remote tool ffmpeg skipped: no answer in 10 s",
        ]);
        assert_eq!(
            reported(&said, &sent),
            [
                ("node".to_owned(), Reported::Version("24.1.0".to_owned())),
                ("pnpm".to_owned(), Reported::Present),
                ("pwsh".to_owned(), Reported::Missing),
                ("ffmpeg".to_owned(), Reported::Skipped("no answer in 10 s".to_owned())),
            ],
            "rust, which no line names, has no entry"
        );
    }

    #[test]
    fn a_tool_line_for_a_name_not_sent_is_not_read_and_the_last_line_of_a_name_wins() {
        let sent = owned(&["node"]);
        let said = owned(&[
            "gate: remote tool node 20.0.0",
            "gate: remote tool nodejs 24.0.0",
            "gate: remote tool node24.0.0",
            "gate: remote tool pwsh 7.5.0",
            "gate: remote tool node 24.1.0",
        ]);
        assert_eq!(reported(&said, &sent), [("node".to_owned(), Reported::Version("24.1.0".to_owned()))]);
        assert_eq!(reported(&owned(&["gate: remote tool nodejs 24.0.0"]), &sent), []);
    }

    #[test]
    fn a_served_tool_says_nothing() {
        let text = "[tools]\nnode = \"24\"\npwsh = \"\"\nffmpeg = \"\"\nrust = \"1.89\"\n";
        let said = [
            "gate: remote tool node 24.0.0",
            "gate: remote tool pwsh present",
            "gate: remote tool ffmpeg 7.1",
            "gate: remote tool rust 1.90.0",
        ];
        assert_eq!(judged(text, &said), Judged::default());
    }

    #[test]
    fn a_missing_or_too_old_tool_is_a_need_in_file_order() {
        let text = "[tools]\npwsh = \"\"\ncargo-nextest = \"0.9.100\"\nnode = \"24\"\nffmpeg = \"7\"\n";
        let said = [
            "gate: remote tool node missing",
            "gate: remote tool cargo-nextest 0.9.80",
            "gate: remote tool pwsh missing",
            "gate: remote tool ffmpeg 7.0.0",
        ];
        assert_eq!(
            judged(text, &said),
            Judged {
                needs: vec![
                    lacks("pwsh", "none"),
                    lacks("cargo-nextest 0.9.100 or later", "0.9.80"),
                    lacks("node 24.0.0 or later", "none"),
                ],
                skips: Vec::new(),
            }
        );
    }

    /// A box line whose skip carries no reason says a skip with an empty reason, never a version spelled `skipped:`.
    #[test]
    fn a_tool_line_skipped_with_no_reason_is_a_skip_and_not_a_version() {
        assert_eq!(reported_as("skipped:"), Reported::Skipped(String::new()));
        assert_eq!(reported_as("skipped:  "), Reported::Skipped(String::new()));
        assert_eq!(
            reported_as(" skipped: no probe for godot"),
            Reported::Skipped("no probe for godot".to_owned())
        );
        assert_eq!(reported_as("0.9.80"), Reported::Version("0.9.80".to_owned()));
    }

    #[test]
    fn each_tool_not_checked_says_its_skip_line_in_file_order_after_the_problems() {
        let text = "[tools]\nnode = \"24\"\nnodejs = \"24\"\n\"my\\ntool\" = \"1\"\ngodot = \"4.7\"\npnpm = \"10\"\npwsh = \"7.5\"\n\
                    rust = \"1.89\"\nffmpeg = \"x\"\n";
        let said = [
            "gate: remote tool node skipped: no answer in 10 s",
            "gate: remote tool godot skipped: no probe for godot",
            "gate: remote tool pnpm present",
            "gate: remote tool pwsh 7.x",
        ];
        assert_eq!(
            judged(text, &said),
            Judged {
                needs: Vec::new(),
                skips: owned(&[
                    "gate: .gate/toolchain.toml: tools.ffmpeg is not a version: \"x\"; skipping it",
                    "gate: builder did not probe node: no answer in 10 s; not checking it",
                    "gate: .gate/toolchain.toml names unknown tool nodejs; skipping it",
                    "gate: .gate/toolchain.toml names unknown tool \"my\\ntool\"; skipping it",
                    "gate: builder did not probe godot: no probe for godot; not checking it",
                    "gate: builder did not probe pnpm: found, version not readable; not checking it",
                    "gate: builder did not probe pwsh: found, version not readable; not checking it",
                    "gate: builder did not probe rust: the builder gave no answer; not checking it",
                ]),
            }
        );
    }

    #[test]
    fn a_known_tool_left_unsent_says_nothing() {
        let read = manifest("[tools]\nnode = \"24\"\nnodejs = \"1\"\n");
        assert_eq!(
            judge(Some(&Ok(read)), &[], &[], "builder"),
            Judged {
                needs: Vec::new(),
                skips: owned(&["gate: .gate/toolchain.toml names unknown tool nodejs; skipping it"]),
            },
            "a preflight that could not carry the needs asked about none"
        );
    }

    #[test]
    fn a_repo_with_no_manifest_says_nothing_and_one_that_cannot_be_read_says_one_skip_line() {
        assert_eq!(judge(None, &[], &[], "builder"), Judged::default());
        let unreadable = Err("line 1, column 7: invalid table header".to_owned());
        assert_eq!(
            judge(Some(&unreadable), &[], &[], "builder"),
            Judged {
                needs: Vec::new(),
                skips: owned(&["gate: .gate/toolchain.toml: line 1, column 7: invalid table header; skipping it"]),
            }
        );
    }

    #[test]
    fn a_builder_s_reason_holding_control_characters_is_said_as_one_escaped_line() {
        let said = judged("[tools]\nnode = \"24\"\n", &["gate: remote tool node skipped: a\rb\u{1b}[2J"]);
        assert_eq!(
            said.skips,
            ["gate: builder did not probe node: a\\rb\\u{1b}[2J; not checking it"],
            "{said:?}"
        );
    }

    #[test]
    fn a_manifest_problem_holding_a_bidi_mark_is_said_as_one_escaped_line() {
        let problem = Manifest {
            problems: owned(&["tools.\u{202e}x is bad"]),
            ..Manifest::default()
        };
        assert_eq!(
            judge(Some(&Ok(problem)), &[], &[], "builder").skips,
            ["gate: .gate/toolchain.toml: tools.\\u{202e}x is bad; skipping it"]
        );
        let unreadable = Err("line 1\u{2028}gate: forged".to_owned());
        assert_eq!(
            judge(Some(&unreadable), &[], &[], "builder").skips,
            ["gate: .gate/toolchain.toml: line 1\\u{2028}gate: forged; skipping it"]
        );
    }

    #[test]
    fn an_honest_builder_s_escaped_reason_is_said_unchanged() {
        let said = judged(
            "[tools]\nnode = \"24\"\n",
            &["gate: remote tool node skipped: cannot run node: a\\u{1b}b"],
        );
        assert_eq!(
            said.skips,
            ["gate: builder did not probe node: cannot run node: a\\u{1b}b; not checking it"]
        );
    }

    #[test]
    fn a_too_old_version_with_endless_leading_zeros_is_cut() {
        let found = format!("{}1.0.0", "0".repeat(10_000));
        let said = judged("[tools]\nnode = \"24\"\n", &[&format!("gate: remote tool node {found}")]);
        assert_eq!(said.needs.len(), 1, "{said:?}");
        let shown = match &said.needs[0].kind {
            NeedKind::Tool { found } => found.clone(),
            NeedKind::Sdk => String::new(),
        };
        assert_eq!(shown.len(), FAILURE_BYTES, "cut to the failure cap: {said:?}");
        assert!(found.starts_with(shown.as_str()));
    }

    #[test]
    fn only_a_known_name_in_the_charset_is_sent() {
        let named = |name: &str, known: bool| ToolNeed {
            name: name.to_owned(),
            min: None,
            known,
        };
        assert!(is_sent(&named("cargo-nextest", true)));
        for name in ["Node", "no de", "nöde", "node\n", "", &"n".repeat(33)] {
            assert!(!is_sent(&named(name, true)), "{name:?} fails the charset, known or not");
        }
        assert!(is_sent(&named(&"n".repeat(32), true)), "32 characters is the most");
        assert!(!is_sent(&named("nodejs", false)), "an unknown name");
    }

    #[test]
    fn a_tool_stands_met_too_old_missing_not_readable_or_skipped() {
        let min = Some(version(24, 0, 0));
        let at = |text: &str| Reported::Version(text.to_owned());
        assert_eq!(standing(min, &at("24.1.0")), Standing::Met);
        assert_eq!(standing(min, &at("22.1.0")), Standing::TooOld("22.1.0".to_owned()));
        assert_eq!(standing(min, &at("v24")), Standing::NotReadable);
        assert_eq!(standing(min, &Reported::Present), Standing::NotReadable);
        assert_eq!(standing(None, &Reported::Present), Standing::Met);
        assert_eq!(standing(None, &at("1.0.0")), Standing::Met);
        assert_eq!(standing(None, &Reported::Missing), Standing::Missing);
        assert_eq!(
            standing(min, &Reported::Skipped("no answer in 10 s".to_owned())),
            Standing::Skipped("no answer in 10 s".to_owned())
        );
    }

    #[test]
    fn the_manifest_s_skip_lines_name_the_file_and_the_problem() {
        assert_eq!(
            skip_line("line 2, column 8: invalid string"),
            "gate: .gate/toolchain.toml: line 2, column 8: invalid string; skipping it"
        );
        assert_eq!(
            over_cap_line(8127),
            "gate: .gate/toolchain.toml: its tools make the preflight call longer than the 8127 characters cmd takes; skipping it"
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
