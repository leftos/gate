//! The output globs as the PowerShell gate matched them (remote-wrapper.ps1's `ConvertTo-OutputRegex`), plus the paths
//! never brought back whatever the globs: `$OutputExcluded`'s, any `.git*` component or `.lfsconfig`, matched as the
//! sync-back matches them, and any path that does not fit the outputs archive: one that is not plain, which the
//! archive's reader refuses, or the archive's own list name.

use regex::Regex;

use crate::boxside::payload::escaped;
use crate::outputs::archive::LIST_NAME;
use crate::outputs::names::{PATH_DEPTH_LIMIT, is_git_s, is_lfs_config, is_plain_part, is_plain_path, is_spelled_git_s, is_spelled_lfs_config};

/// The paths never brought back, whatever the globs: anything under a `.git`, `.tmp` or `TestResults` folder at any
/// depth, and a `.env` file (`.env`, `.env.<name>`, `<name>.env`). Matched without regard to case. Its `.git/` arm
/// stays though [`is_git_s`] covers the same paths.
const EXCLUDED: &str = r"(?i)(^|/)(\.git|\.tmp|TestResults)/|(^|/)\.env\.[^/]*$|\.env$";

/// A run's output globs. Each is relative to the work folder's root and `/`-separated: `**/` is any number of folders,
/// none included, `**` anything, `*` any run of characters within one name, `?` one character within one name, and
/// every other character itself. They match without regard to case, as the PowerShell gate's did. Whatever the globs, a
/// path a sync-back never writes — a `.git*` component or a `.lfsconfig`, read by [`is_git_s`] and [`is_lfs_config`] —
/// never comes back and its folder is never entered, nor does a path that does not fit the outputs archive ([`fits`]):
/// one that is not plain ([`is_plain_path`]) or the archive's own list name. The box's walk notes such a file
/// ([`Globs::refuses_name`]), and reads a folder whose name is not plain only to count them ([`Globs::counts_below`]).
pub(crate) struct Globs {
    patterns: Vec<Regex>,
    excluded: Regex,
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

    /// The matcher of `globs`, each a `noun`; else why one of them cannot be matched, naming it by `noun`.
    fn named(globs: &[String], noun: &str) -> Result<Globs, String> {
        let patterns = globs
            .iter()
            .map(|glob| Regex::new(&regex_of(glob)).map_err(|error| format!("the {noun} '{}' cannot be matched: {error}", escaped(glob))))
            .collect::<Result<_, _>>()?;
        let excluded = Regex::new(EXCLUDED).map_err(|error| format!("the output exclusions cannot be matched: {error}"))?;
        Ok(Globs { patterns, excluded })
    }

    /// Whether the file at the `/`-separated relative `path` may come back: [`Globs::selects`] it, it is neither a
    /// `.git*` path nor a `.lfsconfig`, 8.3 aliases included ([`is_git_s`], [`is_lfs_config`]), and its name fits the
    /// outputs archive ([`fits`]).
    pub(crate) fn matches(&self, path: &str) -> bool {
        fits(path) && !is_git_s(path) && !is_lfs_config(path) && self.selects(path)
    }

    /// Whether the file at the `/`-separated relative `path` would come back but for its name: [`Globs::selects`] it,
    /// yet its name does not fit the outputs archive ([`fits`]).
    pub(crate) fn refuses_name(&self, path: &str) -> bool {
        !fits(path) && self.selects(path)
    }

    /// Whether a glob matches the file at the `/`-separated relative `path`, no exclusion holds it, and no component
    /// of it is spelled as a `.git*` name or a `.lfsconfig` ([`is_spelled_git_s`], [`is_spelled_lfs_config`]); an 8.3
    /// alias of one counts as any other name.
    pub(crate) fn selects(&self, path: &str) -> bool {
        !is_spelled_git_s(path)
            && !is_spelled_lfs_config(path)
            && !self.excluded.is_match(path)
            && self.patterns.iter().any(|pattern| pattern.is_match(path))
    }

    /// Whether a walk enters the folder at the `/`-separated relative `path`, whose folders it entered: no exclusion
    /// holds everything in it, it is no `.git*` folder ([`is_git_s`]) and its own name is plain ([`is_plain_part`]).
    pub(crate) fn enters(&self, path: &str) -> bool {
        !is_git_s(path) && path.rsplit('/').next().is_some_and(is_plain_part) && !self.excluded.is_match(&format!("{path}/"))
    }

    /// Whether a walk reads the folder at the `/`-separated relative `path` only to count the files below it that
    /// [`Globs::refuses_name`] would: no exclusion holds everything in it, no component of it is spelled as a `.git*`
    /// name, and it has fewer than [`PATH_DEPTH_LIMIT`] parts, so a file below it may have that many.
    pub(crate) fn counts_below(&self, path: &str) -> bool {
        !is_spelled_git_s(path) && path.split('/').count() < PATH_DEPTH_LIMIT && !self.excluded.is_match(&format!("{path}/"))
    }
}

/// Whether the `/`-separated `path` fits the outputs archive: a plain path ([`is_plain_path`]) other than the archive's
/// own list, [`LIST_NAME`], in any letter case.
fn fits(path: &str) -> bool {
    is_plain_path(path) && !path.eq_ignore_ascii_case(LIST_NAME)
}

/// `glob` as an anchored, case-insensitive regex.
fn regex_of(glob: &str) -> String {
    let mut text = String::from("(?i)^");
    let mut rest = glob;
    while let Some(character) = rest.chars().next() {
        if let Some(after) = rest.strip_prefix("**/") {
            text.push_str("(?:.*/)?");
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
    text.push('$');
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
