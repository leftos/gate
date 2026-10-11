//! The output globs of a run and the paths never brought back whatever the globs, as the PowerShell gate matches them
//! (remote-wrapper.ps1's `ConvertTo-OutputRegex` and `$OutputExcluded`).

use regex::Regex;

use crate::boxside::payload::escaped;

/// The paths never brought back, whatever the globs: anything under a `.git`, `.tmp` or `TestResults` folder at any
/// depth, and a `.env` file (`.env`, `.env.<name>`, `<name>.env`). Matched without regard to case.
const EXCLUDED: &str = r"(?i)(^|/)(\.git|\.tmp|TestResults)/|(^|/)\.env\.[^/]*$|\.env$";

/// A run's output globs. Each is relative to the work folder's root and `/`-separated: `**/` is any number of folders,
/// none included, `**` anything, `*` any run of characters within one name, `?` one character within one name, and
/// every other character itself. They match without regard to case, as the PowerShell gate's did.
pub(crate) struct Globs {
    patterns: Vec<Regex>,
    excluded: Regex,
}

impl Globs {
    /// The matcher of `globs`; else why one of them cannot be matched.
    pub(crate) fn new(globs: &[String]) -> Result<Globs, String> {
        let patterns = globs
            .iter()
            .map(|glob| Regex::new(&regex_of(glob)).map_err(|error| format!("the output glob '{}' cannot be matched: {error}", escaped(glob))))
            .collect::<Result<_, _>>()?;
        let excluded = Regex::new(EXCLUDED).map_err(|error| format!("the output exclusions cannot be matched: {error}"))?;
        Ok(Globs { patterns, excluded })
    }

    /// Whether the file at the `/`-separated relative `path` may come back: a glob matches it and no exclusion does.
    pub(crate) fn matches(&self, path: &str) -> bool {
        !self.excluded.is_match(path) && self.patterns.iter().any(|pattern| pattern.is_match(path))
    }

    /// Whether a walk enters the folder at the `/`-separated relative `path`: no exclusion holds everything in it.
    pub(crate) fn enters(&self, path: &str) -> bool {
        !self.excluded.is_match(&format!("{path}/"))
    }
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
            &["bin/a.dll", "src/app/bin/Debug/a.dll", "bin/sub/"],
            &["binx/a.dll", "abin/a.dll", "src/bin", "bin"],
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
        check("bin/*/a.dll", &["bin/Debug/a.dll", "bin//a.dll"], &["bin/a.dll", "bin/x/y/a.dll"]);
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
            "bin/.gitignore",
            ".github/a.yml",
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
        for entered in ["bin", ".github", "a/.tmpx", ".env", "Über"] {
            assert!(matcher.enters(entered), "{entered} is entered");
        }
    }
}
