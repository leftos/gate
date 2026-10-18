//! The in-worktree path rewrite: a routed command's words as the builder runs them in the gate's folder there.
//! Every drive path in a word that lies inside the worktree or a declared sibling beside it is made relative to the
//! gate's folder; any other drive path marks the word as outside the worktree. A command's first non-empty word that is
//! a drive path outside the worktree and its siblings, every segment of it a name Win32 reads as written
//! ([`plain_segment`]), is sent as the program's name instead, and the run says `gate: routed {given} as {name}` for it.
//!
//! A drive path starts at a drive letter, a colon and a separator not preceded by a letter, digit, `_` or `-`,
//! anywhere in the word (`--flag=X:\a`, `-p:Name=X:\a`, `"X:\a;X:\b"`, a script's text), each such start a path of its
//! own, judged on its own. It runs to the first of `, ; " ' ) | & < >`. In a word that holds one of those or a line
//! break (a script's text or a list) white space ends it too; in any other word, one argument, a path runs through its
//! spaces.
//!
//! A word is outside too when it holds a drive-relative path (`C:..\x`), a drive path right after a device prefix
//! (`\\?\`, `\\.\`), or, at any place a path may start in it, a root-relative, share or device path or relative text
//! whose `..` climb out of the worktree.

use std::path::PathBuf;

use crate::like::{eq_ignoring_case, lower_char};

/// Characters that end a path in a word.
const PATH_ENDS: [char; 9] = [',', ';', '"', '\'', ')', '|', '&', '<', '>'];

/// Characters a program's file name cannot hold: the wildcards, the redirections and a quote. A line break and the
/// characters Win32 reads another way are refused elsewhere ([`program_file_name`], [`plain_segment`]).
const BANNED_NAME_CHARS: [char; 6] = ['*', '?', '"', '<', '>', '|'];

/// A command's words rewritten for the builder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rewritten {
    /// Each word with its in-worktree paths made relative to the gate's folder; outside paths stay as written, apart
    /// from a first word naming a program by such a path, which is the program's name ([`Rewritten::program_named`]).
    pub words: Vec<String>,
    /// The first word, as given, that names a path outside the worktree, when one does, apart from a first word sent as
    /// a program's name ([`Rewritten::program_named`]), which is not reported here.
    pub outside: Option<String>,
    /// The absolute path, written with `\`, that the [`Rewritten::outside`] word names outside the worktree, when the
    /// word is no script's text or list and that path is a drive path, its `..` resolved and a subst drive's letter
    /// replaced by what it stands for, or relative text climbing out, resolved against the gate's folder below the root;
    /// `None` for any other word, a share or device path among them.
    pub outside_path: Option<PathBuf>,
    /// Whether the command's first non-empty word named a program by an absolute path outside the worktree and was
    /// sent as the program's name: the builder finds the program on its `PATH`, and the name is no file of the gate's
    /// folder.
    pub program_named: bool,
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
///
/// A word is outside, whatever its drive paths, when it holds a drive-relative path (`C:..\x`, `--x=C:y`), whose
/// meaning depends on the folder current on that drive; a drive path right after `\\?\`, `\\.\`, `//?/` or `//./`; or,
/// at any place a path may start in it (the word's start, after white space, after any of
/// `, ; " ' ) | & < > ( =`, after the colon of a switch starting the word, a white-space token or following `"`, `'` or `,`, after an `@` right at
/// any of these), a drive-relative path, a root-relative path (`\src`, and in a value position `/src`), a share or
/// device path (two separators) or relative text that climbs out as above, read both to the end of the word and to the
/// end of the path there. A word's rewritten text is the same either way.
///
/// A command's first non-empty word that is a drive path (`F:\x\y.exe`) outside the worktree and its siblings, with no
/// other drive path in it and no segment of it empty, `.` or `..`, is sent as the program's name instead: its file name
/// with a trailing `.exe`, in any letter case, removed, when that leaves a name a program can be named by
/// ([`program_name`]). That name counts as inside, and the builder looks the program up on its `PATH`
/// ([`Rewritten::program_named`]).
#[must_use]
pub fn rewrite_words(place: &RewritePlace<'_>, words: &[String], drives: &[(char, String)], siblings: &[SiblingRoot]) -> Rewritten {
    rewrite_named(
        place,
        words,
        drives,
        siblings.iter().map(|sibling| (sibling.name.as_str(), sibling.root.as_str())),
    )
}

/// [`rewrite_words`] for siblings the caller holds as a declared name and a root each, in declared order: a routed run's
/// carried siblings need not be rebuilt as [`SiblingRoot`]s for it.
#[must_use]
pub(crate) fn rewrite_named<'s>(
    place: &RewritePlace<'_>,
    words: &[String],
    drives: &[(char, String)],
    siblings: impl Iterator<Item = (&'s str, &'s str)>,
) -> Rewritten {
    let cwd: Vec<&str> = place.prefix.split(['\\', '/']).filter(|segment| !segment.is_empty()).collect();
    let bases = Bases {
        root: root_spellings(place.root, drives),
        leaf: place.leaf,
        siblings: siblings.map(|(name, root)| (name, root_spellings(root, drives))).collect(),
        floor: drive_floor(place.folder, cwd.len()),
    };
    let mut outside = None;
    let mut path = None;
    let mut program_named = false;
    let mut rewritten = Vec::with_capacity(words.len());
    let first = words.iter().position(|word| !word.is_empty());
    for (index, word) in words.iter().enumerate() {
        let (text, is_outside) = rewrite_word(&bases, &cwd, word);
        if let Some(name) = program_name(word, first == Some(index), is_outside) {
            program_named = true;
            rewritten.push(name);
        } else {
            rewritten.push(text);
            if is_outside && outside.is_none() {
                outside = Some(word.clone());
                path = outside_path(&bases, &cwd, drives, word);
            }
        }
    }
    Rewritten {
        words: rewritten,
        outside,
        outside_path: path,
        program_named,
    }
}

/// The absolute path the outside `word` names ([`Rewritten::outside_path`]): its first drive path the rewrite judged
/// outside, not after a device prefix ([`drive_path`]), else the text at its first path start that climbs out
/// ([`climbed_path`]). `None` for a share or device path, a word holding a drive-relative path, and a script's text or
/// a list, whose paths end where the word's characters say.
fn outside_path(bases: &Bases<'_>, cwd: &[&str], drives: &[(char, String)], word: &str) -> Option<PathBuf> {
    let script = word.contains(PATH_ENDS) || word.contains(['\n', '\r']);
    if script || word.starts_with(r"\\") || word.starts_with("//") || drive_relative(word) {
        return None;
    }
    let outside_drive = drive_starts(word)
        .into_iter()
        .find(|&index| !after_device_prefix(word, index) && path_edit(bases, cwd, word, index, false).is_none());
    match outside_drive {
        Some(index) => drive_path(&word[index..], drives),
        None => climb_in(bases, cwd, word),
    }
}

/// The absolute path the text at the first path start of `word` that climbs out of the root names ([`climbed_path`]),
/// read to the end of the word, else to the end of the path there; starts holding a rooted, drive-relative or drive
/// path are passed over.
fn climb_in(bases: &Bases<'_>, cwd: &[&str], word: &str) -> Option<PathBuf> {
    path_starts(word).into_iter().find_map(|start| {
        let text = &word[start.index..];
        if rooted(text, start.value) || drive_relative_at(text.as_bytes()) || drive_at(text.as_bytes(), 0) {
            return None;
        }
        [text, &text[..path_end(text, 0, true)]]
            .into_iter()
            .find(|text| climbs(cwd, bases, text))
            .and_then(|text| climbed_path(bases, cwd, text))
    })
}

/// The drive path `path` as an absolute path written with `\`: its `.` and empty segments dropped, each `..` taking one
/// off and stopping at the drive's root as Windows does, and its drive letter replaced by the path a subst drive of
/// `drives` stands for. `None` when that path starts with no drive letter (a subst drive standing for a share).
fn drive_path(path: &str, drives: &[(char, String)]) -> Option<PathBuf> {
    let letter = char::from(path.as_bytes()[0]).to_ascii_uppercase();
    let base = drives.iter().find(|(drive, _)| *drive == letter).map_or_else(
        || path[..2].to_owned(),
        |(_, target)| target.replace('/', "\\").trim_end_matches('\\').to_owned(),
    );
    root_letter(&base)?;
    let mut segments = Vec::new();
    for segment in path[3..].split(['\\', '/']).filter(|segment| !segment.is_empty() && *segment != ".") {
        if segment == ".." {
            segments.pop();
        } else {
            segments.push(segment);
        }
    }
    Some(PathBuf::from(format!("{base}\\{}", segments.join("\\"))))
}

/// The absolute path, written with `\`, that `text`, relative text climbing out of the root, names from the gate's
/// folder `cwd` below a root on a drive: each `..` taking one off and stopping at the drive's root. `None` for a root
/// on no drive, and when a `..` stops at the root of the drive the gate's folder is spelled on ([`drive_floor`]),
/// which leaves the path inside the worktree.
fn climbed_path(bases: &Bases<'_>, cwd: &[&str], text: &str) -> Option<PathBuf> {
    let root = &bases.root.first()?.text;
    root_letter(root)?;
    let mut parts = root.split('/');
    let drive = parts.next()?;
    let mut segments: Vec<&str> = parts.filter(|segment| !segment.is_empty()).chain(cwd.iter().copied()).collect();
    let mut depth = Some(cwd.len());
    for segment in text.split(['\\', '/']).filter(|segment| !segment.is_empty() && *segment != ".") {
        if segment != ".." {
            depth = depth.map(|depth| depth + 1);
            segments.push(segment);
            continue;
        }
        if depth.is_some_and(|depth| bases.floor.is_some_and(|floor| depth <= floor)) {
            return None;
        }
        depth = depth.and_then(|depth| depth.checked_sub(1));
        segments.pop();
    }
    Some(PathBuf::from(format!("{drive}\\{}", segments.join("\\"))))
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

/// The drive letter, upper-cased, that `root` starts with (`X:`); `None` when it starts with none.
pub(crate) fn root_letter(root: &str) -> Option<char> {
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
    let judged = judged_outside(cwd, bases, word);
    let starts = drive_starts(word);
    if starts.is_empty() {
        return (word.to_owned(), judged);
    }
    let script = word.contains(PATH_ENDS) || word.contains(['\n', '\r']);
    let mut text = String::with_capacity(word.len());
    let mut outside = judged || starts.iter().any(|&index| after_device_prefix(word, index));
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

/// The name the command's first non-empty word `word` is sent as, when `first` and [`rewrite_word`] judged it outside
/// the worktree: its file name ([`last_segment`]) with a trailing `.exe`, in any letter case, removed, when that leaves
/// a name a program can be named by ([`program_file_name`]). `None` for a word that is no such path: not the command's
/// first non-empty word, not outside, not a sole drive path ([`drive_rest`]) whose every segment Win32 reads as written
/// ([`plain_segment`]), naming no such file name, or naming nothing once a trailing `.exe` is removed.
fn program_name(word: &str, first: bool, outside: bool) -> Option<String> {
    if !first || !outside {
        return None;
    }
    let stem = without_exe(last_segment(drive_rest(word)?)?);
    (!stem.is_empty() && program_file_name(stem)).then(|| stem.to_owned())
}

/// What follows a word's drive letter, `:`, and separator, either way, when that is the word's only drive path; `None`
/// when the word starts with anything else, or holds another drive path.
fn drive_rest(word: &str) -> Option<&str> {
    matches!(drive_starts(word).as_slice(), [0]).then(|| &word[3..])
}

/// The last of `rest`'s segments, a drive path below its drive, when every segment of it is one Win32 reads as written,
/// folders and file name alike ([`plain_segment`]).
fn last_segment(rest: &str) -> Option<&str> {
    if !rest.split(['\\', '/']).all(plain_segment) {
        return None;
    }
    rest.rsplit(['\\', '/']).next()
}

/// Whether `segment`, a folder or file name of a path, is one Win32 reads as written: not empty, not `.` or `..`, the
/// climb and the folder itself, without `:`, without an 8.3 short name's `~` and ASCII digit
/// ([`names_a_short_name`]) and not ending in `.` or a space, which Win32 strips. A segment Win32 reads as another name
/// than it spells, a folder's trailing dot dropped or an 8.3 short name expanded among them, leaves the word naming a
/// folder the caller may not have meant.
fn plain_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && !segment.contains(':')
        && !names_a_short_name(segment)
        && !segment.ends_with(['.', ' '])
}

/// `name` without a trailing `.exe`, in any letter case.
fn without_exe(name: &str) -> &str {
    let at = name.len().saturating_sub(4);
    match name.as_bytes().get(at..) {
        Some(removed) if removed.eq_ignore_ascii_case(b".exe") => &name[..at],
        _ => name,
    }
}

/// Whether `name`, a file name, is one a program can be named by: without `*`, `?`, `"`, `<`, `>`, `|` and without white
/// space. The characters Win32 reads another way, the colon, the ending `.` or space and the 8.3 short name among them,
/// are refused for every segment of the path ([`plain_segment`]).
fn program_file_name(name: &str) -> bool {
    !name.contains(BANNED_NAME_CHARS) && !name.chars().any(char::is_whitespace)
}

/// Whether `name` holds a `~` followed by an ASCII digit: an 8.3 short name like `TOOLNA~1`, which names another file
/// of the same folder.
fn names_a_short_name(name: &str) -> bool {
    name.as_bytes()
        .windows(2)
        .any(|pair| matches!(pair, [b'~', digit] if digit.is_ascii_digit()))
}

/// The byte indexes in `word` where a drive path starts: an ASCII letter, `:` and `\` or `/`, not preceded by a
/// letter, digit, `_` or `-`.
fn drive_starts(word: &str) -> Vec<usize> {
    let bytes = word.as_bytes();
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 2 < bytes.len() {
        if drive_at(bytes, index) && !follows_word_character(word, index) {
            starts.push(index);
            index += 3;
        } else {
            index += 1;
        }
    }
    starts
}

/// Whether an ASCII letter, `:` and `\` or `/` stand at `index` in `bytes`.
fn drive_at(bytes: &[u8], index: usize) -> bool {
    bytes.len() > index + 2 && bytes[index].is_ascii_alphabetic() && bytes[index + 1] == b':' && matches!(bytes[index + 2], b'\\' | b'/')
}

/// Whether the drive path at `index` in `word` follows a device prefix, `\\?\` or `\\.\`, with either separator.
fn after_device_prefix(word: &str, index: usize) -> bool {
    let separator = |byte: u8| matches!(byte, b'\\' | b'/');
    index.checked_sub(4).is_some_and(
        |from| matches!(word.as_bytes()[from..index], [first, second, b'?' | b'.', last] if separator(first) && separator(second) && separator(last)),
    )
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

/// Whether `word` names a place outside the worktree by a path's form rather than its spelling: it holds a
/// drive-relative path ([`drive_relative`]), or at one of its path starts ([`path_starts`]) a rooted path ([`rooted`]),
/// a drive-relative one ([`drive_relative_at`]) or relative text that [`climbs`] out, read both to the end of the word
/// and to the end of the path there. A drive path at a start is left to the rewrite, which judges it by its spelling.
fn judged_outside(cwd: &[&str], bases: &Bases<'_>, word: &str) -> bool {
    drive_relative(word)
        || path_starts(word).into_iter().any(|start| {
            let text = &word[start.index..];
            if rooted(text, start.value) || drive_relative_at(text.as_bytes()) {
                return true;
            }
            !drive_at(text.as_bytes(), 0) && (climbs(cwd, bases, text) || climbs(cwd, bases, &text[..path_end(text, 0, true)]))
        })
}

/// Whether `word` holds a drive-relative path: an ASCII letter and `:` not followed by `\`, `/` or `:` (or ending the
/// word), the letter following none of a letter, digit, `_`, `-`, `.`, `/`, `\` or `:`. So `C:..\x`, `C:` and
/// `--x=C:y` are drive-relative, and `-c:Release`, `/t:Build`, `$env:PATH`, `https://h`, `crate::mod`, `10:30` and
/// `a/C:x` are not.
fn drive_relative(word: &str) -> bool {
    let bytes = word.as_bytes();
    (0..bytes.len().saturating_sub(1)).any(|index| {
        let drive = bytes[index].is_ascii_alphabetic() && bytes[index + 1] == b':';
        let relative = || !matches!(bytes.get(index + 2), Some(b'\\' | b'/' | b':'));
        let free = || {
            !word[..index]
                .chars()
                .next_back()
                .is_some_and(|before| before.is_alphanumeric() || matches!(before, '_' | '-' | '.' | '/' | '\\' | ':'))
        };
        drive && relative() && free()
    })
}

/// A place in a word where a path may start: its byte index, and whether it is a value position, where a single `/`
/// roots a path.
#[derive(Clone, Copy)]
struct PathStart {
    index: usize,
    value: bool,
}

/// Whether `text`, at a path start, begins with a drive-relative path: an ASCII letter and `:` not followed by `\`,
/// `/` or `:` (`C:..\x`, `C:`; not `crate::mod`).
fn drive_relative_at(text: &[u8]) -> bool {
    matches!(text, [letter, b':', ..] if letter.is_ascii_alphabetic()) && !matches!(text.get(2), Some(b'\\' | b'/' | b':'))
}

/// Where paths may start in `word`: its start; after white space, a line break or any of `, ; " ' ) | & < > (`; and,
/// as value positions, after `=`, after the colon of a switch (`-x:`, `--x:`, `/x:`) that starts the word, a white-space
/// token or follows `"`, `'` or `,` (an argument's start), and after an `@` right at any of these.
fn path_starts(word: &str) -> Vec<PathStart> {
    let mut starts = Vec::new();
    push_start(&mut starts, word, PathStart { index: 0, value: false }, true);
    for (at, character) in word.char_indices() {
        let index = at + character.len_utf8();
        let value = character == '=';
        if value || character.is_whitespace() || character == '(' || PATH_ENDS.contains(&character) {
            let token = character.is_whitespace() || matches!(character, '"' | '\'' | ',');
            push_start(&mut starts, word, PathStart { index, value }, token);
        }
    }
    starts
}

/// Adds `start` in `word` to `starts`, then, when it begins an argument (`token`), the value of a switch there, and
/// after either the value following an `@` right at it.
fn push_start(starts: &mut Vec<PathStart>, word: &str, start: PathStart, token: bool) {
    let switch = switch_value_start(&word[start.index..]).filter(|_| token).map(|offset| PathStart {
        index: start.index + offset,
        value: true,
    });
    for found in std::iter::once(start).chain(switch) {
        starts.push(found);
        if word[found.index..].starts_with('@') {
            starts.push(PathStart {
                index: found.index + 1,
                value: true,
            });
        }
    }
}

/// The byte index just past the colon of a leading `-x:`, `--x:` or `/x:` switch: a switch name of a letter then
/// letters, digits, `_`, `.` or `-`, and a colon.
fn switch_value_start(word: &str) -> Option<usize> {
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
    (mark == ':').then(|| word.len() - rest.len() + colon + 1)
}

/// Whether `text`, at a path start, begins a rooted path: a `\` (root-relative, or with another separator a share or
/// device path), two separators, or, in a `value` position, a single `/`. At any other start a single `/` is a switch
/// (`/p:`, `/c`) or text (`test(/^run::/)`).
fn rooted(text: &str, value: bool) -> bool {
    let mut characters = text.chars();
    match characters.next() {
        Some('\\') => true,
        Some('/') => value || matches!(characters.next(), Some('\\' | '/')),
        _ => false,
    }
}

/// Whether `text`, read as a path relative to the gate's folder, climbs above the root to anywhere but into one of the
/// siblings, or climbs past the root of the drive the gate's folder is spelled on ([`drive_floor`]), where Windows
/// stops `..`. Only text starting with `..` or holding a separator is read as a path, and never text starting with a
/// separator: [`rooted`] judges that.
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
