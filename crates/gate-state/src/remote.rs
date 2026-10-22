//! The gate's `remote.json`: its `slots`, a whole number 1 to 16, or 1 when the key is missing, which sizes `host`'s
//! remote pool; and its `builders` and `hosts` entries, read through the gate's own reader ([`gate_router::Config`]) for
//! the pools the dashboard lists beside `host`'s.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use gate_router::{BuilderOs, Config, builder_id};
use serde_json::Value;

use crate::counts::{format_count_value, parse_counts, property, prose};
use crate::model::{SlotCount, StateInputs};

/// The most slots the remote pool may have.
const MAX_SLOTS: u32 = 16;

/// The remote pool's size as `remote.json` gives it.
pub(crate) struct RemoteCount {
    /// The slots gates may take: `slots`, 1 when the key is missing, or 0 when the file is ignored.
    pub count: u32,
    /// Why the file was ignored, or `None`.
    pub problem: Option<String>,
}

/// A `builders` or `hosts` entry as the dashboard lists it.
pub(crate) struct BuilderSetting {
    /// The builder's alias, the ssh destination the file spells.
    pub alias: String,
    /// The builder's operating system, `windows` or `macos` as `remote.json` spells it; `None` when the file's value
    /// names neither.
    pub os: Option<&'static str>,
    /// The slots gates may take: the entry's `slots`, or 0 for an entry of a file the gate refuses.
    pub count: u32,
    /// Whether a pick may send a run to the builder.
    pub routed: bool,
    /// Why the gate refuses the file, on each entry of a refused one; `None` for a file the gate can use.
    pub problem: Option<String>,
}

/// The `builders` and `hosts` entries the gate's `remote.json` names.
#[derive(Default)]
pub(crate) struct BuilderSettings {
    /// The `builders` entries, in file order.
    pub entries: Vec<BuilderSetting>,
    /// The `hosts` entries that are no `builders` entry, in file order.
    pub hosts: Vec<BuilderSetting>,
    /// Why the file names no entry for a pool its claims show — it cannot be read, or the gate refuses it — which such a
    /// pool takes in place of the orphan text: a pool named by no entry of a file nothing can be read from, or of one
    /// the gate refuses whole, is no orphan; `None` otherwise.
    pub problem: Option<String>,
}

/// `remote.json` as one poll reads it: its bytes, or why there are none.
pub(crate) enum RemoteFile {
    /// There is no `remote.json`.
    Missing,
    /// The file is there but cannot be read, as a save holds it: the error as the read gave it.
    Unreadable(String),
    /// The file's bytes.
    Read(Vec<u8>),
}

/// The `remote.json` at `path`, read once for one poll so a poll never mixes two saves: absent, unreadable, or its bytes.
pub(crate) fn read_remote_file(path: &Path) -> RemoteFile {
    match fs::read(path) {
        Ok(bytes) => RemoteFile::Read(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => RemoteFile::Missing,
        Err(error) => RemoteFile::Unreadable(error.to_string()),
    }
}

/// The `builders` and `hosts` entries [`remote.json`](RemoteFile) names, read through the gate's own reader
/// ([`Config::parse`]) so the dashboard lists the pools the gate routes to. A file the gate refuses still lists each
/// `builders` and `hosts` entry its raw JSON names, with no slots and the refusal as its problem: the claim files their
/// pool ids name are shown as extra rows, and so does a refused file's own problem reach a pool no entry names. A file
/// that cannot be read names no entry, and sets the file's problem. A missing file, and one whose `builders` no JSON
/// object holds, names none.
pub(crate) fn builder_settings(file: &RemoteFile) -> BuilderSettings {
    match file {
        RemoteFile::Missing => BuilderSettings::default(),
        RemoteFile::Unreadable(error) => BuilderSettings {
            problem: Some(format!("remote.json cannot be read: {}", prose(error))),
            ..BuilderSettings::default()
        },
        RemoteFile::Read(bytes) => match Config::parse(&String::from_utf8_lossy(bytes)) {
            Ok(config) => from_config(&config, bytes),
            Err(reason) => from_refused(bytes, &reason),
        },
    }
}

/// The entries [`Config`] reads from a file the gate can use for routing, and the `hosts` entries its raw JSON names
/// when `Config` refuses that value: the gate then routes on the `builders` entries alone, but the file still names the
/// hosts a pinned run reaches.
fn from_config(config: &Config, bytes: &[u8]) -> BuilderSettings {
    let entries: Vec<BuilderSetting> = config
        .builder_entries()
        .map(|builder| BuilderSetting {
            alias: builder.alias.clone(),
            os: Some(os_text(builder.os)),
            count: builder.slots,
            routed: config.builder_routed(builder),
            problem: None,
        })
        .collect();
    // `Config::builders` is `host`, then each `hosts` entry: a `hosts` entry is a Windows builder with the top-level
    // slots, which only a run pinned to it reaches.
    let slots = config.slots();
    let hosts = match config.builders() {
        Ok(aliases) => aliases
            .iter()
            .skip(1)
            .map(|alias| BuilderSetting {
                alias: alias.clone(),
                os: Some(os_text(BuilderOs::Windows)),
                count: slots,
                routed: false,
                problem: None,
            })
            .collect(),
        Err(problem) => raw_hosts(bytes, problem, slots, &entries),
    };
    BuilderSettings {
        entries,
        hosts,
        problem: None,
    }
}

/// The `hosts` strings the raw JSON `bytes` names, each a Windows builder with `slots` slots and `problem` as its
/// problem, the first entry of a pool id only: a `hosts` value the gate refuses lists none of them, though the file
/// still names the hosts a pinned run reaches with a claim.
fn raw_hosts(bytes: &[u8], problem: &str, slots: u32, entries: &[BuilderSetting]) -> Vec<BuilderSetting> {
    let Ok(settings) = parse_counts(bytes) else {
        return Vec::new();
    };
    let mut seen: Vec<String> = entries.iter().map(|entry| builder_id(&entry.alias)).collect();
    let mut hosts = Vec::new();
    for item in property(&settings, "hosts").and_then(Value::as_array).into_iter().flatten() {
        let Some(alias) = item.as_str() else {
            continue;
        };
        let id = builder_id(alias);
        if seen.contains(&id) {
            continue;
        }
        seen.push(id);
        hosts.push(BuilderSetting {
            alias: alias.to_owned(),
            os: Some(os_text(BuilderOs::Windows)),
            count: slots,
            routed: false,
            problem: Some(problem.to_owned()),
        });
    }
    hosts
}

/// The `builders` and `hosts` entries the raw JSON `bytes` names, each with `reason` as its problem and no slots, the
/// first entry of a pool id only: a file may spell one builder twice, in two letter cases. A `builders` entry must name
/// its `host`; a `hosts` entry is a string, a Windows builder as the file the gate can use gives it. The file's own
/// problem is the refusal, so a pool no entry names says why it cannot be named.
fn from_refused(bytes: &[u8], reason: &str) -> BuilderSettings {
    let Ok(settings) = parse_counts(bytes) else {
        return BuilderSettings {
            problem: Some(reason.to_owned()),
            ..BuilderSettings::default()
        };
    };
    let mut seen: Vec<String> = Vec::new();
    let mut entries: Vec<BuilderSetting> = Vec::new();
    for item in property(&settings, "builders").and_then(Value::as_array).into_iter().flatten() {
        let Some(object) = item.as_object() else {
            continue;
        };
        let Some(alias) = property(object, "host").and_then(Value::as_str) else {
            continue;
        };
        let id = builder_id(alias);
        if seen.contains(&id) {
            continue;
        }
        seen.push(id);
        entries.push(BuilderSetting {
            os: property(object, "os").and_then(Value::as_str).and_then(known_os),
            alias: alias.to_owned(),
            count: 0,
            routed: property(object, "routed").and_then(Value::as_bool).unwrap_or(true),
            problem: Some(reason.to_owned()),
        });
    }
    let mut hosts: Vec<BuilderSetting> = Vec::new();
    for item in property(&settings, "hosts").and_then(Value::as_array).into_iter().flatten() {
        let Some(alias) = item.as_str() else {
            continue;
        };
        let id = builder_id(alias);
        if seen.contains(&id) {
            continue;
        }
        seen.push(id);
        hosts.push(BuilderSetting {
            alias: alias.to_owned(),
            os: Some(os_text(BuilderOs::Windows)),
            count: 0,
            routed: false,
            problem: Some(reason.to_owned()),
        });
    }
    BuilderSettings {
        entries,
        hosts,
        problem: Some(reason.to_owned()),
    }
}

/// A builder's operating system as `remote.json` spells it.
fn os_text(os: BuilderOs) -> &'static str {
    match os {
        BuilderOs::Windows => "windows",
        BuilderOs::Macos => "macos",
    }
}

/// The spelling of `text` when it names an operating system `remote.json` holds, matched in any ASCII letter case.
fn known_os(text: &str) -> Option<&'static str> {
    if text.eq_ignore_ascii_case("windows") {
        Some("windows")
    } else if text.eq_ignore_ascii_case("macos") {
        Some("macos")
    } else {
        None
    }
}

/// `<local app data>\gate\remote.json`.
pub(crate) fn remote_settings_path(inputs: &StateInputs) -> PathBuf {
    inputs.local_app_data.join("gate").join("remote.json")
}

/// The remote pool's size from the `remote.json` one poll read as `file`, or `None` when the file does not exist. A file
/// with no `slots` key (matched in any letter case) means one remote slot. A file that cannot be read, is not a JSON
/// object, or whose `slots` is not a whole number 1 to 16 gives a count of 0 and a problem line naming `path`.
pub(crate) fn remote_count(file: &RemoteFile, path: &Path) -> Option<RemoteCount> {
    let ignored = |reason: &str| RemoteCount {
        count: 0,
        problem: Some(format!("gate: ignored {}: {reason}; using no remote slots", path.display())),
    };
    let settings = match file {
        RemoteFile::Missing => return None,
        RemoteFile::Unreadable(error) => return Some(ignored(&format!("it could not be read: {}", prose(error)))),
        RemoteFile::Read(bytes) => match parse_counts(bytes) {
            Ok(settings) => settings,
            Err(reason) => return Some(ignored(&reason)),
        },
    };
    let Some(value) = property(&settings, "slots") else {
        return Some(RemoteCount { count: 1, problem: None });
    };
    match SlotCount::from_json(value).map(SlotCount::get).filter(|count| *count <= MAX_SLOTS) {
        Some(count) => Some(RemoteCount { count, problem: None }),
        None => Some(ignored(&format!(
            "slots must be a whole number 1 to {MAX_SLOTS}, got {}",
            format_count_value(value)
        ))),
    }
}
