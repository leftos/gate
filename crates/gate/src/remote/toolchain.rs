//! The repo's toolchain manifest, `.gate/toolchain.toml`, and the pins a repo already declares in other files, read as
//! data: what a routed run's builder must have beyond the .NET SDK. Nothing here runs a process or reads a file; the
//! caller hands over each file's text.
//!
//! The manifest is a `[tools]` table of `name = "version"`, where a version is a minimum (`major[.minor[.patch]]`, its
//! missing parts 0) and `""` means present at any version, a top-level `workloads` array of .NET workload ids, and an
//! `[env]` table of `NAME = "unreal"`, variables a routed run sets to the chosen Unreal Engine install's root. A
//! file that does not parse is unreadable whole; a bad entry is skipped alone, with a problem naming it. The entries
//! read otherwise are `godot` and `unreal`, whose values pin an exact version, as the pins read beside the manifest do:
//! each local dotnet tool's version in `.config/dotnet-tools.json` and the `Godot.NET.Sdk` version a project file names.
//! A manifest naming unreal needs a `netfx-sdk`, the .NET Framework SDK `UnrealBuildTool` looks for, at 4.6 or later
//! besides, read as though it named one.

use serde_json::Value as Json;
use toml::{Table, Value};

use gate_args::MOST_ENV_NAMES;
use gate_router::{escaped, parse_failure, spelled};
use gate_wire::{Probed, engine_env_name_refused};

use crate::remote::git::{FAILURE_BYTES, first_bytes};
use crate::remote::parity::{Need, NeedKind, number};

/// The tool names the manifest knows, as its `[tools]` keys spell them.
pub(crate) const KNOWN_TOOLS: [&str; 10] = [
    "rust",
    "cargo-nextest",
    "node",
    "pnpm",
    "pwsh",
    "godot",
    "ffmpeg",
    "unreal",
    "git-lfs",
    "netfx-sdk",
];

/// A tool whose manifest entry pins an exact version rather than a minimum, as its key spells it; its project files
/// may pin one too.
const GODOT: &str = "godot";

/// The other tool whose manifest entry pins an exact version rather than a minimum, as its key spells it; only the
/// manifest pins it.
const UNREAL: &str = "unreal";

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

/// A version a repo pins exactly, compared on the parts its text gives: `4.7.2` is that one version, `4.7` any `4.7.x`
/// and `4` any `4.x.y`. What a builder's installs must hold one of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Pin {
    /// The version named, its missing parts 0.
    version: Version,
    /// How many parts the text gave, 1 to 3.
    parts: usize,
}

impl Pin {
    /// The version the text `text` pins, one to three dot-separated numbers of ASCII digits, its missing parts 0;
    /// `None` for anything else, as [`Version::parse`] reads one.
    pub(crate) fn parse(text: &str) -> Option<Pin> {
        let parts = numbers(text)?;
        match parts.len() {
            1..=3 => Pin::of(&parts),
            _ => None,
        }
    }

    /// The version the package version `text` pins, its numbers before any `-` prerelease label or `+` build metadata,
    /// the first three of them, as a package version may have four; `None` when that part is not dot-separated numbers.
    pub(crate) fn release_of(text: &str) -> Option<Pin> {
        let release = text.find(['-', '+']).map_or(text, |at| &text[..at]);
        Pin::of(&numbers(release)?)
    }

    /// The pin the parts `parts` spell, the first three of them, as a package version may have four; `None` for no part.
    fn of(parts: &[u64]) -> Option<Pin> {
        let parts = &parts[..parts.len().min(3)];
        let version = Version::of(parts)?;
        Some(Pin { version, parts: parts.len() })
    }

    /// Whether the install at the version `found` is the one this pin names: the same on the parts the pin gave,
    /// whatever the ones its text left out.
    fn met_by(&self, found: Version) -> bool {
        self.version.major == found.major
            && (self.parts < 2 || self.version.minor == found.minor)
            && (self.parts < 3 || self.version.patch == found.patch)
    }
}

/// The pin as the text spelled it, the parts it gave and no more, so a `4.7` pin reads `4.7`.
impl std::fmt::Display for Pin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Version { major, minor, patch } = self.version;
        match self.parts {
            1 => write!(formatter, "{major}"),
            2 => write!(formatter, "{major}.{minor}"),
            _ => write!(formatter, "{major}.{minor}.{patch}"),
        }
    }
}

/// Whether a tool found at the version `found` spells is at or past the minimum `min`, the highest of several versions
/// weighed when `found` names them `,`-joined, as a probe of several installs does; `None` when `found`, or one of its
/// members, is no version [`Version::parse`] reads, so it cannot be compared.
pub(crate) fn meets(min: Version, found: &str) -> Option<bool> {
    let versions: Option<Vec<Version>> = found.split(',').map(Version::parse).collect();
    versions?.into_iter().max().map(|found| found >= min)
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

/// What a manifest's `[tools] godot` or `[tools] unreal` entry requires of a routed run's builder, where every other
/// entry names a minimum: the version a repo pins exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Exact {
    /// An empty value: any version, so an install of any version at all serves it.
    Any,
    /// The version the value names, on the parts it gives.
    Exactly(Pin),
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
    /// The `[tools] godot` entry when the manifest names godot: the versions it pins, rather than the minimum its
    /// `tools` entry is otherwise read as. `None` when the manifest names no godot, or names one whose value is no
    /// version, which [`Manifest::problems`] holds instead.
    pub godot: Option<Exact>,
    /// The `[tools] unreal` entry when the manifest names unreal, read as [`Manifest::godot`] is.
    pub unreal: Option<Exact>,
    /// The `[env]` names that read, in file order: each a variable a routed run's commands get set to the root of the
    /// Unreal Engine install the run chose, its value in the file being `"unreal"`. A name stays here when the manifest
    /// pins no unreal, so the builder still removes a value it inherited, though [`Manifest::problems`] names it.
    pub env: Vec<String>,
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

/// What a repo's exact Godot pins make of what the builder's probe `reported` of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Pinned {
    /// The builder serves the pins.
    Met,
    /// It does not, and the mismatch line ends in this, as it ends in what the builder has of any other tool.
    Lacks(String),
    /// The repo requires any version and the probe did not answer, so the ordinary skip line covers it.
    Skipped(String),
}

/// How the builder's godot, as its probe `reported` it, stands against the versions `pins` a repo requires, empty for a
/// repo requiring an install of any version: `Met` when every pin is one of the installs the builder listed, `Lacks`
/// with what it has of it — `none` when it found nothing, `not probed (<why>)` when the probe was skipped, else the
/// builder's own answer — and `Skipped` for a repo requiring any version whose probe did not answer.
///
/// An answer that is no version list at all is a mismatch rather than a skip, so a pin that cannot be checked runs the
/// command here rather than on a builder that may lack it. Two limits are known: an install reads as the release of its
/// version, so an install of a prerelease such as `4.7.2-beta.1` is the `4.7.2` an install of that release is, both
/// sides dropping the label; and an install the box's own listing did not reach, as one still being installed, is
/// simply not on its list, which reads as the builder lacking it and falls back here needlessly.
pub(crate) fn pinned(pins: &[Pin], reported: &Reported) -> Pinned {
    match reported {
        Reported::Missing => Pinned::Lacks("none".to_owned()),
        Reported::Skipped(why) if pins.is_empty() => Pinned::Skipped(why.clone()),
        Reported::Skipped(why) => Pinned::Lacks(format!("not probed ({why})")),
        Reported::Present if pins.is_empty() => Pinned::Met,
        Reported::Present => Pinned::Lacks("present".to_owned()),
        Reported::Version(list) => {
            let found: Vec<Version> = list.split(',').map(str::trim).filter_map(Version::parse).collect();
            if pins.iter().all(|pin| found.iter().any(|found| pin.met_by(*found))) {
                Pinned::Met
            } else {
                Pinned::Lacks(listed(list))
            }
        }
    }
}

/// The versions a builder's answer `list` names, as a mismatch line spells what it has: its `,`-separated members, each
/// trimmed, joined by `, `.
fn listed(list: &str) -> String {
    list.split(',')
        .map(str::trim)
        .filter(|member| !member.is_empty())
        .collect::<Vec<&str>>()
        .join(", ")
}

/// What the manifest declares that a builder lacks, and the lines that say what was not checked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Judged {
    /// One need per tool missing or too old, in the manifest's order.
    pub needs: Vec<Need>,
    /// One line per manifest problem, then one per tool skipped, in the manifest's order.
    pub skips: Vec<String>,
}

/// How a routed run's lines name the manifest the snapshot's tree holds.
pub(crate) const MANIFEST_SAID: &str = ".gate/toolchain.toml";

/// The line said, on a routed run, for a problem of the manifest named `file` ([`MANIFEST_SAID`], or the full path of
/// a copy in the declarations folder): an entry skipped as bad, or a whole file that cannot be read or is no regular
/// file. Both are escaped, as the problem may quote the file.
pub(crate) fn skip_line(file: &str, problem: &str) -> String {
    format!("gate: {}: {}; skipping it", escaped(file), escaped(problem))
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

/// The `Needs` a routed run's preflight sends for a repo whose manifest read `manifest` and whose project files pin the
/// versions `pins`: the manifest's own tools ([`needs_of`]) with `godot` moved last, carrying the versions
/// [`godot_required`] reads of the repo, joined by `,`, or empty for any version. A Godot command (`godot_command`) of a
/// repo requiring no version of godot still sends its name, so the builder lists the installs the run may choose from.
/// The box ignores the text, and reads only the names.
pub(crate) fn routed_needs(manifest: Option<&Result<Manifest, String>>, pins: &[Pin], godot_command: bool) -> Vec<(String, String)> {
    let mut needs = match manifest {
        Some(Ok(manifest)) => needs_of(manifest),
        _ => Vec::new(),
    };
    needs.retain(|(name, _)| name != GODOT);
    match godot_required(manifest, pins) {
        Some(godot) => {
            let versions: Vec<String> = godot.iter().map(ToString::to_string).collect();
            needs.push((GODOT.to_owned(), versions.join(",")));
        }
        // A Godot command of a repo requiring no version still has the builder list its installs, so the run can use
        // the newest.
        None if godot_command => needs.push((GODOT.to_owned(), String::new())),
        None => {}
    }
    needs
}

/// Whether a repo's Godot pins were read, given its manifest read `manifest` (`None` when the repo has none) and the
/// lines `skips` for the project files left unread: the manifest reads or is absent, and no project file was left unread.
/// A repo with neither a manifest nor a pin is the plain no-pin case, not an unreadable one.
pub(crate) fn godot_pins_read(manifest: Option<&Result<Manifest, String>>, skips: &[String]) -> bool {
    !matches!(manifest, Some(Err(_))) && skips.is_empty()
}

/// The versions a repo requires of a routed run's builder Godot installs: the manifest's own `godot` entry when it names
/// one, whatever the project files pin, else the versions `pins` those files name, each of a version only once. `None`
/// when the repo requires nothing of godot, so no need is sent and nothing is judged; a manifest that cannot be read, or
/// that the repo has none of, does not stop the project files' pins from applying.
pub(crate) fn godot_required(manifest: Option<&Result<Manifest, String>>, pins: &[Pin]) -> Option<Vec<Pin>> {
    if let Some(godot) = readable(manifest).and_then(|manifest| manifest.godot) {
        return Some(exact_pins(godot));
    }
    (!pins.is_empty()).then(|| distinct(pins))
}

/// The versions a repo requires of a routed run's builder Unreal Engine installs: the manifest's own `unreal` entry,
/// empty for any version. `None` when the manifest names no unreal, cannot be read or the repo has none, so nothing is
/// judged and no version is chosen.
pub(crate) fn unreal_required(manifest: Option<&Result<Manifest, String>>) -> Option<Vec<Pin>> {
    readable(manifest).and_then(|manifest| manifest.unreal).map(exact_pins)
}

/// The names a repo's manifest `[env]` sets to the root of the Unreal Engine install a routed run chose
/// ([`Manifest::env`]); none when the manifest names none, cannot be read or the repo has none.
pub(crate) fn engine_env(manifest: Option<&Result<Manifest, String>>) -> Vec<String> {
    readable(manifest).map(|manifest| manifest.env.clone()).unwrap_or_default()
}

/// The manifest `manifest` when the repo has one that reads.
fn readable(manifest: Option<&Result<Manifest, String>>) -> Option<&Manifest> {
    manifest.and_then(|read| read.as_ref().ok())
}

/// The versions an exact manifest entry `entry` requires: none for any version, else its one pin.
fn exact_pins(entry: Exact) -> Vec<Pin> {
    match entry {
        Exact::Any => Vec::new(),
        Exact::Exactly(pin) => vec![pin],
    }
}

/// The Godot install a routed run's builder runs the repo's command on, as [`godot_choice`] picks it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GodotChoice {
    /// No version: the repo requires nothing of godot, or any version, or no install the builder listed meets its pin.
    /// What a routed run does with it is [`no_chosen_godot`]'s decision: a Godot console command runs on the newest
    /// install the builder listed, warning when the repo pins no version, and stays here when the builder lists no
    /// install, did not list its installs, or the repo's pins could not be read. A pin no listed install meets is the
    /// parity mismatch's own refusal.
    None,
    /// The version, `major.minor.patch`, of the install the builder listed that the run is to use.
    Version(String),
    /// The repo pins several versions, here `, `-joined in the order [`godot_required`] gives them; a routed run needs one.
    Several(String),
}

/// The line said when a repo pins the several Godot versions `list`, which keeps a routed run here.
pub(crate) fn several_godot_line(list: &str) -> String {
    format!("gate: the repo pins several Godot versions ({list}); a routed run needs one")
}

/// The line said when a repo pinning no Godot version has the builder `host` list no Godot install either, which keeps a
/// routed run here.
pub(crate) fn no_godot_install_line(host: &str) -> String {
    format!("gate: the repo pins no Godot version and {host} lists no Godot install; a routed run needs one")
}

/// The line said when the preflight did not list the builder's Godot installs, which keeps a routed run here.
pub(crate) fn godot_not_listed_line(host: &str) -> String {
    format!("gate: {host} did not list its Godot installs; a routed run needs one")
}

/// The line said when a repo's Godot pins could not be read, which keeps a routed run here.
pub(crate) fn unreadable_godot_pins_line() -> String {
    "gate: the repo's Godot pins could not be read; a routed run needs one".to_owned()
}

/// The warning a routed run of a repo pinning no Godot version says when it uses the newest install `version` the
/// builder `host` listed.
pub(crate) fn no_pin_godot_warning(version: &str, host: &str) -> String {
    format!(
        "gate: warning: the repo pins no Godot version, so this run uses {version}, the newest on {host}; pin the \
         version it needs in .gate/toolchain.toml ([tools] godot = \"x.y.z\") or as the .csproj's Godot.NET.Sdk version"
    )
}

/// Which of the builder's Godot installs a routed run uses, given the versions `required` the repo requires
/// ([`godot_required`]; `None` when it requires nothing) and what the preflight `reported` of each tool: none for no pin
/// or any version, several for more than one pin, else the highest version the builder listed that meets the one pin
/// ([`Pin::met_by`]), none when no listed version meets it.
pub(crate) fn godot_choice(required: Option<&[Pin]>, reported: &[(String, Reported)]) -> GodotChoice {
    let pin = match required.unwrap_or_default() {
        [] => return GodotChoice::None,
        [pin] => pin,
        pins => {
            let list: Vec<String> = pins.iter().map(ToString::to_string).collect();
            return GodotChoice::Several(list.join(", "));
        }
    };
    highest_meeting(pin, reported, GODOT).map_or(GodotChoice::None, GodotChoice::Version)
}

/// Which of the builder's Unreal Engine installs a routed run uses, given the versions `required` the repo requires
/// ([`unreal_required`]; `None` when it requires nothing) and what the preflight `reported` of each tool: the highest
/// version the builder listed that meets the one pin, as `major.minor.patch`, or for any version (an entry of `""`) the
/// highest it listed at all, as godot's no-pin choice takes the newest; none for no entry or a pin no listed version
/// meets.
pub(crate) fn unreal_choice(required: Option<&[Pin]>, reported: &[(String, Reported)]) -> Option<String> {
    match required? {
        [] => listed_versions(reported, UNREAL).max().map(|version| version.to_string()),
        [pin] => highest_meeting(pin, reported, UNREAL),
        _ => None,
    }
}

/// The highest version, `major.minor.patch`, the preflight `reported` of the tool `name` that meets `pin`
/// ([`Pin::met_by`]); `None` when no listed version does.
fn highest_meeting(pin: &Pin, reported: &[(String, Reported)], name: &str) -> Option<String> {
    let highest = listed_versions(reported, name).filter(|found| pin.met_by(*found)).max();
    highest.map(|version| version.to_string())
}

/// The versions the preflight `reported` of the builder's installs of the tool `name`, in the order it listed them;
/// none when it said nothing of that tool, or no version list.
fn listed_versions<'a>(reported: &'a [(String, Reported)], name: &str) -> impl Iterator<Item = Version> + 'a {
    reported
        .iter()
        .find_map(|(tool, answer)| match answer {
            Reported::Version(list) if tool == name => Some(list),
            _ => None,
        })
        .into_iter()
        .flat_map(|list| list.split(','))
        .filter_map(|member| Version::parse(member.trim()))
}

/// The newest `major.minor.patch` of the Godot installs the preflight `reported`; `None` when it listed none.
pub(crate) fn newest_godot(reported: &[(String, Reported)]) -> Option<String> {
    listed_versions(reported, GODOT).max().map(|version| version.to_string())
}

/// What a routed run whose repo chooses no Godot version ([`GodotChoice::None`]) does with its command, as
/// [`no_chosen_godot`] decides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NoChosenGodot {
    /// Nothing: the command is not a Godot console, or the repo pins a version no listed install meets, which the parity
    /// mismatch refuses.
    None,
    /// The newest install the builder listed, `major.minor.patch`, and the warning to say before the run, if any.
    Newest { version: String, warning: Option<String> },
    /// The run stays here, refused `parity`, saying this line.
    Refuse(String),
}

/// What a routed run of a repo choosing no Godot version does, given the versions `required` the repo requires
/// ([`godot_required`]; `None` when it requires nothing), whether its Godot pins were `pins_read`
/// ([`godot_pins_read`]), whether the call runs a Godot console (`godot_command`), what the preflight
/// `reported` of the builder's installs, and the builder `host`: nothing for any other command, the newest install with a
/// warning when the repo pins none and without one when it takes any, and a refusal line when no version is known. An
/// unmet pin is the parity mismatch's own refusal, with no line here. So is a `godot = ""` repo whose builder reports
/// godot missing, its judge's mismatch line carrying that refusal. A report that lists no version at all — a skipped
/// probe, no answer, a `present` godot or a version list with nothing in it — is not a missing install, and is refused
/// with [`godot_not_listed_line`].
pub(crate) fn no_chosen_godot(
    required: Option<&[Pin]>,
    pins_read: bool,
    godot_command: bool,
    reported: &[(String, Reported)],
    host: &str,
) -> NoChosenGodot {
    if !godot_command {
        return NoChosenGodot::None;
    }
    let answer = reported.iter().find_map(|(name, answer)| (name == GODOT).then_some(answer));
    match required {
        Some(pins) if !pins.is_empty() => NoChosenGodot::None,
        None if !pins_read => NoChosenGodot::Refuse(unreadable_godot_pins_line()),
        required => {
            if let Some(version) = newest_godot(reported) {
                return NoChosenGodot::Newest {
                    warning: required.is_none().then(|| no_pin_godot_warning(&version, host)),
                    version,
                };
            }
            match answer {
                Some(Reported::Missing) if required.is_some() => NoChosenGodot::None,
                Some(Reported::Missing) => NoChosenGodot::Refuse(no_godot_install_line(host)),
                _ => NoChosenGodot::Refuse(godot_not_listed_line(host)),
            }
        }
    }
}

/// The pins among `pins` that are not read already, in their order.
fn distinct(pins: &[Pin]) -> Vec<Pin> {
    let mut distinct: Vec<Pin> = Vec::with_capacity(pins.len());
    for pin in pins {
        if !distinct.contains(pin) {
            distinct.push(*pin);
        }
    }
    distinct
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
/// is no regular file), declares that the builder `host` lacks, given the versions `godot` the repo requires of the
/// builder's Godot installs ([`godot_required`]; `None` when it requires nothing of godot), the tools `sent` in the
/// preflight's `Needs` and what it `reported` of them: a need for each sent tool it found missing or below its minimum,
/// and one for a `godot`, or a manifest `unreal` ([`unreal_required`]), its installs do not serve, and a skip line for a
/// manifest that cannot be read, then for each problem of the manifest, then, in the manifest's order, for each tool not
/// sent as unknown and each sent tool it did not probe, gave no answer for, or found at a version that cannot be read
/// against a minimum. A known tool left unsent, as when the preflight could not carry the needs, says nothing, and so
/// does a pinned tool whose own need was not sent. Every text from the manifest or the builder is escaped, and the
/// builder's cut to [`FAILURE_BYTES`]. The manifest comes with how its lines name its file ([`skip_line`]).
pub(crate) fn judge(
    manifest: Option<(&Result<Manifest, String>, &str)>,
    godot: Option<&[Pin]>,
    sent: &[String],
    reported: &[(String, Reported)],
    host: &str,
) -> Judged {
    let mut judged = Judged::default();
    let file = manifest.map_or(MANIFEST_SAID, |(_, file)| file);
    let manifest = manifest.map(|(read, _)| read);
    if let Some(Err(why)) = manifest {
        judged.skips.push(skip_line(file, why));
    }
    let unreal = unreal_required(manifest);
    let exact = [(GODOT, godot), (UNREAL, unreal.as_deref())];
    if let Some(Ok(manifest)) = manifest {
        judged.skips.extend(manifest.problems.iter().map(|problem| skip_line(file, problem)));
        for tool in &manifest.tools {
            // A pinned tool is weighed exactly, below, whatever minimum its own entry spells.
            if exact.iter().any(|(name, pins)| pins.is_some() && tool.name == *name) {
                continue;
            }
            if !is_sent(tool) {
                let name = one_line(&tool.name);
                judged
                    .skips
                    .push(format!("gate: {} names unknown tool {name}; skipping it", escaped(file)));
                continue;
            }
            judge_tool(&mut judged, tool, sent, reported, host);
        }
    }
    for (name, pins) in exact {
        if let Some(pins) = pins.filter(|_| sent.iter().any(|sent| sent == name)) {
            judge_exact(&mut judged, name, pins, reported, host);
        }
    }
    judged
}

/// Adds to `judged` what the builder `host` lacks of the tool `tool`, as the preflight `reported` of it: a need when it
/// is missing or too old, or a skip line when its probe did not answer; nothing for a tool not `sent`.
fn judge_tool(judged: &mut Judged, tool: &ToolNeed, sent: &[String], reported: &[(String, Reported)], host: &str) {
    if !sent.contains(&tool.name) {
        return;
    }
    let answer = reported.iter().find(|(name, _)| *name == tool.name).map(|(_, answer)| answer);
    let why = match answer.map(|answer| standing(tool.min, answer)) {
        Some(Standing::Met) => return,
        Some(Standing::TooOld(found)) => {
            judged.needs.push(lacking(tool, shown(&found)));
            return;
        }
        Some(Standing::Missing) => {
            judged.needs.push(lacking(tool, "none".to_owned()));
            return;
        }
        None => NO_ANSWER.to_owned(),
        Some(Standing::NotReadable) => NOT_READABLE.to_owned(),
        Some(Standing::Skipped(why)) => shown(&why),
    };
    let name = one_line(&tool.name);
    judged.skips.push(format!("gate: {host} did not probe {name}: {why}; not checking it"));
}

/// The tool a snapshot holding Git LFS pointers needs on the builder, at [`GIT_LFS_MIN`] or later.
const GIT_LFS: &str = "git-lfs";
/// The oldest git-lfs a run with LFS pointers takes: one older counts as missing.
const GIT_LFS_MIN: Version = Version {
    major: 3,
    minor: 1,
    patch: 3,
};

/// git-lfs at [`GIT_LFS_MIN`] or later, as a need of a snapshot holding LFS pointers.
fn git_lfs_need() -> ToolNeed {
    ToolNeed {
        name: GIT_LFS.to_owned(),
        min: Some(GIT_LFS_MIN),
        known: true,
    }
}

/// The minimum a run with LFS pointers weighs git-lfs at: the higher of [`GIT_LFS_MIN`] and `declared`, the manifest's
/// own minimum, `None` for an entry naming no version.
fn git_lfs_floor(declared: Option<Version>) -> Version {
    declared.map_or(GIT_LFS_MIN, |min| min.max(GIT_LFS_MIN))
}

/// Adds a `git-lfs` need of [`GIT_LFS_MIN`] or later to the preflight's `needs` of a run whose snapshot holds LFS
/// pointers or whose repo's attributes name the LFS filter; a git-lfs they name already is raised to it when its version is lower, or names none.
pub(crate) fn with_git_lfs(needs: &mut Vec<(String, String)>) {
    match needs.iter_mut().find(|(name, _)| name == GIT_LFS) {
        Some((_, version)) => *version = git_lfs_floor(Version::parse(version)).to_string(),
        None => needs.push((GIT_LFS.to_owned(), GIT_LFS_MIN.to_string())),
    }
}

/// What the builder `host` lacks for a run whose snapshot holds LFS pointers or whose repo's attributes name the LFS
/// filter, as [`judge`] weighs `manifest` and the Godot versions `godot`, with git-lfs weighed at [`GIT_LFS_MIN`] or
/// later, or the manifest's own minimum when higher ([`add_git_lfs`]), once. A manifest that cannot be read has git-lfs
/// weighed at [`GIT_LFS_MIN`] besides its skip line.
pub(crate) fn judge_with_git_lfs(
    manifest: Option<(&Result<Manifest, String>, &str)>,
    godot: Option<&[Pin]>,
    sent: &[String],
    reported: &[(String, Reported)],
    host: &str,
) -> Judged {
    let file = manifest.map_or(MANIFEST_SAID, |(_, file)| file);
    let floored = match manifest.map(|(read, _)| read) {
        Some(Ok(read)) => {
            let mut read = read.clone();
            add_git_lfs(&mut read);
            Some(Ok(read))
        }
        other => other.cloned(),
    };
    let mut judged = judge(floored.as_ref().map(|read| (read, file)), godot, sent, reported, host);
    if readable(floored.as_ref()).is_none() {
        judge_tool(&mut judged, &git_lfs_need(), sent, reported, host);
    }
    judged
}

/// Adds git-lfs, [`GIT_LFS_MIN`] or later, to `manifest`'s tools, for a repo whose attributes name the LFS filter; a
/// git-lfs it declares already is weighed at the higher of its own minimum and [`GIT_LFS_MIN`].
pub(crate) fn add_git_lfs(manifest: &mut Manifest) {
    match manifest.tools.iter_mut().find(|tool| tool.name == GIT_LFS) {
        Some(tool) => tool.min = Some(git_lfs_floor(tool.min)),
        None => manifest.tools.push(git_lfs_need()),
    }
}

/// Adds to `judged` what the builder `host` lacks of the versions `pins` a repo requires exactly of the tool `name`, as
/// the preflight `reported` of it ([`pinned`]): a need when its installs do not serve the pins, or a skip line when the
/// repo requires any version and the probe did not answer.
fn judge_exact(judged: &mut Judged, name: &str, pins: &[Pin], reported: &[(String, Reported)], host: &str) {
    let answer = reported.iter().find(|(tool, _)| tool == name).map(|(_, answer)| answer);
    match answer.map(|answer| pinned(pins, answer)) {
        Some(Pinned::Met) => {}
        Some(Pinned::Lacks(found)) => judged.needs.push(exact_need(name, pins, shown(&found))),
        Some(Pinned::Skipped(why)) => {
            let why = shown(&why);
            judged.skips.push(format!("gate: {host} did not probe {name}: {why}; not checking it"));
        }
        None if pins.is_empty() => {
            let why = shown(NO_ANSWER);
            judged.skips.push(format!("gate: {host} did not probe {name}: {why}; not checking it"));
        }
        None => judged.needs.push(exact_need(name, pins, shown(&format!("not probed ({NO_ANSWER})")))),
    }
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

/// The need of the versions `pins` a repo requires exactly of the tool `name`, of which the builder has `found`: its
/// name and the versions, joined as a mismatch line spells them and each given the parts its pin gave, or the name
/// alone for a repo requiring an install of any version.
fn exact_need(name: &str, pins: &[Pin], found: String) -> Need {
    let versions: Vec<String> = pins.iter().map(ToString::to_string).collect();
    let text = if versions.is_empty() {
        name.to_owned()
    } else {
        format!("{name} {} exactly", versions.join(", "))
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
/// `workloads` that is not an array, a workload that is not a string or is empty, a bad `env` entry ([`read_env`]), and
/// a top-level key other than `tools`, `workloads` and `env`. A `godot` entry that reads is kept as its exact pin in
/// [`Manifest::godot`] as well as among the tools. `env` names kept in a manifest that pins no unreal are each named
/// as a problem too, and kept.
pub(crate) fn read(text: &str) -> Result<Manifest, String> {
    let table = text.parse::<Table>().map_err(|error| parse_failure(text, &error))?;
    let mut manifest = Manifest::default();
    for (key, value) in table {
        match key.as_str() {
            "tools" => read_tools(value, &mut manifest),
            "workloads" => read_workloads(value, &mut manifest),
            "env" => read_env(value, &mut manifest),
            _ => manifest
                .problems
                .push(format!("{} is not a key of the manifest (tools, workloads, env)", spelled(&key))),
        }
    }
    if manifest.unreal.is_none() {
        for name in &manifest.env {
            manifest
                .problems
                .push(format!("env.{} names the unreal install but the manifest pins no unreal", spelled(name)));
        }
    }
    add_netfx_sdk(&mut manifest);
    Ok(manifest)
}

/// The tool an Unreal Engine build needs beside the engine: the .NET Framework SDK `UnrealBuildTool` looks for.
const NETFX_SDK: &str = "netfx-sdk";
/// The oldest .NET Framework SDK `UnrealBuildTool` takes, as its own error names it: one older counts as missing.
const NETFX_SDK_MIN: Version = Version {
    major: 4,
    minor: 6,
    patch: 0,
};

/// Adds a netfx-sdk of [`NETFX_SDK_MIN`] or later to the tools of a manifest that names unreal, at a version or at
/// any, as the repo's Unreal Engine builds need one; a netfx-sdk it names already is weighed at the higher of its own
/// minimum and [`NETFX_SDK_MIN`]. A manifest naming no unreal keeps its tools as they are.
fn add_netfx_sdk(manifest: &mut Manifest) {
    if manifest.unreal.is_none() {
        return;
    }
    match manifest.tools.iter_mut().find(|tool| tool.name == NETFX_SDK) {
        Some(tool) => tool.min = Some(tool.min.map_or(NETFX_SDK_MIN, |min| min.max(NETFX_SDK_MIN))),
        None => manifest.tools.push(ToolNeed {
            name: NETFX_SDK.to_owned(),
            min: Some(NETFX_SDK_MIN),
            known: true,
        }),
    }
}

/// The one value an `[env]` entry may have: the root of the Unreal Engine install a routed run chose.
const ENV_UNREAL: &str = "unreal";

/// Reads the `env` value `value` into `manifest`, each entry a name or a problem: a value that is not the string
/// `"unreal"`, a name the gate refuses ([`engine_env_name_refused`]), and a name an earlier entry kept in another
/// ASCII letter case, the first one kept. Only the first [`MOST_ENV_NAMES`] entries are read; the rest are one problem.
fn read_env(value: Value, manifest: &mut Manifest) {
    let entries = match value {
        Value::Table(entries) => entries,
        other => {
            manifest.problems.push(format!("env is not a table (TOML {})", other.type_str()));
            return;
        }
    };
    if entries.len() > MOST_ENV_NAMES {
        manifest.problems.push(format!("env has more than {MOST_ENV_NAMES} names"));
    }
    for (name, value) in entries.into_iter().take(MOST_ENV_NAMES) {
        match env_entry(&name, &value, &manifest.env) {
            Ok(()) => manifest.env.push(name),
            Err(problem) => manifest.problems.push(problem),
        }
    }
}

/// Whether the `[env]` entry `name` whose value is `value` is kept beside the names `kept`; `Err` with its problem.
fn env_entry(name: &str, value: &Value, kept: &[String]) -> Result<(), String> {
    let Value::String(text) = value else {
        return Err(format!("env.{} is not a string (TOML {})", spelled(name), value.type_str()));
    };
    if engine_env_name_refused(name) {
        return Err(format!("env.{} is a name the gate sets or refuses", spelled(name)));
    }
    if text != ENV_UNREAL {
        return Err(format!("env.{} is not \"{ENV_UNREAL}\": {text:?}", spelled(name)));
    }
    if kept.iter().any(|earlier| earlier.eq_ignore_ascii_case(name)) {
        return Err(format!("env.{} is named twice", spelled(name)));
    }
    Ok(())
}

/// `text` without a leading byte order mark.
fn without_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
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
            Ok(tool) => {
                if tool.name == GODOT {
                    manifest.godot = exact_entry(&value);
                }
                if tool.name == UNREAL {
                    manifest.unreal = exact_entry(&value);
                }
                manifest.tools.push(tool);
            }
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

/// The `[tools] godot` or `[tools] unreal` entry the value `value` pins, [`tool_need`] having just read it: `None` for a
/// value that is no string or names no version at all, which [`tool_need`] has already named as a problem, so that no
/// entry is read as a pin and, for godot, the project files' own pins apply.
fn exact_entry(value: &Value) -> Option<Exact> {
    match value.as_str()? {
        "" => Some(Exact::Any),
        text => Pin::parse(text).map(Exact::Exactly),
    }
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
/// version names, on the parts its text gives, as [`Pin::release_of`] reads one. Nothing else is read: not an
/// `<Import Sdk="..."/>`, nor an `<Sdk>` element nested deeper. The scan skips comments, CDATA, processing
/// instructions and declarations, reads attribute values in either quote as written (entities are not decoded), spells
/// element and attribute names exactly, and the SDK name in any letter case. An SDK with no version gives no pin, nor
/// does one whose version names no release, such as the `Godot.NET.Sdk/min=4.7.2` form or an `<Sdk>` element with only
/// a `MinimumVersion`.
pub(crate) fn godot_sdk_pins(csproj_text: &str) -> Vec<Pin> {
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
    from_root.chain(from_elements).filter_map(Pin::release_of).collect()
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
        Exact, GodotChoice, Judged, MANIFEST_SAID, Manifest, NoChosenGodot, Pin, Pinned, Reported, Standing, ToolNeed, Version, add_git_lfs,
        dotnet_tools, engine_env, godot_choice, godot_not_listed_line, godot_pins_read, godot_required, godot_sdk_pins, is_sent, judge,
        judge_with_git_lfs, meets, needs_of, newest_godot, no_chosen_godot, no_godot_install_line, no_pin_godot_warning, pinned, read, read_bytes,
        reported, reported_as, routed_needs, several_godot_line, skip_line, standing, unreadable_godot_pins_line, unreadable_line, unreal_choice,
        with_git_lfs,
    };
    use crate::remote::git::FAILURE_BYTES;
    use crate::remote::parity::{Need, NeedKind};

    fn version(major: u64, minor: u64, patch: u64) -> Version {
        Version { major, minor, patch }
    }

    /// The pins `texts` spell.
    fn pins_of(texts: &[&str]) -> Vec<Pin> {
        texts.iter().map(|text| Pin::parse(text).expect("the pin reads")).collect()
    }

    /// What the preflight said of godot: the builder's install list `list`, beside another tool's answer.
    fn godot_listed(list: &str) -> Vec<(String, Reported)> {
        vec![
            ("node".to_owned(), Reported::Version("4.7.2".to_owned())),
            ("godot".to_owned(), Reported::Version(list.to_owned())),
        ]
    }

    /// A manifest's `unreal = ""` chooses the highest engine the builder listed, as godot's no-pin choice takes the
    /// newest; no entry chooses none, and a builder listing none leaves nothing to choose.
    #[test]
    fn an_empty_unreal_entry_chooses_the_highest_listed_version() {
        let listed = vec![
            ("node".to_owned(), Reported::Version("24.0.0".to_owned())),
            ("unreal".to_owned(), Reported::Version("5.7.3,5.10.0,5.8.0".to_owned())),
        ];

        assert_eq!(unreal_choice(Some(&[]), &listed).as_deref(), Some("5.10.0"));
        assert_eq!(unreal_choice(None, &listed), None, "no unreal entry");
        assert_eq!(unreal_choice(Some(&[]), &[]), None, "no unreal listed");
    }

    #[test]
    fn one_full_pin_chooses_the_install_of_that_version() {
        let chosen = godot_choice(Some(&pins_of(&["4.7.2"])), &godot_listed("4.3.0,4.7.2,4.8.0"));
        assert_eq!(chosen, GodotChoice::Version("4.7.2".to_owned()));
    }

    #[test]
    fn a_partial_pin_chooses_the_highest_install_meeting_it_whatever_the_list_order() {
        let chosen = godot_choice(Some(&pins_of(&["4.7"])), &godot_listed("4.7.10,4.8.0,4.7.2,4.6.9"));
        assert_eq!(chosen, GodotChoice::Version("4.7.10".to_owned()));
        let major = godot_choice(Some(&pins_of(&["4"])), &godot_listed("4.3.0,5.0.0,4.7.2"));
        assert_eq!(major, GodotChoice::Version("4.7.2".to_owned()));
    }

    #[test]
    fn several_differing_pins_choose_none_and_name_them_in_order() {
        let chosen = godot_choice(Some(&pins_of(&["4.7.2", "4.3"])), &godot_listed("4.3.0,4.7.2"));
        assert_eq!(chosen, GodotChoice::Several("4.7.2, 4.3".to_owned()));
        assert_eq!(
            several_godot_line("4.7.2, 4.3"),
            "gate: the repo pins several Godot versions (4.7.2, 4.3); a routed run needs one"
        );
    }

    #[test]
    fn any_version_or_no_pin_chooses_none() {
        assert_eq!(godot_choice(Some(&[]), &godot_listed("4.7.2")), GodotChoice::None, "any version");
        assert_eq!(godot_choice(None, &godot_listed("4.7.2")), GodotChoice::None, "no pin");
    }

    #[test]
    fn the_lines_say_what_a_run_with_no_chosen_version_does() {
        assert_eq!(
            no_godot_install_line("builder"),
            "gate: the repo pins no Godot version and builder lists no Godot install; a routed run needs one"
        );
        assert_eq!(
            unreadable_godot_pins_line(),
            "gate: the repo's Godot pins could not be read; a routed run needs one"
        );
        assert_eq!(
            no_pin_godot_warning("4.7.2", "builder"),
            "gate: warning: the repo pins no Godot version, so this run uses 4.7.2, the newest on builder; pin the version it needs in \
             .gate/toolchain.toml ([tools] godot = \"x.y.z\") or as the .csproj's Godot.NET.Sdk version"
        );
    }

    #[test]
    fn a_godot_command_of_a_repo_with_no_chosen_version_uses_the_newest_or_stays_here() {
        let installs = godot_listed("4.3.0,4.7.2");
        assert_eq!(newest_godot(&installs), Some("4.7.2".to_owned()));
        assert_eq!(newest_godot(&godot_listed("")), None, "an empty install list");
        assert_eq!(
            no_chosen_godot(None, true, true, &installs, "builder"),
            NoChosenGodot::Newest {
                version: "4.7.2".to_owned(),
                warning: Some(no_pin_godot_warning("4.7.2", "builder")),
            },
            "no pin: the newest, warned"
        );
        assert_eq!(
            no_chosen_godot(Some(&[]), true, true, &installs, "builder"),
            NoChosenGodot::Newest {
                version: "4.7.2".to_owned(),
                warning: None,
            },
            "any version: the newest, with nothing said"
        );
        assert_eq!(
            no_chosen_godot(None, true, true, &godot_listed(""), "builder"),
            NoChosenGodot::Refuse(godot_not_listed_line("builder")),
            "a version list with no version in it"
        );
        assert_eq!(
            no_chosen_godot(Some(&[]), true, true, &[], "builder"),
            NoChosenGodot::Refuse(godot_not_listed_line("builder")),
            "no godot answer at all"
        );
        let missing = [("godot".to_owned(), Reported::Missing)];
        let skipped = [("godot".to_owned(), Reported::Skipped("no answer in 2 s".to_owned()))];
        assert_eq!(
            no_chosen_godot(Some(&[]), true, true, &missing, "builder"),
            NoChosenGodot::None,
            "any version and a missing godot: the mismatch already refuses the run"
        );
        assert_eq!(
            no_chosen_godot(Some(&[]), true, true, &skipped, "builder"),
            NoChosenGodot::Refuse(godot_not_listed_line("builder")),
            "any version and a skipped probe"
        );
        assert_eq!(
            no_chosen_godot(None, true, true, &missing, "builder"),
            NoChosenGodot::Refuse(no_godot_install_line("builder")),
            "no pin and a missing godot"
        );
        assert_eq!(
            no_chosen_godot(None, true, true, &skipped, "builder"),
            NoChosenGodot::Refuse(godot_not_listed_line("builder")),
            "no pin and a skipped probe"
        );
        assert_eq!(
            no_chosen_godot(Some(&[]), false, true, &installs, "builder"),
            NoChosenGodot::Newest {
                version: "4.7.2".to_owned(),
                warning: None,
            },
            "any version is the manifest's own word, whatever a project file left unread"
        );
        assert_eq!(
            no_chosen_godot(None, false, true, &installs, "builder"),
            NoChosenGodot::Refuse(unreadable_godot_pins_line()),
            "the pins could not be read, so a version may be there"
        );
        let unreadable: Result<Manifest, String> = Err("line 1, column 7: unclosed table".to_owned());
        assert!(!godot_pins_read(Some(&unreadable), &[]), "a manifest that cannot be read");
        assert!(
            !godot_pins_read(None, &["gate: a project file was left unread".to_owned()]),
            "a project file left unread"
        );
        assert!(godot_pins_read(None, &[]), "no manifest and nothing left unread");
        assert!(godot_pins_read(Some(&Ok(manifest(""))), &[]), "a manifest that reads");
        assert_eq!(
            no_chosen_godot(Some(&pins_of(&["4.7.2"])), true, true, &godot_listed("4.3.0"), "builder"),
            NoChosenGodot::None,
            "an unmet pin is the mismatch's own refusal"
        );
        assert_eq!(
            no_chosen_godot(Some(&pins_of(&["4.7.2"])), false, true, &godot_listed("4.3.0"), "builder"),
            NoChosenGodot::None,
            "an unmet pin keeps the mismatch's own refusal, even with a project file left unread"
        );
        assert_eq!(
            no_chosen_godot(None, true, false, &installs, "builder"),
            NoChosenGodot::None,
            "a command that is no Godot console"
        );
    }

    #[test]
    fn a_pin_no_listed_install_meets_chooses_none() {
        let pins = pins_of(&["4.7.2"]);
        assert_eq!(godot_choice(Some(&pins), &godot_listed("4.3.0,4.7.1")), GodotChoice::None, "not listed");
        assert_eq!(godot_choice(Some(&pins), &[]), GodotChoice::None, "no godot answer");
        let missing = [("godot".to_owned(), Reported::Missing)];
        assert_eq!(godot_choice(Some(&pins), &missing), GodotChoice::None, "missing");
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
                "\"a b\" is not a key of the manifest (tools, workloads, env)",
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
        assert_eq!(read.problems, ["siblings is not a key of the manifest (tools, workloads, env)"]);
    }

    /// The text of a manifest pinning unreal 5.8 whose `[env]` table holds `entries`.
    fn with_env(entries: &str) -> String {
        format!("[tools]\nunreal = \"5.8\"\n[env]\n{entries}")
    }

    #[test]
    fn an_env_entry_naming_unreal_is_read() {
        let read = manifest(&with_env("UE_MCP_TEST_ENGINE_ROOT = \"unreal\"\n"));
        assert_eq!(read.env, owned(&["UE_MCP_TEST_ENGINE_ROOT"]));
        assert_eq!(read.problems, Vec::<String>::new());
        assert_eq!(engine_env(Some(&Ok(read))), owned(&["UE_MCP_TEST_ENGINE_ROOT"]));
        assert_eq!(engine_env(None), Vec::<String>::new(), "no manifest names none");
        assert_eq!(
            engine_env(Some(&Err("bad".to_owned()))),
            Vec::<String>::new(),
            "an unreadable one names none"
        );
    }

    #[test]
    fn an_env_value_other_than_unreal_is_a_problem() {
        let read = manifest(&with_env("A = \"godot\"\nB = \"Unreal\"\nC = 1\nD = \"unreal\"\n"));
        assert_eq!(read.env, owned(&["D"]));
        assert_eq!(
            read.problems,
            [
                "env.A is not \"unreal\": \"godot\"",
                "env.B is not \"unreal\": \"Unreal\"",
                "env.C is not a string (TOML integer)",
            ]
        );
        let read = manifest("env = \"unreal\"\n[tools]\nunreal = \"\"\n");
        assert_eq!(read.env, Vec::<String>::new());
        assert_eq!(read.problems, ["env is not a table (TOML string)"]);
    }

    #[test]
    fn an_env_name_the_gate_refuses_is_a_problem() {
        for name in [
            "UE_ROOT",
            "GODOT_PATH",
            "GATE_ANYTHING",
            "ue_root",
            "GIT_WORK_TREE",
            "git_dir",
            "A-B",
            "_1-",
        ] {
            let read = manifest(&with_env(&format!("{name} = \"unreal\"\nKEPT = \"unreal\"\n")));
            assert_eq!(read.env, owned(&["KEPT"]), "{name}");
            assert_eq!(read.problems, [format!("env.{name} is a name the gate sets or refuses")]);
        }
        let read = manifest(&with_env("\"A=B\" = \"unreal\"\n"));
        assert_eq!(read.problems, ["env.\"A=B\" is a name the gate sets or refuses"], "a malformed name");
    }

    #[test]
    fn an_env_of_more_than_64_names_keeps_the_first_64_and_is_one_problem() {
        let names = |count: usize| -> Vec<String> { (0..count).map(|at| format!("N{at}")).collect() };
        let entries = |count: usize| -> String { names(count).iter().map(|name| name.clone() + " = \"unreal\"\n").collect() };
        let read = manifest(&with_env(&entries(66)));
        assert_eq!(read.env, names(64));
        assert_eq!(read.problems, ["env has more than 64 names"]);
        assert_eq!(manifest(&with_env(&entries(64))).problems, Vec::<String>::new(), "64 is within the cap");
    }

    #[test]
    fn an_env_name_twice_in_another_case_is_a_problem() {
        let read = manifest(&with_env("Engine_Root = \"unreal\"\nENGINE_ROOT = \"unreal\"\n"));
        assert_eq!(read.env, owned(&["Engine_Root"]), "the first is kept");
        assert_eq!(read.problems, ["env.ENGINE_ROOT is named twice"]);
    }

    #[test]
    fn an_env_entry_without_an_unreal_pin_is_a_problem_and_still_named() {
        let read = manifest("[env]\nUE_MCP_TEST_ENGINE_ROOT = \"unreal\"\n[tools]\nnode = \"24\"\n");
        assert_eq!(read.env, owned(&["UE_MCP_TEST_ENGINE_ROOT"]));
        assert_eq!(
            read.problems,
            ["env.UE_MCP_TEST_ENGINE_ROOT names the unreal install but the manifest pins no unreal"]
        );
        assert_eq!(engine_env(Some(&Ok(read))), owned(&["UE_MCP_TEST_ENGINE_ROOT"]));
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

    /// What `judge` makes of the manifest `text`, whose repo's project files pin the versions `pins` of godot, when the
    /// builder said the lines `said` to a preflight sending its needs.
    fn judged_with(text: &str, pins: &[Pin], said: &[&str]) -> Judged {
        let readable: Result<Manifest, String> = Ok(manifest(text));
        let sent: Vec<String> = routed_needs(Some(&readable), pins, false).into_iter().map(|(name, _)| name).collect();
        let said = owned(said);
        let godot = godot_required(Some(&readable), pins);
        judge(
            Some((&readable, MANIFEST_SAID)),
            godot.as_deref(),
            &sent,
            &reported(&said, &sent),
            "builder",
        )
    }

    /// [`judged_with`] for a repo whose project files pin no godot.
    fn judged(text: &str, said: &[&str]) -> Judged {
        judged_with(text, &[], said)
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
            reported_as(" skipped: Godot_console.exe --version gave no version: hello"),
            Reported::Skipped("Godot_console.exe --version gave no version: hello".to_owned())
        );
        assert_eq!(reported_as("0.9.80"), Reported::Version("0.9.80".to_owned()));
    }

    #[test]
    fn each_tool_not_checked_says_its_skip_line_in_file_order_after_the_problems() {
        let text = "[tools]\nnode = \"24\"\nnodejs = \"24\"\n\"my\\ntool\" = \"1\"\npnpm = \"10\"\npwsh = \"7.5\"\n\
                    rust = \"1.89\"\nffmpeg = \"x\"\n";
        let said = [
            "gate: remote tool node skipped: no answer in 10 s",
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
            judge(Some((&Ok(read), MANIFEST_SAID)), None, &[], &[], "builder"),
            Judged {
                needs: Vec::new(),
                skips: owned(&["gate: .gate/toolchain.toml names unknown tool nodejs; skipping it"]),
            },
            "a preflight that could not carry the needs asked about none"
        );
    }

    #[test]
    fn a_repo_with_no_manifest_says_nothing_and_one_that_cannot_be_read_says_one_skip_line() {
        assert_eq!(judge(None, None, &[], &[], "builder"), Judged::default());
        let unreadable = Err("line 1, column 7: invalid table header".to_owned());
        assert_eq!(
            judge(Some((&unreadable, MANIFEST_SAID)), None, &[], &[], "builder"),
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
            judge(Some((&Ok(problem), MANIFEST_SAID)), None, &[], &[], "builder").skips,
            ["gate: .gate/toolchain.toml: tools.\\u{202e}x is bad; skipping it"]
        );
        let unreadable = Err("line 1\u{2028}gate: forged".to_owned());
        assert_eq!(
            judge(Some((&unreadable, MANIFEST_SAID)), None, &[], &[], "builder").skips,
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
            skip_line(MANIFEST_SAID, "line 2, column 8: invalid string"),
            "gate: .gate/toolchain.toml: line 2, column 8: invalid string; skipping it"
        );
    }

    /// The pin the version text `text` spells, as a repo writes one.
    fn pin_of(text: &str) -> Pin {
        Pin::parse(text).expect("a pin")
    }

    /// The pin of the version `major.minor.patch`.
    fn pin(major: u64, minor: u64, patch: u64) -> Pin {
        pin_of(&format!("{major}.{minor}.{patch}"))
    }

    /// A pin is met by any listed install of that version, and every pin of several needs an install of its own.
    #[test]
    fn a_godot_pin_is_met_by_any_listed_install() {
        let at = |text: &str| Reported::Version(text.to_owned());
        assert_eq!(pinned(&[pin(4, 7, 2)], &at("4.7.2")), Pinned::Met, "the only install");
        assert_eq!(pinned(&[pin(4, 7, 2)], &at("4.3.0,4.7.2")), Pinned::Met, "the last of several");
        assert_eq!(pinned(&[pin(4, 7, 2)], &at("4.7.2,4.3.0")), Pinned::Met, "the first of several");
        assert_eq!(pinned(&[pin(4, 7, 2), pin(4, 3, 0)], &at("4.3.0,4.7.2")), Pinned::Met, "one each");
    }

    /// A pin no install holds is a mismatch, ending in the list the builder gave, its members trimmed and joined.
    #[test]
    fn a_godot_pin_missing_from_the_list_is_a_mismatch() {
        let at = |text: &str| Reported::Version(text.to_owned());
        assert_eq!(pinned(&[pin(4, 7, 2)], &at("4.3.0")), Pinned::Lacks("4.3.0".to_owned()));
        assert_eq!(pinned(&[pin(4, 7, 2)], &at("4.3.0,4.5.1")), Pinned::Lacks("4.3.0, 4.5.1".to_owned()));
        assert_eq!(
            pinned(&[pin(4, 7, 2)], &at("next")),
            Pinned::Lacks("next".to_owned()),
            "an answer that is no version"
        );
        assert_eq!(
            pinned(&[pin(4, 7, 2)], &at("4.7.3,abc")),
            Pinned::Lacks("4.7.3, abc".to_owned()),
            "a member that is no version is no install"
        );
        assert_eq!(
            pinned(&[pin(4, 7, 2), pin(4, 8, 0)], &at("4.7.2")),
            Pinned::Lacks("4.7.2".to_owned()),
            "one of two pins is not enough"
        );
    }

    /// A pin of two parts is met by any patch of its major and minor; one of three parts names one version alone.
    #[test]
    fn a_two_part_godot_pin_matches_any_patch() {
        let at = |text: &str| Reported::Version(text.to_owned());
        let two = pin_of("4.7");
        assert_eq!(pinned(&[two], &at("4.7.0")), Pinned::Met, "the patch the box writes out for a zero one");
        assert_eq!(pinned(&[two], &at("4.7.3")), Pinned::Met, "any patch of that minor");
        assert_eq!(pinned(&[two], &at("4.7.99")), Pinned::Met);
        assert_eq!(pinned(&[two], &at("4.6.9")), Pinned::Lacks("4.6.9".to_owned()), "another minor");
        assert_eq!(pinned(&[two], &at("4.8.0")), Pinned::Lacks("4.8.0".to_owned()), "another minor above it");
        assert_eq!(
            pinned(&[pin(4, 7, 0)], &at("4.7.3")),
            Pinned::Lacks("4.7.3".to_owned()),
            "three parts name 4.7.0 alone"
        );
    }

    /// An empty manifest entry asks for an install of any version, which one install at any version serves.
    #[test]
    fn an_empty_manifest_godot_entry_is_met_by_any_version() {
        assert_eq!(judged("[tools]\ngodot = \"\"\n", &["gate: remote tool godot 4.0.0"]), Judged::default());
        assert_eq!(
            judged("[tools]\ngodot = \"\"\n", &["gate: remote tool godot present"]),
            Judged::default(),
            "an install whose version the box did not read"
        );
        assert_eq!(
            judged("[tools]\ngodot = \"\"\n", &["gate: remote tool godot missing"]),
            Judged {
                needs: vec![lacks("godot", "none")],
                skips: Vec::new(),
            },
            "no install at all is still a need"
        );
        assert_eq!(
            judged_with("[tools]\ngodot = \"\"\n", &[pin(4, 7, 2)], &["gate: remote tool godot 4.0.0"]),
            Judged::default(),
            "the empty entry wins over what the project files pin"
        );
    }

    /// A pinned godot the probe did not answer for is a mismatch, so a pin that cannot be checked runs the command
    /// here: a repo that requires any version has no pin to check and keeps the ordinary skip line.
    #[test]
    fn a_skipped_godot_probe_is_a_mismatch_when_a_pin_needs_checking() {
        let why = "Godot_console.exe --version gave no version: hello";
        let skipped = format!("gate: remote tool godot skipped: {why}");
        let said = [skipped.as_str()];
        assert_eq!(
            judged("[tools]\ngodot = \"4.7.2\"\n", &said),
            Judged {
                needs: vec![lacks("godot 4.7.2 exactly", &format!("not probed ({why})"))],
                skips: Vec::new(),
            }
        );
        assert_eq!(
            judged("[tools]\ngodot = \"\"\n", &said),
            Judged {
                needs: Vec::new(),
                skips: owned(&[&format!("gate: builder did not probe godot: {why}; not checking it")]),
            }
        );
    }

    /// A builder holding no godot at all is a mismatch, and the line says it has `none`.
    #[test]
    fn a_missing_godot_is_a_mismatch_saying_none() {
        assert_eq!(pinned(&[pin(4, 7, 2)], &Reported::Missing), Pinned::Lacks("none".to_owned()));
        assert_eq!(
            judged("[tools]\ngodot = \"4.7.2\"\n", &["gate: remote tool godot missing"]),
            Judged {
                needs: vec![lacks("godot 4.7.2 exactly", "none")],
                skips: Vec::new(),
            }
        );
    }

    /// A manifest pinning unreal at `pin`, as `[tools] unreal = "<pin>"`.
    fn unreal_manifest(pin: &str) -> String {
        format!("[tools]\nunreal = \"{pin}\"\n")
    }

    /// The builder's unreal tool line, answering `what`.
    fn unreal_line(what: &str) -> String {
        format!("gate: remote tool unreal {what}")
    }

    /// The tool line of a builder listing a .NET Framework SDK that serves an Unreal-pinned repo.
    const NETFX_SDK_SERVED: &str = "gate: remote tool netfx-sdk 4.6.2,4.8";

    /// What `judge` makes of a manifest pinning unreal at `pin`, when the builder answered `what` of unreal and lists a
    /// .NET Framework SDK that serves ([`NETFX_SDK_SERVED`]).
    fn unreal_judged(pin: &str, what: &str) -> Judged {
        judged(&unreal_manifest(pin), &[&unreal_line(what), NETFX_SDK_SERVED])
    }

    /// The need of a .NET Framework SDK an Unreal-pinned repo has, of which the builder has `found`.
    fn netfx_lacks(found: &str) -> Need {
        lacks("netfx-sdk 4.6.0 or later", found)
    }

    /// A manifest pinning unreal, at a version or at any, needs a .NET Framework SDK 4.6 or later besides, sent with the
    /// manifest's own needs, after them.
    #[test]
    fn an_unreal_pin_adds_a_netfx_sdk_4_6_or_later() {
        let netfx = tool("netfx-sdk", Some(version(4, 6, 0)), true);
        for pin in ["5.8", ""] {
            let read = manifest(&format!("[tools]\nunreal = \"{pin}\"\nnode = \"24\"\n"));
            assert_eq!(read.tools.last(), Some(&netfx), "{pin:?}: {read:?}");
            assert_eq!(read.tools.iter().filter(|tool| tool.name == "netfx-sdk").count(), 1, "{pin:?}");
            assert_eq!(
                needs_of(&read).last(),
                Some(&("netfx-sdk".to_owned(), "4.6.0".to_owned())),
                "{pin:?}: the need is sent"
            );
        }
    }

    /// A manifest's own netfx-sdk entry beside an unreal pin is weighed at the higher of its minimum and 4.6, once.
    #[test]
    fn a_declared_netfx_sdk_beside_an_unreal_pin_is_weighed_at_4_6_or_its_own_minimum_if_higher() {
        let declared = |text: &str| -> Vec<ToolNeed> {
            let read = manifest(&format!("[tools]\nnetfx-sdk = \"{text}\"\nunreal = \"5.8\"\n"));
            read.tools.into_iter().filter(|tool| tool.name == "netfx-sdk").collect()
        };
        assert_eq!(declared("4"), [tool("netfx-sdk", Some(version(4, 6, 0)), true)]);
        assert_eq!(declared(""), [tool("netfx-sdk", Some(version(4, 6, 0)), true)]);
        assert_eq!(declared("4.8"), [tool("netfx-sdk", Some(version(4, 8, 0)), true)]);
    }

    /// A manifest with no unreal pin needs no .NET Framework SDK, and one it names itself keeps its own minimum.
    #[test]
    fn no_unreal_pin_adds_no_netfx_sdk() {
        let read = manifest("[tools]\nnode = \"24\"\n");
        assert!(read.tools.iter().all(|tool| tool.name != "netfx-sdk"), "{read:?}");
        assert!(needs_of(&read).iter().all(|(name, _)| name != "netfx-sdk"));
        assert_eq!(
            manifest("[tools]\nnetfx-sdk = \"4\"\n").tools,
            [tool("netfx-sdk", Some(version(4, 0, 0)), true)],
            "a known tool a manifest may name itself"
        );
    }

    /// An Unreal-pinned repo's builder with no .NET Framework SDK, or only ones older than 4.6, lacks it; one listing
    /// 4.6 or later, alone or among older ones, serves it.
    #[test]
    fn an_unreal_pin_judges_the_netfx_sdk_missing_too_old_or_ok() {
        let unreal = unreal_line("5.8.0");
        let with = |netfx: &str| judged(&unreal_manifest("5.8"), &[&unreal, &format!("gate: remote tool netfx-sdk {netfx}")]);
        let lacking = |found: &str| Judged {
            needs: vec![netfx_lacks(found)],
            skips: Vec::new(),
        };

        assert_eq!(with("missing"), lacking("none"), "missing");
        assert_eq!(with("4.5.2"), lacking("4.5.2"), "too old");
        assert_eq!(with("4.5,4.5.2"), lacking("4.5,4.5.2"), "every one too old");
        assert_eq!(with("4.6"), Judged::default(), "4.6 itself");
        assert_eq!(with("4.8"), Judged::default(), "later");
        assert_eq!(with("4.5.2,4.8"), Judged::default(), "a later one among older ones");
    }

    /// A builder that says nothing of the .NET Framework SDK an Unreal-pinned repo needs is said not to have probed it.
    #[test]
    fn an_unreal_pin_with_no_netfx_sdk_answer_says_it_was_not_probed() {
        assert_eq!(
            judged(&unreal_manifest("5.8"), &[&unreal_line("5.8.0")]),
            Judged {
                needs: Vec::new(),
                skips: owned(&["gate: builder did not probe netfx-sdk: the builder gave no answer; not checking it"]),
            }
        );
    }

    /// Several versions, as a probe of several installs names them, meet a minimum when the highest does.
    #[test]
    fn meets_weighs_the_highest_of_several_versions() {
        let min = version(4, 6, 0);
        assert_eq!(meets(min, "4.5.2,4.8"), Some(true));
        assert_eq!(meets(min, "4.8,4.5.2"), Some(true), "in any order");
        assert_eq!(meets(min, "4.5,4.5.2"), Some(false));
        assert_eq!(meets(min, "4.5,next"), None, "a member that is no version");
        assert_eq!(meets(min, "4.8,"), None, "an empty member");
    }

    /// An unreal pin is met by any listed engine of that version, first, last or only.
    #[test]
    fn an_unreal_pin_is_met_by_any_listed_install() {
        for list in ["5.8.0", "5.7.3,5.8.0", "5.8.0,5.9.0"] {
            assert_eq!(unreal_judged("5.8.0", list), Judged::default(), "{list}");
        }
    }

    /// An unreal pin no listed engine holds is a mismatch, ending in the list the builder gave, its members joined.
    #[test]
    fn an_unreal_pin_missing_from_the_list_is_a_mismatch() {
        assert_eq!(
            unreal_judged("5.8.0", "5.7.3,5.9.0"),
            Judged {
                needs: vec![lacks("unreal 5.8.0 exactly", "5.7.3, 5.9.0")],
                skips: Vec::new(),
            }
        );
        assert_eq!(
            unreal_judged("5.8.0", "next"),
            Judged {
                needs: vec![lacks("unreal 5.8.0 exactly", "next")],
                skips: Vec::new(),
            },
            "an answer that is no version list"
        );
    }

    /// An unreal pin of two parts is met by any patch of its major and minor, and by no other minor.
    #[test]
    fn a_two_part_unreal_pin_matches_any_patch() {
        assert_eq!(unreal_judged("5.8", "5.8.0"), Judged::default());
        assert_eq!(unreal_judged("5.8", "5.8.3"), Judged::default());
        assert_eq!(
            unreal_judged("5.8", "5.7.9"),
            Judged {
                needs: vec![lacks("unreal 5.8 exactly", "5.7.9")],
                skips: Vec::new(),
            }
        );
    }

    /// An empty manifest unreal entry asks for an engine of any version, which one engine at any version serves.
    #[test]
    fn an_empty_manifest_unreal_entry_is_met_by_any_version() {
        assert_eq!(unreal_judged("", "4.27.2"), Judged::default());
        assert_eq!(
            unreal_judged("", "missing"),
            Judged {
                needs: vec![lacks("unreal", "none")],
                skips: Vec::new(),
            },
            "no engine at all is still a need"
        );
    }

    /// A pinned unreal the probe did not answer for is a mismatch; a repo that requires any version keeps the skip line.
    #[test]
    fn a_skipped_unreal_probe_is_a_mismatch_when_a_pin_needs_checking() {
        let why = "GATE_UNREAL_ROOT unreal is not an absolute path on a drive";
        let skipped = format!("skipped: {why}");
        assert_eq!(
            unreal_judged("5.8", &skipped),
            Judged {
                needs: vec![lacks("unreal 5.8 exactly", &format!("not probed ({why})"))],
                skips: Vec::new(),
            }
        );
        assert_eq!(
            unreal_judged("", &skipped),
            Judged {
                needs: Vec::new(),
                skips: owned(&[&format!("gate: builder did not probe unreal: {why}; not checking it")]),
            }
        );
    }

    /// A builder holding no engine at all is a mismatch, and the line says it has `none`.
    #[test]
    fn a_missing_unreal_is_a_mismatch_saying_none() {
        assert_eq!(
            unreal_judged("5.8", "missing"),
            Judged {
                needs: vec![lacks("unreal 5.8 exactly", "none")],
                skips: Vec::new(),
            }
        );
    }

    #[test]
    fn a_run_with_lfs_pointers_needs_git_lfs_3_1_3_or_later_once() {
        let mut needs = vec![("node".to_owned(), "24.0.0".to_owned())];
        with_git_lfs(&mut needs);
        with_git_lfs(&mut needs);
        assert_eq!(
            needs,
            [("node".to_owned(), "24.0.0".to_owned()), ("git-lfs".to_owned(), "3.1.3".to_owned())]
        );
        let sent = owned(&["git-lfs"]);
        let judged_with = |manifest: Option<(&Result<Manifest, String>, &str)>, answer: Reported| {
            judge_with_git_lfs(manifest, None, &sent, &[("git-lfs".to_owned(), answer)], "builder")
        };

        let missing = judged_with(None, Reported::Missing);
        let met = judged_with(None, Reported::Version("3.8.0".to_owned()));

        assert_eq!(missing.needs.len(), 1, "{missing:?}");
        assert!(missing.needs[0].text.contains("git-lfs"), "{missing:?}");
        assert_eq!(met, Judged::default());
        let oldest = judged_with(None, Reported::Version("3.1.3".to_owned()));
        let older = judged_with(None, Reported::Version("3.1.2".to_owned()));
        assert_eq!(oldest, Judged::default(), "3.1.3 is taken");
        assert_eq!(older.needs, [lacks("git-lfs 3.1.3 or later", "3.1.2")], "3.1.2 counts as missing");
        let mut read = manifest("[tools]\nnode = \"24\"\n");
        add_git_lfs(&mut read);
        add_git_lfs(&mut read);
        assert_eq!(needs_of(&read), needs, "the check sends what the run sends");
    }

    /// A manifest's own git-lfs entry is weighed at the higher of its minimum and 3.1.3, in the need sent, in the
    /// judgement and in the check, once.
    #[test]
    fn a_declared_git_lfs_is_weighed_at_3_1_3_or_its_own_minimum_if_higher() {
        let sent = owned(&["git-lfs"]);
        let declared = |text: &str| Ok(manifest(&format!("[tools]\ngit-lfs = \"{text}\"\n")));
        let judged_with = |manifest: &Result<Manifest, String>, found: &str| {
            judge_with_git_lfs(
                Some((manifest, MANIFEST_SAID)),
                None,
                &sent,
                &[("git-lfs".to_owned(), Reported::Version(found.to_owned()))],
                "builder",
            )
        };

        assert_eq!(judged_with(&declared("3"), "3.0.2").needs, [lacks("git-lfs 3.1.3 or later", "3.0.2")]);
        assert_eq!(judged_with(&declared("3"), "3.1.3"), Judged::default(), "3.1.3 routes");
        assert_eq!(judged_with(&declared(""), "3.0.2").needs, [lacks("git-lfs 3.1.3 or later", "3.0.2")]);
        assert_eq!(judged_with(&declared("3.5"), "3.4.0").needs, [lacks("git-lfs 3.5.0 or later", "3.4.0")]);
        let missing = judge_with_git_lfs(
            Some((&declared("3"), MANIFEST_SAID)),
            None,
            &sent,
            &[("git-lfs".to_owned(), Reported::Missing)],
            "builder",
        );
        assert_eq!(missing.needs, [lacks("git-lfs 3.1.3 or later", "none")], "one need");
        for (text, wanted) in [("3.0.0", "3.1.3"), ("", "3.1.3"), ("4.0.0", "4.0.0")] {
            let mut needs = vec![("git-lfs".to_owned(), text.to_owned())];
            with_git_lfs(&mut needs);
            assert_eq!(needs, [("git-lfs".to_owned(), wanted.to_owned())], "{text}");
        }
        let mut read = manifest("[tools]\ngit-lfs = \"3\"\n");
        add_git_lfs(&mut read);
        assert_eq!(
            needs_of(&read),
            [("git-lfs".to_owned(), "3.1.3".to_owned())],
            "the check weighs it so too"
        );
    }

    /// An unreal pin is exact, never the minimum its manifest text would otherwise read as: a later engine misses it.
    #[test]
    fn an_unreal_pin_is_not_judged_as_a_minimum() {
        assert_eq!(
            unreal_judged("5.8", "5.9.0"),
            Judged {
                needs: vec![lacks("unreal 5.8 exactly", "5.9.0")],
                skips: Vec::new(),
            }
        );
    }

    /// The manifest's own `godot` entry wins over what the project files pin, and an entry or a manifest that cannot be
    /// read leaves those pins applying.
    #[test]
    fn the_manifest_godot_entry_wins_over_csproj_pins() {
        let pins = [pin(4, 8, 0)];
        assert_eq!(
            manifest("[tools]\ngodot = \"4.7\"\n").godot,
            Some(Exact::Exactly(pin_of("4.7"))),
            "the entry is read as a pin, on the parts it gives"
        );
        assert_eq!(manifest("[tools]\ngodot = \"\"\n").godot, Some(Exact::Any));
        assert_eq!(manifest("[tools]\nnode = \"24\"\n").godot, None, "a manifest naming no godot");
        assert_eq!(
            routed_needs(Some(&Ok(manifest("[tools]\ngodot = \"4.7\"\n"))), &pins, false),
            [("godot".to_owned(), "4.7".to_owned())]
        );
        assert_eq!(
            routed_needs(Some(&Ok(manifest("[tools]\nnode = \"24\"\n"))), &pins, false),
            [("node".to_owned(), "24.0.0".to_owned()), ("godot".to_owned(), "4.8.0".to_owned())],
            "with no entry the files' pins apply, after the manifest's own tools"
        );
        assert_eq!(
            routed_needs(Some(&Ok(manifest("[tools]\nnode = \"24\"\n"))), &[], false),
            [("node".to_owned(), "24.0.0".to_owned())],
            "a repo requiring no godot sends no need for it to a command that is no Godot console"
        );
        assert_eq!(
            routed_needs(Some(&Ok(manifest("[tools]\nnode = \"24\"\n"))), &[], true),
            [("node".to_owned(), "24.0.0".to_owned()), ("godot".to_owned(), String::new())],
            "a Godot command of a repo requiring none has the builder list its installs"
        );
        let unreadable: Result<Manifest, String> = Err("line 1, column 1: x".to_owned());
        assert_eq!(
            godot_required(Some(&unreadable), &pins),
            Some(pins.to_vec()),
            "an unreadable manifest stops nothing"
        );
        assert_eq!(godot_required(None, &pins), Some(pins.to_vec()), "and so does no manifest at all");
        assert_eq!(godot_required(None, &[]), None, "a repo pinning nothing requires nothing");
        assert_eq!(godot_required(Some(&Ok(manifest("[tools]\ngodot = \"\"\n"))), &pins), Some(Vec::new()));
    }

    /// Every pin of several must be an install the builder has, and the same version pinned twice is one pin.
    #[test]
    fn several_csproj_pins_each_need_an_install() {
        let pins = [pin(4, 7, 2), pin(4, 8, 0)];
        let at = |text: &str| Reported::Version(text.to_owned());
        assert_eq!(
            pinned(&pins, &at("4.7.2")),
            Pinned::Lacks("4.7.2".to_owned()),
            "the second pin has no install"
        );
        assert_eq!(pinned(&pins, &at("4.7.2,4.8.0")), Pinned::Met);
        assert_eq!(
            judged_with(
                "[tools]\nnode = \"24\"\n",
                &pins,
                &["gate: remote tool godot 4.7.2", "gate: remote tool node 24.0.0"]
            ),
            Judged {
                needs: vec![lacks("godot 4.7.2, 4.8.0 exactly", "4.7.2")],
                skips: Vec::new(),
            }
        );
        assert_eq!(godot_required(None, &[pin(4, 7, 2), pin(4, 7, 2)]), Some(vec![pin(4, 7, 2)]));
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
        assert_eq!(godot_sdk_pins(csproj), [pin(4, 7, 2)]);
        let declared = "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!-- the client -->\n\
                        <Project Sdk='Microsoft.NET.Sdk; godot.net.sdk/4.4.0-beta.1'>\n</Project>\n";
        assert_eq!(godot_sdk_pins(declared), [pin(4, 4, 0)], "a list, a prerelease, either quote");
        assert_eq!(
            godot_sdk_pins("<Project Sdk=\"Godot.NET.Sdk/4.7.2.1\">\n</Project>\n"),
            [pin(4, 7, 2)],
            "four parts name the first three, as a package version may have four"
        );
    }

    #[test]
    fn a_csproj_s_sdk_element_names_its_godot_sdk() {
        let csproj = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!-- <Sdk Name=\"Godot.NET.Sdk\" Version=\"9.9.9\" /> -->\n\
                      <Project>\n  <Sdk Name=\"Godot.NET.Sdk\" Version=\"4.7.2\" />\n  \
                      <Sdk Name=\"Microsoft.NET.Sdk\" Version=\"10.0.100\" />\n  <Sdk Name=\"Godot.NET.Sdk\" />\n  \
                      <PropertyGroup><TargetFramework>net10.0</TargetFramework></PropertyGroup>\n</Project>\n";
        assert_eq!(godot_sdk_pins(csproj), [pin(4, 7, 2)], "a comment and an SDK with no version name none");
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
        assert_eq!(godot_sdk_pins(nested), [pin(4, 6, 0)], "an Sdk element is read only as the root's child");
        assert_eq!(
            godot_sdk_pins("<Other Sdk=\"Godot.NET.Sdk/4.7.2\"><Sdk Name=\"Godot.NET.Sdk\" Version=\"4.7.2\"/></Other>"),
            [],
            "a root that is not a Project"
        );
    }

    #[test]
    fn a_quoted_greater_than_sign_does_not_end_the_tag() {
        let csproj = "<Project Condition=\"'$(A)' > '1'\" Sdk=\"Godot.NET.Sdk/4.7.2\">\n</Project>\n";
        assert_eq!(godot_sdk_pins(csproj), [pin(4, 7, 2)]);
    }

    #[test]
    fn a_cdata_section_holds_no_element() {
        let csproj = "<Project>\n  <![CDATA[<Sdk Name=\"Godot.NET.Sdk\" Version=\"9.9.9\"/>]]>\n  \
                      <Sdk Name=\"Godot.NET.Sdk\" Version=\"4.7.2\"/>\n</Project>\n";
        assert_eq!(godot_sdk_pins(csproj), [pin(4, 7, 2)]);
    }

    #[test]
    fn whitespace_around_an_attribute_s_equals_sign_is_read() {
        assert_eq!(godot_sdk_pins("<Project Sdk = 'Godot.NET.Sdk/4.7.2'>\n</Project>\n"), [pin(4, 7, 2)]);
    }
}
