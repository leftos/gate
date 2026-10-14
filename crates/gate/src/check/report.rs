//! What `gate check` says, as a pure function of what it found ([`Checked`]): a header, each item's line in the order
//! manifest tools, the .NET SDK, workloads, siblings, an install hint per missing or too-old item, then a summary, all
//! prefixed `check: <where>: ` (`install: <where>: ` for a hint), and the problems of the `.gate` files on stderr as
//! `check: .gate/<file>: <problem>`. The exit status is 0 only when nothing is missing or too old: a skipped item never
//! fails the check.

use crate::answer::FAILED_STATUS;
use crate::boxside::payload::escaped;
use crate::boxside::probe::Probed;
use crate::remote::toolchain::{ToolNeed, Version, meets, spelled};

/// The `<where>` of this PC's lines.
pub(crate) const HERE: &str = "here";

/// A manifest tool and what its probe found; an unknown tool's probe is never read.
#[derive(Debug)]
pub(crate) struct Tool {
    pub need: ToolNeed,
    pub probed: Probed,
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

/// A sibling the repo declares: its name, where it must be, and whether a git worktree is there, or why that cannot
/// be told.
#[derive(Debug)]
pub(crate) struct Sibling {
    pub name: String,
    pub path: String,
    pub present: Result<bool, String>,
}

/// How the lines name the manifest.
pub(crate) const TOOLCHAIN: &str = ".gate/toolchain.toml";
/// How the lines name the siblings file.
pub(crate) const SIBLINGS: &str = ".gate/siblings.toml";
/// How the lines name the root SDK pin file.
pub(crate) const GLOBAL_JSON: &str = "global.json";

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
    /// Whether the repo has a `.gate/toolchain.toml`, readable or not.
    pub declares_toolchain: bool,
    /// The files that are there but cannot be read whole, so their items could not be checked, in the order the
    /// summary names them: the manifest, its siblings, the root `global.json`.
    pub unreadable: Vec<&'static str>,
    /// The problems of the files the check reads, in the order found.
    pub problems: Vec<Problem>,
    /// The manifest's tools, in manifest order.
    pub tools: Vec<Tool>,
    /// The SDK pin's check; `None` when the root has no `global.json`.
    pub sdk: Option<Sdk>,
    /// The manifest's workload ids, which the check lists without checking them.
    pub workloads: Vec<String>,
    /// The declared siblings, in declared order.
    pub siblings: Vec<Sibling>,
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
    /// Said, but neither checked nor skipped: the workloads.
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

/// What the check says of `checked` at `at`, `here` for this PC.
pub(crate) fn said(at: &str, checked: &Checked) -> Said {
    let mut lines = vec![Line::Out(format!("check: {at}: {} at {}", checked.repo, checked.root))];
    if !checked.declares_toolchain {
        lines.push(Line::Out(format!("check: {at}: {} declares no toolchain", checked.repo)));
    }
    lines.extend(
        checked
            .problems
            .iter()
            .map(|problem| Line::Err(format!("check: {}: {}", problem.file, escaped(&problem.text)))),
    );
    let items = items(checked);
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
        format!("could not read {}; {}", checked.unreadable.join(", "), counts(missing, too_old, skipped))
    };
    lines.push(Line::Out(format!("check: {at}: {summary}")));
    let failed = !checked.unreadable.is_empty() || missing + too_old > 0;
    let status = if failed { u32::from(FAILED_STATUS) } else { 0 };
    Said { lines, status }
}

/// Every item of `checked`, in the order the lines say them.
fn items(checked: &Checked) -> Vec<Item> {
    let mut items: Vec<Item> = checked.tools.iter().map(tool_item).collect();
    items.extend(checked.sdk.as_ref().map(sdk_item));
    if !checked.workloads.is_empty() {
        let ids: Vec<String> = checked.workloads.iter().map(|id| one_line(id)).collect();
        let text = format!("workloads {} not checked yet", ids.join(", "));
        items.push(Item::new(text, Kind::Unchecked));
    }
    items.extend(checked.siblings.iter().map(sibling_item));
    items
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

/// The line of a manifest tool as its probe found it, against its minimum.
fn tool_item(tool: &Tool) -> Item {
    let name = one_line(&tool.need.name);
    if !tool.need.known {
        return Item::new(format!("tools.{name} skipped: not a tool gate knows"), Kind::Skipped);
    }
    let min = tool.need.min;
    let needs = min.map_or_else(|| "needs it present".to_owned(), |min| format!("needs {min} or later"));
    let hint = |kind| install_hint(&name, min, kind).map(|command| (name.clone(), command));
    match (&tool.probed, min) {
        (Probed::Version(found), Some(min)) => match meets(min, found) {
            Some(true) => Item::new(format!("{name} {} ok ({needs})", escaped(found)), Kind::Ok),
            Some(false) => Item::hinted(format!("{name} {} too old ({needs})", escaped(found)), Kind::TooOld, hint(Kind::TooOld)),
            None => Item::new(format!("{name} skipped: found, version not readable ({needs})"), Kind::Skipped),
        },
        (Probed::Version(found), None) => Item::new(format!("{name} {} ok", escaped(found)), Kind::Ok),
        (Probed::Present, None) => Item::new(format!("{name} ok (version not readable)"), Kind::Ok),
        (Probed::Present, Some(_)) => Item::new(format!("{name} skipped: found, version not readable ({needs})"), Kind::Skipped),
        (Probed::Missing, _) => Item::hinted(format!("{name} missing ({needs})"), Kind::Missing, hint(Kind::Missing)),
        (Probed::Skipped(why), _) => Item::new(format!("{name} skipped: {}", escaped(why)), Kind::Skipped),
    }
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

/// The line of the SDK pin's check.
fn sdk_item(sdk: &Sdk) -> Item {
    match sdk {
        Sdk::Checked { need, served: true, .. } => Item::new(format!("{need} ok"), Kind::Ok),
        Sdk::Checked { version, need, have, .. } => {
            let have = if have.is_empty() { "none".to_owned() } else { have.join(", ") };
            let major = version.split('.').next().unwrap_or(version);
            let hint = (".NET SDK".to_owned(), format!("winget install Microsoft.DotNet.SDK.{major}"));
            Item::hinted(format!("{need} missing (this PC has {have})"), Kind::Missing, Some(hint))
        }
        Sdk::Skipped(why) => Item::new(format!(".NET SDK skipped: {}", escaped(why)), Kind::Skipped),
    }
}

/// The line of a declared sibling.
fn sibling_item(sibling: &Sibling) -> Item {
    let Sibling { name, path, present } = sibling;
    match present {
        Ok(true) => Item::new(format!("sibling {name} ok ({path})"), Kind::Ok),
        Ok(false) => Item::hinted(
            format!("sibling {name} missing (no git worktree at {path})"),
            Kind::Missing,
            Some((format!("sibling {name}"), format!("clone it to {path}"))),
        ),
        Err(why) => Item::new(format!("sibling {name} skipped: {}", escaped(why)), Kind::Skipped),
    }
}

#[cfg(test)]
mod tests {
    use super::{Checked, GLOBAL_JSON, HERE, Line, Problem, SIBLINGS, Said, Sdk, Sibling, TOOLCHAIN, Tool, said};
    use crate::boxside::probe::Probed;
    use crate::remote::toolchain::{ToolNeed, Version};

    fn need(name: &str, min: Option<&str>) -> ToolNeed {
        ToolNeed {
            name: name.to_owned(),
            min: min.and_then(Version::parse),
            known: !name.starts_with("nodejs"),
        }
    }

    fn tool(name: &str, min: Option<&str>, probed: Probed) -> Tool {
        Tool {
            need: need(name, min),
            probed,
        }
    }

    fn found(version: &str) -> Probed {
        Probed::Version(version.to_owned())
    }

    fn checked(tools: Vec<Tool>) -> Checked {
        Checked {
            repo: "app".to_owned(),
            root: "X:/dev/app".to_owned(),
            declares_toolchain: true,
            unreadable: Vec::new(),
            problems: Vec::new(),
            tools,
            sdk: None,
            workloads: Vec::new(),
            siblings: Vec::new(),
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
                present: Ok(false),
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
            tool("godot", None, Probed::Skipped("no probe for godot".to_owned())),
        ]);
        assert_eq!(
            body(&checked),
            [
                "check: here: node skipped: no answer in 10 s",
                "check: here: godot skipped: no probe for godot",
            ]
        );
        assert_eq!(summary(&checked), "check: here: nothing missing (2 skipped)");
        assert_eq!(said(HERE, &checked).status, 0);
    }

    #[test]
    fn the_summary_counts_missing_old_and_skipped() {
        let mixed = checked(vec![
            tool("node", Some("24"), found("22.1.0")),
            tool("pnpm", None, Probed::Missing),
            tool("cargo-nextest", None, Probed::Missing),
            tool("godot", None, Probed::Skipped("no probe for godot".to_owned())),
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
                present: Ok(false),
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
                    present: Ok(true),
                },
                Sibling {
                    name: "b".to_owned(),
                    path: "X:/dev/b".to_owned(),
                    present: Ok(false),
                },
                Sibling {
                    name: "c".to_owned(),
                    path: "X:/dev/c".to_owned(),
                    present: Err("cannot tell whether X:/dev/c\\.git exists: access denied".to_owned()),
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
                "check: here: sibling c skipped: cannot tell whether X:/dev/c\\.git exists: access denied",
                "install: here: node: winget install OpenJS.NodeJS",
                "install: here: .NET SDK: winget install Microsoft.DotNet.SDK.10",
                "install: here: sibling b: clone it to X:/dev/b",
            ]
        );
        assert_eq!(summary(&checked), "check: here: 3 missing, 0 too old, 1 skipped");
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
