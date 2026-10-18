//! The routing file, `remote.json`, read from its text: the builder's `host`, the optional `hosts` (further builders,
//! which `gate push` reaches and routing neither reads nor refuses), the remote pool's `slots` (a whole
//! number 1 to 16, 1 when missing), `repos` (each repo name's list of `-like` patterns for the commands that route) and
//! the optional `outputs` (each repo name's globs for the files a routed run brings back), the optional `inputs` (each
//! repo name's globs for the ignored files a routed run sends) and the optional `siblings` (each repo name's list of
//! the sibling folder names a routed run of it may carry) and the optional `warmup` (the repo names whose
//! `.gate/warmup.toml` may warm their builder work folders) and the optional `desktop` (each repo name's list of the
//! `.gate/desktop.toml` patterns whose commands may run on the builder's desktop) and the optional `hooks` (the repo
//! names whose gates started inside a git hook may route). Keys match in any letter case and unknown keys are ignored.
//! The text may hold `//` and `/* */` comments and trailing commas, as the
//! PowerShell gate's JSON reader allows; single-quoted strings are refused. A file that breaks a rule is unusable as a
//! whole, for the reason the PowerShell gate gives (or, for a host ssh or git would misread, a reason of its own),
//! which the caller says in [`ignored_line`].

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::key::is_layout_name;
use crate::like::{Pattern, eq_ignoring_case};

/// The output globs of a repo the `outputs` key does not name.
pub const DEFAULT_OUTPUTS: [&str; 2] = ["**/bin/**", "**/.godot/mono/temp/bin/**"];

/// The output globs of a cargo repo the `outputs` key does not name: the top-level files of each profile folder, not
/// `deps/`, `build/` or `incremental/`.
pub const CARGO_OUTPUTS: [&str; 3] = ["target/*/*.exe", "target/*/*.dll", "target/*/*.pdb"];

/// The most slots the remote pool may have.
const MAX_SLOTS: u32 = 16;

/// A usable routing file.
#[derive(Debug, Clone)]
pub struct Config {
    /// `host`, then each `hosts` entry; never empty.
    builders: Vec<String>,
    /// Why `hosts` cannot be used, which routing ignores.
    hosts_problem: Option<String>,
    slots: u32,
    repos: Vec<(String, Vec<Pattern>)>,
    outputs: Vec<(String, Vec<Pattern>)>,
    inputs: Vec<(String, Vec<Pattern>)>,
    /// Each repo name's allowed sibling names, or why `siblings` cannot be used, which allows none.
    siblings: Result<Vec<(String, Vec<String>)>, String>,
    /// The repo names whose warm-up may run, or why `warmup` cannot be used, which consents to none.
    warmup: Result<Vec<String>, String>,
    /// Each repo name's allowed desktop patterns, or why `desktop` cannot be used, which allows none.
    desktop: Result<Vec<(String, Vec<String>)>, String>,
    /// The repo names whose gates started inside a git hook may route, or why `hooks` cannot be used, which consents to
    /// none.
    hooks: Result<Vec<String>, String>,
}

/// A routing file that is there but cannot be used: not a file, unreadable, or refused by [`Config::parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigProblem {
    /// The routing file's path.
    pub path: PathBuf,
    /// Why it cannot be used, as [`ignored_line`] says it.
    pub reason: String,
}

/// One repo name's pattern list.
type RepoTable = Vec<(String, Vec<Pattern>)>;

/// The line the caller says when it ignores the routing file at `path` for `reason`:
/// `gate: ignored <path>: <reason>; running locally`.
#[must_use]
pub fn ignored_line(path: &Path, reason: &str) -> String {
    format!("gate: ignored {}: {reason}; running locally", path.display())
}

impl Config {
    /// Reads the routing file's text; a leading byte order mark, comments and trailing commas are skipped.
    ///
    /// # Errors
    ///
    /// The reason the file is unusable, worded as the PowerShell gate words it: text that is not JSON, two keys of one
    /// object differing only in letter case, a value that is not a JSON object, a missing or empty `host`, a `slots`
    /// that is not a whole number 1 to 16, a missing `repos`, a `repos`, `outputs` or `inputs` that is not an object of
    /// arrays of non-empty valid patterns, or an output or input glob holding `[` or `]`.
    ///
    /// A `host` that is not an ssh alias or `user@host` of letters, digits, `.`, `_` and `-` is refused with a reason of
    /// the gate's own, since the PowerShell gate accepts it. A bad `hosts` leaves the file usable for routing, and
    /// [`Config::builders`] gives its reason. A bad `siblings` leaves it usable too, allowing no sibling, and
    /// [`Config::allowed_siblings`] gives its reason. So does a bad `warmup`, consenting to no warm-up, and
    /// [`Config::warmup_consented`] gives its reason, and a bad `desktop`, allowing no desktop pattern, and
    /// [`Config::allowed_desktop`] gives its reason, and a bad `hooks`, consenting to no hook routing, and
    /// [`Config::hooks_allowed`] gives its reason.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = strict_json(text.strip_prefix('\u{feff}').unwrap_or(text));
        // PowerShell's JSON reader gives no value at all for blank text, which is then no JSON object.
        if text.trim().is_empty() {
            return Err("not a JSON object".to_owned());
        }
        let value: Value = serde_json::from_str(&text).map_err(|error| format!("Conversion from JSON failed with error: {error}"))?;
        check_key_casing(&value)?;
        let Value::Object(data) = value else {
            return Err("not a JSON object".to_owned());
        };
        let host = read_host(&data)?;
        let slots = read_slots(&data)?;
        let repos = read_table(&data, "repos", true)?;
        let outputs = read_table(&data, "outputs", false)?;
        let inputs = read_table(&data, "inputs", false)?;
        let (builders, hosts_problem) = match read_builders(&data, &host) {
            Ok(builders) => (builders, None),
            Err(problem) => (vec![host], Some(problem)),
        };
        Ok(Self {
            builders,
            hosts_problem,
            slots,
            repos,
            outputs,
            inputs,
            siblings: read_siblings(&data),
            warmup: read_warmup_consent(&data),
            desktop: read_desktop_consent(&data),
            hooks: read_hooks_consent(&data),
        })
    }

    /// The `.gate/desktop.toml` patterns `desktop` allows `repo`'s commands to run on the builder's desktop, its repo
    /// name matched in any letter case: none when it does not name the repo. A declared pattern is allowed only when it
    /// is listed exactly, letter case included.
    ///
    /// # Errors
    ///
    /// Why `desktop` cannot be used: not an object of arrays of non-empty strings. Such a value allows no desktop
    /// pattern, and the rest of the file is read whatever it holds.
    pub fn allowed_desktop(&self, repo: &str) -> Result<&[String], &str> {
        match &self.desktop {
            Err(problem) => Err(problem),
            Ok(table) => Ok(find(table, repo).unwrap_or(&[])),
        }
    }

    /// Whether `warmup` lets `repo` run its warm-up, its repo name matched in any letter case: `false` when it does not
    /// name the repo or there is no such key.
    ///
    /// # Errors
    ///
    /// Why `warmup` cannot be used: not an array of non-empty strings. Such a value consents to no warm-up, and the rest
    /// of the file is read whatever it holds.
    pub fn warmup_consented(&self, repo: &str) -> Result<bool, &str> {
        match &self.warmup {
            Err(problem) => Err(problem),
            Ok(names) => Ok(names.iter().any(|name| eq_ignoring_case(name, repo))),
        }
    }

    /// Whether `hooks` lets a gate started inside a git hook in `repo` route, its repo name matched in any letter case:
    /// `false` when it does not name the repo or there is no such key.
    ///
    /// # Errors
    ///
    /// Why `hooks` cannot be used: not an array of non-empty strings. Such a value consents to no repo, and the rest of
    /// the file is read whatever it holds.
    pub fn hooks_allowed(&self, repo: &str) -> Result<bool, &str> {
        match &self.hooks {
            Err(problem) => Err(problem),
            Ok(names) => Ok(names.iter().any(|name| eq_ignoring_case(name, repo))),
        }
    }

    /// Whether `hooks` names any repo at all: `false` when the key is missing, the list is empty, or its value cannot
    /// be used ([`Config::hooks_allowed`]).
    #[must_use]
    pub fn hooks_name_a_repo(&self) -> bool {
        match &self.hooks {
            Ok(names) => !names.is_empty(),
            Err(_) => false,
        }
    }

    /// The sibling names `siblings` allows `repo` to carry, its repo name matched in any letter case: none when it does
    /// not name the repo.
    ///
    /// # Errors
    ///
    /// Why `siblings` cannot be used: not an object of arrays of folder names the builder can lay out
    /// ([`is_layout_name`]). Such a value allows no sibling, and the rest of the file is read whatever it holds.
    pub fn allowed_siblings(&self, repo: &str) -> Result<&[String], &str> {
        match &self.siblings {
            Err(problem) => Err(problem),
            Ok(table) => Ok(find(table, repo).unwrap_or(&[])),
        }
    }

    /// The builder's host alias, the one a routed run goes to.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.builders[0]
    }

    /// Every builder: `host`, then each `hosts` entry in file order, an entry equal to an earlier one in any letter case
    /// skipped.
    ///
    /// # Errors
    ///
    /// Why `hosts` cannot be used: not an array of strings, or an entry `host` would refuse. Routing reads the file
    /// whatever `hosts` holds.
    pub fn builders(&self) -> Result<&[String], &str> {
        match &self.hosts_problem {
            Some(problem) => Err(problem),
            None => Ok(&self.builders),
        }
    }

    /// The remote pool's size.
    #[must_use]
    pub fn slots(&self) -> u32 {
        self.slots
    }

    /// The patterns `repos` lists for `repo`, matched in any letter case, or `None` when it does not list it.
    #[must_use]
    pub fn patterns(&self, repo: &str) -> Option<&[Pattern]> {
        find(&self.repos, repo)
    }

    /// The output globs `outputs` lists for `repo`, or `None` when it does not name it.
    #[must_use]
    pub fn own_outputs(&self, repo: &str) -> Option<Vec<String>> {
        find(&self.outputs, repo).map(|globs| globs.iter().map(|glob| glob.as_str().to_owned()).collect())
    }

    /// The output globs of `repo`: its own `outputs` list, or [`DEFAULT_OUTPUTS`] when `outputs` does not name it.
    #[must_use]
    pub fn outputs(&self, repo: &str) -> Vec<String> {
        self.own_outputs(repo)
            .unwrap_or_else(|| DEFAULT_OUTPUTS.iter().map(|glob| (*glob).to_owned()).collect())
    }

    /// The input globs `inputs` lists for `repo`, matched in any letter case: none when it does not name it.
    #[must_use]
    pub fn inputs(&self, repo: &str) -> Vec<String> {
        find(&self.inputs, repo).map_or_else(Vec::new, |globs| globs.iter().map(|glob| glob.as_str().to_owned()).collect())
    }
}

/// The text as strict JSON: its `//` line comments, `/* */` block comments and trailing commas (a `,` followed only by
/// white space or comments before `}` or `]`) taken out, each only outside a string. A block comment leaves a space
/// and its line breaks, so the tokens around it stay apart and the parser's line numbers stay true; one never closed
/// is left for the parser to refuse.
fn strict_json(text: &str) -> String {
    drop_trailing_commas(&drop_comments(text))
}

/// Where a JSON text's strings are, one character at a time.
#[derive(Default)]
struct Strings {
    inside: bool,
    escaped: bool,
}

impl Strings {
    /// Whether `ch`, the next character, lies outside every string; a string's quotes count as inside it.
    fn outside(&mut self, ch: char) -> bool {
        if !self.inside {
            self.inside = ch == '"';
            return !self.inside;
        }
        if self.escaped {
            self.escaped = false;
        } else if ch == '\\' {
            self.escaped = true;
        } else if ch == '"' {
            self.inside = false;
        }
        false
    }
}

fn drop_comments(text: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut strings = Strings::default();
    let mut rest = text;
    while let Some(ch) = rest.chars().next() {
        if strings.outside(ch)
            && let Some(after) = skip_comment(rest, &mut kept)
        {
            rest = after;
            continue;
        }
        kept.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    kept
}

/// When `rest` starts with a comment, the text after it, with what stands for it pushed to `kept`: nothing for a line
/// comment, whose line break stays in the text, and a space and its line breaks for a closed block comment.
fn skip_comment<'a>(rest: &'a str, kept: &mut String) -> Option<&'a str> {
    if let Some(after) = rest.strip_prefix("//") {
        return Some(after.find('\n').map_or("", |end| &after[end..]));
    }
    let after = rest.strip_prefix("/*")?;
    let end = after.find("*/")?;
    kept.push(' ');
    kept.extend(after[..end].chars().filter(|ch| *ch == '\n'));
    Some(&after[end + 2..])
}

/// The text with each comma outside a string that only white space separates from a `}` or `]` made a space.
fn drop_trailing_commas(text: &str) -> String {
    let mut strings = Strings::default();
    text.char_indices()
        .map(|(index, ch)| {
            let outside = strings.outside(ch);
            let trailing = outside && ch == ',' && text[index + 1..].trim_start().starts_with(['}', ']']);
            if trailing { ' ' } else { ch }
        })
        .collect()
}

/// The list `table` holds for `repo`, its name matched in any letter case.
fn find<'a, T>(table: &'a [(String, Vec<T>)], repo: &str) -> Option<&'a [T]> {
    table
        .iter()
        .find(|(name, _)| eq_ignoring_case(name, repo))
        .map(|(_, items)| items.as_slice())
}

/// Refuses an object anywhere in the file holding two keys that differ only in letter case, as PowerShell's
/// `ConvertFrom-Json` does: its objects look keys up in any case.
fn check_key_casing(value: &Value) -> Result<(), String> {
    match value {
        Value::Object(object) => {
            let keys: Vec<&String> = object.keys().collect();
            for (index, key) in keys.iter().enumerate() {
                if let Some(existing) = keys[..index].iter().find(|earlier| eq_ignoring_case(earlier, key)) {
                    return Err(format!(
                        "Cannot convert the JSON string because it contains keys with different casing. Please use the \
                         -AsHashTable switch instead. The key that was attempted to be added to the existing key \
                         '{existing}' was '{key}'."
                    ));
                }
            }
            object.values().try_for_each(check_key_casing)
        }
        Value::Array(items) => items.iter().try_for_each(check_key_casing),
        _ => Ok(()),
    }
}

/// The value of an object's key, matched in any letter case.
fn property<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    object.iter().find(|(name, _)| eq_ignoring_case(name, key)).map(|(_, value)| value)
}

/// The `host`: an ssh alias or `user@host`, both parts of letters, digits, `.`, `_` and `-`, neither starting with
/// `-`. The host reaches ssh's argument list, a git `host:path` URL and `ssh_config` `%h` expansion, so only those
/// characters are safe in all three.
fn read_host(data: &Map<String, Value>) -> Result<String, String> {
    let value = property(data, "host").ok_or_else(|| "host is missing".to_owned())?;
    checked_host(value, &HOST_REFUSALS)
}

/// The builders: `host`, then each entry of the optional `hosts`, an array of strings each checked as `host` is, in
/// file order, an entry equal to an earlier builder in any letter case skipped.
fn read_builders(data: &Map<String, Value>, host: &str) -> Result<Vec<String>, String> {
    let mut builders = vec![host.to_owned()];
    let Some(value) = property(data, "hosts") else {
        return Ok(builders);
    };
    let Value::Array(entries) = value else {
        return Err("hosts must be an array of strings".to_owned());
    };
    for entry in entries {
        let entry = checked_host(entry, &HOSTS_REFUSALS)?;
        if !builders.iter().any(|earlier| eq_ignoring_case(earlier, &entry)) {
            builders.push(entry);
        }
    }
    Ok(builders)
}

/// The reasons a key holding a host gives for each kind of host it refuses.
struct HostRefusals {
    /// Not a string, or an empty one.
    empty: &'static str,
    /// One letter, which git reads as a drive.
    drive: &'static str,
    /// Not an ssh alias or `user@host` of letters, digits, `.`, `_` and `-`.
    characters: &'static str,
}

/// The reasons of `host`. A host that is not an ssh alias or `user@host` of letters, digits, `.`, `_` and `-` is
/// refused for a reason of the gate's own, since the PowerShell gate accepts such a host; one of one letter because
/// git reads `b:path` as a path on drive `B:`, not as `path` on the host.
const HOST_REFUSALS: HostRefusals = HostRefusals {
    empty: "host must be a non-empty string",
    drive: "host must not be a single letter, which a git host:path URL reads as a drive",
    characters: "host must be an ssh alias or user@host of letters, digits, '.', '_' and '-', not starting with '-'",
};

/// The reasons of a `hosts` entry, worded as those of `host`.
const HOSTS_REFUSALS: HostRefusals = HostRefusals {
    empty: "hosts entries must each be a non-empty string",
    drive: "hosts entries must not be a single letter, which a git host:path URL reads as a drive",
    characters: "hosts entries must each be an ssh alias or user@host of letters, digits, '.', '_' and '-', not starting with '-'",
};

/// Whether `host` is one a routing file's `host` may hold: an ssh alias or `user@host` of letters, digits, `.`, `_`
/// and `-`, not starting with `-`, and not one letter.
#[must_use]
pub fn is_usable_host(host: &str) -> bool {
    checked_host(&Value::String(host.to_owned()), &HOST_REFUSALS).is_ok()
}

/// `value` as a host, or the reason from `refusals` it is refused for.
fn checked_host(value: &Value, refusals: &HostRefusals) -> Result<String, String> {
    match value {
        Value::String(host) if is_drive_letter(host) => Err(refusals.drive.to_owned()),
        Value::String(host) if is_host(host) => Ok(host.clone()),
        Value::String(host) if !host.is_empty() => Err(refusals.characters.to_owned()),
        _ => Err(refusals.empty.to_owned()),
    }
}

/// Whether `host` is one ASCII letter, which a git `host:path` URL reads as a drive.
fn is_drive_letter(host: &str) -> bool {
    host.len() == 1 && host.bytes().all(|byte| byte.is_ascii_alphabetic())
}

/// Whether `host` is an ssh alias or `user@host`: at most one `@`, each part a [`host_part`].
fn is_host(host: &str) -> bool {
    match host.split_once('@') {
        Some((user, name)) => host_part(user) && host_part(name),
        None => host_part(host),
    }
}

/// Whether `part` is a non-empty run of ASCII letters, digits, `.`, `_` and `-` that does not start with `-`.
fn host_part(part: &str) -> bool {
    let mut chars = part.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first != '-' && host_part_char(first) && chars.all(host_part_char)
}

/// Whether `ch` may stand in a host part: an ASCII letter, a digit, `.`, `_` or `-`.
fn host_part_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-')
}

/// `slots`, 1 when missing: any JSON number that is a whole number 1 to 16, so `2` and `2.0` are both 2.
fn read_slots(data: &Map<String, Value>) -> Result<u32, String> {
    let Some(value) = property(data, "slots") else {
        return Ok(1);
    };
    value
        .as_f64()
        .and_then(|number| (1..=MAX_SLOTS).find(|count| f64::from(*count).total_cmp(&number).is_eq()))
        .ok_or_else(|| format!("slots must be a whole number 1 to {MAX_SLOTS}"))
}

/// The `repos`, `outputs` or `inputs` table: each repo name's pattern list, in file order; an empty table when the key is
/// missing and not `required`.
fn read_table(data: &Map<String, Value>, key: &str, required: bool) -> Result<RepoTable, String> {
    let Some(value) = property(data, key) else {
        return if required { Err(format!("{key} is missing")) } else { Ok(Vec::new()) };
    };
    let Value::Object(table) = value else {
        return Err(format!("{key} must be a JSON object"));
    };
    table.iter().map(|(name, list)| Ok((name.clone(), read_list(key, name, list)?))).collect()
}

/// The `siblings` table: each repo name's allowed sibling names, in file order, each a folder name the builder can lay
/// out ([`is_layout_name`]); an empty table when the key is missing. A problem writes the repo name quoted and escaped,
/// so no control character in it reaches the console.
fn read_siblings(data: &Map<String, Value>) -> Result<Vec<(String, Vec<String>)>, String> {
    let Some(value) = property(data, "siblings") else {
        return Ok(Vec::new());
    };
    let Value::Object(table) = value else {
        return Err("siblings must be a JSON object".to_owned());
    };
    table
        .iter()
        .map(|(repo, list)| {
            let Value::Array(items) = list else {
                return Err(format!("siblings.{repo:?} is not an array of strings"));
            };
            let names = items
                .iter()
                .enumerate()
                .map(|(index, item)| match item {
                    Value::String(name) if is_layout_name(name) => Ok(name.clone()),
                    Value::String(name) if !name.is_empty() => {
                        Err(format!("siblings.{repo:?}[{index}] is not a folder name the builder can lay out"))
                    }
                    _ => Err(format!("siblings.{repo:?}[{index}] is not a non-empty string")),
                })
                .collect::<Result<Vec<String>, String>>()?;
            Ok((repo.clone(), names))
        })
        .collect()
}

/// The `warmup` list: the repo names whose warm-up may run, in file order; none when the key is missing.
fn read_warmup_consent(data: &Map<String, Value>) -> Result<Vec<String>, String> {
    let Some(value) = property(data, "warmup") else {
        return Ok(Vec::new());
    };
    let Value::Array(items) = value else {
        return Err("warmup must be an array of repo names".to_owned());
    };
    items
        .iter()
        .enumerate()
        .map(|(index, item)| match item {
            Value::String(name) if !name.is_empty() => Ok(name.clone()),
            _ => Err(format!("warmup[{index}] is not a non-empty string")),
        })
        .collect()
}

/// The `hooks` list: the repo names whose gates started inside a git hook may route, in file order; none when the key
/// is missing.
fn read_hooks_consent(data: &Map<String, Value>) -> Result<Vec<String>, String> {
    let Some(value) = property(data, "hooks") else {
        return Ok(Vec::new());
    };
    let Value::Array(items) = value else {
        return Err("hooks must be an array of repo names".to_owned());
    };
    items
        .iter()
        .enumerate()
        .map(|(index, item)| match item {
            Value::String(name) if !name.is_empty() => Ok(name.clone()),
            _ => Err(format!("hooks[{index}] is not a non-empty string")),
        })
        .collect()
}

/// The `desktop` table: each repo name's allowed desktop patterns, in file order, each a non-empty string kept as
/// written; an empty table when the key is missing. A problem writes the repo name quoted and escaped, so no control
/// character in it reaches the console.
fn read_desktop_consent(data: &Map<String, Value>) -> Result<Vec<(String, Vec<String>)>, String> {
    let Some(value) = property(data, "desktop") else {
        return Ok(Vec::new());
    };
    let Value::Object(table) = value else {
        return Err("desktop must be a JSON object".to_owned());
    };
    table
        .iter()
        .map(|(repo, list)| {
            let Value::Array(items) = list else {
                return Err(format!("desktop.{repo:?} is not an array of strings"));
            };
            let patterns = items
                .iter()
                .enumerate()
                .map(|(index, item)| match item {
                    Value::String(pattern) if !pattern.is_empty() => Ok(pattern.clone()),
                    _ => Err(format!("desktop.{repo:?}[{index}] is not a non-empty string")),
                })
                .collect::<Result<Vec<String>, String>>()?;
            Ok((repo.clone(), patterns))
        })
        .collect()
}

fn read_list(key: &str, name: &str, list: &Value) -> Result<Vec<Pattern>, String> {
    let Value::Array(items) = list else {
        return Err(format!("{key}.{name} is not an array of strings"));
    };
    let mut patterns = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        patterns.push(read_pattern(key, item).map_err(|problem| format!("{key}.{name}[{index}] {problem}"))?);
    }
    Ok(patterns)
}

/// One item of a pattern list, or what is wrong with it: not a non-empty string, not a valid wildcard pattern, or, as
/// an output or input glob, holding `[` or `]`, which `-like` reads as a set and the glob matcher as the character
/// itself.
fn read_pattern(key: &str, item: &Value) -> Result<Pattern, &'static str> {
    let Some(text) = item.as_str().filter(|text| !text.is_empty()) else {
        return Err("is not a non-empty string");
    };
    let pattern = Pattern::parse(text).map_err(|_| "is not a valid wildcard pattern")?;
    if text.contains(['[', ']']) {
        match key {
            "outputs" => return Err("is not a valid output glob: '[' and ']' are not allowed"),
            "inputs" => return Err("is not a valid input glob: '[' and ']' are not allowed"),
            _ => {}
        }
    }
    Ok(pattern)
}
