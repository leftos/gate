//! The routing file, `remote.json`, read from its text: the builder's `host`, the optional `hosts` (further builders,
//! which `gate push` reaches and routing neither reads nor refuses), the remote pool's `slots` (a whole
//! number 1 to 16, 1 when missing), `repos` (each repo name's list of `-like` patterns for the commands that route) and
//! the optional `outputs` (each repo name's globs for the files a routed run brings back), the optional `inputs` (each
//! repo name's globs for the ignored files a routed run sends) and the optional `siblings` (each repo name's list of
//! the sibling folder names a routed run of it may carry) and the optional `warmup` (the repo names whose
//! `.gate/warmup.toml` may warm their builder work folders) and the optional `desktop` (each repo name's list of the
//! `.gate/desktop.toml` patterns whose commands may run on the builder's desktop) and the optional `hooks` (the repo
//! names whose gates started inside a git hook may route) and the optional `sendBuild` (the repo names whose routed
//! `dotnet test --no-build` may send the PC's build) and the optional `declarations` (each repo name's folder
//! holding the `.gate` files the repo has none of) and the optional `builders` (further builders a routed run may go
//! to, each with its own `host`, `os`, `root`, `slots`, `repos` and `outputs`). Keys match in any letter case and
//! unknown keys are ignored.
//! The text may hold `//` and `/* */` comments and trailing commas, as the
//! PowerShell gate's JSON reader allows; single-quoted strings are refused. A file that breaks a rule is unusable as a
//! whole, for the reason the PowerShell gate gives (or, for a host ssh or git would misread, a reason of its own),
//! which the caller says in [`ignored_line`].

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::key::{is_layout_char, is_layout_name};
use crate::like::{Pattern, eq_ignoring_case};

/// The output globs of a repo the `outputs` key does not name.
pub const DEFAULT_OUTPUTS: [&str; 2] = ["**/bin/**", "**/.godot/mono/temp/bin/**"];

/// The output globs of a cargo repo the `outputs` key does not name: the top-level files of each profile folder, not
/// `deps/`, `build/` or `incremental/`.
pub const CARGO_OUTPUTS: [&str; 3] = ["target/*/*.exe", "target/*/*.dll", "target/*/*.pdb"];

/// The most slots the remote pool may have.
const MAX_SLOTS: u32 = 16;

/// The operating system a builder runs, which sets how a routed run's paths are written for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuilderOs {
    /// A Windows builder: paths keep the separator they were written with.
    Windows,
    /// A macOS builder: every path the rewrite recognises is written with `/`.
    Macos,
}

/// A builder a routed run may go to: the top-level `host`, or a `builders` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Builder {
    /// The builder's ssh host alias, as the file spells it.
    pub alias: String,
    /// The builder's operating system; `host` is always Windows.
    pub os: BuilderOs,
    /// The builder's root as the file writes it; `None` for the Windows default.
    pub root: Option<String>,
    /// The builder's remote pool size.
    pub slots: u32,
    /// The first 8 lower-case hex digits of the SHA-256 of the lower-cased alias for a `builders` entry and for a `hosts`
    /// entry ([`Config::named_builder`]), so neither shares `host`'s remote pool or hash caches; `None` for `host`.
    pub id: Option<String>,
}

/// A `builders` entry: the builder, the repos it runs with their patterns, its own output globs, and whether a pick
/// may send a run to it; a not-routed entry is reached only by a run pinned to it, `gate push` and `gate check`.
#[derive(Debug, Clone)]
struct BuilderEntry {
    builder: Builder,
    repos: RepoTable,
    outputs: RepoTable,
    routed: bool,
}

/// A usable routing file.
#[derive(Debug, Clone)]
pub struct Config {
    /// `host`, then each `hosts` entry; never empty.
    builders: Vec<String>,
    /// Why `hosts` cannot be used, which routing ignores.
    hosts_problem: Option<String>,
    /// `host` as a builder, with the top-level `slots`.
    host: Builder,
    /// The `builders` entries, in file order.
    entries: Vec<BuilderEntry>,
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
    /// The repo names whose routed `dotnet test --no-build` may send the PC's build, or why `sendBuild` cannot be used,
    /// which lets none.
    send_build: Result<Vec<String>, String>,
    /// Each repo name's declarations folder, as written, or why `declarations` cannot be used, which names none.
    declarations: Result<Vec<(String, String)>, String>,
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
    /// A `builders` that is not an array of objects, or an entry with a bad `host`, `os`, `root`, `slots`, `repos` or
    /// `outputs`, or whose `host` repeats the top-level `host`, a `hosts` entry or an earlier entry's in any ASCII letter
    /// case, is refused
    /// with a reason of the gate's own naming the entry (`builders[0].root must be an absolute path for a macos
    /// builder`): it changes where commands go.
    ///
    /// A `host` that is not an ssh alias or `user@host` of letters, digits, `.`, `_` and `-` is refused with a reason of
    /// the gate's own, since the PowerShell gate accepts it. A bad `hosts` leaves the file usable for routing, and
    /// [`Config::builders`] gives its reason. A bad `siblings` leaves it usable too, allowing no sibling, and
    /// [`Config::allowed_siblings`] gives its reason. So does a bad `warmup`, consenting to no warm-up, and
    /// [`Config::warmup_consented`] gives its reason, and a bad `desktop`, allowing no desktop pattern, and
    /// [`Config::allowed_desktop`] gives its reason, and a bad `hooks`, consenting to no hook routing, and
    /// [`Config::hooks_allowed`] gives its reason, and a bad `sendBuild`, letting no repo send its build, and
    /// [`Config::send_build_allowed`] gives its reason, and a bad `declarations`, naming no folder, and
    /// [`Config::declarations_folder`] gives its reason.
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
        let slots = read_slots(&data, "")?;
        let repos = read_table(&data, "repos", true, "")?;
        let outputs = read_table(&data, "outputs", false, "")?;
        let inputs = read_table(&data, "inputs", false, "")?;
        let entries = read_builder_entries(&data, &host)?;
        let (builders, hosts_problem) = match read_builders(&data, &host) {
            Ok(builders) => (builders, None),
            Err(problem) => (vec![host.clone()], Some(problem)),
        };
        Ok(Self {
            builders,
            hosts_problem,
            host: Builder {
                alias: host,
                os: BuilderOs::Windows,
                root: None,
                slots,
                id: None,
            },
            entries,
            repos,
            outputs,
            inputs,
            siblings: read_siblings(&data),
            warmup: read_warmup_consent(&data),
            desktop: read_desktop_consent(&data),
            hooks: read_hooks_consent(&data),
            send_build: read_names(&data, "sendBuild"),
            declarations: read_declarations(&data),
        })
    }

    /// The folder `declarations` names for `repo`, as written, its repo name matched in any letter case: `None` when it
    /// does not name the repo or there is no such key. Whether the folder can be read is its reader's to judge.
    ///
    /// # Errors
    ///
    /// Why `declarations` cannot be used: not an object of non-empty strings. Such a value names no folder, and the
    /// rest of the file is read whatever it holds.
    pub fn declarations_folder(&self, repo: &str) -> Result<Option<&str>, &str> {
        match &self.declarations {
            Err(problem) => Err(problem),
            Ok(table) => Ok(table
                .iter()
                .find(|(name, _)| eq_ignoring_case(name, repo))
                .map(|(_, folder)| folder.as_str())),
        }
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

    /// Whether `sendBuild` lets a routed `dotnet test --no-build` of `repo` send the PC's build of its project, so the
    /// builder keeps the word, its repo name matched in any letter case: `false` when it does not name the repo or there
    /// is no such key, and the run's `--no-build` is dropped on the builder as before.
    ///
    /// # Errors
    ///
    /// Why `sendBuild` cannot be used: not an array of non-empty strings. Such a value lets no repo send, and the rest of
    /// the file is read whatever it holds.
    pub fn send_build_allowed(&self, repo: &str) -> Result<bool, &str> {
        match &self.send_build {
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
        self.host.slots
    }

    /// The `builders` entries' builders, in file order.
    pub fn builder_entries(&self) -> impl Iterator<Item = &Builder> {
        self.entries.iter().map(|entry| &entry.builder)
    }

    /// Whether `builder`, a `builders` entry as [`Config::builder_entries`] gives it, may be among the builders a pick
    /// sends a run to: `false` for an entry the file marks `"routed": false`, which only a run pinned to it, `gate push`
    /// and `gate check` reach. It is `false` too for `host` and for a `hosts` entry, which are no entry of this list.
    #[must_use]
    pub fn builder_routed(&self, builder: &Builder) -> bool {
        self.entries.iter().any(|entry| entry.routed && entry.builder == *builder)
    }

    /// The builder whose alias is `alias` in any ASCII letter case: `host`, else a `builders` entry; `None` for any
    /// other alias, a `hosts` entry among them.
    #[must_use]
    pub fn builder(&self, alias: &str) -> Option<&Builder> {
        std::iter::once(&self.host)
            .chain(self.builder_entries())
            .find(|builder| builder.alias.eq_ignore_ascii_case(alias))
    }

    /// The builder the file lists as `host` in any ASCII letter case, its alias as the file spells it: `host`, a `hosts`
    /// entry, or a `builders` entry, routed or not. A `hosts` entry that is no `builders` entry is a Windows builder with
    /// the default root and the top-level slots, as `host` is, and an id of its own as a `builders` entry has, so its
    /// remote pool and its hash caches are not `host`'s. `host` and the `builders` entries are looked at first, so an
    /// unusable `hosts` value refuses only a name found nowhere else. `Ok(None)` for a host the file does not list.
    /// What `gate run --builder` and `gate check --builder` both name a builder by.
    ///
    /// # Errors
    ///
    /// Why `hosts` cannot be used ([`Config::builders`]), for a host that is neither `host` nor a `builders` entry.
    pub fn named_builder(&self, host: &str) -> Result<Option<Builder>, &str> {
        if let Some(builder) = self.builder(host) {
            return Ok(Some(builder.clone()));
        }
        let in_hosts = self.builders()?.iter().find(|alias| alias.eq_ignore_ascii_case(host));
        Ok(in_hosts.map(|alias| Builder {
            alias: alias.clone(),
            os: BuilderOs::Windows,
            root: None,
            slots: self.slots(),
            id: Some(builder_id(alias)),
        }))
    }

    /// The patterns `builder`'s own listing gives `repo`: its `builders` entry's `repos`, routed or not, or the top-level
    /// `repos` for `host`; `None` when that listing does not name the repo, and for a `hosts` entry, which lists none.
    pub(crate) fn own_patterns(&self, builder: &Builder, repo: &str) -> Option<&[Pattern]> {
        match self.entries.iter().find(|entry| entry.builder == *builder) {
            Some(entry) => find(&entry.repos, repo),
            None if *builder == self.host => self.patterns(repo),
            None => None,
        }
    }

    /// `host` as a builder.
    pub(crate) fn host_builder(&self) -> &Builder {
        &self.host
    }

    /// The builders that may run `repo`'s commands, in the order a command's pick tries them, each with the patterns
    /// listed for the repo: each routed `builders` entry whose `repos` lists it, in file order, then `host` when `repos`
    /// does. An entry marked `"routed": false` is never among them.
    pub(crate) fn routes(&self, repo: &str) -> Vec<(&Builder, &[Pattern])> {
        let entries = self
            .entries
            .iter()
            .filter(|entry| entry.routed)
            .filter_map(|entry| find(&entry.repos, repo).map(|patterns| (&entry.builder, patterns)));
        entries.chain(self.patterns(repo).map(|patterns| (&self.host, patterns))).collect()
    }

    /// Whether any builder may run `repo`'s commands: `repos` or a `builders` entry's `repos` lists it, its name matched
    /// in any letter case.
    #[must_use]
    pub fn is_routed(&self, repo: &str) -> bool {
        !self.routes(repo).is_empty()
    }

    /// Whether any listing names `repo`, matched in any letter case: the top-level `repos`, or a `builders` entry's
    /// `repos`, routed or not. A repo only a not-routed entry lists is reached only by a run pinned to that entry.
    #[must_use]
    pub fn lists(&self, repo: &str) -> bool {
        self.patterns(repo).is_some() || self.entries.iter().any(|entry| find(&entry.repos, repo).is_some())
    }

    /// The output globs listed for `repo` on `builder`: the entry's own `outputs` for a `builders` entry, the top-level
    /// `outputs` for `host` and a `hosts` entry; `None` when that list does not name the repo.
    pub(crate) fn own_outputs_on(&self, builder: &Builder, repo: &str) -> Option<Vec<String>> {
        match self.entries.iter().find(|entry| entry.builder == *builder) {
            Some(entry) => find(&entry.outputs, repo).map(globs_text),
            None => self.own_outputs(repo),
        }
    }

    /// The patterns `repos` lists for `repo`, matched in any letter case, or `None` when it does not list it.
    #[must_use]
    pub fn patterns(&self, repo: &str) -> Option<&[Pattern]> {
        find(&self.repos, repo)
    }

    /// The output globs `outputs` lists for `repo`, or `None` when it does not name it.
    #[must_use]
    pub fn own_outputs(&self, repo: &str) -> Option<Vec<String>> {
        find(&self.outputs, repo).map(globs_text)
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

/// The globs' texts.
fn globs_text(globs: &[Pattern]) -> Vec<String> {
    globs.iter().map(|glob| glob.as_str().to_owned()).collect()
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

/// `slots`, 1 when missing: any JSON number that is a whole number 1 to 16, so `2` and `2.0` are both 2. A reason
/// starts with `prefix`, which names the object holding the key (`builders[0].`), empty at the top level.
fn read_slots(data: &Map<String, Value>, prefix: &str) -> Result<u32, String> {
    let Some(value) = property(data, "slots") else {
        return Ok(1);
    };
    value
        .as_f64()
        .and_then(|number| (1..=MAX_SLOTS).find(|count| f64::from(*count).total_cmp(&number).is_eq()))
        .ok_or_else(|| format!("{prefix}slots must be a whole number 1 to {MAX_SLOTS}"))
}

/// The `repos`, `outputs` or `inputs` table: each repo name's pattern list, in file order; an empty table when the key is
/// missing and not `required`. A reason starts with `prefix`, which names the object holding the key
/// (`builders[0].`), empty at the top level.
fn read_table(data: &Map<String, Value>, key: &str, required: bool, prefix: &str) -> Result<RepoTable, String> {
    let Some(value) = property(data, key) else {
        return if required {
            Err(format!("{prefix}{key} is missing"))
        } else {
            Ok(Vec::new())
        };
    };
    let Value::Object(table) = value else {
        return Err(format!("{prefix}{key} must be a JSON object"));
    };
    table
        .iter()
        .map(|(name, list)| Ok((name.clone(), read_list(prefix, key, name, list)?)))
        .collect()
}

/// The `builders` entries, in file order; none when the key is missing. Any problem refuses the file.
fn read_builder_entries(data: &Map<String, Value>, host: &str) -> Result<Vec<BuilderEntry>, String> {
    let Some(value) = property(data, "builders") else {
        return Ok(Vec::new());
    };
    let Value::Array(items) = value else {
        return Err("builders must be an array of objects".to_owned());
    };
    let hosts: Vec<(usize, &str)> = match property(data, "hosts") {
        Some(Value::Array(hosts)) => hosts
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| entry.as_str().map(|entry| (index, entry)))
            .collect(),
        _ => Vec::new(),
    };
    let mut entries: Vec<BuilderEntry> = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let prefix = format!("builders[{index}].");
        let Value::Object(object) = item else {
            return Err(format!("builders[{index}] must be a JSON object"));
        };
        let entry = read_builder_entry(object, &prefix)?;
        if let Some(named) = named_already(&entry.builder.alias, host, &hosts, &entries) {
            return Err(format!("{prefix}host repeats {named}"));
        }
        entries.push(entry);
    }
    Ok(entries)
}

/// Where the file already names the builder `alias`, in any ASCII letter case: `host`, a string entry of `hosts`
/// (`hosts[M]`, `hosts` holding each string entry with its index) or an earlier `builders` entry (`builders[M].host`).
fn named_already(alias: &str, host: &str, hosts: &[(usize, &str)], entries: &[BuilderEntry]) -> Option<String> {
    if alias.eq_ignore_ascii_case(host) {
        return Some("host".to_owned());
    }
    if let Some((index, _)) = hosts.iter().find(|(_, entry)| entry.eq_ignore_ascii_case(alias)) {
        return Some(format!("hosts[{index}]"));
    }
    entries
        .iter()
        .position(|earlier| earlier.builder.alias.eq_ignore_ascii_case(alias))
        .map(|earlier| format!("builders[{earlier}].host"))
}

/// One `builders` entry, its reasons starting with `prefix` (`builders[0].`): `host` checked as the top-level one is,
/// `os` `windows` or `macos` in any ASCII letter case, `root` ([`read_builder_root`]), `slots` as the top-level one,
/// `routed` `true` (the default) or `false`, `repos` an object naming at least one repo, which a not-routed entry may
/// leave empty or missing, and `outputs` as the top-level one.
fn read_builder_entry(object: &Map<String, Value>, prefix: &str) -> Result<BuilderEntry, String> {
    let host = property(object, "host").ok_or_else(|| format!("{prefix}host is missing"))?;
    let alias = checked_host(host, &HOST_REFUSALS).map_err(|reason| format!("{prefix}{reason}"))?;
    let os = match property(object, "os") {
        None => return Err(format!("{prefix}os is missing")),
        Some(Value::String(os)) if os.eq_ignore_ascii_case("windows") => BuilderOs::Windows,
        Some(Value::String(os)) if os.eq_ignore_ascii_case("macos") => BuilderOs::Macos,
        Some(_) => return Err(format!(r#"{prefix}os must be "windows" or "macos""#)),
    };
    let root = read_builder_root(object, os, prefix)?;
    let slots = read_slots(object, prefix)?;
    let routed = match property(object, "routed") {
        None => true,
        Some(Value::Bool(routed)) => *routed,
        Some(_) => return Err(format!("{prefix}routed must be true or false")),
    };
    let repos = read_table(object, "repos", routed, prefix)?;
    if routed && repos.is_empty() {
        return Err(format!("{prefix}repos must name at least one repo"));
    }
    let outputs = read_table(object, "outputs", false, prefix)?;
    let id = Some(builder_id(&alias));
    Ok(BuilderEntry {
        builder: Builder { alias, os, root, slots, id },
        repos,
        outputs,
        routed,
    })
}

/// A `builders` entry's `root`. A macOS builder needs one: `/`, then names separated by single `/`, none empty, `.` or
/// `..`, each of ASCII letters, digits, `.`, `_` and `-` only. A Windows builder's is `None` when missing, the default
/// root, else an absolute drive path the wire can carry as written: a letter, `:`, `\`, then names separated by single
/// `\`, each a name the builder can lay out ([`is_layout_name`]): ASCII letters, digits, `.`, `_` and `-`, neither
/// starting nor ending with `.`, and no reserved device name.
fn read_builder_root(object: &Map<String, Value>, os: BuilderOs, prefix: &str) -> Result<Option<String>, String> {
    let value = property(object, "root");
    match os {
        BuilderOs::Macos => match value {
            Some(Value::String(root)) if is_unix_root(root) => Ok(Some(root.clone())),
            _ => Err(format!("{prefix}root must be an absolute path for a macos builder")),
        },
        BuilderOs::Windows => match value {
            None => Ok(None),
            Some(Value::String(root)) if is_drive_root(root) => Ok(Some(root.clone())),
            Some(_) => Err(format!("{prefix}root must be an absolute drive path for a windows builder")),
        },
    }
}

/// Whether `root` is an absolute macOS path a builder root may be ([`read_builder_root`]): `/`, then names separated by
/// single `/`, none empty, `.` or `..`, each of ASCII letters, digits, `.`, `_` and `-` only, so no shell reads any of
/// it as more than one plain word.
#[must_use]
pub fn is_unix_root(root: &str) -> bool {
    root.strip_prefix('/')
        .is_some_and(|rest| rest.split('/').all(|name| root_name(name) && name.chars().all(is_layout_char)))
}

/// Whether `root` is an absolute drive path a Windows builder root may be ([`read_builder_root`]).
fn is_drive_root(root: &str) -> bool {
    let bytes = root.as_bytes();
    let drive = bytes.len() > 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\';
    drive && root[3..].split('\\').all(is_layout_name)
}

/// Whether `name`, one name of a builder root, is neither empty, `.` nor `..` and holds no control character.
fn root_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.chars().any(char::is_control)
}

/// A `builders` entry's id: the first 8 lower-case hex digits of the SHA-256 of `alias` in ASCII lower case.
///
/// A `hosts` entry's id is the same function over its alias, and the gate names that entry's remote slots, its claims
/// and its cache folders by it; `host` has none.
#[must_use]
pub fn builder_id(alias: &str) -> String {
    let digest = Sha256::digest(alias.to_ascii_lowercase().as_bytes());
    let mut id = String::with_capacity(8);
    id.extend(digest.iter().take(4).map(|byte| format!("{byte:02x}")));
    id
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
    read_names(data, "hooks")
}

/// The list of repo names at `key`, in file order; none when the key is missing. A value that is no array, or an entry
/// that is no non-empty string, is why the list cannot be used.
fn read_names(data: &Map<String, Value>, key: &str) -> Result<Vec<String>, String> {
    let Some(value) = property(data, key) else {
        return Ok(Vec::new());
    };
    let Value::Array(items) = value else {
        return Err(format!("{key} must be an array of repo names"));
    };
    items
        .iter()
        .enumerate()
        .map(|(index, item)| match item {
            Value::String(name) if !name.is_empty() => Ok(name.clone()),
            _ => Err(format!("{key}[{index}] is not a non-empty string")),
        })
        .collect()
}

/// The `declarations` table: each repo name's declarations folder, in file order, a non-empty string kept as written;
/// an empty table when the key is missing. A problem writes the repo name quoted and escaped, so no control character
/// in it reaches the console.
fn read_declarations(data: &Map<String, Value>) -> Result<Vec<(String, String)>, String> {
    let Some(value) = property(data, "declarations") else {
        return Ok(Vec::new());
    };
    let Value::Object(table) = value else {
        return Err("declarations must be a JSON object".to_owned());
    };
    table
        .iter()
        .map(|(repo, folder)| match folder {
            Value::String(folder) if !folder.is_empty() => Ok((repo.clone(), folder.clone())),
            _ => Err(format!("declarations.{repo:?} is not a non-empty string")),
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

/// The pattern list of the repo `name` in the table `key`, a reason starting with `prefix` ([`read_table`]).
fn read_list(prefix: &str, key: &str, name: &str, list: &Value) -> Result<Vec<Pattern>, String> {
    let Value::Array(items) = list else {
        return Err(format!("{prefix}{key}.{name} is not an array of strings"));
    };
    let mut patterns = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        patterns.push(read_pattern(key, item).map_err(|problem| format!("{prefix}{key}.{name}[{index}] {problem}"))?);
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
