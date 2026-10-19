//! What `gate check` says, as a pure function of where it looked ([`At`]) and what it found ([`Checked`]): a header, a
//! builder's created folders, each item's line in the order manifest tools, the .NET SDK, workloads, the routing file's
//! `siblings` problem, siblings, the warm-up's items (`warm-up: ...`), an install hint per missing or too-old item, then
//! a summary, all prefixed `check: <where>: ` (`install: <where>: ` for a hint), `<where>` being `here` for this PC and
//! the host for a builder, and the problems of the `.gate` files on stderr as `check: .gate/<file>: <problem>`. A
//! builder's hints are commands that work over ssh, and its SDK line names the builder. The exit status is 0 only when
//! nothing is missing or too old: a skipped item, and a warm-up line that only informs, never fails the check.

use gate_router::warm::{shown_value, warming_text};
use gate_router::{SiblingFolder, spelled};

use crate::answer::FAILED_STATUS;
use crate::boxside::payload::escaped;
use crate::remote::toolchain::{Pin, Pinned, Reported, Standing, ToolNeed, Version, pinned, standing};

/// Where the check looked: this PC, or the builder `host`, whose work folder for the repo is `key`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum At<'a> {
    Here,
    Builder { host: &'a str, key: &'a str },
}

impl At<'_> {
    /// The `<where>` of the lines: `here`, or the builder's host.
    fn name(&self) -> &str {
        match self {
            At::Here => "here",
            At::Builder { host, .. } => host,
        }
    }
}

/// This PC, where the check looks without `--builder`.
pub(crate) const HERE: At<'static> = At::Here;

/// The most created folders, and the most SDKs of a list, a builder's lines show: the rest are counted, so a builder
/// cannot flood the console with them.
const BUILDER_SHOWN: usize = 20;

/// A manifest tool and what its probe found; an unknown tool's is never read.
#[derive(Debug)]
pub(crate) struct Tool {
    pub need: ToolNeed,
    pub reported: Reported,
    /// Why the check did not look for the tool where it ran, its probe aside: an unreal on a PC with no
    /// `GATE_UNREAL_ROOT`. Said as `<name> not checked here: <why>`, counted neither missing nor skipped.
    pub not_checked: Option<String>,
}

/// The .NET SDK the root `global.json` pins, as checked against this machine's SDKs.
#[derive(Debug)]
pub(crate) enum Sdk {
    /// The pin read and the SDK list read: the pinned `version`, the need in parity's words (`.NET SDK <version>
    /// (rollForward <policy>)`), the SDKs this machine has, and whether they serve the pin.
    Checked {
        version: String,
        need: String,
        have: Vec<String>,
        served: bool,
    },
    /// The pin or the SDK list could not be read, for this reason.
    Skipped(String),
}

/// A sibling the repo declares: its name, where it must be, and what the check found of it.
#[derive(Debug)]
pub(crate) struct Sibling {
    pub name: String,
    pub path: String,
    pub found: Found,
}

/// What the check found of a declared sibling, asked in routing's order: consent, then the name, then the folder.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Found {
    /// This PC's routing file does not allow the repo to carry it, as routing asks; its folder is not looked for.
    NotAllowed,
    /// What the folder check found ([`route::sibling_folder`](crate::route::sibling_folder)), or why that cannot be
    /// told; a sibling named like the worktree's own folder is refused without a look.
    Folder(Result<SiblingFolder, String>),
}

/// How the lines name the manifest.
pub(crate) const TOOLCHAIN: &str = ".gate/toolchain.toml";
/// How the lines name the siblings file.
pub(crate) const SIBLINGS: &str = ".gate/siblings.toml";
/// How the lines name the root SDK pin file.
pub(crate) const GLOBAL_JSON: &str = "global.json";
/// How the lines name the warm-up file.
pub(crate) const WARMUP: &str = ".gate/warmup.toml";
/// This PC's routing file, as a problem line names it.
pub(crate) const REMOTE_JSON: &str = "remote.json";

/// One item of the repo's warm-up, as the check found it: the warm-up file and `remote.json`'s consent, then, with
/// both, the work folder's warm-up record on this PC, and for a builder what its preflight said of its work folder.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Warmup {
    /// The repo has a warm-up file, but `remote.json` does not consent to its warm-ups.
    Unconsented,
    /// `remote.json` consents to the repo's warm-ups, but it has no warm-up file.
    NoFile,
    /// `remote.json`'s `warmup` value cannot be used, for this reason, and the repo has a warm-up file.
    ConsentProblem(String),
    /// The consented warm-up file has problems, said on lines of their own, so `gate warm` cannot run.
    FileProblems,
    /// The state root, where the record is, cannot be found, for this reason.
    NoStateRoot(String),
    /// The record says the work folder is warm on `host` for the file on disk.
    Warm { host: String },
    /// The work folder's warm-up lock is held: `host` is warming it.
    Warming { host: String },
    /// A warm-up on `host` failed `minutes_ago` whole minutes ago, for `reason`.
    Failed { host: String, minutes_ago: u64, reason: String },
    /// The work folder is not warm on `host` and nothing holds a warm-up back; `stale` when the record still says
    /// `warming` with nothing holding its lock.
    NotWarm { host: String, stale: bool },
    /// The record cannot be read: the text. `gate warm` overwrites it, and only a `layout` fallback's delete reads it.
    UnreadableRecord(String),
    /// The lock cannot be probed: the text. `gate warm` takes it, and a routed run waiting on the work folder's key
    /// probes it.
    UnreadableLock(String),
    /// What a builder's preflight said of its work folder: `warm` when its `.warm` holds the warm-up file's hash.
    Builder { warm: bool },
}

/// A problem of a file the check reads: how the lines name the file and the problem, as its reader words it.
#[derive(Debug)]
pub(crate) struct Problem {
    pub file: &'static str,
    pub text: String,
}

/// What the check found of one repo.
#[derive(Debug)]
pub(crate) struct Checked {
    /// The repo's name, as routing names it.
    pub repo: String,
    /// The worktree root, as git spells it.
    pub root: String,
    /// Each file taken from the repo's declarations folder, as the lines name it in `.gate`, with its full path there,
    /// which its problem lines and the summary name instead.
    pub read_from: Vec<(&'static str, String)>,
    /// Each file of the worktree's own that shadows a copy in the declarations folder, as the lines name it in `.gate`,
    /// with the copy's full path.
    pub shadowed: Vec<(&'static str, String)>,
    /// Why the repo's declarations folder cannot be used, judged whether or not a read reached it.
    pub declarations_problem: Option<String>,
    /// Whether the repo has a `.gate/toolchain.toml`, readable or not.
    pub declares_toolchain: bool,
    /// The files that are there but cannot be read whole, so their items could not be checked, in the order the
    /// summary names them: the manifest, its siblings, the root `global.json`.
    pub unreadable: Vec<&'static str>,
    /// The problems of the files the check reads, in the order found.
    pub problems: Vec<Problem>,
    /// The manifest's tools, in manifest order.
    pub tools: Vec<Tool>,
    /// The exact Godot versions the repo requires, as [`toolchain::godot_required`](crate::remote::toolchain::godot_required)
    /// reads them: empty for any version,
    /// `None` when the manifest names no godot. The `godot` tool is judged by them, as a routed run is.
    pub godot: Option<Vec<Pin>>,
    /// The exact Unreal Engine versions the manifest requires, as
    /// [`toolchain::unreal_required`](crate::remote::toolchain::unreal_required) reads them: empty for any version,
    /// `None` when the manifest names no unreal. The `unreal` tool is judged by them, as a routed run is.
    pub unreal: Option<Vec<Pin>>,
    /// The SDK pin's check; `None` when the root has no `global.json`.
    pub sdk: Option<Sdk>,
    /// The manifest's workload ids, which the check lists without checking them.
    pub workloads: Vec<String>,
    /// Why this PC's routing file's `siblings` value cannot be used, when the repo declares siblings and it cannot.
    pub consent_problem: Option<String>,
    /// Why this PC's routing file's `hooks` value cannot be used, when it cannot.
    pub hooks_problem: Option<String>,
    /// The declared siblings, in declared order.
    pub siblings: Vec<Sibling>,
    /// The folders a builder's preflight said it created, in its order; none for this PC.
    pub created: Vec<String>,
    /// Whether the repo declares siblings a builder is not checked for, as a routed run lays them out; never for this
    /// PC, which checks each in `siblings`.
    pub siblings_skipped: bool,
    /// The warm-up's items, in order; none when the repo has neither a warm-up file nor `remote.json`'s consent.
    pub warmup: Vec<Warmup>,
}

impl Checked {
    /// The exact versions the repo requires of the tool `name`, for the tools whose manifest entry is an exact pin
    /// (`godot` and `unreal`); `None` for any other tool, and for one of them the manifest does not name.
    fn exact_pins(&self, name: &str) -> Option<&[Pin]> {
        match name {
            "godot" => self.godot.as_deref(),
            "unreal" => self.unreal.as_deref(),
            _ => None,
        }
    }

    /// How the problem lines and the summary name the file `file`: its full path when it was taken from the
    /// declarations folder, escaped, else as it is.
    fn named(&self, file: &str) -> String {
        self.read_from
            .iter()
            .find(|(said, _)| *said == file)
            .map_or_else(|| file.to_owned(), |(_, path)| escaped(path))
    }
}

/// One line the check says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Line {
    Out(String),
    Err(String),
}

/// The lines the check says, in order, and its exit status.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Said {
    pub lines: Vec<Line>,
    pub status: u32,
}

/// How an item counts toward the summary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Ok,
    Missing,
    TooOld,
    Skipped,
    /// Said, but neither checked nor skipped, never counted: the workloads, and the warm-up's lines that only inform.
    Unchecked,
}

/// One item's line, how it counts, and its install hint: the subject and the command.
struct Item {
    text: String,
    kind: Kind,
    hint: Option<(String, String)>,
}

impl Item {
    fn new(text: String, kind: Kind) -> Item {
        Item { text, kind, hint: None }
    }

    fn hinted(text: String, kind: Kind, hint: Option<(String, String)>) -> Item {
        Item { text, kind, hint }
    }
}

/// What the check says of `checked`, found `at` this PC or a builder.
pub(crate) fn said(place: At<'_>, checked: &Checked) -> Said {
    let at = place.name();
    let header = match place {
        At::Here => format!("{} at {}", checked.repo, checked.root),
        At::Builder { key, .. } => format!("{} (work folder {key})", checked.repo),
    };
    let mut lines = vec![Line::Out(format!("check: {at}: {header}"))];
    lines.extend(
        checked
            .created
            .iter()
            .take(BUILDER_SHOWN)
            .map(|folder| Line::Out(format!("check: {at}: created {}", escaped(folder)))),
    );
    let more = checked.created.len().saturating_sub(BUILDER_SHOWN);
    if more > 0 {
        lines.push(Line::Out(format!("check: {at}: and {more} more created folders")));
    }
    lines.extend(
        checked
            .read_from
            .iter()
            .map(|(file, path)| Line::Out(format!("check: {at}: {file}: read from {}", escaped(path)))),
    );
    lines.extend(
        checked
            .shadowed
            .iter()
            .map(|(file, path)| Line::Out(format!("check: {at}: {file}: the worktree's copy wins over {}", escaped(path)))),
    );
    if !checked.declares_toolchain {
        lines.push(Line::Out(format!("check: {at}: {} declares no toolchain", checked.repo)));
    }
    lines.extend(
        checked
            .problems
            .iter()
            .map(|problem| Line::Err(format!("check: {}: {}", checked.named(problem.file), escaped(&problem.text)))),
    );
    let items = items(place, checked);
    lines.extend(items.iter().map(|item| Line::Out(format!("check: {at}: {}", item.text))));
    lines.extend(
        items
            .iter()
            .filter_map(|item| item.hint.as_ref())
            .map(|(subject, command)| Line::Out(format!("install: {at}: {subject}: {command}"))),
    );
    let count = |kind: Kind| items.iter().filter(|item| item.kind == kind).count();
    let (missing, too_old, skipped) = (count(Kind::Missing), count(Kind::TooOld), count(Kind::Skipped));
    let summary = if checked.unreadable.is_empty() {
        summary(missing, too_old, skipped)
    } else {
        let named: Vec<String> = checked.unreadable.iter().map(|file| checked.named(file)).collect();
        format!("could not read {}; {}", named.join(", "), counts(missing, too_old, skipped))
    };
    lines.push(Line::Out(format!("check: {at}: {summary}")));
    let failed = !checked.unreadable.is_empty() || missing + too_old > 0;
    let status = if failed { u32::from(FAILED_STATUS) } else { 0 };
    Said { lines, status }
}

/// Every item of `checked`, found `at` this PC or a builder, in the order the lines say them.
fn items(at: At<'_>, checked: &Checked) -> Vec<Item> {
    let mut items: Vec<Item> = checked
        .tools
        .iter()
        .map(|tool| tool_item(at, checked.exact_pins(&tool.need.name), tool))
        .collect();
    items.extend(checked.sdk.as_ref().map(|sdk| sdk_item(at, sdk)));
    if !checked.workloads.is_empty() {
        let ids: Vec<String> = checked.workloads.iter().map(|id| one_line(id)).collect();
        let text = format!("workloads {} not checked yet", ids.join(", "));
        items.push(Item::new(text, Kind::Unchecked));
    }
    if let Some(problem) = &checked.consent_problem {
        items.push(Item::new(format!("remote.json: {}", escaped(problem)), Kind::Missing));
    }
    if let Some(problem) = &checked.hooks_problem {
        items.push(Item::new(format!("remote.json: {}", escaped(problem)), Kind::Missing));
    }
    if let Some(why) = &checked.declarations_problem {
        items.push(Item::new(
            format!("declarations for {}: {}", escaped(&checked.repo), escaped(why)),
            Kind::Missing,
        ));
    }
    items.extend(checked.siblings.iter().map(sibling_item));
    if checked.siblings_skipped {
        items.push(Item::new("siblings skipped: a routed run lays them out".to_owned(), Kind::Skipped));
    }
    items.extend(checked.warmup.iter().map(|warmup| warmup_item(checked, warmup)));
    items
}

/// The line of a warm-up item of the repo `checked` checks, its warm-up file named as its problem lines name it
/// ([`Checked::named`]): what fails the check is what keeps the repo's matched runs here or leaves its consent unread,
/// and the record's other states are said without counting.
fn warmup_item(checked: &Checked, warmup: &Warmup) -> Item {
    let (text, kind) = match warmup {
        Warmup::Unconsented => (
            format!(
                "{} is there, but remote.json does not consent to warm-ups for {}",
                checked.named(WARMUP),
                escaped(&checked.repo)
            ),
            Kind::Unchecked,
        ),
        Warmup::NoFile => (format!("remote.json consents, but the repo has no {WARMUP}"), Kind::Unchecked),
        Warmup::ConsentProblem(why) => (format!("remote.json: {}", escaped(why)), Kind::Missing),
        Warmup::FileProblems => (
            format!("{} has problems, so gate warm cannot prewarm a new worktree", checked.named(WARMUP)),
            Kind::Missing,
        ),
        Warmup::NoStateRoot(why) => (format!("the state root cannot be found: {}", escaped(why)), Kind::Skipped),
        Warmup::Warm { host } => (format!("warm on {}", shown_value(host)), Kind::Ok),
        Warmup::Warming { host } => (warming_text(host), Kind::Unchecked),
        Warmup::Failed { host, minutes_ago, reason } => (
            format!(
                "the warm-up on {} failed {minutes_ago} min ago: {}; run gate warm to try again",
                shown_value(host),
                shown_value(reason)
            ),
            Kind::Unchecked,
        ),
        Warmup::NotWarm { host, stale: false } => (format!("not warm on {}; gate warm warms it", shown_value(host)), Kind::Unchecked),
        Warmup::NotWarm { host, stale: true } => (
            format!(
                "not warm on {}; the last warm-up ended without a verdict; gate warm warms it",
                shown_value(host)
            ),
            Kind::Unchecked,
        ),
        Warmup::UnreadableRecord(text) => (
            format!("{text}; gate warm overwrites it, and only a layout fallback's delete reads it"),
            Kind::Unchecked,
        ),
        Warmup::UnreadableLock(text) => (
            format!("{text}; gate warm and a routed run waiting on the work folder read it"),
            Kind::Unchecked,
        ),
        Warmup::Builder { warm: true } => ("the work folder is warm".to_owned(), Kind::Ok),
        Warmup::Builder { warm: false } => ("the work folder is not warm".to_owned(), Kind::Unchecked),
    };
    Item::new(format!("warm-up: {text}"), kind)
}

/// A name or id the manifest gives, as its reader spells a key ([`spelled`]), with any mark that reorders text or
/// separates lines escaped too ([`escaped`]): never more than one line, so it cannot forge another.
fn one_line(text: &str) -> String {
    escaped(&spelled(text))
}

/// The summary: `nothing missing`, or the counts of missing and too-old items; the skipped ones counted only when
/// there are any.
fn summary(missing: usize, too_old: usize, skipped: usize) -> String {
    match (missing + too_old, skipped) {
        (0, 0) => "nothing missing".to_owned(),
        (0, skipped) => format!("nothing missing ({skipped} skipped)"),
        _ => counts(missing, too_old, skipped),
    }
}

/// The counts of missing and too-old items, and of the skipped ones when there are any.
fn counts(missing: usize, too_old: usize, skipped: usize) -> String {
    if skipped == 0 {
        format!("{missing} missing, {too_old} too old")
    } else {
        format!("{missing} missing, {too_old} too old, {skipped} skipped")
    }
}

/// The line of a manifest tool as its probe found it `at` this PC or a builder, against its minimum, or against the
/// exact versions `exact` the repo requires of it when its manifest entry is an exact pin.
fn tool_item(at: At<'_>, exact: Option<&[Pin]>, tool: &Tool) -> Item {
    let name = one_line(&tool.need.name);
    if !tool.need.known {
        return Item::new(format!("tools.{name} skipped: not a tool gate knows"), Kind::Skipped);
    }
    if let Some(why) = &tool.not_checked {
        return Item::new(format!("{name} not checked here: {}", escaped(why)), Kind::Unchecked);
    }
    let min = tool.need.min;
    let needs = min.map_or_else(|| "needs it present".to_owned(), |min| format!("needs {min} or later"));
    let hint = |kind| {
        let command = match at {
            At::Here => install_hint(&name, min, kind),
            At::Builder { host, .. } => builder_hint(host, &name, min),
        };
        command.map(|command| (name.clone(), command))
    };
    if let Some(pins) = exact.filter(|pins| !pins.is_empty()) {
        return pinned_item(&name, pins, &tool.reported, &hint);
    }
    let reported = &tool.reported;
    match (standing(min, reported), reported) {
        (Standing::Met, Reported::Version(found)) if min.is_some() => Item::new(format!("{name} {} ok ({needs})", escaped(found)), Kind::Ok),
        (Standing::Met, Reported::Version(found)) => Item::new(format!("{name} {} ok", escaped(found)), Kind::Ok),
        (Standing::Met, _) => Item::new(format!("{name} ok (version not readable)"), Kind::Ok),
        (Standing::TooOld(found), _) => Item::hinted(format!("{name} {} too old ({needs})", escaped(&found)), Kind::TooOld, hint(Kind::TooOld)),
        (Standing::NotReadable, _) => Item::new(format!("{name} skipped: found, version not readable ({needs})"), Kind::Skipped),
        (Standing::Missing, _) => Item::hinted(format!("{name} missing ({needs})"), Kind::Missing, hint(Kind::Missing)),
        (Standing::Skipped(why), _) => Item::new(format!("{name} skipped: {}", escaped(&why)), Kind::Skipped),
    }
}

/// The line of the manifest tool `name`, `godot` or `unreal`, against the exact versions `pins` the repo requires, as a
/// routed run judges the same answer: met, or missing saying what the probe found of the pin, with the same hint a
/// missing tool has.
fn pinned_item(name: &str, pins: &[Pin], reported: &Reported, hint: &impl Fn(Kind) -> Option<(String, String)>) -> Item {
    let needs = format!("needs {} exactly", pins_text(pins));
    match pinned(pins, reported) {
        Pinned::Met => match reported {
            Reported::Version(found) => Item::new(format!("{name} {} ok ({needs})", escaped(found)), Kind::Ok),
            _ => Item::new(format!("{name} ok (version not readable)"), Kind::Ok),
        },
        Pinned::Lacks(what) => Item::hinted(
            format!("{name} missing ({needs}: {})", escaped(&what)),
            Kind::Missing,
            hint(Kind::Missing),
        ),
        Pinned::Skipped(why) => Item::new(format!("{name} skipped: {}", escaped(&why)), Kind::Skipped),
    }
}

/// The pins `pins` as the lines say them, their texts joined by ` and `.
fn pins_text(pins: &[Pin]) -> String {
    pins.iter().map(ToString::to_string).collect::<Vec<String>>().join(" and ")
}

/// The command that installs the tool `name` on this PC, `kind` saying whether it is missing or too old, its minimum
/// `min` where the command takes one; `None` for a tool with no hint. A rust with a minimum is made the default
/// toolchain with `rustup default`, which installs it when it is not there: after rustup itself when rust is missing,
/// alone when rust is there but too old.
fn install_hint(name: &str, min: Option<Version>, kind: Kind) -> Option<String> {
    let command = match name {
        "rust" => {
            return Some(match (min, kind) {
                (None, _) => "winget install Rustlang.Rustup".to_owned(),
                (Some(min), Kind::TooOld) => format!("rustup default {min}"),
                (Some(min), _) => format!("winget install Rustlang.Rustup, then rustup default {min}"),
            });
        }
        "cargo-nextest" => "cargo install cargo-nextest --locked",
        "node" => "winget install OpenJS.NodeJS",
        "pnpm" => "corepack enable pnpm",
        "pwsh" => "winget install Microsoft.PowerShell",
        "ffmpeg" => "winget install Gyan.FFmpeg",
        _ => return None,
    };
    Some(command.to_owned())
}

/// The command that installs the tool `name` on the builder `host` over ssh, missing or too old alike, its minimum
/// `min` where the command takes one; the builder's toolchain script for a tool no such command installs, and for a
/// rust with no minimum to name; `None` for a tool with no hint.
fn builder_hint(host: &str, name: &str, min: Option<Version>) -> Option<String> {
    let script = |step: &str| {
        format!(
            "copy tools/builder/ from the gate repo to {host} and run install-toolchains.ps1 -User <user> -Only {step} there, \
             elevated (docs/routing-a-repo.md, Builder setup)"
        )
    };
    Some(match (name, min) {
        ("cargo-nextest", _) => "cargo install cargo-nextest --locked".to_owned(),
        ("pnpm", Some(min)) => format!("npm install -g pnpm@{min}"),
        ("pnpm", None) => "npm install -g pnpm".to_owned(),
        ("rust", Some(min)) => format!("rustup toolchain install {min} && rustup default {min}"),
        ("rust", None) => script("Rust"),
        ("node", _) => script("Node.js"),
        ("pwsh", _) => script("PowerShell"),
        ("ffmpeg", _) => script("FFmpeg"),
        _ => return None,
    })
}

/// The line of the SDK pin's check `at` this PC or a builder.
fn sdk_item(at: At<'_>, sdk: &Sdk) -> Item {
    match sdk {
        Sdk::Checked { need, served: true, .. } => Item::new(format!("{need} ok"), Kind::Ok),
        Sdk::Checked { version, need, have, .. } => {
            let (whose, command, have) = match at {
                At::Here => {
                    let major = version.split('.').next().unwrap_or(version);
                    let have = if have.is_empty() { "none".to_owned() } else { have.join(", ") };
                    ("this PC".to_owned(), format!("winget install Microsoft.DotNet.SDK.{major}"), have)
                }
                At::Builder { host, .. } => (host.to_owned(), format!("dotnet-install.ps1 -Version {version}"), builder_sdks(have)),
            };
            let hint = (".NET SDK".to_owned(), command);
            Item::hinted(format!("{need} missing ({whose} has {have})"), Kind::Missing, Some(hint))
        }
        Sdk::Skipped(why) => Item::new(format!(".NET SDK skipped: {}", escaped(why)), Kind::Skipped),
    }
}

/// A builder's SDK list `have` as its line says it: `none`, or its first [`BUILDER_SHOWN`] entries and how many more.
fn builder_sdks(have: &[String]) -> String {
    if have.is_empty() {
        return "none".to_owned();
    }
    let shown = have.get(..BUILDER_SHOWN).unwrap_or(have).join(", ");
    match have.len().saturating_sub(BUILDER_SHOWN) {
        0 => shown,
        more => format!("{shown}, and {more} more"),
    }
}

/// The line of a declared sibling; a refused one worded as routing words it, `sibling {name} {reason}`.
fn sibling_item(sibling: &Sibling) -> Item {
    let Sibling { name, path, found } = sibling;
    let folder = match found {
        Found::NotAllowed => return Item::new(format!("sibling {name} not allowed in remote.json"), Kind::Missing),
        Found::Folder(folder) => folder,
    };
    match folder {
        Ok(SiblingFolder::Present(_)) => Item::new(format!("sibling {name} ok ({path})"), Kind::Ok),
        Ok(SiblingFolder::Refused(reason)) => Item::new(format!("sibling {name} {reason}"), Kind::Missing),
        Ok(SiblingFolder::Missing) => Item::hinted(
            format!("sibling {name} missing (no git worktree at {path})"),
            Kind::Missing,
            Some((format!("sibling {name}"), format!("clone it to {path}"))),
        ),
        Err(why) => Item::new(format!("sibling {name} skipped: {}", escaped(why)), Kind::Skipped),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::{At, Checked, Found, GLOBAL_JSON, HERE, Line, Problem, SIBLINGS, Said, Sdk, Sibling, TOOLCHAIN, Tool, WARMUP, Warmup, said};
    use gate_desktop::version::build_version;
    use gate_router::warm::{FailureClass, FileRead, Record, RecordRead, RecordState, WarmupFile, lock_problem_text};
    use gate_router::{FolderId, SiblingFolder};

    use crate::boxside::probe::Probed;
    use crate::check::local::record_item;
    use crate::remote::toolchain::{Pin, Reported, ToolNeed, Version};

    fn need(name: &str, min: Option<&str>) -> ToolNeed {
        ToolNeed {
            name: name.to_owned(),
            min: min.and_then(Version::parse),
            known: !name.starts_with("nodejs"),
        }
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "each case builds its probe in place, as the check's probe hands it over"
    )]
    fn tool(name: &str, min: Option<&str>, probed: Probed) -> Tool {
        Tool {
            need: need(name, min),
            reported: Reported::from(&probed),
            not_checked: None,
        }
    }

    fn found(version: &str) -> Probed {
        Probed::Version(version.to_owned())
    }

    fn checked(tools: Vec<Tool>) -> Checked {
        Checked {
            repo: "app".to_owned(),
            root: "X:/dev/app".to_owned(),
            read_from: Vec::new(),
            shadowed: Vec::new(),
            declarations_problem: None,
            declares_toolchain: true,
            unreadable: Vec::new(),
            problems: Vec::new(),
            tools,
            godot: None,
            unreal: None,
            sdk: None,
            workloads: Vec::new(),
            consent_problem: None,
            hooks_problem: None,
            siblings: Vec::new(),
            created: Vec::new(),
            siblings_skipped: false,
            warmup: Vec::new(),
        }
    }

    fn out(text: &str) -> Line {
        Line::Out(text.to_owned())
    }

    /// The stdout lines between the header and the summary, the hints among them.
    fn body(checked: &Checked) -> Vec<String> {
        let said = said(HERE, checked);
        let outs: Vec<String> = said
            .lines
            .iter()
            .filter_map(|line| match line {
                Line::Out(text) => Some(text.clone()),
                Line::Err(_) => None,
            })
            .collect();
        outs[1..outs.len() - 1].to_vec()
    }

    fn summary(checked: &Checked) -> String {
        let said = said(HERE, checked);
        let last = said.lines.last();
        assert!(matches!(last, Some(Line::Out(_))), "the last line is a stdout summary, got {last:?}");
        match last {
            Some(Line::Out(text)) => text.clone(),
            _ => String::new(),
        }
    }

    #[test]
    fn a_tool_at_its_minimum_is_ok() {
        let checked = checked(vec![
            tool("rust", Some("1.89"), found("1.89.0")),
            tool("node", Some("24"), found("24.19.0")),
            tool("pwsh", None, found("7.6.6")),
            tool("ffmpeg", None, Probed::Present),
        ]);
        assert_eq!(
            said(HERE, &checked),
            Said {
                lines: vec![
                    out("check: here: app at X:/dev/app"),
                    out("check: here: rust 1.89.0 ok (needs 1.89.0 or later)"),
                    out("check: here: node 24.19.0 ok (needs 24.0.0 or later)"),
                    out("check: here: pwsh 7.6.6 ok"),
                    out("check: here: ffmpeg ok (version not readable)"),
                    out("check: here: nothing missing"),
                ],
                status: 0,
            }
        );
    }

    #[test]
    fn a_tool_below_its_minimum_is_too_old() {
        let checked = checked(vec![
            tool("node", Some("24"), found("22.1.0")),
            tool("rust", Some("1.89"), found("1.88.9")),
        ]);
        assert_eq!(
            body(&checked),
            [
                "check: here: node 22.1.0 too old (needs 24.0.0 or later)",
                "check: here: rust 1.88.9 too old (needs 1.89.0 or later)",
                "install: here: node: winget install OpenJS.NodeJS",
                "install: here: rust: rustup default 1.89.0",
            ]
        );
        assert_eq!(summary(&checked), "check: here: 0 missing, 2 too old");
    }

    #[test]
    fn a_missing_rust_with_a_minimum_is_installed_then_made_the_default() {
        let checked = checked(vec![tool("rust", Some("1.89"), Probed::Missing)]);
        assert_eq!(
            body(&checked),
            [
                "check: here: rust missing (needs 1.89.0 or later)",
                "install: here: rust: winget install Rustlang.Rustup, then rustup default 1.89.0",
            ]
        );
    }

    #[test]
    fn a_manifest_text_holding_a_line_end_stays_one_line() {
        let checked = Checked {
            workloads: vec!["w\ninstall: here: node: y".to_owned()],
            ..checked(vec![tool("nodejs\ninstall: here: rust: x", None, Probed::Missing)])
        };
        let lines = said(HERE, &checked).lines;
        assert_eq!(
            body(&checked),
            [
                "check: here: tools.\"nodejs\\ninstall: here: rust: x\" skipped: not a tool gate knows",
                "check: here: workloads \"w\\ninstall: here: node: y\" not checked yet",
            ]
        );
        let texts: Vec<&str> = lines
            .iter()
            .map(|line| match line {
                Line::Out(text) | Line::Err(text) => text.as_str(),
            })
            .collect();
        assert!(texts.iter().all(|text| !text.contains('\n')), "{texts:?}");
        assert!(
            !texts.iter().any(|text| text.starts_with("install:")),
            "no install line is forged: {texts:?}"
        );
    }

    #[test]
    fn an_unreadable_manifest_fails_the_check_and_says_so() {
        let unreadable = Checked {
            unreadable: vec![TOOLCHAIN],
            problems: vec![Problem {
                file: ".gate/toolchain.toml",
                text: "line 1, column 7: invalid table header".to_owned(),
            }],
            ..checked(Vec::new())
        };
        assert_eq!(
            summary(&unreadable),
            "check: here: could not read .gate/toolchain.toml; 0 missing, 0 too old"
        );
        assert_eq!(said(HERE, &unreadable).status, 1);
        let with_others = Checked {
            unreadable: vec![TOOLCHAIN],
            sdk: Some(Sdk::Skipped("global.json has no readable sdk pin".to_owned())),
            siblings: vec![Sibling {
                name: "b".to_owned(),
                path: "X:/dev/b".to_owned(),
                found: Found::Folder(Ok(SiblingFolder::Missing)),
            }],
            ..checked(Vec::new())
        };
        assert_eq!(
            summary(&with_others),
            "check: here: could not read .gate/toolchain.toml; 1 missing, 0 too old, 1 skipped",
            "the rest is still checked"
        );
    }

    #[test]
    fn a_missing_tool_says_its_install_hint() {
        let checked = checked(vec![
            tool("cargo-nextest", Some("0.9.100"), Probed::Missing),
            tool("rust", None, Probed::Missing),
            tool("pnpm", None, Probed::Missing),
            tool("pwsh", None, Probed::Missing),
            tool("ffmpeg", None, Probed::Missing),
        ]);
        assert_eq!(
            body(&checked),
            [
                "check: here: cargo-nextest missing (needs 0.9.100 or later)",
                "check: here: rust missing (needs it present)",
                "check: here: pnpm missing (needs it present)",
                "check: here: pwsh missing (needs it present)",
                "check: here: ffmpeg missing (needs it present)",
                "install: here: cargo-nextest: cargo install cargo-nextest --locked",
                "install: here: rust: winget install Rustlang.Rustup",
                "install: here: pnpm: corepack enable pnpm",
                "install: here: pwsh: winget install Microsoft.PowerShell",
                "install: here: ffmpeg: winget install Gyan.FFmpeg",
            ]
        );
    }

    #[test]
    fn a_present_only_need_says_it_needs_to_be_present() {
        let checked = checked(vec![tool("node", None, Probed::Missing)]);
        assert_eq!(
            body(&checked),
            [
                "check: here: node missing (needs it present)",
                "install: here: node: winget install OpenJS.NodeJS",
            ]
        );
        assert_eq!(said(HERE, &checked).status, 1);
    }

    #[test]
    fn a_found_tool_with_no_version_against_a_minimum_is_skipped() {
        let checked = checked(vec![tool("ffmpeg", Some("7"), Probed::Present)]);
        assert_eq!(
            body(&checked),
            ["check: here: ffmpeg skipped: found, version not readable (needs 7.0.0 or later)"]
        );
        assert_eq!(summary(&checked), "check: here: nothing missing (1 skipped)");
        assert_eq!(said(HERE, &checked).status, 0);
    }

    #[test]
    fn skipped_probes_do_not_count_as_missing() {
        let checked = checked(vec![
            tool("node", Some("24"), Probed::Skipped("no answer in 10 s".to_owned())),
            tool("godot", None, Probed::Skipped("no answer in 10 s".to_owned())),
        ]);
        assert_eq!(
            body(&checked),
            [
                "check: here: node skipped: no answer in 10 s",
                "check: here: godot skipped: no answer in 10 s",
            ]
        );
        assert_eq!(summary(&checked), "check: here: nothing missing (2 skipped)");
        assert_eq!(said(HERE, &checked).status, 0);
    }

    /// A pinned godot the probe found at another version is missing, as a routed run judges the pin exact on the parts
    /// it gives, though the check's generic rule would read the found version as past a minimum.
    #[test]
    fn a_pinned_godot_at_another_version_is_missing() {
        let Some(pin) = Pin::parse("4.7.2") else {
            unreachable!("4.7.2 is a pin");
        };
        let checked = Checked {
            godot: Some(vec![pin]),
            ..checked(vec![tool("godot", Some("4.7.2"), found("4.7.3"))])
        };
        assert_eq!(body(&checked), ["check: here: godot missing (needs 4.7.2 exactly: 4.7.3)"]);
        assert_eq!(summary(&checked), "check: here: 1 missing, 0 too old");
        assert_eq!(said(HERE, &checked).status, 1);
    }

    /// A pinned godot the probe found at the pin's version is ok, whatever else its version list holds, and a partial
    /// pin is met by any install on the parts it gives.
    #[test]
    fn a_pinned_godot_met_is_ok() {
        let Some(pin) = Pin::parse("4.7.2") else {
            unreachable!("4.7.2 is a pin");
        };
        let met = Checked {
            godot: Some(vec![pin]),
            ..checked(vec![tool("godot", Some("4.7.2"), found("4.3.0,4.7.2"))])
        };
        assert_eq!(body(&met), ["check: here: godot 4.3.0,4.7.2 ok (needs 4.7.2 exactly)"]);
        assert_eq!(summary(&met), "check: here: nothing missing");
        assert_eq!(said(HERE, &met).status, 0);

        let Some(pin) = Pin::parse("4.5") else {
            unreachable!("4.5 is a pin");
        };
        let partial = Checked {
            godot: Some(vec![pin]),
            ..checked(vec![tool("godot", Some("4.5"), found("4.5.3"))])
        };
        assert_eq!(body(&partial), ["check: here: godot 4.5.3 ok (needs 4.5 exactly)"]);
        assert_eq!(summary(&partial), "check: here: nothing missing");
        assert_eq!(said(HERE, &partial).status, 0);
    }

    /// A pinned unreal the probe found at another version is missing, as a routed run judges the pin exact on the parts
    /// it gives, though the found version is past the pin.
    #[test]
    fn a_pinned_unreal_at_another_version_is_missing() {
        let Some(pin) = Pin::parse("5.8") else {
            unreachable!("5.8 is a pin");
        };
        let checked = Checked {
            unreal: Some(vec![pin]),
            ..checked(vec![tool("unreal", Some("5.8"), found("5.9.0"))])
        };
        assert_eq!(body(&checked), ["check: here: unreal missing (needs 5.8 exactly: 5.9.0)"]);
        assert_eq!(summary(&checked), "check: here: 1 missing, 0 too old");
        assert_eq!(said(HERE, &checked).status, 1);
    }

    /// A pinned unreal the probe found at a version the pin names is ok, whatever else its version list holds.
    #[test]
    fn a_pinned_unreal_met_is_ok() {
        let Some(pin) = Pin::parse("5.8") else {
            unreachable!("5.8 is a pin");
        };
        let met = Checked {
            unreal: Some(vec![pin]),
            ..checked(vec![tool("unreal", Some("5.8"), found("5.7.3,5.8.1"))])
        };
        assert_eq!(body(&met), ["check: here: unreal 5.7.3,5.8.1 ok (needs 5.8 exactly)"]);
        assert_eq!(summary(&met), "check: here: nothing missing");
        assert_eq!(said(HERE, &met).status, 0);
    }

    #[test]
    fn the_summary_counts_missing_old_and_skipped() {
        let mixed = checked(vec![
            tool("node", Some("24"), found("22.1.0")),
            tool("pnpm", None, Probed::Missing),
            tool("cargo-nextest", None, Probed::Missing),
            tool("godot", None, Probed::Skipped("no answer in 10 s".to_owned())),
        ]);
        assert_eq!(summary(&mixed), "check: here: 2 missing, 1 too old, 1 skipped");
        let workloads = Checked {
            workloads: vec!["wasm-tools".to_owned()],
            ..checked(Vec::new())
        };
        assert_eq!(
            summary(&workloads),
            "check: here: nothing missing",
            "the workloads line is neither missing nor skipped"
        );
    }

    #[test]
    fn the_exit_status_is_0_only_when_nothing_is_missing() {
        assert_eq!(said(HERE, &checked(Vec::new())).status, 0);
        assert_eq!(said(HERE, &checked(vec![tool("pnpm", None, Probed::Missing)])).status, 1);
        assert_eq!(said(HERE, &checked(vec![tool("node", Some("24"), found("23.9.9"))])).status, 1);
        let sibling_missing = Checked {
            siblings: vec![Sibling {
                name: "b".to_owned(),
                path: "X:/dev/b".to_owned(),
                found: Found::Folder(Ok(SiblingFolder::Missing)),
            }],
            ..checked(Vec::new())
        };
        assert_eq!(said(HERE, &sibling_missing).status, 1);
        let sdk_missing = Checked {
            sdk: Some(Sdk::Checked {
                version: "10.0.401".to_owned(),
                need: ".NET SDK 10.0.401 (rollForward patch)".to_owned(),
                have: Vec::new(),
                served: false,
            }),
            ..checked(Vec::new())
        };
        assert_eq!(said(HERE, &sdk_missing).status, 1);
    }

    #[test]
    fn items_come_in_manifest_order_then_sdk_then_siblings() {
        let checked = Checked {
            sdk: Some(Sdk::Checked {
                version: "10.0.401".to_owned(),
                need: ".NET SDK 10.0.401 (rollForward patch)".to_owned(),
                have: vec!["10.0.300".to_owned(), "8.0.404".to_owned()],
                served: false,
            }),
            workloads: vec!["wasm-tools".to_owned(), "maui".to_owned()],
            siblings: vec![
                Sibling {
                    name: "yaat-server".to_owned(),
                    path: "X:/dev/yaat-server".to_owned(),
                    found: Found::Folder(Ok(SiblingFolder::Present(FolderId { volume: 1, file: [2; 16] }))),
                },
                Sibling {
                    name: "b".to_owned(),
                    path: "X:/dev/b".to_owned(),
                    found: Found::Folder(Ok(SiblingFolder::Missing)),
                },
                Sibling {
                    name: "c".to_owned(),
                    path: "X:/dev/c".to_owned(),
                    found: Found::Folder(Err("cannot read the sibling folder X:/dev/c: access denied".to_owned())),
                },
                Sibling {
                    name: "d".to_owned(),
                    path: "X:/dev/d".to_owned(),
                    found: Found::Folder(Ok(SiblingFolder::Refused("is a link or a junction".to_owned()))),
                },
                Sibling {
                    name: "e".to_owned(),
                    path: "X:/dev/e".to_owned(),
                    found: Found::NotAllowed,
                },
            ],
            ..checked(vec![tool("pwsh", None, found("7.6.6")), tool("node", None, Probed::Missing)])
        };
        assert_eq!(
            body(&checked),
            [
                "check: here: pwsh 7.6.6 ok",
                "check: here: node missing (needs it present)",
                "check: here: .NET SDK 10.0.401 (rollForward patch) missing (this PC has 10.0.300, 8.0.404)",
                "check: here: workloads wasm-tools, maui not checked yet",
                "check: here: sibling yaat-server ok (X:/dev/yaat-server)",
                "check: here: sibling b missing (no git worktree at X:/dev/b)",
                "check: here: sibling c skipped: cannot read the sibling folder X:/dev/c: access denied",
                "check: here: sibling d is a link or a junction",
                "check: here: sibling e not allowed in remote.json",
                "install: here: node: winget install OpenJS.NodeJS",
                "install: here: .NET SDK: winget install Microsoft.DotNet.SDK.10",
                "install: here: sibling b: clone it to X:/dev/b",
            ]
        );
        assert_eq!(summary(&checked), "check: here: 5 missing, 0 too old, 1 skipped");
    }

    #[test]
    fn a_siblings_value_that_cannot_be_used_is_an_escaped_missing_line_before_the_siblings() {
        let checked = Checked {
            consent_problem: Some("siblings.\"app\"[1] is not a non-empty string\u{1b}[2J\u{202e}".to_owned()),
            siblings: vec![Sibling {
                name: "b".to_owned(),
                path: "X:/dev/b".to_owned(),
                found: Found::NotAllowed,
            }],
            ..checked(Vec::new())
        };
        assert_eq!(
            body(&checked),
            [
                r#"check: here: remote.json: siblings."app"[1] is not a non-empty string\u{1b}[2J\u{202e}"#,
                "check: here: sibling b not allowed in remote.json",
            ]
        );
        assert_eq!(summary(&checked), "check: here: 2 missing, 0 too old");
        assert_eq!(said(HERE, &checked).status, 1);
    }

    const HOST: &str = "builder-1";
    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const OTHER_HASH: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    /// When the records of the warm-up cases were written.
    fn written() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    /// A record of `state` on `host` for the file of `hash`, written by this gate build at [`written`].
    fn record(host: &str, state: RecordState, hash: &str) -> RecordRead {
        RecordRead::Parsed(Record {
            host: host.to_owned(),
            state,
            at: written(),
            hash: hash.to_owned(),
            gate: build_version(),
        })
    }

    fn failed(class: FailureClass, reason: &str) -> RecordState {
        RecordState::Failed {
            class,
            reason: reason.to_owned(),
        }
    }

    /// The item of `record` for the file of [`HASH`] on [`HOST`], `elapsed` after it was written, its lock held or not.
    fn warm_item(record: &RecordRead, lock_held: bool, elapsed: Duration) -> Warmup {
        item_at(record, lock_held, written() + elapsed)
    }

    /// The item of `record` for the file of [`HASH`] on [`HOST`] at `now`, its lock held or not.
    fn item_at(record: &RecordRead, lock_held: bool, now: SystemTime) -> Warmup {
        let file = FileRead::Parsed {
            file: WarmupFile {
                commands: vec![vec!["dotnet".to_owned(), "build".to_owned()]],
                timeout_seconds: 1800,
            },
            hash: HASH.to_owned(),
        };
        record_item(record, &file, lock_held, HOST, now)
    }

    /// What the check says, and its status, of the warm-up items `warmup` and the file problems `problems` alone.
    fn warm_said(warmup: Vec<Warmup>, problems: &[&str]) -> Said {
        let checked = Checked {
            problems: problems
                .iter()
                .map(|text| Problem {
                    file: WARMUP,
                    text: (*text).to_owned(),
                })
                .collect(),
            warmup,
            ..checked(Vec::new())
        };
        said(HERE, &checked)
    }

    /// The lines of a check of one warm-up item alone: the header, the item's line `text`, the summary `summary`.
    fn one_item(text: &str, summary: &str) -> Vec<Line> {
        vec![out("check: here: app at X:/dev/app"), out(text), out(summary)]
    }

    #[test]
    fn a_warm_record_of_the_host_and_file_is_ok_and_comes_after_the_siblings() {
        let warm = warm_item(&record(HOST, RecordState::Warm, HASH), false, Duration::from_mins(90));
        assert_eq!(warm, Warmup::Warm { host: HOST.to_owned() });
        let checked = Checked {
            siblings: vec![Sibling {
                name: "b".to_owned(),
                path: "X:/dev/b".to_owned(),
                found: Found::Folder(Ok(SiblingFolder::Present(FolderId { volume: 1, file: [2; 16] }))),
            }],
            warmup: vec![warm],
            ..checked(Vec::new())
        };
        assert_eq!(
            body(&checked),
            ["check: here: sibling b ok (X:/dev/b)", "check: here: warm-up: warm on builder-1"]
        );
        assert_eq!(said(HERE, &checked).status, 0);
    }

    #[test]
    fn a_held_lock_says_the_host_is_warming_and_does_not_fail() {
        for (name, read) in [
            ("no record", RecordRead::Absent),
            ("a warming record", record(HOST, RecordState::Warming, HASH)),
            ("a warm record", record(HOST, RecordState::Warm, HASH)),
        ] {
            let item = warm_item(&read, true, Duration::from_secs(5));
            assert_eq!(
                warm_said(vec![item], &[]),
                Said {
                    lines: one_item(
                        "check: here: warm-up: builder-1 is warming the work folder",
                        "check: here: nothing missing"
                    ),
                    status: 0,
                },
                "{name}"
            );
        }
    }

    #[test]
    fn a_failed_record_inside_its_backoff_says_its_age_and_to_run_gate_warm() {
        let commands = record(HOST, failed(FailureClass::Commands, "command 1 stopped with status 3"), HASH);
        for (elapsed, line) in [
            (
                Duration::ZERO,
                "check: here: warm-up: the warm-up on builder-1 failed 1 min ago: command 1 stopped with status 3; run gate warm to try again",
            ),
            (
                Duration::from_secs(90),
                "check: here: warm-up: the warm-up on builder-1 failed 1 min ago: command 1 stopped with status 3; run gate warm to try again",
            ),
            (
                Duration::from_secs(20),
                "check: here: warm-up: the warm-up on builder-1 failed 1 min ago: command 1 stopped with status 3; run gate warm to try again",
            ),
            (
                Duration::from_mins(29),
                "check: here: warm-up: the warm-up on builder-1 failed 29 min ago: command 1 stopped with status 3; run gate warm to try again",
            ),
        ] {
            assert_eq!(
                warm_said(vec![warm_item(&commands, false, elapsed)], &[]),
                Said {
                    lines: one_item(line, "check: here: nothing missing"),
                    status: 0,
                },
                "{elapsed:?}"
            );
        }
        let refused = record(HOST, failed(FailureClass::Refused, "the builder is busy\u{1b}[2J"), HASH);
        assert_eq!(
            warm_said(vec![warm_item(&refused, false, Duration::from_secs(10))], &[]).lines[1],
            out(r"check: here: warm-up: the warm-up on builder-1 failed 1 min ago: the builder is busy\u{1b}[2J; run gate warm to try again"),
            "a refusal, its reason escaped"
        );
    }

    #[test]
    fn each_record_that_leaves_the_folder_cold_says_gate_warm_warms_it() {
        let commands = || failed(FailureClass::Commands, "command 1 stopped with status 3");
        let mut other_build = record(HOST, commands(), HASH);
        if let RecordRead::Parsed(found) = &mut other_build {
            found.gate = "0.0.0-other".to_owned();
        }
        let later = |elapsed: Duration| written() + elapsed;
        for (name, read, now) in [
            ("no record", RecordRead::Absent, written()),
            (
                "a failure past its backoff",
                record(HOST, commands(), HASH),
                later(Duration::from_mins(30)),
            ),
            ("a failure of another gate build", other_build, later(Duration::from_mins(1))),
            (
                "a failure dated in the future",
                record(HOST, commands(), HASH),
                written() - Duration::from_mins(1),
            ),
            ("a warm record of another host", record("builder-2", RecordState::Warm, HASH), written()),
            ("a warm record of another file", record(HOST, RecordState::Warm, OTHER_HASH), written()),
        ] {
            assert_eq!(
                warm_said(vec![item_at(&read, false, now)], &[]),
                Said {
                    lines: one_item(
                        "check: here: warm-up: not warm on builder-1; gate warm warms it",
                        "check: here: nothing missing"
                    ),
                    status: 0,
                },
                "{name}"
            );
        }
        let stale = warm_item(&record(HOST, RecordState::Warming, HASH), false, Duration::from_mins(5));
        assert_eq!(
            warm_said(vec![stale], &[]).lines[1],
            out("check: here: warm-up: not warm on builder-1; the last warm-up ended without a verdict; gate warm warms it")
        );
    }

    #[test]
    fn an_unreadable_record_is_said_without_failing_the_check() {
        let unreadable = RecordRead::Unreadable {
            path: r"C:\state\remote\warm\app-1.json".to_owned(),
            error: "is larger than 64 KiB".to_owned(),
        };
        assert_eq!(
            warm_said(vec![warm_item(&unreadable, false, Duration::ZERO)], &[]),
            Said {
                lines: one_item(
                    r"check: here: warm-up: the warm-up record C:\state\remote\warm\app-1.json cannot be read: is larger than 64 KiB; gate warm overwrites it, and only a layout fallback's delete reads it",
                    "check: here: nothing missing"
                ),
                status: 0,
            }
        );
    }

    #[test]
    fn a_lock_that_cannot_be_probed_is_said_without_failing_the_check() {
        let item = Warmup::UnreadableLock(lock_problem_text(r"C:\state\remote\warm\app-1.lock", "Access is denied. (os error 5)"));
        assert_eq!(
            warm_said(vec![item], &[]),
            Said {
                lines: one_item(
                    r"check: here: warm-up: the warm-up lock C:\state\remote\warm\app-1.lock cannot be read: Access is denied. (os error 5); gate warm and a routed run waiting on the work folder read it",
                    "check: here: nothing missing"
                ),
                status: 0,
            }
        );
    }

    #[test]
    fn a_warm_up_file_without_consent_is_an_info_line_and_its_problems_still_print() {
        assert_eq!(
            warm_said(vec![Warmup::Unconsented], &["commands[0] is empty"]),
            Said {
                lines: vec![
                    out("check: here: app at X:/dev/app"),
                    Line::Err("check: .gate/warmup.toml: commands[0] is empty".to_owned()),
                    out("check: here: warm-up: .gate/warmup.toml is there, but remote.json does not consent to warm-ups for app"),
                    out("check: here: nothing missing"),
                ],
                status: 0,
            }
        );
    }

    #[test]
    fn consent_without_a_warm_up_file_is_an_info_line() {
        assert_eq!(
            warm_said(vec![Warmup::NoFile], &[]),
            Said {
                lines: one_item(
                    "check: here: warm-up: remote.json consents, but the repo has no .gate/warmup.toml",
                    "check: here: nothing missing"
                ),
                status: 0,
            }
        );
    }

    #[test]
    fn a_warmup_value_that_cannot_be_used_is_an_escaped_missing_line() {
        assert_eq!(
            warm_said(
                vec![Warmup::ConsentProblem("warmup[0] is not a non-empty string\u{202e}".to_owned())],
                &[]
            ),
            Said {
                lines: one_item(
                    r"check: here: warm-up: remote.json: warmup[0] is not a non-empty string\u{202e}",
                    "check: here: 1 missing, 0 too old"
                ),
                status: 1,
            }
        );
    }

    #[test]
    fn a_hooks_value_that_cannot_be_used_is_an_escaped_missing_line() {
        let checked = Checked {
            hooks_problem: Some("hooks[0] is not a non-empty string\u{202e}".to_owned()),
            ..checked(Vec::new())
        };
        assert_eq!(
            said(HERE, &checked),
            Said {
                lines: one_item(
                    r"check: here: remote.json: hooks[0] is not a non-empty string\u{202e}",
                    "check: here: 1 missing, 0 too old"
                ),
                status: 1,
            }
        );
    }

    #[test]
    fn a_consented_warm_up_file_with_problems_prints_each_and_fails_the_check() {
        assert_eq!(
            warm_said(
                vec![Warmup::FileProblems],
                &["commands[0] is empty", "extra is not a key (commands, timeout)"]
            ),
            Said {
                lines: vec![
                    out("check: here: app at X:/dev/app"),
                    Line::Err("check: .gate/warmup.toml: commands[0] is empty".to_owned()),
                    Line::Err("check: .gate/warmup.toml: extra is not a key (commands, timeout)".to_owned()),
                    out("check: here: warm-up: .gate/warmup.toml has problems, so gate warm cannot prewarm a new worktree"),
                    out("check: here: 1 missing, 0 too old"),
                ],
                status: 1,
            }
        );
    }

    #[test]
    fn a_state_root_that_cannot_be_found_is_skipped() {
        let why = "cannot read the LOCALAPPDATA variable";
        assert_eq!(
            warm_said(vec![Warmup::NoStateRoot(why.to_owned())], &[]),
            Said {
                lines: one_item(
                    &format!("check: here: warm-up: the state root cannot be found: {why}"),
                    "check: here: nothing missing (1 skipped)"
                ),
                status: 0,
            }
        );
    }

    #[test]
    fn a_refused_sibling_reads_as_routing_words_it_with_no_hint() {
        let checked = Checked {
            siblings: vec![Sibling {
                name: "app".to_owned(),
                path: "X:/dev/app".to_owned(),
                found: Found::Folder(Ok(SiblingFolder::Refused("is the worktree's own folder".to_owned()))),
            }],
            ..checked(Vec::new())
        };
        assert_eq!(body(&checked), ["check: here: sibling app is the worktree's own folder"]);
        assert_eq!(summary(&checked), "check: here: 1 missing, 0 too old");
    }

    #[test]
    fn the_sdk_pin_served_is_ok_and_one_that_cannot_be_read_is_skipped() {
        let served = Checked {
            sdk: Some(Sdk::Checked {
                version: "10.0.401".to_owned(),
                need: ".NET SDK 10.0.401 (rollForward patch)".to_owned(),
                have: vec!["10.0.402".to_owned()],
                served: true,
            }),
            ..checked(Vec::new())
        };
        assert_eq!(body(&served), ["check: here: .NET SDK 10.0.401 (rollForward patch) ok"]);
        let none = Checked {
            sdk: Some(Sdk::Checked {
                version: "8.0.100".to_owned(),
                need: ".NET SDK 8.0.100 (rollForward patch)".to_owned(),
                have: Vec::new(),
                served: false,
            }),
            ..checked(Vec::new())
        };
        assert_eq!(
            body(&none),
            [
                "check: here: .NET SDK 8.0.100 (rollForward patch) missing (this PC has none)",
                "install: here: .NET SDK: winget install Microsoft.DotNet.SDK.8",
            ]
        );
        let skipped = Checked {
            sdk: Some(Sdk::Skipped("global.json has no readable sdk pin".to_owned())),
            ..checked(Vec::new())
        };
        assert_eq!(body(&skipped), ["check: here: .NET SDK skipped: global.json has no readable sdk pin"]);
        assert_eq!(summary(&skipped), "check: here: nothing missing (1 skipped)");
    }

    #[test]
    fn an_unknown_tool_is_a_skip_line() {
        let checked = checked(vec![tool("nodejs", Some("24"), Probed::Skipped("unknown tool".to_owned()))]);
        assert_eq!(body(&checked), ["check: here: tools.nodejs skipped: not a tool gate knows"]);
        assert_eq!(said(HERE, &checked).status, 0);
    }

    #[test]
    fn an_unreadable_siblings_file_fails_the_check_and_says_so() {
        let unreadable = Checked {
            unreadable: vec![SIBLINGS],
            ..checked(Vec::new())
        };
        assert_eq!(
            summary(&unreadable),
            "check: here: could not read .gate/siblings.toml; 0 missing, 0 too old"
        );
        assert_eq!(said(HERE, &unreadable).status, 1);
        let both = Checked {
            unreadable: vec![TOOLCHAIN, SIBLINGS],
            ..checked(Vec::new())
        };
        assert_eq!(
            summary(&both),
            "check: here: could not read .gate/toolchain.toml, .gate/siblings.toml; 0 missing, 0 too old",
            "the manifest comes first"
        );
    }

    #[test]
    fn an_unreadable_global_json_fails_the_check_and_is_named_last() {
        let unreadable = Checked {
            unreadable: vec![GLOBAL_JSON],
            ..checked(Vec::new())
        };
        assert_eq!(summary(&unreadable), "check: here: could not read global.json; 0 missing, 0 too old");
        assert_eq!(said(HERE, &unreadable).status, 1);
        let all = Checked {
            unreadable: vec![TOOLCHAIN, SIBLINGS, GLOBAL_JSON],
            ..checked(Vec::new())
        };
        assert_eq!(
            summary(&all),
            "check: here: could not read .gate/toolchain.toml, .gate/siblings.toml, global.json; 0 missing, 0 too old",
            "the manifest, then its siblings, then the pin"
        );
    }

    #[test]
    fn a_manifest_problem_is_a_line_and_the_rest_are_checked() {
        let checked = Checked {
            problems: vec![
                Problem {
                    file: ".gate/toolchain.toml",
                    text: "tools.node is not a version: \">=24\"".to_owned(),
                },
                Problem {
                    file: ".gate/siblings.toml",
                    text: "siblings[0] is empty".to_owned(),
                },
            ],
            ..checked(vec![tool("pwsh", None, found("7.6.6"))])
        };
        assert_eq!(
            said(HERE, &checked).lines,
            [
                out("check: here: app at X:/dev/app"),
                Line::Err("check: .gate/toolchain.toml: tools.node is not a version: \">=24\"".to_owned()),
                Line::Err("check: .gate/siblings.toml: siblings[0] is empty".to_owned()),
                out("check: here: pwsh 7.6.6 ok"),
                out("check: here: nothing missing"),
            ]
        );
    }

    /// The builder of the builder cases, and the key of its work folder.
    const BUILDER: At<'static> = At::Builder {
        host: "box-1",
        key: "app-1a2b3c4d",
    };

    /// The stdout lines of a builder's check between its header and its summary, the hints among them.
    fn builder_body(checked: &Checked) -> Vec<String> {
        let said = said(BUILDER, checked);
        let outs: Vec<String> = said
            .lines
            .iter()
            .filter_map(|line| match line {
                Line::Out(text) => Some(text.clone()),
                Line::Err(_) => None,
            })
            .collect();
        outs[1..outs.len() - 1].to_vec()
    }

    #[test]
    fn a_builder_check_says_its_work_folder_the_folders_it_created_and_skips_the_siblings() {
        let checked = Checked {
            created: vec![r"D:\gate-remote\app.git".to_owned()],
            siblings_skipped: true,
            ..checked(vec![tool("pwsh", None, found("7.6.6"))])
        };
        assert_eq!(
            said(BUILDER, &checked),
            Said {
                lines: vec![
                    out("check: box-1: app (work folder app-1a2b3c4d)"),
                    out(r"check: box-1: created D:\gate-remote\app.git"),
                    out("check: box-1: pwsh 7.6.6 ok"),
                    out("check: box-1: siblings skipped: a routed run lays them out"),
                    out("check: box-1: nothing missing (1 skipped)"),
                ],
                status: 0,
            }
        );
    }

    #[test]
    fn a_builder_says_at_most_20_created_folders_then_how_many_more() {
        let many = Checked {
            created: (1..=23).map(|n| format!(r"D:\gate-remote\r{n}.git")).collect(),
            ..checked(Vec::new())
        };
        let lines = said(BUILDER, &many).lines;
        let created: Vec<&Line> = lines
            .iter()
            .filter(|line| matches!(line, Line::Out(text) if text.contains(" created ")))
            .collect();
        assert_eq!(created.len(), 21, "{lines:?}");
        assert_eq!(created[19], &out(r"check: box-1: created D:\gate-remote\r20.git"));
        assert_eq!(created[20], &out("check: box-1: and 3 more created folders"));
        let twenty = Checked {
            created: (1..=20).map(|n| format!("r{n}")).collect(),
            ..checked(Vec::new())
        };
        assert!(
            !said(BUILDER, &twenty)
                .lines
                .iter()
                .any(|line| matches!(line, Line::Out(text) if text.contains("more created"))),
            "20 are all said"
        );
    }

    #[test]
    fn a_builder_s_sdk_list_says_at_most_20_entries_then_how_many_more() {
        let missing = |count: usize| Checked {
            sdk: Some(Sdk::Checked {
                version: "10.0.401".to_owned(),
                need: ".NET SDK 10.0.401 (rollForward patch)".to_owned(),
                have: (1..=count).map(|n| format!("8.0.{n}")).collect(),
                served: false,
            }),
            ..checked(Vec::new())
        };
        let twenty: Vec<String> = (1..=20).map(|n| format!("8.0.{n}")).collect();
        assert_eq!(
            builder_body(&missing(25))[0],
            format!(
                "check: box-1: .NET SDK 10.0.401 (rollForward patch) missing (box-1 has {}, and 5 more)",
                twenty.join(", ")
            )
        );
        assert_eq!(
            builder_body(&missing(20))[0],
            format!(
                "check: box-1: .NET SDK 10.0.401 (rollForward patch) missing (box-1 has {})",
                twenty.join(", ")
            )
        );
        let here = said(HERE, &missing(25)).lines;
        assert!(
            here.iter().any(|line| matches!(line, Line::Out(text) if text.ends_with(", 8.0.25)"))),
            "this PC's list is said whole: {here:?}"
        );
    }

    #[test]
    fn a_created_folder_the_builder_names_stays_one_line() {
        let checked = Checked {
            created: vec!["D:\\x\ninstall: box-1: node: y\u{202e}".to_owned()],
            ..checked(Vec::new())
        };
        assert_eq!(
            said(BUILDER, &checked).lines[1],
            out(r"check: box-1: created D:\x\ninstall: box-1: node: y\u{202e}")
        );
    }

    #[test]
    fn a_missing_tool_on_a_builder_says_a_hint_that_works_over_ssh() {
        let checked = checked(vec![
            tool("cargo-nextest", Some("0.9.100"), Probed::Missing),
            tool("pnpm", Some("10"), Probed::Missing),
            tool("pnpm", None, Probed::Missing),
            tool("rust", Some("1.89"), Probed::Missing),
            tool("rust", None, Probed::Missing),
            tool("node", None, Probed::Missing),
            tool("pwsh", None, Probed::Missing),
            tool("ffmpeg", None, Probed::Missing),
            tool("godot", None, Probed::Missing),
        ]);
        let hints: Vec<String> = builder_body(&checked).into_iter().filter(|line| line.starts_with("install: ")).collect();
        assert_eq!(
            hints,
            [
                "install: box-1: cargo-nextest: cargo install cargo-nextest --locked",
                "install: box-1: pnpm: npm install -g pnpm@10.0.0",
                "install: box-1: pnpm: npm install -g pnpm",
                "install: box-1: rust: rustup toolchain install 1.89.0 && rustup default 1.89.0",
                "install: box-1: rust: copy tools/builder/ from the gate repo to box-1 and run install-toolchains.ps1 -User <user> -Only Rust there, elevated (docs/routing-a-repo.md, Builder setup)",
                "install: box-1: node: copy tools/builder/ from the gate repo to box-1 and run install-toolchains.ps1 -User <user> -Only Node.js there, elevated (docs/routing-a-repo.md, Builder setup)",
                "install: box-1: pwsh: copy tools/builder/ from the gate repo to box-1 and run install-toolchains.ps1 -User <user> -Only PowerShell there, elevated (docs/routing-a-repo.md, Builder setup)",
                "install: box-1: ffmpeg: copy tools/builder/ from the gate repo to box-1 and run install-toolchains.ps1 -User <user> -Only FFmpeg there, elevated (docs/routing-a-repo.md, Builder setup)",
            ],
            "godot has no hint"
        );
        assert_eq!(said(BUILDER, &checked).status, 1);
    }

    #[test]
    fn a_tool_too_old_on_a_builder_says_the_same_hint() {
        let checked = checked(vec![
            tool("rust", Some("1.89"), found("1.88.0")),
            tool("pnpm", Some("10"), found("9.1.0")),
            tool("node", Some("24"), found("22.1.0")),
        ]);
        assert_eq!(
            builder_body(&checked),
            [
                "check: box-1: rust 1.88.0 too old (needs 1.89.0 or later)",
                "check: box-1: pnpm 9.1.0 too old (needs 10.0.0 or later)",
                "check: box-1: node 22.1.0 too old (needs 24.0.0 or later)",
                "install: box-1: rust: rustup toolchain install 1.89.0 && rustup default 1.89.0",
                "install: box-1: pnpm: npm install -g pnpm@10.0.0",
                "install: box-1: node: copy tools/builder/ from the gate repo to box-1 and run install-toolchains.ps1 -User <user> -Only Node.js there, elevated (docs/routing-a-repo.md, Builder setup)",
            ]
        );
    }

    #[test]
    fn a_builder_s_sdk_line_names_the_builder() {
        let missing = |have: Vec<String>| Checked {
            sdk: Some(Sdk::Checked {
                version: "10.0.401".to_owned(),
                need: ".NET SDK 10.0.401 (rollForward patch)".to_owned(),
                have,
                served: false,
            }),
            ..checked(Vec::new())
        };
        assert_eq!(
            builder_body(&missing(vec!["8.0.404".to_owned(), "10.0.300".to_owned()])),
            [
                "check: box-1: .NET SDK 10.0.401 (rollForward patch) missing (box-1 has 8.0.404, 10.0.300)",
                "install: box-1: .NET SDK: dotnet-install.ps1 -Version 10.0.401",
            ]
        );
        assert_eq!(
            builder_body(&missing(Vec::new()))[0],
            "check: box-1: .NET SDK 10.0.401 (rollForward patch) missing (box-1 has none)"
        );
    }

    #[test]
    fn a_repo_without_a_manifest_says_it_declares_no_toolchain() {
        let checked = Checked {
            declares_toolchain: false,
            ..checked(Vec::new())
        };
        assert_eq!(
            said(HERE, &checked).lines,
            [
                out("check: here: app at X:/dev/app"),
                out("check: here: app declares no toolchain"),
                out("check: here: nothing missing"),
            ]
        );
    }
}
