//! The output globs as the PowerShell gate matched them (remote-wrapper.ps1's `ConvertTo-OutputRegex`), plus the paths
//! never brought back whatever the globs: `$OutputExcluded`'s, any `.git*` component or `.lfsconfig`, matched as the
//! sync-back matches them, and any path that does not fit the outputs archive: one that is not plain, which the
//! archive's reader refuses, or the archive's own list name. A glob that names a `TestResults` folder lifts that one
//! arm of the exclusions for itself ([`EXCLUDED_ARMS`], [`EXCLUDED_TEST_RESULTS`]).

use std::sync::LazyLock;

use gate_router::escaped;
use regex::Regex;

use crate::outputs::archive::LIST_NAME;
use crate::outputs::names::{PATH_DEPTH_LIMIT, is_git_s, is_lfs_config, is_plain_part, is_plain_path, is_spelled_git_s, is_spelled_lfs_config};

/// The arm of [`EXCLUDED_ARMS`] holding the contents of a `.git` or a `.tmp` folder at any depth. It stays though
/// [`is_git_s`] covers the same paths.
const EXCLUDED_GIT_TMP: &str = r"(^|/)(\.git|\.tmp)/";

/// The arm of [`EXCLUDED_ARMS`] holding the contents of a `TestResults` folder at any depth: the one a glob that names
/// the folder lifts for itself ([`Pattern`]).
const EXCLUDED_TEST_RESULTS: &str = r"(^|/)TestResults/";

/// The arm of [`EXCLUDED_ARMS`] holding a `.env` file (`.env`, `.env.<name>`, `<name>.env`).
const EXCLUDED_ENV: &str = r"(^|/)\.env\.[^/]*$|\.env$";

/// The arms of the outputs' exclusions, each a regex over a `/`-separated relative path, matched without regard to
/// case. Every glob is held by all of them but for a glob that names a `TestResults` folder, which lifts
/// [`EXCLUDED_TEST_RESULTS`] for itself alone.
const EXCLUDED_ARMS: [&str; 3] = [EXCLUDED_GIT_TMP, EXCLUDED_TEST_RESULTS, EXCLUDED_ENV];

/// The paths never brought back, whatever the globs: every arm of [`EXCLUDED_ARMS`].
static EXCLUDED: LazyLock<Regex> = LazyLock::new(|| exclusions_regex(&EXCLUDED_ARMS));

/// [`EXCLUDED`] without [`EXCLUDED_TEST_RESULTS`]: the paths never brought back even to a glob that names a
/// `TestResults` folder.
static EXCLUDED_BEYOND_TEST_RESULTS: LazyLock<Regex> = LazyLock::new(|| exclusions_regex(&arms_beyond_test_results()));

/// One `TestResults` folder segment, folded as [`EXCLUDED_TEST_RESULTS`] and the glob regexes fold it: without regard to
/// case, a non-ASCII spelling such as `Teſtresults` included.
static TEST_RESULTS_SEGMENT: LazyLock<Regex> = LazyLock::new(|| always_valid(r"(?i)^TestResults$"));

/// The exclusion `arms`, joined into one regex matched without regard to case.
fn exclusions_regex(arms: &[&str]) -> Regex {
    always_valid(&format!("(?i){}", arms.join("|")))
}

/// The arms of [`EXCLUDED_ARMS`] but [`EXCLUDED_TEST_RESULTS`].
fn arms_beyond_test_results() -> Vec<&'static str> {
    EXCLUDED_ARMS.iter().copied().filter(|arm| *arm != EXCLUDED_TEST_RESULTS).collect()
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
/// The `.git`, `.tmp` and `.env` arms of [`EXCLUDED_ARMS`] hold every glob. A glob one of whose folder segments is
/// exactly `TestResults`, in any letter case, lifts [`EXCLUDED_TEST_RESULTS`] for itself alone: it brings back what it
/// matches below a `TestResults` folder, at most as many such folders as it names, and the walk enters a `TestResults`
/// folder when a glob of the set names it on the way to it.
pub(crate) struct Globs {
    patterns: Vec<Pattern>,
    /// The exclusions every glob is held by: [`EXCLUDED`], or the inputs' own regex.
    excluded: Regex,
    /// The exclusions a glob that names a `TestResults` folder is held by instead: [`EXCLUDED_BEYOND_TEST_RESULTS`], or
    /// the inputs' own regex again.
    beyond_test_results: Regex,
}

/// What a matcher's exclusions say of one file's `/`-separated relative `path`, worked out once for the patterns of a
/// call.
#[derive(Clone, Copy)]
struct Exclusions {
    /// [`Globs::excluded`] holds the path.
    held: bool,
    /// [`Globs::beyond_test_results`] holds it, asked only when it is held.
    beyond: bool,
    /// The `TestResults` folders above it, counted only when it is held.
    components: usize,
}

/// One of [`Globs`]' globs.
struct Pattern {
    regex: Regex,
    /// The `TestResults` folders the glob names, `None` when it names none ([`test_results_of`]).
    test_results: Option<TestResults>,
}

/// The `TestResults` folders a glob names.
struct TestResults {
    /// How many of the glob's folder segments are exactly `TestResults`, in any letter case.
    folders: usize,
    /// The matcher of a path at or below the glob's folders up to and including its first `TestResults` segment
    /// ([`regex_upto`]): the walk reads below a `TestResults` folder only where this holds.
    prefix: Regex,
}

impl Globs {
    /// The matcher of the output globs `globs`; else why one of them cannot be matched.
    pub(crate) fn new(globs: &[String]) -> Result<Globs, String> {
        Globs::named(globs, "output glob")
    }

    /// The matcher of a routed run's `--bring-tracked` globs `globs`, which the output globs' exclusions hold for too;
    /// else why one of them cannot be matched.
    pub(crate) fn bring_tracked(globs: &[String]) -> Result<Globs, String> {
        Globs::named(globs, "--bring-tracked glob")
    }

    /// The matcher of a routed run's input globs `globs`, with the inputs' own exclusions `excluded`, a regex of the
    /// `/`-separated relative paths never sent, in place of the outputs' (so `.tmp` is reachable); they hold a glob that
    /// names `TestResults` as they hold any other, since they have no such arm to lift. The `.git*`, `.lfsconfig` and
    /// plain-name rules hold as for outputs. Else why a glob or the exclusions cannot be matched.
    pub(crate) fn inputs(globs: &[String], excluded: &str) -> Result<Globs, String> {
        let patterns = patterns_of(globs, "input glob", false)?;
        let excluded = Regex::new(excluded).map_err(|error| format!("the input glob exclusions cannot be matched: {error}"))?;
        Ok(Globs {
            patterns,
            beyond_test_results: excluded.clone(),
            excluded,
        })
    }

    /// The matcher of `globs`, each a `noun`, held by [`EXCLUDED`] and by [`EXCLUDED_BEYOND_TEST_RESULTS`] for a glob
    /// that names `TestResults`; else why one of them cannot be matched, naming it by `noun`.
    fn named(globs: &[String], noun: &str) -> Result<Globs, String> {
        Ok(Globs {
            patterns: patterns_of(globs, noun, true)?,
            excluded: Regex::clone(&EXCLUDED),
            beyond_test_results: Regex::clone(&EXCLUDED_BEYOND_TEST_RESULTS),
        })
    }

    /// Whether the file at the `/`-separated relative `path` may come back: [`Globs::selects`] it, it is neither a
    /// `.git*` path nor a `.lfsconfig`, 8.3 aliases included ([`is_git_s`], [`is_lfs_config`]), and its name fits the
    /// outputs archive ([`fits`]).
    pub(crate) fn matches(&self, path: &str) -> bool {
        fits(path) && !is_git_s(path) && !is_lfs_config(path) && self.selects(path)
    }

    /// Whether the glob at `index` of those this matcher was built from brings back the file at the `/`-separated
    /// relative `path`: it matches it and, as [`Globs::selects`] asks, its own exclusions leave it.
    pub(crate) fn matched_by(&self, index: usize, path: &str) -> bool {
        let Some(pattern) = self.patterns.get(index) else {
            return false;
        };
        !is_spelled_git_s(path) && !is_spelled_lfs_config(path) && pattern.brings_back(path, self.exclusions_of(path))
    }

    /// Whether the file at the `/`-separated relative `path` would come back but for its name: [`Globs::selects`] it,
    /// yet its name does not fit the outputs archive ([`fits`]).
    pub(crate) fn refuses_name(&self, path: &str) -> bool {
        !fits(path) && self.selects(path)
    }

    /// Whether a glob matches the file at the `/`-separated relative `path`, no exclusion holds it for that glob, and no
    /// component of it is spelled as a `.git*` name or a `.lfsconfig` ([`is_spelled_git_s`], [`is_spelled_lfs_config`]);
    /// an 8.3 alias of one counts as any other name. A glob that names a `TestResults` folder is held by every arm of
    /// [`EXCLUDED_ARMS`] but [`EXCLUDED_TEST_RESULTS`] for the paths it lifts that arm on; every other glob is held by all of
    /// them.
    pub(crate) fn selects(&self, path: &str) -> bool {
        if is_spelled_git_s(path) || is_spelled_lfs_config(path) {
            return false;
        }
        let exclusions = self.exclusions_of(path);
        self.patterns.iter().any(|pattern| pattern.brings_back(path, exclusions))
    }

    /// What this matcher's exclusions say of the file at the `/`-separated relative `path`, worked out once for the
    /// patterns of a call: [`Globs::beyond_test_results`] and the `TestResults` folders are asked only of a path
    /// [`Globs::excluded`] holds.
    fn exclusions_of(&self, path: &str) -> Exclusions {
        if !self.excluded.is_match(path) {
            return Exclusions {
                held: false,
                beyond: false,
                components: 0,
            };
        }
        Exclusions {
            held: true,
            beyond: self.beyond_test_results.is_match(path),
            components: test_results_above(path),
        }
    }

    /// Whether a walk enters the folder at the `/`-separated relative `path`, whose folders it entered: no exclusion
    /// holds everything in it, it is no `.git*` folder ([`is_git_s`]) and its own name is plain ([`is_plain_part`]). A
    /// `TestResults` folder is entered when a glob of the set names it on the way to it, and only then.
    pub(crate) fn enters(&self, path: &str) -> bool {
        !is_git_s(path) && path.rsplit('/').next().is_some_and(is_plain_part) && !self.holds_folder(path)
    }

    /// Whether a walk reads the folder at the `/`-separated relative `path` only to count the files below it that
    /// [`Globs::refuses_name`] would: no exclusion holds everything in it, no component of it is spelled as a `.git*`
    /// name, and it has fewer than [`PATH_DEPTH_LIMIT`] parts, so a file below it may have that many. A `TestResults`
    /// folder a glob names is counted below as any other.
    pub(crate) fn counts_below(&self, path: &str) -> bool {
        !is_spelled_git_s(path) && path.split('/').count() < PATH_DEPTH_LIMIT && !self.holds_folder(path)
    }

    /// Whether an exclusion holds everything in the folder at the `/`-separated relative `path`: [`Globs::excluded`]
    /// holds it, and no glob that names the `TestResults` folders it lies under reaches below it. Every arm but
    /// [`EXCLUDED_TEST_RESULTS`] holds a folder whatever the globs name.
    fn holds_folder(&self, path: &str) -> bool {
        let folder = format!("{path}/");
        if !self.excluded.is_match(&folder) {
            return false;
        }
        if self.beyond_test_results.is_match(&folder) {
            return true;
        }
        let components = test_results_components(path);
        !self.patterns.iter().any(|pattern| pattern.reaches(path, components))
    }
}

impl Pattern {
    /// Whether this glob brings back the file at the `/`-separated relative `path`: it matches it and, its exclusions
    /// being `exclusions`, no arm holds it for that glob alone.
    fn brings_back(&self, path: &str, exclusions: Exclusions) -> bool {
        self.regex.is_match(path) && (!exclusions.held || (self.lifts(exclusions.components) && !exclusions.beyond))
    }

    /// Whether the glob lifts [`EXCLUDED_TEST_RESULTS`] for a file with `components` `TestResults` folders above it: it
    /// names at least that many.
    fn lifts(&self, components: usize) -> bool {
        self.test_results.as_ref().is_some_and(|named| components <= named.folders)
    }

    /// Whether the walk reads below the folder at the `/`-separated relative `path`, which lies under `components`
    /// `TestResults` folders: the glob names at least that many, and the path is at or below the first of those it
    /// names.
    fn reaches(&self, path: &str, components: usize) -> bool {
        self.lifts(components) && self.test_results.as_ref().is_some_and(|named| named.prefix.is_match(path))
    }
}

/// The [`Pattern`]s of `globs`, each a `noun`, finding each glob's `TestResults` folders when `naming` — the output and
/// `--bring-tracked` matchers, whose exclusions have an arm to lift — and leaving them out for the input globs; else why
/// one cannot be matched, naming it by `noun`.
fn patterns_of(globs: &[String], noun: &str, naming: bool) -> Result<Vec<Pattern>, String> {
    globs.iter().map(|glob| pattern_of(glob, noun, naming)).collect()
}

/// The [`Pattern`] of `glob`, one of the `noun`s, its `TestResults` folders found when `naming`; else why it cannot be
/// matched, naming it by `noun`.
fn pattern_of(glob: &str, noun: &str, naming: bool) -> Result<Pattern, String> {
    let rejected = |error: regex::Error| format!("the {noun} '{}' cannot be matched: {error}", escaped(glob));
    let test_results = if naming { test_results_of(glob).map_err(rejected)? } else { None };
    Ok(Pattern {
        regex: Regex::new(&regex_of(glob, false)).map_err(rejected)?,
        test_results,
    })
}

/// The `TestResults` folders `glob` names, `None` when it names none: one of its folder segments — every segment but
/// the last, which names a file — is exactly `TestResults`, in any letter case. How many of them are, and the matcher
/// of the glob's path up to and including its first one ([`regex_upto`]). A segment that only holds the word
/// (`*TestResults*`, `TestResults2`, `Test*`) names none.
fn test_results_of(glob: &str) -> Result<Option<TestResults>, regex::Error> {
    let segments: Vec<&str> = glob.split('/').collect();
    let folders = segments.len().saturating_sub(1);
    let Some(first) = (0..folders).find(|&index| TEST_RESULTS_SEGMENT.is_match(segments[index])) else {
        return Ok(None);
    };
    let named = segments[..folders]
        .iter()
        .filter(|segment| TEST_RESULTS_SEGMENT.is_match(segment))
        .count();
    Ok(Some(TestResults {
        folders: named,
        prefix: Regex::new(&regex_upto(&segments[..=first].join("/")))?,
    }))
}

/// The number of the `/`-separated components of `path` that are a `TestResults` folder, folded as
/// [`TEST_RESULTS_SEGMENT`] folds them.
fn test_results_components(path: &str) -> usize {
    path.split('/').filter(|part| TEST_RESULTS_SEGMENT.is_match(part)).count()
}

/// The number of the folders above the file at the `/`-separated relative `path` that are a `TestResults` folder, folded
/// as [`TEST_RESULTS_SEGMENT`] folds them: every component but the file's own name.
fn test_results_above(path: &str) -> usize {
    path.rsplit_once('/').map_or(0, |(folders, _)| test_results_components(folders))
}

/// Whether the `/`-separated `path` fits the outputs archive: a plain path ([`is_plain_path`]) other than the archive's
/// own list, [`LIST_NAME`], in any letter case.
fn fits(path: &str) -> bool {
    is_plain_path(path) && !path.eq_ignore_ascii_case(LIST_NAME)
}

/// `glob` as an anchored, case-insensitive regex naming the first place a path spells its own parts as whole components:
/// its leading `**/` matches as few folders as it can, and the one capture group holds the folders themselves, its end
/// standing at a component boundary. What a caller names the `/`-separated prefix a glob's folders end at with
/// ([`crate::outputs::stale`]).
pub(crate) fn regex_upto(glob: &str) -> String {
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
}
