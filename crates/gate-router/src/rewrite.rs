//! The in-worktree path rewrite: a routed command's words as the builder runs them in the gate's folder there.
//! Every path in a word that lies inside the worktree or a declared sibling beside it is made relative to the gate's
//! folder; any other path, a UNC path or a relative path climbing above the root other than into a sibling marks the
//! word as outside the worktree.
//!
//! A path starts at a drive letter, a colon and a separator not preceded by a letter, digit, `_` or `-`, anywhere in
//! the word (`--flag=X:\a`, `-p:Name=X:\a`, `"X:\a;X:\b"`, a script's text), each such start a path of its own, judged
//! on its own. It runs to the first of `, ; " ' ) | & < >`. In a word that holds one of those or a line break (a
//! script's text or a list) white space ends it too; in any other word, one argument, a path runs through its spaces.

use crate::like::{eq_ignoring_case, lower_char};

/// Characters that end a path in a word.
const PATH_ENDS: [char; 9] = [',', ';', '"', '\'', ')', '|', '&', '<', '>'];

/// A command's words rewritten for the builder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rewritten {
    /// Each word with its in-worktree paths made relative to the gate's folder; outside paths stay as written.
    pub words: Vec<String>,
    /// The first word, as given, that names a path outside the worktree.
    pub outside: Option<String>,
}

/// A declared sibling of the worktree: its folder name as declared and its root beside the worktree root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiblingRoot {
    /// The folder name as `.gate/siblings.toml` declares it.
    pub name: String,
    /// The sibling's root: the worktree root's parent and the name, joined with the root's separator.
    pub root: String,
}

/// What the rewrite reads of the gate's place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RewritePlace<'a> {
    /// The worktree root as `git rev-parse --show-toplevel` gives it.
    pub root: &'a str,
    /// The gate's folder below the root as `git rev-parse --show-prefix` gives it: `sub/dir/`, empty at the root.
    pub prefix: &'a str,
    /// The gate's folder as the gate spells it, which may be on a drive or a share standing for a folder of the worktree.
    pub folder: &'a str,
    /// The root's last segment, its folder name.
    pub leaf: &'a str,
}

/// `words` as the builder runs them in the gate's folder. `place` is the gate's place ([`RewritePlace`]); `drives` is
/// the subst table: a drive letter for the path its device stands for, and `siblings` the declared siblings' roots. A
/// drive whose path is the root or a separator-bounded prefix of it gives the root another spelling on that letter;
/// any other adds none. The root compares in any letter case and with either
/// separator; a path naming the gate's folder becomes `.`, any other loses the root and the separators after it,
/// gaining one `..` a level of the gate's folder (written with `\` when the path held one, else `/`).
///
/// A path inside a sibling's root, spelled any way the root can be, becomes `..` a level of the gate's folder and one
/// more, then the sibling's declared name and the rest of the path, with the same separator rule; a relative path whose
/// `..` climb exactly to the root's parent and then enters a sibling is inside it. Leaving the sibling again, naming
/// the parent itself or entering any other folder of it is outside.
///
/// An absolute path whose `..` climb out of the root it starts below, the worktree's or a sibling's, and land below
/// the worktree root (entered from the parent by the root's last segment) or a sibling's root is sent as where it
/// lands: `.` for the gate's folder, else `..` a level of the gate's folder, one more for a sibling, then the sibling's
/// declared name and the segments below, a separator ending the path kept. Landing anywhere else is outside, and so is
/// any climb out of a root spelled as a bare subst drive that stands for it (`S:\..`): Windows stops `..` there.
///
/// For the same reason a relative path is outside when its `..` climb past the root of the drive or share the gate's
/// folder is spelled on, when that root is the worktree root or a folder below it: the folder's spelling sits no more
/// folders below its drive or share root than the folder sits below the worktree root, as many included. From `S:\` standing for the
/// worktree root, `..\b` names `S:\b`, no sibling.
#[must_use]
pub fn rewrite_words(place: &RewritePlace<'_>, words: &[String], drives: &[(char, String)], siblings: &[SiblingRoot]) -> Rewritten {
    let cwd: Vec<&str> = place.prefix.split(['\\', '/']).filter(|segment| !segment.is_empty()).collect();
    let bases = Bases {
        root: root_spellings(place.root, drives),
        leaf: place.leaf,
        siblings: siblings
            .iter()
            .map(|sibling| (sibling.name.as_str(), root_spellings(&sibling.root, drives)))
            .collect(),
        floor: drive_floor(place.folder, cwd.len()),
    };
    let mut outside = None;
    let mut rewritten = Vec::with_capacity(words.len());
    for word in words {
        let (text, is_outside) = rewrite_word(&bases, &cwd, word);
        rewritten.push(text);
        if is_outside && outside.is_none() {
            outside = Some(word.clone());
        }
    }
    Rewritten { words: rewritten, outside }
}

/// The drive letters, in upper case and each once, that a path in `words` starts on, other than the root's own: the
/// letters whose subst target the caller looks up.
pub(crate) fn drive_letters(root: &str, words: &[String]) -> Vec<char> {
    let own = root_letter(root);
    let used = words.iter().flat_map(|word| {
        drive_starts(word)
            .into_iter()
            .map(|index| char::from(word.as_bytes()[index]).to_ascii_uppercase())
    });
    let mut letters = Vec::new();
    for letter in used {
        if Some(letter) != own && !letters.contains(&letter) {
            letters.push(letter);
        }
    }
    letters
}

fn root_letter(root: &str) -> Option<char> {
    let mut characters = root.chars();
    let letter = characters.next()?;
    (letter.is_ascii_alphabetic() && characters.next() == Some(':')).then(|| letter.to_ascii_uppercase())
}

/// One spelling of a root, as words are compared with it.
struct Spelling {
    /// The spelling, with `/` separators and no trailing one.
    text: String,
    /// Whether the spelling is a bare drive (`S:`), the root of a drive that stands for the root itself: Windows stops
    /// `..` there, so a path on it never climbs out of the root.
    drive_root: bool,
}

/// The root as words are compared with it: `/` separators and no trailing one, then its spelling on each subst drive
/// whose path is the root or a separator-bounded prefix of it; each marked as a drive root when it is a bare drive.
fn root_spellings(root: &str, drives: &[(char, String)]) -> Vec<Spelling> {
    let base = root.replace('\\', "/").trim_end_matches('/').to_owned();
    let mut spellings = vec![Spelling {
        drive_root: base.len() == 2 && root_letter(&base).is_some(),
        text: base.clone(),
    }];
    for (letter, target) in drives {
        let target = target.replace('\\', "/");
        let target = target.trim_end_matches('/');
        if target.is_empty() {
            continue;
        }
        if let Some(rest) = after_prefix(&base, target, |_| false) {
            spellings.push(Spelling {
                text: format!("{letter}:{rest}"),
                drive_root: rest.is_empty(),
            });
        }
    }
    spellings
}

/// What follows `prefix`, a path with `/` separators, at the start of `text`, compared in any letter case with `\` in
/// `text` read as `/`, when it is bounded there: nothing follows, a separator does, or `ends` says what follows ends
/// the path. So `X:/a/wt` and `X:/a/w.old` do not start with `X:/a/w`.
fn after_prefix<'a>(text: &'a str, prefix: &str, ends: impl Fn(&'a str) -> bool) -> Option<&'a str> {
    let mut rest = text.chars();
    for expected in prefix.chars() {
        let actual = rest.next()?;
        let actual = if actual == '\\' { '/' } else { actual };
        if lower_char(actual) != lower_char(expected) {
            return None;
        }
    }
    let rest = rest.as_str();
    let bounded = rest.is_empty() || rest.starts_with(['\\', '/']) || ends(rest);
    bounded.then_some(rest)
}

/// How many folders below the worktree root the root of the drive or share the gate's `folder` is spelled on sits, the
/// folder being `depth` folders below the worktree root: Windows stops `..` there. `None` when that drive or share root
/// is above the worktree root, and when the spelling names no drive or share.
fn drive_floor(folder: &str, depth: usize) -> Option<usize> {
    depth.checked_sub(depth_below_drive_root(folder)?)
}

/// How many folders below the root of its drive (`X:\`, `\\?\X:\`) or share (`\\host\share\`, `\\?\UNC\host\share\`)
/// the absolute `folder` is, read from its spelling with either separator; `None` for a spelling that names neither.
fn depth_below_drive_root(folder: &str) -> Option<usize> {
    let folder = folder.replace('/', "\\");
    let below = match folder.strip_prefix(r"\\?\").or_else(|| folder.strip_prefix(r"\\.\")) {
        Some(device) if device.split('\\').next().is_some_and(|first| first.eq_ignore_ascii_case("UNC")) => after_segments(device, 3)?,
        Some(device) => after_segments(device, 1)?,
        None if folder.starts_with(r"\\") => after_segments(&folder[2..], 2)?,
        None if root_letter(&folder).is_some() => &folder[2..],
        None => return None,
    };
    Some(below.split('\\').filter(|segment| !segment.is_empty() && *segment != ".").count())
}

/// What follows the first `count` segments of `path`, each separated by one `\` and none empty; `None` when it has
/// fewer.
fn after_segments(path: &str, count: usize) -> Option<&str> {
    let mut parts = path.splitn(count + 1, '\\');
    for _ in 0..count {
        if parts.next()?.is_empty() {
            return None;
        }
    }
    Some(parts.next().unwrap_or(""))
}

/// The root's parent, the separator before its last segment and that segment, trailing separators aside; a root with
/// no separator is all last segment.
pub(crate) fn split_root(root: &str) -> (&str, char, &str) {
    let root = root.trim_end_matches(['\\', '/']);
    match root.rfind(['\\', '/']) {
        Some(at) => (&root[..at], char::from(root.as_bytes()[at]), &root[at + 1..]),
        None => ("", '/', root),
    }
}

/// What a word's paths are judged against: the root's spellings ([`root_spellings`]) and its last segment, each
/// sibling's declared name with its root's, and how many folders below the root a relative path's `..` stop
/// ([`drive_floor`]).
struct Bases<'s> {
    root: Vec<Spelling>,
    leaf: &'s str,
    siblings: Vec<(&'s str, Vec<Spelling>)>,
    floor: Option<usize>,
}

impl<'s> Bases<'s> {
    /// The declared name of the sibling `segment` names, ASCII letter case aside.
    fn sibling_named(&self, segment: &str) -> Option<&'s str> {
        self.siblings
            .iter()
            .map(|(name, _)| *name)
            .find(|name| name.eq_ignore_ascii_case(segment))
    }
}

/// One word with its in-worktree and in-sibling paths rewritten, and whether it names a path outside them.
fn rewrite_word(bases: &Bases<'_>, cwd: &[&str], word: &str) -> (String, bool) {
    if word.starts_with(r"\\") || word.starts_with("//") {
        return (word.to_owned(), true);
    }
    let starts = drive_starts(word);
    if starts.is_empty() {
        return (word.to_owned(), relative_escape(cwd, bases, word));
    }
    let script = word.contains(PATH_ENDS) || word.contains(['\n', '\r']);
    let mut text = String::with_capacity(word.len());
    let mut outside = false;
    let mut at = 0;
    for index in starts {
        match path_edit(bases, cwd, word, index, script) {
            None => outside = true,
            Some(edit) if index >= at => {
                text.push_str(&word[at..index]);
                text.push_str(&edit.text);
                at = edit.end;
            }
            Some(_) => {}
        }
    }
    text.push_str(&word[at..]);
    (text, outside)
}

/// The byte indexes in `word` where a drive path starts: an ASCII letter, `:` and `\` or `/`, not preceded by a
/// letter, digit, `_` or `-`.
fn drive_starts(word: &str) -> Vec<usize> {
    let bytes = word.as_bytes();
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 2 < bytes.len() {
        let drive = bytes[index].is_ascii_alphabetic() && bytes[index + 1] == b':' && matches!(bytes[index + 2], b'\\' | b'/');
        if drive && !follows_word_character(word, index) {
            starts.push(index);
            index += 3;
        } else {
            index += 1;
        }
    }
    starts
}

fn follows_word_character(word: &str, index: usize) -> bool {
    word[..index]
        .chars()
        .next_back()
        .is_some_and(|before| before.is_alphanumeric() || before == '_' || before == '-')
}

/// The span `index..end` of a word replaced by `text`.
struct Edit {
    end: usize,
    text: String,
}

/// How the path at `index` is sent when it lies inside the worktree or a sibling in one of their roots' spellings;
/// `None` when it lies outside in every one, its `..` climbing out included, and when it climbs out of a root spelled
/// as a drive root ([`Spelling::drive_root`]).
fn path_edit(bases: &Bases<'_>, cwd: &[&str], word: &str, index: usize, script: bool) -> Option<Edit> {
    let (spelling, after, sibling) = match spelled_at(&bases.root, word, index, script) {
        Some((spelling, after)) => (spelling, after, None),
        None => bases
            .siblings
            .iter()
            .find_map(|(name, spellings)| spelled_at(spellings, word, index, script).map(|(spelling, after)| (spelling, after, Some(*name))))?,
    };
    let span = Span::new(word, index, after, script);
    let direct = match sibling {
        None => worktree_edit(cwd, word, span),
        Some(name) => sibling_edit(cwd, word, span, name),
    };
    if direct.is_some() || spelling.drive_root {
        return direct;
    }
    let start = sibling.map_or_else(|| Landing::Worktree(Vec::new()), |name| Landing::Sibling(vec![name]));
    climb_edit(bases, cwd, word, span, start)
}

/// The first of the root spellings `spellings` that the path at `index` in `word` starts with ([`root_end`]), and the
/// byte index just past it.
fn spelled_at<'b>(spellings: &'b [Spelling], word: &str, index: usize, script: bool) -> Option<(&'b Spelling, usize)> {
    spellings
        .iter()
        .find_map(|spelling| root_end(&spelling.text, word, index, script).map(|after| (spelling, after)))
}

/// How the path `span` of `word` is sent when it lies below the root of the sibling `name`: `..` a level of the gate's
/// folder and one more, then the name in place of the root's spelling, the rest as written; `None` when its `..` climb
/// out of the sibling.
fn sibling_edit(cwd: &[&str], word: &str, span: Span, name: &str) -> Option<Edit> {
    resolve(&[], &word[span.after..span.end])?;
    let separator = separator_of(&word[span.index..span.end]);
    let mut text = vec![".."; cwd.len() + 1].join(separator);
    text.push_str(separator);
    text.push_str(name);
    Some(Edit { end: span.after, text })
}

/// A path in a word: the byte index it starts at, the one just past its root's spelling and the one it ends at.
#[derive(Clone, Copy)]
struct Span {
    index: usize,
    after: usize,
    end: usize,
}

impl Span {
    /// The path starting at `index` in `word` whose root spelling ends at `after`, running on as [`path_end`] reads it.
    fn new(word: &str, index: usize, after: usize, script: bool) -> Span {
        Span {
            index,
            after,
            end: path_end(word, after, script),
        }
    }
}

/// `\` when `path` holds one, else `/`: the separator a rewritten path is written with.
fn separator_of(path: &str) -> &'static str {
    if path.contains('\\') { "\\" } else { "/" }
}

/// Where a path lands: the worktree or a sibling, with the segments below its root; a sibling's first segment is its
/// declared name.
enum Landing<'a> {
    Worktree(Vec<&'a str>),
    Sibling(Vec<&'a str>),
}

/// How the path `span` of `word` is sent when its `..` climb out of the root it starts below, `start`, as [`land`]
/// walks them: the landing as [`landing_text`] writes it, ending with the separator that ends the path, if any, unless
/// it is `.`; `None` when the path lands anywhere but inside the worktree or a sibling.
fn climb_edit<'a>(bases: &Bases<'a>, cwd: &[&str], word: &'a str, span: Span, start: Landing<'a>) -> Option<Edit> {
    let path = &word[span.index..span.end];
    let landing = land(bases, start, &word[span.after..span.end])?;
    let mut text = landing_text(cwd, landing, separator_of(path));
    let trailing = path.chars().next_back().filter(|last| matches!(last, '\\' | '/'));
    if let Some(last) = trailing.filter(|_| text != ".") {
        text.push(last);
    }
    Some(Edit { end: span.end, text })
}

/// Where `rest`, a path below the root `start` lands in with its segments, climbs to as [`Level::step`] walks an
/// absolute path; `None` when it ends anywhere but inside the worktree or a sibling.
fn land<'a>(bases: &Bases<'a>, start: Landing<'a>, rest: &'a str) -> Option<Landing<'a>> {
    let (mut level, mut segments) = match start {
        Landing::Worktree(segments) => (Level::Worktree(segments.len()), segments),
        Landing::Sibling(segments) => (Level::Sibling(segments.len().saturating_sub(1)), segments),
    };
    for segment in rest.split(['\\', '/']).filter(|segment| !segment.is_empty() && *segment != ".") {
        let next = level.step(segment, bases, true)?;
        follow(&mut segments, (level, next), segment, bases)?;
        level = next;
    }
    match level {
        Level::Worktree(_) => Some(Landing::Worktree(segments)),
        Level::Sibling(_) => Some(Landing::Sibling(segments)),
        Level::Parent => None,
    }
}

/// Brings `segments`, a landing's segments at `from`, to the step over `segment` that leads to `to`: reaching the
/// parent clears them, a `..` takes one off, entering a sibling starts them with its declared name, entering the
/// worktree leaves them empty and any other segment is added.
fn follow<'a>(segments: &mut Vec<&'a str>, (from, to): (Level, Level), segment: &'a str, bases: &Bases<'a>) -> Option<()> {
    match (from, to) {
        (_, Level::Parent) => segments.clear(),
        _ if segment == ".." => {
            segments.pop();
        }
        (Level::Parent, Level::Sibling(_)) => segments.push(bases.sibling_named(segment)?),
        (Level::Parent, _) => {}
        _ => segments.push(segment),
    }
    Some(())
}

/// The text `landing` is sent as from the gate's folder `cwd`, joined by `separator`: `.` for the gate's folder, else
/// `..` a level of the gate's folder, one more for a sibling, then the landing's segments.
fn landing_text(cwd: &[&str], landing: Landing<'_>, separator: &str) -> String {
    let (ups, segments) = match landing {
        Landing::Worktree(segments) if eq_ignoring_case(&segments.join("/"), &cwd.join("/")) => return ".".to_owned(),
        Landing::Worktree(segments) => (cwd.len(), segments),
        Landing::Sibling(segments) => (cwd.len() + 1, segments),
    };
    let mut parts = vec![".."; ups];
    parts.extend(segments);
    parts.join(separator)
}

/// How the path `span` of `word` is sent when it lies below the worktree root; `None` when its `..` climb out of it.
fn worktree_edit(cwd: &[&str], word: &str, span: Span) -> Option<Edit> {
    let target = resolve(&[], &word[span.after..span.end])?;
    if eq_ignoring_case(&target.join("/"), &cwd.join("/")) {
        return Some(Edit {
            end: span.end,
            text: ".".to_owned(),
        });
    }
    if cwd.is_empty() {
        return Some(Edit {
            end: separator_end(word, span.after),
            text: String::new(),
        });
    }
    Some(Edit {
        end: span.after,
        text: vec![".."; cwd.len()].join(separator_of(&word[span.index..span.end])),
    })
}

/// The byte index just past the root spelled `base` at `index` in `word`, as [`after_prefix`] reads it, a character
/// ending the path ([`path_end`]) bounding it too.
fn root_end(base: &str, word: &str, index: usize, script: bool) -> Option<usize> {
    let ends = |rest: &str| {
        rest.chars()
            .next()
            .is_some_and(|next| PATH_ENDS.contains(&next) || (script && next.is_whitespace()))
    };
    let rest = after_prefix(&word[index..], base, ends)?;
    Some(word.len() - rest.len())
}

fn separator_end(word: &str, start: usize) -> usize {
    start + word[start..].bytes().take_while(|byte| matches!(byte, b'\\' | b'/')).count()
}

/// Where the path running on from `start` ends: the first character ending a path, or the word's length.
fn path_end(word: &str, start: usize, script: bool) -> usize {
    word[start..]
        .char_indices()
        .find(|(_, character)| PATH_ENDS.contains(character) || (script && character.is_whitespace()))
        .map_or(word.len(), |(at, _)| start + at)
}

/// The segments of `path` below the folder whose segments are `base`, `.` and empty segments dropped and each `..`
/// taking one off; `None` when a `..` climbs above the folder `base` starts from.
fn resolve<'a>(base: &[&'a str], path: &'a str) -> Option<Vec<&'a str>> {
    let mut segments = base.to_vec();
    for segment in path.split(['\\', '/']) {
        if segment == ".." {
            segments.pop()?;
        } else if !segment.is_empty() && segment != "." {
            segments.push(segment);
        }
    }
    Some(segments)
}

/// Whether a word holding no drive path names a folder above the worktree root outside the siblings: the word itself,
/// the text after its last `=` (`--project=../x`) or the text after a leading switch prefix (`-p:../x`, `/p:../x`).
fn relative_escape(cwd: &[&str], bases: &Bases<'_>, word: &str) -> bool {
    let after_equals = word.rfind('=').map(|at| &word[at + 1..]);
    [Some(word), after_equals, switch_value(word)]
        .into_iter()
        .flatten()
        .any(|text| climbs(cwd, bases, text))
}

/// The value of a `-x:value`, `--x:value` or `/x:value` word: a switch name of a letter then letters, digits, `_`,
/// `.` or `-`, a colon, and a value of one line.
fn switch_value(word: &str) -> Option<&str> {
    let rest = word
        .strip_prefix("--")
        .or_else(|| word.strip_prefix('-'))
        .or_else(|| word.strip_prefix('/'))?;
    if !rest.starts_with(|first: char| first.is_ascii_alphabetic()) {
        return None;
    }
    let (colon, mark) = rest
        .char_indices()
        .find(|(_, character)| !(character.is_alphanumeric() || matches!(character, '_' | '.' | '-')))?;
    if mark != ':' {
        return None;
    }
    let value = &rest[colon + 1..];
    let value = value.strip_suffix('\n').unwrap_or(value);
    (!value.is_empty() && !value.contains('\n')).then_some(value)
}

/// Whether `text`, read as a path relative to the gate's folder, climbs above the root to anywhere but into one of the
/// siblings, or climbs past the root of the drive the gate's folder is spelled on ([`drive_floor`]), where Windows
/// stops `..`. Only text starting with `..` or holding a separator is read as a path, and never text starting with a
/// separator: that is a switch (`/p:`, `/c`) or a path on the current drive, none of the worktree's.
fn climbs(cwd: &[&str], bases: &Bases<'_>, text: &str) -> bool {
    if text.starts_with(['\\', '/']) || (!text.contains(['\\', '/']) && !text.starts_with("..")) {
        return false;
    }
    let mut level = Level::Worktree(cwd.len());
    for segment in text.split(['\\', '/']).filter(|segment| !segment.is_empty() && *segment != ".") {
        let stopped = segment == ".." && level.at_floor(bases.floor);
        let Some(next) = level.step(segment, bases, false).filter(|_| !stopped) else {
            return true;
        };
        level = next;
    }
    level == Level::Parent
}

/// Where a path has got to, segment by segment.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    /// This many folders below the worktree root.
    Worktree(usize),
    /// The worktree root's parent.
    Parent,
    /// This many folders below a sibling's root.
    Sibling(usize),
}

impl Level {
    /// Where `segment`, neither empty nor `.`, leads from here; `None` when it leaves the worktree and the siblings.
    /// Only an `absolute` path, which is sent as where it lands, may climb out of a sibling to the parent and enter the
    /// worktree from there by the root's last segment; a relative one is sent as written, so it may not.
    fn step(self, segment: &str, bases: &Bases<'_>, absolute: bool) -> Option<Level> {
        match (self, segment == "..") {
            (Level::Worktree(depth), true) => Some(depth.checked_sub(1).map_or(Level::Parent, Level::Worktree)),
            (Level::Worktree(depth), false) => Some(Level::Worktree(depth + 1)),
            (Level::Parent, true) => None,
            (Level::Parent, false) => Level::below_parent(segment, bases, absolute),
            (Level::Sibling(0), true) => absolute.then_some(Level::Parent),
            (Level::Sibling(depth), true) => Some(Level::Sibling(depth - 1)),
            (Level::Sibling(depth), false) => Some(Level::Sibling(depth + 1)),
        }
    }

    /// Whether this is at or above the root of the drive the gate's folder is spelled on, `floor` folders below the
    /// worktree root: a `..` from here would climb past it.
    fn at_floor(self, floor: Option<usize>) -> bool {
        matches!((self, floor), (Level::Worktree(depth), Some(floor)) if depth <= floor)
    }

    /// Where `segment` leads from the parent: into the sibling it names, else, for an `absolute` path, into the
    /// worktree when it is the root's last segment (ASCII letter case aside); `None` anywhere else.
    fn below_parent(segment: &str, bases: &Bases<'_>, absolute: bool) -> Option<Level> {
        if bases.sibling_named(segment).is_some() {
            Some(Level::Sibling(0))
        } else {
            (absolute && segment.eq_ignore_ascii_case(bases.leaf)).then_some(Level::Worktree(0))
        }
    }
}
