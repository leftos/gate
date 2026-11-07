//! `gate remote`'s edit of the routing file's text: one repo's pattern list in one section gains or loses patterns, and
//! the whole file is written back in its own indent and line ending. The edit is refused, with nothing to write, when
//! the file is already unusable, holds comments or trailing commas a rewrite would drop, or when the result would be
//! refused by [`Config::parse`], changes anything but the one list, or adds a problem the router reports.

use std::fmt;

use serde_json::{Map, Value};

use crate::config::Config;
use crate::escape::escaped;

/// The sections `gate remote` edits: each an object of repo names to lists of strings.
pub const EDIT_SECTIONS: [&str; 9] = [
    "repos",
    "first",
    "outputs",
    "failureOutputs",
    "inputs",
    "desktop",
    "desktopSession",
    "siblings",
    "carriedFolders",
];

/// The sections a `builders` entry has, which `gate remote --builder` edits.
pub const BUILDER_SECTIONS: [&str; 4] = ["repos", "first", "outputs", "failureOutputs"];

/// The indent of a file with no indented line.
const DEFAULT_INDENT: &str = "    ";

/// What an edit does to the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditVerb {
    /// Appends each pattern the list does not hold.
    Add,
    /// Takes each pattern out of the list.
    Remove,
}

/// One edit: the list of `repo` in `section`, at the top level or in the `builders` entry whose `host` is `builder`.
#[derive(Clone, Copy, Debug)]
pub struct Edit<'a> {
    /// Add or remove.
    pub verb: EditVerb,
    /// One of [`EDIT_SECTIONS`], or of [`BUILDER_SECTIONS`] with a builder.
    pub section: &'a str,
    /// The `host` of the `builders` entry to edit, matched in any ASCII letter case; `None` edits the top level.
    pub builder: Option<&'a str>,
    /// The repo name, matched in any ASCII letter case; a key the file holds keeps its spelling.
    pub repo: &'a str,
    /// The patterns, each compared as an exact string.
    pub patterns: &'a [String],
}

/// What an edit came to: the file's new text, the original's when nothing changed, and one line per pattern saying what
/// happened to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edited {
    /// The file's new text, byte order mark included when the file had one; the original text when nothing changed.
    pub text: String,
    /// One `remote: <section>.<repo>: ...` line per pattern.
    pub lines: Vec<String>,
    /// Whether the list changed, so the file is to be written.
    pub changed: bool,
}

/// Why an edit was refused; the file is to be left as it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditRefusal {
    /// [`Config::parse`] refuses the file as it stands: its reason.
    Unusable(String),
    /// The file holds comments or trailing commas, which a rewrite would drop.
    Comments,
    /// A section `gate remote` does not edit, as given.
    UnknownSection(String),
    /// A section a `builders` entry does not have, as given.
    NotABuilderSection(String),
    /// A `--builder` no `builders` entry's `host` names, as given.
    UnknownBuilder(String),
    /// The edit or its result fails a guard: which, and why.
    Guard(String),
}

impl fmt::Display for EditRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EditRefusal::Unusable(reason) => f.write_str(reason),
            EditRefusal::Comments => f.write_str("the file holds comments or trailing commas, which a rewrite would drop; edit it by hand"),
            EditRefusal::UnknownSection(section) => write!(f, "unknown section {}; one of {}", escaped(section), EDIT_SECTIONS.join(", ")),
            EditRefusal::NotABuilderSection(section) => {
                write!(f, "a builder has no section {}; one of {}", escaped(section), BUILDER_SECTIONS.join(", "))
            }
            EditRefusal::UnknownBuilder(host) => write!(
                f,
                "{} is not a builders entry of remote.json; --builder edits builders[] entries only, and the top-level host's lists are \
                 edited without it",
                escaped(host)
            ),
            EditRefusal::Guard(why) => f.write_str(why),
        }
    }
}

/// Makes `edit` on the routing file's `text`.
///
/// # Errors
///
/// [`EditRefusal`]: a section outside [`EDIT_SECTIONS`] (or [`BUILDER_SECTIONS`] with a builder), a file
/// [`Config::parse`] refuses or that holds comments or trailing commas, an unknown builder, a section or list that is not
/// an object or an array, or a result that [`Config::parse`] refuses, that differs from the file outside the one list,
/// or that has a soft problem the file did not have.
pub fn edit_remote_json(text: &str, edit: &Edit<'_>) -> Result<Edited, EditRefusal> {
    check_section(edit)?;
    let before = Config::parse(text).map_err(EditRefusal::Unusable)?;
    let (bom, body) = text.strip_prefix('\u{feff}').map_or(("", text), |body| ("\u{feff}", body));
    let original: Value = serde_json::from_str(body).map_err(|_| EditRefusal::Comments)?;
    let mut edited = original.clone();
    let (lines, changed) = apply(&mut edited, edit)?;
    if !changed {
        return Ok(Edited {
            text: text.to_owned(),
            lines,
            changed,
        });
    }
    let eol = if body.contains("\r\n") { "\r\n" } else { "\n" };
    let mut new_text = bom.to_owned();
    render(
        &edited,
        &Layout {
            indent: &indent_of(body),
            eol,
        },
        0,
        &mut new_text,
    );
    new_text.push_str(eol);
    guard(&before, &original, &edited, &new_text[bom.len()..], edit)?;
    Ok(Edited {
        text: new_text,
        lines,
        changed,
    })
}

/// Refuses a section `gate remote` does not edit, or one a builder lacks when the edit names a builder.
fn check_section(edit: &Edit<'_>) -> Result<(), EditRefusal> {
    if !EDIT_SECTIONS.contains(&edit.section) {
        return Err(EditRefusal::UnknownSection(edit.section.to_owned()));
    }
    if edit.builder.is_some() && !BUILDER_SECTIONS.contains(&edit.section) {
        return Err(EditRefusal::NotABuilderSection(edit.section.to_owned()));
    }
    Ok(())
}

/// Makes the edit on the file's value: its lines, and whether the list changed.
fn apply(file: &mut Value, edit: &Edit<'_>) -> Result<(Vec<String>, bool), EditRefusal> {
    let (scope, prefix) = scope_of(file, edit)?;
    let label_of = |repo: &str| format!("{prefix}{}.{}", edit.section, escaped(repo));
    let Some(list) = list_of(scope, edit, &prefix)? else {
        let label = label_of(edit.repo);
        let lines = edit
            .patterns
            .iter()
            .map(|pattern| format!("remote: {label}: absent (no {})", escaped(pattern)))
            .collect();
        return Ok((lines, false));
    };
    let (repo, items) = list;
    let label = label_of(&repo);
    let mut changed = false;
    let mut lines = Vec::new();
    for pattern in edit.patterns {
        let held = items.iter().any(|item| item.as_str() == Some(pattern));
        let shown = escaped(pattern);
        let line = match (edit.verb, held) {
            (EditVerb::Add, true) => format!("unchanged (already has {shown})"),
            (EditVerb::Add, false) => {
                items.push(Value::String(pattern.clone()));
                format!("+ {shown}")
            }
            (EditVerb::Remove, true) => {
                items.retain(|item| item.as_str() != Some(pattern));
                format!("- {shown}")
            }
            (EditVerb::Remove, false) => format!("absent (no {shown})"),
        };
        changed |= matches!((edit.verb, held), (EditVerb::Add, false) | (EditVerb::Remove, true));
        lines.push(format!("remote: {label}: {line}"));
    }
    Ok((lines, changed))
}

/// The object the edit's section lives in, the top level or the named `builders` entry, and the prefix its lines carry.
fn scope_of<'v>(file: &'v mut Value, edit: &Edit<'_>) -> Result<(&'v mut Map<String, Value>, String), EditRefusal> {
    let not_an_object = || EditRefusal::Guard("the file is not a JSON object".to_owned());
    let top = file.as_object_mut().ok_or_else(not_an_object)?;
    let Some(host) = edit.builder else {
        return Ok((top, String::new()));
    };
    let unknown = || EditRefusal::UnknownBuilder(host.to_owned());
    let entries = key_of(top, "builders")
        .and_then(|key| top.get_mut(&key))
        .and_then(Value::as_array_mut)
        .ok_or_else(unknown)?;
    let (index, entry) = entries
        .iter_mut()
        .enumerate()
        .find(|(_, entry)| entry_host(entry).is_some_and(|named| named.eq_ignore_ascii_case(host)))
        .ok_or_else(unknown)?;
    let entry = entry.as_object_mut().ok_or_else(unknown)?;
    Ok((entry, format!("builders[{index}].")))
}

/// A `builders` entry's `host`, its key matched in any ASCII letter case.
fn entry_host(entry: &Value) -> Option<&str> {
    let object = entry.as_object()?;
    object
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("host"))
        .and_then(|(_, value)| value.as_str())
}

/// The key of `object` equal to `name` in any ASCII letter case, as the object spells it.
fn key_of(object: &Map<String, Value>, name: &str) -> Option<String> {
    object.keys().find(|key| key.eq_ignore_ascii_case(name)).cloned()
}

/// The repo's list in the section, its key as the file spells it: made, with the section, for an add; `None` for a
/// remove that finds no section or no repo.
fn list_of<'s>(scope: &'s mut Map<String, Value>, edit: &Edit<'_>, prefix: &str) -> Result<Option<(String, &'s mut Vec<Value>)>, EditRefusal> {
    let adding = edit.verb == EditVerb::Add;
    let section_key = match key_of(scope, edit.section) {
        Some(key) => key,
        None if adding => {
            scope.insert(edit.section.to_owned(), Value::Object(Map::new()));
            edit.section.to_owned()
        }
        None => return Ok(None),
    };
    let not_object = || EditRefusal::Guard(format!("{prefix}{} is not an object of lists; edit it by hand", edit.section));
    let section = scope.get_mut(&section_key).and_then(Value::as_object_mut).ok_or_else(not_object)?;
    let repo_key = match key_of(section, edit.repo) {
        Some(key) => key,
        None if adding => {
            section.insert(edit.repo.to_owned(), Value::Array(Vec::new()));
            edit.repo.to_owned()
        }
        None => return Ok(None),
    };
    let not_list = || EditRefusal::Guard(format!("{prefix}{}.{} is not a list; edit it by hand", edit.section, escaped(&repo_key)));
    let items = section.get_mut(&repo_key).and_then(Value::as_array_mut).ok_or_else(not_list)?;
    Ok(Some((repo_key, items)))
}

/// The guards a changed file's `new_text` (without its byte order mark) must pass before it is written.
fn guard(before: &Config, original: &Value, edited: &Value, new_text: &str, edit: &Edit<'_>) -> Result<(), EditRefusal> {
    let after = Config::parse(new_text).map_err(|reason| EditRefusal::Guard(format!("the result would be refused: {reason}")))?;
    let read_back: Value =
        serde_json::from_str(new_text).map_err(|error| EditRefusal::Guard(format!("the rewritten file does not read back: {error}")))?;
    if read_back != *edited {
        return Err(EditRefusal::Guard("the rewritten file does not read back as the edit".to_owned()));
    }
    if without_list(original, edit) != without_list(&read_back, edit) {
        return Err(EditRefusal::Guard("the result changes more than the one list".to_owned()));
    }
    let old = soft_problems(before);
    if let Some(problem) = soft_problems(&after).into_iter().find(|problem| !old.contains(problem)) {
        return Err(EditRefusal::Guard(format!("the result has a problem the file did not have: {problem}")));
    }
    Ok(())
}

/// The file's value with the edited list taken out, and its section too when that leaves it empty.
fn without_list(file: &Value, edit: &Edit<'_>) -> Value {
    let mut copy = file.clone();
    let Ok((scope, _)) = scope_of(&mut copy, edit) else {
        return copy;
    };
    if let Some(section_key) = key_of(scope, edit.section) {
        if let Some(section) = scope.get_mut(&section_key).and_then(Value::as_object_mut)
            && let Some(repo_key) = key_of(section, edit.repo)
        {
            section.shift_remove(&repo_key);
        }
        if scope.get(&section_key).and_then(Value::as_object).is_some_and(Map::is_empty) {
            scope.shift_remove(&section_key);
        }
    }
    copy
}

/// The problems the router reports while still using the file.
fn soft_problems(config: &Config) -> Vec<String> {
    let mut problems: Vec<String> = [
        config.builders().err(),
        config.allowed_siblings("").err(),
        config.carried_folders("").err(),
        config.user_tools("").err(),
        config.warmup_consented("").err(),
        config.allowed_desktop("").err(),
        config.allowed_desktop_session("").err(),
        config.hooks_allowed("").err(),
        config.commits_allowed("").err(),
        config.siblings_required("").err(),
        config.ssh().err(),
        config.send_build_allowed("").err(),
        config.declarations_folder("").err(),
    ]
    .into_iter()
    .flatten()
    .map(str::to_owned)
    .collect();
    problems.extend(config.cross_build_problems().into_iter().map(str::to_owned));
    problems
}

/// The leading spaces or tabs of the first indented line of `text`, else [`DEFAULT_INDENT`].
fn indent_of(text: &str) -> String {
    text.lines()
        .find_map(|line| {
            let lead: String = line.chars().take_while(|ch| *ch == ' ' || *ch == '\t').collect();
            (!lead.is_empty() && lead.len() < line.trim_end().len()).then_some(lead)
        })
        .unwrap_or_else(|| DEFAULT_INDENT.to_owned())
}

/// How the rewritten file is laid out.
struct Layout<'a> {
    indent: &'a str,
    eol: &'a str,
}

/// Writes `value` to `out` at `depth` as serde's pretty printer lays it out, in `layout`'s indent and line ending.
fn render(value: &Value, layout: &Layout<'_>, depth: usize, out: &mut String) {
    match value {
        Value::Array(items) if !items.is_empty() => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                open_item(index, layout, depth + 1, out);
                render(item, layout, depth + 1, out);
            }
            close(layout, depth, out);
            out.push(']');
        }
        Value::Object(object) if !object.is_empty() => {
            out.push('{');
            for (index, (key, item)) in object.iter().enumerate() {
                open_item(index, layout, depth + 1, out);
                out.push_str(&Value::String(key.clone()).to_string());
                out.push_str(": ");
                render(item, layout, depth + 1, out);
            }
            close(layout, depth, out);
            out.push('}');
        }
        scalar_or_empty => out.push_str(&scalar_or_empty.to_string()),
    }
}

/// Starts the item at `index` of an array or object on its own line at `depth`.
fn open_item(index: usize, layout: &Layout<'_>, depth: usize, out: &mut String) {
    if index > 0 {
        out.push(',');
    }
    out.push_str(layout.eol);
    out.push_str(&layout.indent.repeat(depth));
}

/// Ends an array's or object's items, putting its closing bracket on its own line at `depth`.
fn close(layout: &Layout<'_>, depth: usize, out: &mut String) {
    out.push_str(layout.eol);
    out.push_str(&layout.indent.repeat(depth));
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a fixture that does not read must fail loudly")]
mod tests {
    use super::soft_problems;
    use crate::config::Config;

    #[test]
    fn soft_problems_lists_an_unusable_user_tools_table() {
        let config = Config::parse(r#"{"host": "builder-1", "repos": {"g": ["x"]}, "userTools": {"g": [{"path": "a", "commands": ["x"]}]}}"#)
            .expect("routing reads the file whatever userTools holds");
        let problem = r#"userTools."g"[0].path is not a home-relative folder (~/...): "a""#;
        assert!(
            soft_problems(&config).iter().any(|listed| listed == problem),
            "{:?}",
            soft_problems(&config)
        );
    }
}
