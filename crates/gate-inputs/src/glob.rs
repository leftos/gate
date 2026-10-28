//! The output globs as the PowerShell gate matched them (remote-wrapper.ps1's `ConvertTo-OutputRegex`), plus the paths
//! never brought back whatever the globs: `$OutputExcluded`'s, any `.git*` component or `.lfsconfig`, matched as the
//! sync-back matches them, and any path that does not fit the outputs archive: one that is not plain, which the
//! archive's reader refuses, or the archive's own list name. A glob that names a `TestResults` folder, or a `.tmp`
//! folder, lifts that one arm of the exclusions for itself ([`EXCLUDED_ARMS`], [`EXCLUDED_TEST_RESULTS`],
//! [`EXCLUDED_TMP`]); the gate's own names under the root's `.tmp` stay excluded whatever the glob
//! ([`EXCLUDED_GATE_FILES`]).

use std::sync::LazyLock;

use gate_router::escaped;
use regex::Regex;

use crate::archive::LIST_NAME;
use crate::names::{PATH_DEPTH_LIMIT, is_git_s, is_lfs_config, is_plain_part, is_plain_path, is_spelled_git_s, is_spelled_lfs_config};

/// The arm of [`EXCLUDED_ARMS`] holding the contents of a `.git` folder at any depth. It stays though [`is_git_s`]
/// covers the same paths.
const EXCLUDED_GIT: &str = r"(^|/)\.git/";

/// The arm of [`EXCLUDED_ARMS`] holding the contents of a `.tmp` folder at any depth: the one a glob that names `.tmp`
/// folders lifts for itself ([`Pattern`]).
const EXCLUDED_TMP: &str = r"(^|/)\.tmp/";

/// The arm of [`EXCLUDED_ARMS`] holding the contents of a `TestResults` folder at any depth: the one a glob that names
/// the folder lifts for itself ([`Pattern`]).
const EXCLUDED_TEST_RESULTS: &str = r"(^|/)TestResults/";

/// The arm of [`EXCLUDED_ARMS`] holding a `.env` file (`.env`, `.env.<name>`, `<name>.env`).
const EXCLUDED_ENV: &str = r"(^|/)\.env\.[^/]*$|\.env$";

/// The gate's own names in the root's `.tmp` folder, and everything below them: `<key>-<nonce>` with any suffix, the key
/// ending in 8 hex digits and the nonce 32 (the outputs' archive and stage, the inputs' and the LFS's archives and
/// diffs). No glob lifts it.
const EXCLUDED_GATE_FILES: &str = r"^\.tmp/[^/]+-[0-9a-f]{8}-[0-9a-f]{32}(?:\.[^/]*)?(?:/|$)";

/// The arms of the outputs' exclusions, each a regex over a `/`-separated relative path, matched without regard to
/// case. Every glob is held by all of them but for a glob that names a `TestResults` or a `.tmp` folder, which lifts
/// [`EXCLUDED_TEST_RESULTS`] or [`EXCLUDED_TMP`] for itself alone.
const EXCLUDED_ARMS: [&str; 4] = [EXCLUDED_GIT, EXCLUDED_TMP, EXCLUDED_TEST_RESULTS, EXCLUDED_ENV];

/// The arms no glob lifts: `.git`, `.env` and the gate's own `.tmp` names.
const NEVER_LIFTED_ARMS: [&str; 3] = [EXCLUDED_GIT, EXCLUDED_ENV, EXCLUDED_GATE_FILES];

/// The paths never brought back, whatever the globs: every arm of [`EXCLUDED_ARMS`].
static EXCLUDED: LazyLock<Regex> = LazyLock::new(|| exclusions_regex(&EXCLUDED_ARMS));

/// The paths never brought back even to a glob that names the `TestResults` and `.tmp` folders above them: every arm
/// of [`NEVER_LIFTED_ARMS`].
static NEVER_LIFTED: LazyLock<Regex> = LazyLock::new(|| exclusions_regex(&NEVER_LIFTED_ARMS));

/// One `TestResults` folder segment, folded as [`EXCLUDED_TEST_RESULTS`] and the glob regexes fold it: without regard to
/// case, a non-ASCII spelling such as `Teſtresults` included.
static TEST_RESULTS_SEGMENT: LazyLock<Regex> = LazyLock::new(|| always_valid(r"(?i)^TestResults$"));

/// One `.tmp` folder segment, folded as [`EXCLUDED_TMP`] folds it: without regard to case.
static TMP_SEGMENT: LazyLock<Regex> = LazyLock::new(|| always_valid(r"(?i)^\.tmp$"));

/// The exclusion `arms`, joined into one regex matched without regard to case.
fn exclusions_regex(arms: &[&str]) -> Regex {
    always_valid(&format!("(?i){}", arms.join("|")))
}

/// `pattern` as a regex; the patterns of this module are its own literals, so one that cannot be matched is a defect.
fn always_valid(pattern: &str) -> Regex {
    match Regex::new(pattern) {
        Ok(regex) => regex,
        Err(error) => unreachable!("the pattern {pattern:?} cannot be matched: {error}"),
    }
}

/// A run's output globs. Each is relative to the work folder's root and `/`-separated: `**/` is any number of folders,
/// none included, `**` anything, `*` any run of characters within one name, `?` one character within one name, and
/// every other character itself. They match without regard to case, as the PowerShell gate's did. Whatever the globs, a
/// path a sync-back never writes — a `.git*` component or a `.lfsconfig`, read by [`is_git_s`] and [`is_lfs_config`] —
/// never comes back and its folder is never entered, nor does a path that does not fit the outputs archive ([`fits`]):
/// one that is not plain ([`is_plain_path`]) or the archive's own list name. The box's walk notes such a file
/// ([`Globs::refuses_name`]), and reads a folder whose name is not plain only to count them ([`Globs::counts_below`]).
///
/// The `.git` and `.env` arms of [`EXCLUDED_ARMS`] hold every glob, as do the gate's own `.tmp` names
/// ([`EXCLUDED_GATE_FILES`]). A glob one of whose folder segments is exactly `TestResults`, in any letter case, lifts
/// [`EXCLUDED_TEST_RESULTS`] for itself alone: it brings back what it matches below a `TestResults` folder, at most as
/// many such folders as it names, and the walk enters a `TestResults` folder when a glob of the set names it on the way
/// to it. A glob that names `.tmp` folders lifts [`EXCLUDED_TMP`] the same way ([`tmp_segments`]): a wildcard never
/// matches a `.tmp` folder, and below its named `.tmp` folder the walk goes only as deep as the glob's own folders
/// reach.
#[derive(Clone)]
pub struct Globs {
    patterns: Vec<Pattern>,
    /// The exclusions every glob is held by: [`EXCLUDED`], or the inputs' own regex.
    excluded: Regex,
    /// The exclusions a glob that names a `TestResults` or a `.tmp` folder is held by instead: [`NEVER_LIFTED`], or the
    /// inputs' own regex again.
    never_lifted: Regex,
}

/// What a matcher's exclusions say of one file's `/`-separated relative `path`, worked out once for the patterns of a
/// call.
#[derive(Clone, Copy)]
struct Exclusions {
    /// [`Globs::excluded`] holds the path.
    held: bool,
    /// [`Globs::never_lifted`] holds it, asked only when it is held.
    beyond: bool,
    /// The `TestResults` folders above it, counted only when it is held.
    test_results: usize,
    /// The `.tmp` folders above it, counted only when it is held.
    tmp: usize,
}

/// One of [`Globs`]' globs.
#[derive(Clone)]
struct Pattern {
    regex: Regex,
    /// The `TestResults` folders the glob names, `None` when it names none ([`test_results_segments`]).
    test_results: Option<Named>,
    /// The `.tmp` folders the glob names, `None` when it names none ([`tmp_segments`]).
    tmp: Option<Named>,
}

/// The folders of one kind (`TestResults`, `.tmp`) a glob names.
#[derive(Clone)]
struct Named {
    /// How many of the glob's folder segments name one.
    folders: usize,
    /// The matcher of a path at or below the glob's folders up to and including its first named segment
    /// ([`regex_upto`]): the walk reads below such a folder only where this holds.
    prefix: Regex,
    /// For each named segment, in order, how many folder levels the glob has below it: `None` when a segment after it,
    /// the file's own included, holds `**`. The walk reads below a `.tmp` folder no deeper than this.
    levels: Vec<Option<usize>>,
}

impl Globs {
    /// The matcher of the output globs `globs`; else why one of them cannot be matched.
    pub fn new(globs: &[String]) -> Result<Globs, String> {
        Globs::named(globs, "output glob")
    }

    /// The matcher of a routed run's `--bring-tracked` globs `globs`, which the output globs' exclusions hold for too;
    /// else why one of them cannot be matched.
    pub fn bring_tracked(globs: &[String]) -> Result<Globs, String> {
        Globs::named(globs, "--bring-tracked glob")
    }

    /// The matcher of a routed run's input globs `globs`, with the inputs' own exclusions `excluded`, a regex of the
    /// `/`-separated relative paths never sent, in place of the outputs' (so `.tmp` is reachable); they hold a glob that
    /// names `TestResults` or `.tmp` as they hold any other, since they have no such arm to lift. The `.git*`,
    /// `.lfsconfig` and plain-name rules hold as for outputs. Else why a glob or the exclusions cannot be matched.
    pub fn inputs(globs: &[String], excluded: &str) -> Result<Globs, String> {
        let patterns = patterns_of(globs, "input glob", false)?;
        let excluded = Regex::new(excluded).map_err(|error| format!("the input glob exclusions cannot be matched: {error}"))?;
        Ok(Globs {
            patterns,
            never_lifted: excluded.clone(),
            excluded,
        })
    }

    /// The matcher of `globs`, each a `noun`, held by [`EXCLUDED`] and by [`NEVER_LIFTED`] for a glob that names
    /// `TestResults` or `.tmp`; else why one of them cannot be matched, naming it by `noun`.
    fn named(globs: &[String], noun: &str) -> Result<Globs, String> {
        Ok(Globs {
            patterns: patterns_of(globs, noun, true)?,
            excluded: Regex::clone(&EXCLUDED),
            never_lifted: Regex::clone(&NEVER_LIFTED),
        })
    }

    /// This matcher narrowed to its globs that name a `TestResults` folder ([`test_results_segments`]), in their order
    /// and held by the same exclusions: those a failed command's outputs are narrowed to, so its test reports come back
    /// and nothing else does. `None` when no glob names one.
    #[must_use]
    pub fn test_results_only(&self) -> Option<Globs> {
        let patterns: Vec<Pattern> = self.patterns.iter().filter(|pattern| pattern.test_results.is_some()).cloned().collect();
        if patterns.is_empty() {
            return None;
        }
        Some(Globs {
            patterns,
            excluded: self.excluded.clone(),
            never_lifted: self.never_lifted.clone(),
        })
    }

    /// This matcher narrowed to its globs that name a `.tmp` folder ([`tmp_segments`]), in their order and held by the
    /// same exclusions: those the box lists before the command under `OutputsFull`, where only a `.tmp` file's stamp
    /// decides whether it comes back. `None` when no glob names one.
    #[must_use]
    pub fn tmp_only(&self) -> Option<Globs> {
        let patterns: Vec<Pattern> = self.patterns.iter().filter(|pattern| pattern.tmp.is_some()).cloned().collect();
        if patterns.is_empty() {
            return None;
        }
        Some(Globs {
            patterns,
            excluded: self.excluded.clone(),
            never_lifted: self.never_lifted.clone(),
        })
    }

    /// This matcher's globs followed by `more`'s, in their order, held by this matcher's exclusions: the set a failed
    /// command brings back, its test reports' globs joined with its failure output globs, both output matchers.
    #[must_use]
    pub fn joined(&self, more: &Globs) -> Globs {
        Globs {
            patterns: self.patterns.iter().chain(&more.patterns).cloned().collect(),
            excluded: self.excluded.clone(),
            never_lifted: self.never_lifted.clone(),
        }
    }

    /// Whether this matcher holds no glob, and so matches nothing: a walk under it finds nothing to list.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Whether the file at the `/`-separated relative `path` may come back: [`Globs::selects`] it, it is neither a
    /// `.git*` path nor a `.lfsconfig`, 8.3 aliases included ([`is_git_s`], [`is_lfs_config`]), and its name fits the
    /// outputs archive ([`fits`]).
    #[must_use]
    pub fn matches(&self, path: &str) -> bool {
        fits(path) && !is_git_s(path) && !is_lfs_config(path) && self.selects(path)
    }

    /// Whether the glob at `index` of those this matcher was built from brings back the file at the `/`-separated
    /// relative `path`: it matches it and, as [`Globs::selects`] asks, its own exclusions leave it.
    #[must_use]
    pub fn matched_by(&self, index: usize, path: &str) -> bool {
        let Some(pattern) = self.patterns.get(index) else {
            return false;
        };
        !is_spelled_git_s(path) && !is_spelled_lfs_config(path) && pattern.brings_back(path, self.exclusions_of(path))
    }

    /// Whether the file at the `/`-separated relative `path` would come back but for its name: [`Globs::selects`] it,
    /// yet its name does not fit the outputs archive ([`fits`]).
    #[must_use]
    pub fn refuses_name(&self, path: &str) -> bool {
        !fits(path) && self.selects(path)
    }

    /// Whether a glob matches the file at the `/`-separated relative `path`, no exclusion holds it for that glob, and no
    /// component of it is spelled as a `.git*` name or a `.lfsconfig` ([`is_spelled_git_s`], [`is_spelled_lfs_config`]);
    /// an 8.3 alias of one counts as any other name. A glob that names `TestResults` or `.tmp` folders is held by every
    /// arm of [`NEVER_LIFTED_ARMS`], and by [`EXCLUDED_TEST_RESULTS`] and [`EXCLUDED_TMP`] only for a path below more of
    /// those folders than it names; every other glob is held by every arm of [`EXCLUDED_ARMS`].
    #[must_use]
    pub fn selects(&self, path: &str) -> bool {
        if is_spelled_git_s(path) || is_spelled_lfs_config(path) {
            return false;
        }
        let exclusions = self.exclusions_of(path);
        self.patterns.iter().any(|pattern| pattern.brings_back(path, exclusions))
    }

    /// What this matcher's exclusions say of the file at the `/`-separated relative `path`, worked out once for the
    /// patterns of a call: [`Globs::never_lifted`] and the `TestResults` and `.tmp` folders are asked only of a path
    /// [`Globs::excluded`] holds.
    fn exclusions_of(&self, path: &str) -> Exclusions {
        if !self.excluded.is_match(path) {
            return Exclusions {
                held: false,
                beyond: false,
                test_results: 0,
                tmp: 0,
            };
        }
        let folders = path.rsplit_once('/').map_or("", |(folders, _)| folders);
        Exclusions {
            held: true,
            beyond: self.never_lifted.is_match(path),
            test_results: components(folders, &TEST_RESULTS_SEGMENT),
            tmp: components(folders, &TMP_SEGMENT),
        }
    }

    /// Whether a walk enters the folder at the `/`-separated relative `path`, whose folders it entered: no exclusion
    /// holds everything in it, it is no `.git*` folder ([`is_git_s`]) and its own name is plain ([`is_plain_part`]). A
    /// `TestResults` or `.tmp` folder is entered when a glob of the set names it on the way to it, and only then.
    #[must_use]
    pub fn enters(&self, path: &str) -> bool {
        !is_git_s(path) && path.rsplit('/').next().is_some_and(is_plain_part) && !self.holds_folder(path)
    }

    /// Whether a walk reads the folder at the `/`-separated relative `path` only to count the files below it that
    /// [`Globs::refuses_name`] would: no exclusion holds everything in it, no component of it is spelled as a `.git*`
    /// name, and it has fewer than [`PATH_DEPTH_LIMIT`] parts, so a file below it may have that many. A `TestResults` or
    /// `.tmp` folder a glob names is counted below as any other.
    #[must_use]
    pub fn counts_below(&self, path: &str) -> bool {
        !is_spelled_git_s(path) && path.split('/').count() < PATH_DEPTH_LIMIT && !self.holds_folder(path)
    }

    /// Whether an exclusion holds everything in the folder at the `/`-separated relative `path`: [`Globs::excluded`]
    /// holds it, and no glob that names the `TestResults` and `.tmp` folders it lies under reaches below it. Every arm of
    /// [`NEVER_LIFTED_ARMS`] holds a folder whatever the globs name, and is asked first.
    fn holds_folder(&self, path: &str) -> bool {
        let folder = format!("{path}/");
        if !self.excluded.is_match(&folder) {
            return false;
        }
        if self.never_lifted.is_match(&folder) {
            return true;
        }
        let test_results = components(path, &TEST_RESULTS_SEGMENT);
        let tmp = components(path, &TMP_SEGMENT);
        !self.patterns.iter().any(|pattern| pattern.reaches(path, test_results, tmp))
    }
}

impl Pattern {
    /// Whether this glob brings back the file at the `/`-separated relative `path`: it matches it and, its exclusions
    /// being `exclusions`, no arm holds it for that glob alone.
    fn brings_back(&self, path: &str, exclusions: Exclusions) -> bool {
        self.regex.is_match(path) && (!exclusions.held || (!exclusions.beyond && self.lifts(exclusions.test_results, exclusions.tmp)))
    }

    /// Whether the glob lifts [`EXCLUDED_TEST_RESULTS`] and [`EXCLUDED_TMP`] for a file with `test_results`
    /// `TestResults` folders and `tmp` `.tmp` folders above it: it names at least that many of each.
    fn lifts(&self, test_results: usize, tmp: usize) -> bool {
        test_results <= self.test_results.as_ref().map_or(0, |named| named.folders) && tmp <= self.tmp.as_ref().map_or(0, |named| named.folders)
    }

    /// Whether the walk reads below the folder at the `/`-separated relative `path`, which lies under `test_results`
    /// `TestResults` folders and `tmp` `.tmp` folders: the glob names at least that many of each, the path is at or below
    /// the first of those it names of each kind the path lies under, and no deeper below its last `.tmp` folder than the
    /// glob's folders reach ([`Named::walks_below`]).
    fn reaches(&self, path: &str, test_results: usize, tmp: usize) -> bool {
        self.lifts(test_results, tmp)
            && (test_results == 0 || self.test_results.as_ref().is_some_and(|named| named.prefix.is_match(path)))
            && (tmp == 0 || self.tmp.as_ref().is_some_and(|named| named.walks_below(path, tmp)))
    }
}

impl Named {
    /// Whether the walk reads below the folder at the `/`-separated relative `path`, whose `nth` `.tmp` folder is its
    /// last: the path is at or below the glob's first named `.tmp` segment, and it lies no more folders below that `.tmp`
    /// folder than the glob has below its `nth` named one ([`Named::levels`]).
    fn walks_below(&self, path: &str, nth: usize) -> bool {
        let Some(levels) = nth.checked_sub(1).and_then(|index| self.levels.get(index)) else {
            return false;
        };
        self.prefix.is_match(path) && levels.is_none_or(|limit| depth_below(path, nth, &TMP_SEGMENT).is_some_and(|depth| depth <= limit))
    }
}

/// The [`Pattern`]s of `globs`, each a `noun`, finding each glob's `TestResults` and `.tmp` folders when `naming` — the
/// output and `--bring-tracked` matchers, whose exclusions have arms to lift — and leaving them out for the input globs;
/// else why one cannot be matched, naming it by `noun`.
fn patterns_of(globs: &[String], noun: &str, naming: bool) -> Result<Vec<Pattern>, String> {
    globs.iter().map(|glob| pattern_of(glob, noun, naming)).collect()
}

/// The [`Pattern`] of `glob`, one of the `noun`s, its `TestResults` and `.tmp` folders found when `naming`; else why it
/// cannot be matched, naming it by `noun`.
fn pattern_of(glob: &str, noun: &str, naming: bool) -> Result<Pattern, String> {
    let rejected = |error: regex::Error| format!("the {noun} '{}' cannot be matched: {error}", escaped(glob));
    let segments: Vec<&str> = glob.split('/').collect();
    let (test_results, tmp) = if naming {
        (
            named_of(&segments, &test_results_segments(&segments)).map_err(rejected)?,
            named_of(&segments, &tmp_segments(&segments)).map_err(rejected)?,
        )
    } else {
        (None, None)
    };
    Ok(Pattern {
        regex: Regex::new(&regex_of(glob, false)).map_err(rejected)?,
        test_results,
        tmp,
    })
}

/// Whether the glob names a `.tmp` folder ([`tmp_segments`]), so it may bring back a file below one.
#[must_use]
pub fn names_tmp(glob: &str) -> bool {
    let segments: Vec<&str> = glob.split('/').collect();
    !tmp_segments(&segments).is_empty()
}

/// The indexes of the glob's `segments` that name a `TestResults` folder: its folder segments — every segment but the
/// last, which names a file — that are exactly `TestResults`, in any letter case. A segment that only holds the word
/// (`*TestResults*`, `TestResults2`, `Test*`) names none.
fn test_results_segments(segments: &[&str]) -> Vec<usize> {
    let folders = segments.len().saturating_sub(1);
    (0..folders).filter(|&index| TEST_RESULTS_SEGMENT.is_match(segments[index])).collect()
}

/// The indexes of the glob's `segments` that name a `.tmp` folder: its folder segments that are exactly `.tmp`, in any
/// letter case, and follow no segment holding `**` with other text beside it (`x**/.tmp`, whose `**/` may match
/// nothing and fuse the `.tmp` into `x.tmp`). A segment that only holds the word (`.t*p`, `.tmp*`) names none.
fn tmp_segments(segments: &[&str]) -> Vec<usize> {
    let folders = segments.len().saturating_sub(1);
    let fused = segments[..folders]
        .iter()
        .position(|segment| segment.contains("**") && *segment != "**")
        .unwrap_or(folders);
    (0..fused).filter(|&index| TMP_SEGMENT.is_match(segments[index])).collect()
}

/// The folders a glob of `segments` names at the indexes `named`, `None` when it names none: how many, the matcher of
/// the glob's path up to and including the first ([`regex_upto`]), and the folder levels the glob has below each.
fn named_of(segments: &[&str], named: &[usize]) -> Result<Option<Named>, regex::Error> {
    let Some(&first) = named.first() else {
        return Ok(None);
    };
    let folders = segments.len().saturating_sub(1);
    let levels = named
        .iter()
        .map(|&index| {
            let below = index + 1;
            (!segments[below..].iter().any(|segment| segment.contains("**"))).then(|| folders.saturating_sub(below))
        })
        .collect();
    Ok(Some(Named {
        folders: named.len(),
        prefix: Regex::new(&regex_upto(&segments[..=first].join("/")))?,
        levels,
    }))
}

/// The number of the `/`-separated components of `path` that `segment` matches.
fn components(path: &str, segment: &Regex) -> usize {
    path.split('/').filter(|part| segment.is_match(part)).count()
}

/// How many components of the `/`-separated `path` follow its `nth` component that `segment` matches; `None` when it
/// has fewer such components.
fn depth_below(path: &str, nth: usize, segment: &Regex) -> Option<usize> {
    let parts: Vec<&str> = path.split('/').collect();
    let index = parts
        .iter()
        .enumerate()
        .filter(|(_, part)| segment.is_match(part))
        .nth(nth.checked_sub(1)?)
        .map(|(index, _)| index)?;
    Some(parts.len() - 1 - index)
}

/// Whether the `/`-separated `path` fits the outputs archive: a plain path ([`is_plain_path`]) other than the archive's
/// own list, [`LIST_NAME`], in any letter case.
fn fits(path: &str) -> bool {
    is_plain_path(path) && !path.eq_ignore_ascii_case(LIST_NAME)
}

/// `glob` as an anchored, case-insensitive regex naming the first place a path spells its own parts as whole components:
/// its leading `**/` matches as few folders as it can, and the one capture group holds the folders themselves, its end
/// standing at a component boundary. What a caller names the `/`-separated prefix a glob's folders end at with (the
/// Windows box's stale-outputs check).
#[must_use]
pub fn regex_upto(glob: &str) -> String {
    regex_of(glob, true)
}

/// `glob` as an anchored, case-insensitive regex; `upto` puts its whole match in one capture group ending at a component
/// boundary and lets a leading `**/` match as few folders as it can ([`regex_upto`]), else it ends at the path's end and
/// `**/` takes as many as it can.
fn regex_of(glob: &str, upto: bool) -> String {
    let (open, any_folders, close) = if upto {
        ("(?i)^(", "(?:.*?/)?", ")(?:/|$)")
    } else {
        ("(?i)^", "(?:.*/)?", "$")
    };
    let mut text = String::from(open);
    let mut rest = glob;
    while let Some(character) = rest.chars().next() {
        if let Some(after) = rest.strip_prefix("**/") {
            text.push_str(any_folders);
            rest = after;
        } else if let Some(after) = rest.strip_prefix("**") {
            text.push_str(".*");
            rest = after;
        } else {
            match character {
                '*' => text.push_str("[^/]*"),
                '?' => text.push_str("[^/]"),
                other => text.push_str(&regex::escape(other.encode_utf8(&mut [0; 4]))),
            }
            rest = &rest[character.len_utf8()..];
        }
    }
    text.push_str(close);
    text
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a glob that cannot be matched must fail the test loudly")]
mod tests {
    use super::Globs;

    fn globs(globs: &[&str]) -> Globs {
        let owned: Vec<String> = globs.iter().map(|glob| (*glob).to_owned()).collect();
        Globs::new(&owned).expect("the globs can be matched")
    }

    /// The regex of each glob of `matcher`, in its order.
    fn regexes(matcher: &Globs) -> Vec<String> {
        matcher.patterns.iter().map(|pattern| pattern.regex.as_str().to_owned()).collect()
    }

    /// The regexes of the globs `of` narrowed to those that name a `TestResults` folder, `None` when none does.
    fn narrowed(of: &[&str]) -> Option<Vec<String>> {
        globs(of).test_results_only().map(|only| regexes(&only))
    }

    #[test]
    fn test_results_only_keeps_a_glob_with_a_test_results_folder() {
        let kept = ["**/TestResults/**", "src/TestResults/*.trx"];
        assert_eq!(narrowed(&kept), Some(regexes(&globs(&kept))));
    }

    #[test]
    fn test_results_only_keeps_a_test_results_folder_in_any_letter_case() {
        let kept = ["**/testresults/**"];
        assert_eq!(narrowed(&kept), Some(regexes(&globs(&kept))));
    }

    #[test]
    fn test_results_only_drops_a_glob_without_a_test_results_folder() {
        for dropped in ["**", "bin/**", "*TestResults*/**", "out/TestResults"] {
            assert_eq!(narrowed(&[dropped]), None, "{dropped} names no TestResults folder");
        }
    }

    #[test]
    fn test_results_only_keeps_the_order_of_a_mixed_list() {
        let mixed = ["b/TestResults/*.trx", "bin/**", "a/TestResults/**", "**"];
        assert_eq!(narrowed(&mixed), Some(regexes(&globs(&["b/TestResults/*.trx", "a/TestResults/**"]))));
        let only = globs(&mixed).test_results_only().expect("two globs name TestResults");
        assert!(only.matches("b/TestResults/r.trx"), "a kept glob still lifts the TestResults exclusion");
        assert!(!only.matches("bin/a.dll"), "a dropped glob brings nothing back");
        assert!(!only.matches("a/TestResults/.tmp/r.trx"), "the other exclusions still hold");
    }

    #[test]
    fn a_glob_too_large_to_match_is_named_by_its_option() {
        let huge = vec!["?".repeat(200_000)];
        let output = Globs::new(&huge).err().expect("the glob is past the matcher's size limit");
        assert!(output.starts_with("the output glob '???"), "{}", &output[..40]);
        let brought = Globs::bring_tracked(&huge).err().expect("the glob is past the matcher's size limit");
        assert!(brought.starts_with("the --bring-tracked glob '???"), "{}", &brought[..40]);
        assert!(brought.contains("' cannot be matched: "), "the matcher's reason follows the glob");
    }

    /// Checks `glob` matches each of `yes` and none of `no`.
    fn check(glob: &str, yes: &[&str], no: &[&str]) {
        let matcher = globs(&[glob]);
        for path in yes {
            assert!(matcher.matches(path), "{glob} matches {path}");
        }
        for path in no {
            assert!(!matcher.matches(path), "{glob} does not match {path}");
        }
    }

    #[test]
    fn a_double_star_and_slash_is_any_number_of_folders_none_included() {
        check(
            "**/bin/**",
            &["bin/a.dll", "src/app/bin/Debug/a.dll"],
            &["binx/a.dll", "abin/a.dll", "src/bin", "bin", "bin/sub/"],
        );
    }

    #[test]
    fn a_double_star_is_anything_slashes_included() {
        check("**", &["a", "a/b/c.dll"], &[]);
        check("out**.log", &["out.log", "out/x/y.log", "outer.log"], &["out.txt", "x/out.log"]);
    }

    #[test]
    fn a_star_is_any_run_of_characters_within_one_name() {
        check("*.dll", &["a.dll", ".dll"], &["x/a.dll", "a.dll.bak"]);
        check("bin/*/a.dll", &["bin/Debug/a.dll"], &["bin/a.dll", "bin/x/y/a.dll", "bin//a.dll"]);
    }

    #[test]
    fn the_cargo_output_globs_reach_a_profile_folder_s_own_files_only() {
        let matcher = globs(&["target/*/*.exe", "target/*/*.dll", "target/*/*.pdb"]);
        for kept in ["target/debug/x.exe", "target/release/x.dll", "target/debug/x.pdb", "target/release/x.exe"] {
            assert!(matcher.matches(kept), "{kept} is brought back");
        }
        for skipped in [
            "target/debug/deps/x.exe",
            "target/debug/build/x/y.exe",
            "target/debug/incremental/a/b.pdb",
            "target/x.exe",
            "x/target/debug/y.exe",
        ] {
            assert!(!matcher.matches(skipped), "{skipped} is never brought back");
        }
    }

    #[test]
    fn a_question_mark_is_one_character_within_one_name() {
        check("bin/?.dll", &["bin/a.dll", "bin/é.dll"], &["bin/ab.dll", "bin/.dll", "bin//.dll"]);
    }

    #[test]
    fn every_other_character_is_itself() {
        check("out/file+1 (x).txt", &["out/file+1 (x).txt"], &["out/filee1 (x).txt", "out/file1 x.txt"]);
        check("a.b", &["a.b"], &["axb"]);
        check("x[1]^$.txt", &["x[1]^$.txt"], &["x1.txt"]);
    }

    #[test]
    fn globs_match_without_regard_to_case_non_ascii_letters_included() {
        check("**/bin/**", &["BIN/A.DLL", "Src/Bin/a.dll"], &[]);
        check("über/*.dll", &["ÜBER/a.dll", "über/Ünïcødé ✓.dll"], &["uber/a.dll"]);
    }

    #[test]
    fn a_path_matches_when_any_glob_does_and_never_with_no_globs() {
        let matcher = globs(&["*.dll", "obj/**"]);
        assert!(matcher.matches("a.dll"));
        assert!(matcher.matches("obj/x/y.json"));
        assert!(!matcher.matches("a.pdb"));
        assert!(!globs(&[]).matches("a.dll"), "no globs match nothing");
    }

    #[test]
    fn the_exclusions_spare_nothing_under_git_tmp_or_test_results_nor_an_env_file() {
        let matcher = globs(&["**"]);
        for excluded in [
            ".tmp/bin/a.dll",
            "TestResults/bin/b.dll",
            "src/TestResults/c.trx",
            "src/testresults/c.trx",
            ".git/bin/d",
            "a/.GIT/config",
            "bin/.env",
            ".env",
            "bin/app.env",
            "bin/APP.ENV",
            "bin/.env.local",
            ".env.x",
        ] {
            assert!(!matcher.matches(excluded), "{excluded} is never brought back");
        }
        for kept in [
            "bin/env",
            "bin/a.envx",
            "bin/.envx",
            "src/.tmpx/a",
            "src/x.tmp/a",
            "MyTestResults/a",
            "bin/.env/a.dll",
        ] {
            assert!(matcher.matches(kept), "{kept} may come back");
        }
    }

    #[test]
    fn a_glob_naming_test_results_reaches_below_it_and_no_other_glob_does() {
        let trx = "tests/Yaat.Sim.Tests/bin/Release/net10.0/TestResults/sim.trx";
        let folder = "tests/Yaat.Sim.Tests/bin/Release/net10.0/TestResults";
        let named = globs(&["tests/*/bin/*/*/TestResults/*.trx"]);
        assert!(named.matches(trx), "a glob that names TestResults brings back a file below it");
        assert!(named.enters(folder), "and the walk enters the folder");
        let below = globs(&["tests/*/bin/*/*/testresults/**"]);
        assert!(below.matches(trx), "named in any letter case");
        assert!(
            !below.matches("tests/p/bin/Release/net10.0/TestResults/.tmp/a.trx"),
            ".tmp stays excluded below it"
        );
        assert!(
            !below.matches("tests/p/bin/Release/net10.0/TestResults/.env"),
            ".env stays excluded below it"
        );
        for other in ["**/bin/**", "**", "tests/**", "tests/*/bin/*/*/*TestResults*/*.trx"] {
            assert!(
                !globs(&[other]).matches(trx),
                "{other} does not name TestResults, so it never brings it back"
            );
            assert!(!globs(&[other]).enters(folder), "{other} does not enter it");
        }
        let mixed = globs(&["**/bin/**", "tests/*/bin/*/*/TestResults/*.trx"]);
        assert!(mixed.matches(trx), "in a set, the glob that names it brings it back");
        assert!(
            !mixed.matches("tests/p/bin/Release/net10.0/TestResults/coverage.xml"),
            "and only what that glob matches"
        );
        let outer = globs(&["out/**/TestResults/*.trx"]);
        assert!(
            outer.matches("out/a/TestResults/a.trx"),
            "a glob's ** reaches the TestResults folder the glob names"
        );
        assert!(
            !outer.matches("out/TestResults/x/TestResults/a.trx"),
            "an outer TestResults folder the glob never named is not carried through"
        );
        assert!(
            !globs(&["**/TestResults"]).matches("src/TestResults/logs/TestResults"),
            "a last segment naming TestResults names a file, so it lifts nothing"
        );
        let naming = globs(&["**/bin/**", "tests/*/bin/*/*/TestResults/*.trx"]);
        assert!(naming.enters(folder), "the walk reads below the folder the glob names");
        assert!(naming.counts_below(folder), "and counts below it");
        for unreached in ["src/TestResults", "TestResults"] {
            assert!(!naming.enters(unreached), "{unreached} is not on the way to the folder the glob names");
            assert!(!naming.counts_below(unreached), "{unreached} is not on the way to it");
        }
        for excluded in [
            "tests/Yaat.Sim.Tests/bin/Release/net10.0/TestResults/.tmp",
            "tests/Yaat.Sim.Tests/bin/Release/net10.0/TestResults/.git",
        ] {
            assert!(!naming.enters(excluded), "{excluded} stays excluded below the folder");
            assert!(!naming.counts_below(excluded), "{excluded} stays excluded below the folder");
        }
        let folded = globs(&["a/TestResults/*/x.trx"]);
        assert!(
            !folded.matches("a/Teſtresults/TestResults/x.trx"),
            "a TestResults folder spelled past ASCII is counted as the exclusion counts it"
        );
        assert!(!folded.enters("a/Teſtresults/TestResults"), "and the walk does not read below it");
        assert!(
            !outer.enters("out/TestResults/x/TestResults"),
            "a folder past the TestResults folders the glob names is not read"
        );
        assert!(!outer.counts_below("out/TestResults/x/TestResults"), "nor counted below");
        assert!(mixed.matched_by(1, trx), "the glob that names TestResults brought the TRX back");
        assert!(!mixed.matched_by(0, trx), "the other glob did not");
        for index in [0, 1] {
            assert!(
                !mixed.matched_by(index, "tests/p/bin/Release/net10.0/TestResults/.tmp/a.trx"),
                "no glob brings a .tmp path back, so none brought it"
            );
        }
        let inputs = Globs::inputs(&["tests/*/bin/*/*/TestResults/*.trx".to_owned()], r"(?i)(^|/)TestResults/")
            .expect("the input glob and its exclusions can be matched");
        assert!(
            !inputs.matches(trx),
            "an input matcher has no TestResults arm to lift, so its own holds the TRX"
        );
        assert!(!inputs.matched_by(0, trx), "so no glob of it brought the TRX back");
    }

    #[test]
    fn a_walk_enters_no_excluded_folder() {
        let matcher = globs(&["**"]);
        for excluded in [".git", "a/.tmp", "x/testresults", "TestResults"] {
            assert!(!matcher.enters(excluded), "{excluded} is not entered");
        }
        for entered in ["bin", "a/.tmpx", ".env", "Über"] {
            assert!(matcher.enters(entered), "{entered} is entered");
        }
    }

    #[test]
    fn a_git_path_or_an_lfsconfig_is_never_brought_back() {
        let matcher = globs(&["**"]);
        for excluded in [
            ".gitattributes",
            "sub/dir/.gitattributes",
            ".gitignore",
            ".GitModules",
            ".lfsconfig",
            "a/.LFSCONFIG",
            ".lfsconfig.",
            ".lfsconfig ",
            r"bin\.gitattributes",
            "bin/.gitattributes:stream",
            "GITATT~1",
            "bin/LFSCON~1",
            ".github/workflows/x.yml",
        ] {
            assert!(!matcher.matches(excluded), "{excluded} is never brought back");
        }
        for kept in [
            "bin/agit/x.dll",
            "bin/x.gitkeep.dll",
            "bin/lfsconfig.txt",
            ".gıtattributes",
            ".lfſconfig",
            "bin/app.dll",
        ] {
            assert!(matcher.matches(kept), "{kept} may come back");
        }
    }

    #[test]
    fn a_star_glob_brings_back_no_git_path_or_lfsconfig_either() {
        let matcher = globs(&["*"]);
        for excluded in [".gitattributes", ".gitignore", ".lfsconfig"] {
            assert!(!matcher.matches(excluded), "{excluded} is never brought back");
        }
        assert!(matcher.matches("app.dll"), "the other names still match");
    }

    #[test]
    fn a_walk_enters_no_git_path() {
        let matcher = globs(&["**"]);
        for excluded in [".github", "sub/.gitlab", "GIT~1"] {
            assert!(!matcher.enters(excluded), "{excluded} is not entered");
        }
        for entered in ["bin", "bin/agit"] {
            assert!(matcher.enters(entered), "{entered} is entered");
        }
    }

    #[test]
    fn a_name_the_archive_reader_refuses_is_never_brought_back() {
        let matcher = globs(&["**"]);
        let deep = vec!["a"; 65].join("/");
        for refused in [
            "bin/m~10.txt",
            "bin/report.",
            "bin/report ",
            "bin/COM1.txt",
            "con/x",
            "bin/a\u{202e}b.dll",
            deep.as_str(),
        ] {
            assert!(!matcher.matches(refused), "{refused} is never brought back");
        }
        let deepest = vec!["a"; 64].join("/");
        for kept in ["bin/a.txt", "bin/l~x.txt", deepest.as_str()] {
            assert!(matcher.matches(kept), "{kept} may come back");
        }
    }

    #[test]
    fn a_walk_enters_no_folder_whose_name_the_archive_reader_refuses() {
        let matcher = globs(&["**"]);
        for refused in ["bin/con", "bin/x~1", "nul.d", "bin/a\u{202e}b"] {
            assert!(!matcher.enters(refused), "{refused} is not entered");
        }
        assert!(matcher.enters(&vec!["a"; 65].join("/")), "a folder's depth alone does not stop the walk");
    }

    #[test]
    fn the_archive_s_own_list_name_is_refused_as_an_output_at_the_root_only() {
        let matcher = globs(&["**"]);
        for refused in [".gate-outputs.json", ".GATE-Outputs.JSON"] {
            assert!(!matcher.matches(refused), "{refused} is never brought back");
            assert!(matcher.refuses_name(refused), "{refused} is refused for its name");
        }
        assert!(matcher.matches("bin/.gate-outputs.json"), "a list name below the root may come back");
    }

    #[test]
    fn a_name_is_refused_aloud_unless_it_is_a_spelled_git_or_lfsconfig_name_or_excluded() {
        let matcher = globs(&["**"]);
        for loud in ["bin/l~10.txt", "GITATT~1", "bin/LFSCON~1", "bin/m~10.txt", "bin/report."] {
            assert!(matcher.refuses_name(loud), "{loud} is refused for its name");
        }
        for silent in [
            ".gitattributes",
            ".lfsconfig",
            ".lfsconfig.",
            ".github/x~1.txt",
            ".tmp/m~1.txt",
            "bin/ok.txt",
        ] {
            assert!(!matcher.refuses_name(silent), "{silent} is not refused for its name");
        }
        assert!(!globs(&["*.dll"]).refuses_name("bin/m~10.txt"), "a name no glob matches is not refused");
    }

    #[test]
    fn a_walk_counts_below_a_folder_whose_name_is_not_plain_down_to_the_depth_limit() {
        let matcher = globs(&["**"]);
        let deep = vec!["a"; 63].join("/");
        for counted in ["bin/x~1", "bin/con", "bin/report.", "GIT~1", deep.as_str()] {
            assert!(matcher.counts_below(counted), "{counted} is read to count its files");
        }
        let too_deep = vec!["a"; 64].join("/");
        for silent in [".github/x~1", ".tmp/x~1", "a/.git", too_deep.as_str()] {
            assert!(!matcher.counts_below(silent), "{silent} is not read");
        }
    }

    /// A run nonce: 32 hex digits.
    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn a_glob_naming_tmp_reaches_the_tmp_folder_it_names() {
        check(".tmp/x.json", &[".tmp/x.json", ".TMP/X.json"], &["a/.tmp/x.json", "x.json"]);
        check(".TMP/*.json", &[".Tmp/x.json"], &[".tmp/a/x.json"]);
        check(".tmp/dumps/**", &[".tmp/dumps/r1/a.dmp", ".tmp/dumps/a.dmp"], &[".tmp/other/a.dmp"]);
        check("**/.tmp/x", &[".tmp/x", "src/.tmp/x"], &["src/.tmp/y"]);
        check(".tmp/**/.tmp/x", &[".tmp/a/.tmp/x"], &[]);
        check("src/.tmp/x", &["src/.tmp/x"], &["other/.tmp/x"]);
        let run = format!(".tmp/run-{NONCE}.json");
        check(".tmp/*", &[run.as_str(), ".tmp/yaat-1a2b3c4d.json"], &[]);
        check(".tmp/gate-*.log", &[".tmp/gate-build.log"], &[".tmp/other.log"]);
        assert!(globs(&[".tmp/x.json"]).enters(".tmp"), "the walk enters the .tmp folder the glob names");
        assert!(
            globs(&["**/.tmp/x"]).enters("src/.tmp"),
            "a leading ** reaches a .tmp folder at any depth"
        );
        assert!(super::names_tmp(".tmp/x.json"), "a literal .tmp folder is named");
    }

    #[test]
    fn a_glob_that_does_not_name_tmp_never_reaches_it() {
        check("**", &["src/.tmpx/a"], &[".tmp/x.json", "src/.tmp/x.json"]);
        for glob in [".t*p/x", ".tm?/x", "*/x", "**/x", ".tmp*/x"] {
            check(glob, &[], &[".tmp/x"]);
            assert!(!globs(&[glob]).enters(".tmp"), "{glob} does not enter .tmp");
            assert!(!super::names_tmp(glob), "{glob} names no .tmp folder");
        }
        check("bin/.tmp", &[], &["bin/.tmp/x"]);
        assert!(!super::names_tmp("bin/.tmp"), "a last segment names a file, not a folder");
    }

    #[test]
    fn a_wildcard_never_matches_a_second_tmp_folder() {
        check(".tmp/dumps/**", &[], &[".tmp/dumps/x/.tmp/a.dmp"]);
        check("**/.tmp/x", &[], &[".tmp/a/.tmp/x"]);
        check(".tmp/*/x", &[], &[".tmp/.tmp/x"]);
        check(".tmp/**/.tmp/x", &[], &[".tmp/a/.tmp/b/.tmp/x"]);
    }

    #[test]
    fn a_tmp_folder_after_a_fused_double_star_is_not_named() {
        check("x**/.tmp/a", &["x.tmp/a"], &["x/.tmp/a", "x/y/.tmp/a"]);
        assert!(!super::names_tmp("x**/.tmp/a"), "the .tmp after a fused ** is not named");
        assert!(!super::names_tmp("**x/.tmp/a"), "nor after a ** with text after it");
        assert!(super::names_tmp("**/.tmp/a"), "a whole ** segment leaves the .tmp after it named");
        assert!(!globs(&["x**/.tmp/a"]).enters("x/.tmp"), "the walk does not enter it");
    }

    #[test]
    fn the_gate_s_own_tmp_names_never_come_back_whatever_the_glob() {
        let key = "yaat-1a2b3c4d";
        let owned: Vec<String> = [
            ".tar.zst",
            "/bin/a.dll",
            ".inputs-diff",
            ".inputs.tar.zst",
            ".sent-build-diff",
            ".sent-build.tar.zst",
            ".lfs-diff",
            ".lfs.tar.zst",
        ]
        .iter()
        .map(|suffix| format!(".tmp/{key}-{NONCE}{suffix}"))
        .chain([format!(".TMP/YAAT-1A2B3C4D-{}.TAR.ZST", NONCE.to_uppercase())])
        .chain([format!(".tmp/my-repo-1a2b3c4d-{NONCE}.tar.zst")])
        .collect();
        for path in &owned {
            for glob in [".tmp/**", ".tmp/*", "**/.tmp/**", "**/.tmp/*", path.as_str()] {
                assert!(!globs(&[glob]).matches(path), "{glob} never brings back {path}");
                assert!(!globs(&[glob]).matched_by(0, path), "{glob} never brought back {path}");
            }
        }
        let stage = format!(".tmp/{key}-{NONCE}");
        for glob in [".tmp/**", "**/.tmp/**", ".tmp/*/bin/a.dll"] {
            assert!(!globs(&[glob]).enters(&stage), "{glob} never enters the stage {stage}");
            assert!(!globs(&[glob]).counts_below(&stage), "{glob} never counts below the stage {stage}");
        }
        let kept = [
            format!(".tmp/run-{NONCE}.json"),
            format!(".tmp/{key}.json"),
            format!(".tmp/{key}-{}.tar.zst", &NONCE[1..]),
            format!(".tmp/{key}-{NONCE}0.tar.zst"),
            format!(".tmp/sub/{key}-{NONCE}.tar.zst"),
            format!(".tmp/{key}-{}.tar.zst", NONCE.replace('0', "g")),
        ];
        for path in &kept {
            assert!(
                globs(&[".tmp/**"]).matches(path),
                "{path} is not the gate's own shape, so .tmp/** brings it back"
            );
        }
    }

    #[test]
    fn a_walk_enters_a_tmp_folder_only_for_a_glob_that_names_it() {
        let taxi = globs(&[".tmp/taxi-*.json"]);
        assert!(taxi.enters(".tmp") && taxi.counts_below(".tmp"), "a glob for .tmp's own files reads .tmp");
        for deeper in [".tmp/sub", ".tmp/sub/deeper"] {
            assert!(!taxi.enters(deeper), "{deeper} lies below what .tmp/taxi-*.json reaches");
            assert!(!taxi.counts_below(deeper), "{deeper} is not counted below");
        }
        let dumps = globs(&[".tmp/dumps/**"]);
        for entered in [".tmp", ".tmp/dumps", ".tmp/dumps/r1", ".tmp/dumps/r1/deep"] {
            assert!(dumps.enters(entered), "a ** after the named .tmp reaches {entered}");
        }
        for unentered in [".tmp/dumps/x/.tmp", "a/.tmp", ".tmp/dumps/.git"] {
            assert!(!dumps.enters(unentered), "{unentered} is not entered");
            assert!(!dumps.counts_below(unentered), "{unentered} is not counted below");
        }
        let two = globs(&["a/*/.tmp/q/r"]);
        assert!(
            two.enters("a/b/.tmp") && two.enters("a/b/.tmp/q"),
            "the walk goes as deep as the glob's folders"
        );
        assert!(!two.enters("a/b/.tmp/q/s"), "and no deeper");
        let nested = globs(&[".tmp/**/.tmp/x"]);
        assert!(
            nested.enters(".tmp/a/b") && nested.enters(".tmp/a/.tmp"),
            "a ** reaches the second named .tmp"
        );
        assert!(!nested.enters(".tmp/a/.tmp/b"), "below the second named .tmp the glob has no folder");
        assert!(!globs(&["src/.tmp/x"]).enters("other/.tmp"), "a .tmp folder elsewhere is not on the way");
        assert!(!globs(&["**"]).enters(".tmp"), "a glob that names no .tmp never enters one");
        let stage = format!(".tmp/yaat-1a2b3c4d-{NONCE}");
        assert!(!globs(&[".tmp/**"]).enters(&stage), "the gate's own stage is never entered");
    }

    #[test]
    fn a_glob_naming_tmp_and_test_results_lifts_both() {
        let both = globs(&[".tmp/TestResults/*.trx"]);
        assert!(both.matches(".tmp/TestResults/r.trx"), "the glob names both folders");
        assert!(both.enters(".tmp") && both.enters(".tmp/TestResults"), "the walk enters both");
        assert!(
            !globs(&[".tmp/**"]).matches(".tmp/TestResults/r.trx"),
            "a glob naming .tmp alone stops at TestResults"
        );
        assert!(!globs(&[".tmp/**"]).enters(".tmp/TestResults"), "and does not enter it");
        assert!(
            !globs(&["**/TestResults/*.trx"]).matches(".tmp/TestResults/r.trx"),
            "a glob naming TestResults alone stops at .tmp"
        );
        assert!(
            !globs(&["tests/*/bin/*/*/TestResults/*.trx"]).matches("tests/p/bin/Release/net10.0/TestResults/.tmp/a.trx"),
            "a .tmp below a TestResults folder stays excluded"
        );
        let mixed = globs(&["**/bin/**", ".tmp/TestResults/*.trx"]);
        assert!(mixed.test_results_only().is_some_and(|only| only.matches(".tmp/TestResults/r.trx")));
    }

    #[test]
    fn a_tmp_name_the_archive_refuses_is_refused_aloud_under_a_tmp_glob() {
        let named = globs(&[".tmp/**"]);
        assert!(named.refuses_name(".tmp/m~1.txt"), "a glob naming .tmp refuses the name aloud");
        assert!(!named.matches(".tmp/m~1.txt"), "and never brings it back");
        assert!(named.counts_below(".tmp/x~1"), "the walk counts below a folder whose name is not plain");
        assert!(
            !globs(&["**"]).refuses_name(".tmp/m~1.txt"),
            "a glob that does not name .tmp stays silent"
        );
        let stage = format!(".tmp/yaat-1a2b3c4d-{NONCE}/m~1.txt");
        assert!(!named.refuses_name(&stage), "the gate's own names stay silent");
    }

    #[test]
    fn bring_tracked_and_matched_by_follow_a_glob_naming_tmp() {
        let bring = Globs::bring_tracked(&[".tmp/*.json".to_owned()]).expect("the glob can be matched");
        assert!(bring.matches(".tmp/x.json"), "a --bring-tracked glob naming .tmp matches below it");
        assert!(
            !Globs::bring_tracked(&["**".to_owned()])
                .expect("the glob can be matched")
                .matches(".tmp/x.json")
        );
        let mixed = globs(&["**", ".tmp/dumps/**"]);
        assert!(mixed.matched_by(1, ".tmp/dumps/a.dmp"), "the glob naming .tmp brought the dump back");
        assert!(!mixed.matched_by(0, ".tmp/dumps/a.dmp"), "the other glob did not");
        let inputs = Globs::inputs(&[".tmp/x".to_owned()], r"(?i)(^|/)\.tmp/").expect("the input glob and its exclusions can be matched");
        assert!(!inputs.matches(".tmp/x"), "an input matcher lifts nothing for a glob naming .tmp");
        assert!(!inputs.enters(".tmp"), "nor enters the .tmp its exclusions hold");
    }
}
