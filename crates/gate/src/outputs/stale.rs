//! The folders a passing routed build left stale: the folders in the worktree, and in each sibling of a pair, that the
//! judging output globs stop at and the run did not bring back. [`stale_folders`] finds them over a walk of one tree,
//! [`stale_line`] says them, and [`stale_lines`] says them for the main worktree and its siblings, with why a tree
//! could not be listed.

use std::collections::BTreeSet;
use std::io;

use regex::Regex;

use crate::boxside::payload::escaped;
use crate::outputs::glob::{Globs, regex_upto};

/// The folders a passing routed build left stale: each folder a glob of `judging` stops at over a file `walk` lists that
/// no glob of `brought_back` matches, each `/`-separated relative to the walk's root, sorted, each once. Coverage is
/// judged by the file and not by the glob: a file another judging glob's folder holds counts too, so a folder whose every
/// matching file came back is not stale.
///
/// # Errors
///
/// Why the tree could not be listed: a glob that cannot be matched, or the walk's own failure.
fn stale_folders(
    mut walk: impl FnMut(&Globs) -> io::Result<Vec<String>>,
    judging: &[String],
    brought_back: &[String],
) -> Result<Vec<String>, String> {
    let judged = deduped(judging);
    if judged.is_empty() || judged.iter().all(|glob| covered(glob, brought_back)) {
        return Ok(Vec::new());
    }
    let union = Globs::new(&judged)?;
    let found = walk(&union).map_err(|error| error.to_string())?;
    let brought = Globs::new(brought_back)?;
    let stops: Vec<Judged> = judged.iter().map(|glob| Judged::new(glob)).collect::<Result<_, _>>()?;
    let mut folders = BTreeSet::new();
    for path in &found {
        if brought.matches(path) {
            continue;
        }
        for judged in &stops {
            if judged.glob.matches(path)
                && let Some(folder) = judged.folder(path)
            {
                folders.insert(folder);
            }
        }
    }
    Ok(folders.into_iter().collect())
}

/// The globs of `judging`, in order, each once: a glob judging twice says nothing new of a folder.
fn deduped(judging: &[String]) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    for glob in judging {
        if !kept.iter().any(|one| one.eq_ignore_ascii_case(glob)) {
            kept.push(glob.clone());
        }
    }
    kept
}

/// Whether a glob of `brought_back` covers `glob`, in any letter case: every file `glob` matches came back.
fn covered(glob: &str, brought_back: &[String]) -> bool {
    brought_back.iter().any(|back| back.eq_ignore_ascii_case(glob))
}

/// One judging glob, ready to name folders: the files it matches, and the folder it names where its own parts end.
struct Judged {
    /// The glob's own matcher ([`Globs::matches`]).
    glob: Globs,
    /// The regex naming the first place a path spells the glob's own folder parts as whole components; `None` when the
    /// glob names no folder.
    stops: Option<Regex>,
}

impl Judged {
    /// The glob `glob` as a judge of the folders the files it matches are stale under; else why it cannot be matched.
    fn new(glob: &str) -> Result<Judged, String> {
        let parts: Vec<&str> = glob.split('/').collect();
        // Every glob names the folder its last plain folder part ends at; one whose last part is not `**` ends in the
        // file it names, which is no folder.
        let named = if parts.last() == Some(&"**") {
            &parts[..]
        } else {
            &parts[..parts.len() - 1]
        };
        let anchor = named.iter().rposition(|part| is_plain(part)).map(|at| at + 1);
        let stops = match anchor {
            Some(end) => Some(
                Regex::new(&regex_upto(&named[..end].join("/")))
                    .map_err(|error| format!("the output glob '{}' cannot be matched: {error}", escaped(glob)))?,
            ),
            None => None,
        };
        Ok(Judged {
            glob: Globs::new(&[glob.to_owned()])?,
            stops,
        })
    }

    /// The folder a file the glob matches at the `/`-separated `path` is stale under, `None` when the glob names none.
    fn folder(&self, path: &str) -> Option<String> {
        let stops = self.stops.as_ref()?;
        let end = stops.captures(path)?.get(1)?.end();
        (end < path.len()).then(|| path[..end].to_owned())
    }
}

/// Whether a glob's part is a folder's own name: no `*` or `?`.
fn is_plain(part: &str) -> bool {
    !part.contains(['*', '?'])
}

/// The lines a passing routed build says of what it left stale in the main worktree at the root `main` and in each of
/// `siblings` (its folder name and root), in their order: [`stale_folders`] over `walk` of each root, judged by
/// `judging`, the main's against `brought_back` and a sibling's against nothing, since no sibling's output is ever
/// brought back. One line names every folder ([`stale_line`]), the main's first, then each sibling's written
/// `../{name}/{folder}`, relative to the main worktree. A tree that cannot be listed says why on its own line after it,
/// the main's as [`could_not_list`] and a sibling's as [`sibling_not_listed`] says it, and the others' folders are
/// still named.
pub(crate) fn stale_lines(
    main: &str,
    siblings: &[(&str, &str)],
    judging: &[String],
    brought_back: &[String],
    mut walk: impl FnMut(&str, &Globs) -> io::Result<Vec<String>>,
) -> Vec<String> {
    let mut folders = Vec::new();
    let mut unlisted = Vec::new();
    match stale_folders(|globs| walk(main, globs), judging, brought_back) {
        Ok(found) => folders.extend(found),
        Err(reason) => unlisted.push(could_not_list(&reason)),
    }
    for (name, root) in siblings {
        match stale_folders(|globs| walk(root, globs), judging, &[]) {
            Ok(found) => folders.extend(found.into_iter().map(|folder| format!("../{name}/{folder}"))),
            Err(reason) => unlisted.push(sibling_not_listed(name, &reason)),
        }
    }
    stale_line(&folders).into_iter().chain(unlisted).collect()
}

/// The line saying the folders could not be found, for `reason`.
#[must_use]
fn could_not_list(reason: &str) -> String {
    format!("gate: routed build may have left stale output; could not list it: {}", escaped(reason))
}

/// The line saying the folders of the sibling `name` could not be found, for `reason`.
#[must_use]
fn sibling_not_listed(name: &str, reason: &str) -> String {
    format!(
        "gate: routed build may have left stale output in ../{name}; could not list it: {}",
        escaped(reason)
    )
}

/// The line saying a routed build left the folders `folders` stale, each escaped, `/`-separated as the walk found them,
/// joined with `, `; `None` when it left none.
#[must_use]
fn stale_line(folders: &[String]) -> Option<String> {
    if folders.is_empty() {
        return None;
    }
    let shown: Vec<String> = folders.iter().map(|folder| escaped(folder)).collect();
    Some(format!("gate: routed build left stale: {}", shown.join(", ")))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its folders or files must fail loudly")]
mod tests {
    use std::io;

    use gate_router::DEFAULT_OUTPUTS;

    use super::{stale_folders, stale_line, stale_lines};
    use crate::outputs::walk::tests::tree;
    use crate::outputs::walk::walk;

    /// The output globs of a repo the routing file's `outputs` key does not name, as the router gives them.
    const DEFAULTS: [&str; 2] = DEFAULT_OUTPUTS;

    /// `globs` as the routing file gives them.
    fn globs(globs: &[&str]) -> Vec<String> {
        globs.iter().map(|glob| (*glob).to_owned()).collect()
    }

    /// The folders `stale_folders` names over the listing `files`, judged by `judging` against `brought`.
    fn folders(files: &[&str], judging: &[&str], brought: &[&str]) -> Vec<String> {
        let listing: Vec<String> = files.iter().map(|file| (*file).to_owned()).collect();
        stale_folders(|_| Ok(listing.clone()), &globs(judging), &globs(brought)).expect("the globs can be matched")
    }

    #[test]
    fn a_repo_with_no_output_globs_names_the_folders_the_default_globs_stop_at() {
        let root = tree("defaults", &["src/App/bin/Debug/x.dll", "src/Game/.godot/mono/temp/bin/Debug/y.dll"]);

        let found = stale_folders(
            |globs| walk(&root, globs).map(|files| files.into_iter().map(|file| file.path).collect()),
            &globs(&DEFAULTS),
            &globs(&[]),
        )
        .expect("the tree can be walked");

        assert_eq!(
            found,
            ["src/App/bin", "src/Game/.godot/mono/temp/bin"],
            "the Godot folder, which both default globs match, is named once"
        );
    }

    #[test]
    fn a_run_that_brought_a_glob_back_names_no_folder_its_files_came_back_for() {
        let files = ["src/App/bin/Debug/x.dll", "src/Game/.godot/mono/temp/bin/Debug/y.dll"];

        assert_eq!(
            folders(&files, &DEFAULTS, &["**/bin/**"]),
            Vec::<String>::new(),
            "both files came back under the bin glob, the Godot folder's own file among them"
        );
    }

    #[test]
    fn a_run_that_brought_the_godot_glob_back_leaves_the_bin_folder_stale() {
        let files = ["src/App/bin/Debug/x.dll", "src/Game/.godot/mono/temp/bin/Debug/y.dll"];

        assert_eq!(
            folders(&files, &DEFAULTS, &["**/.godot/mono/temp/bin/**"]),
            ["src/App/bin"],
            "the Godot folder's only file came back; the App bin folder's did not"
        );
    }

    #[test]
    fn a_folder_is_named_at_the_first_place_the_glob_s_parts_match_whole_components() {
        assert_eq!(
            folders(&["src/App/bin/Debug/net8.0/BinaryFormatter.dll"], &["**/bin/**"], &[]),
            ["src/App/bin"],
            "the Bin of BinaryFormatter.dll is no folder of its own"
        );
        assert_eq!(
            folders(&["a/bin/x/bin/y.dll"], &["**/bin/**"], &[]),
            ["a/bin"],
            "the first place from the left, not the deepest"
        );
    }

    #[test]
    fn a_glob_ending_in_a_file_names_the_folder_its_last_plain_folder_part_ends_at() {
        assert_eq!(folders(&["publish/App.exe"], &["publish/App.exe"], &[]), ["publish"]);
        assert_eq!(
            folders(&["src/bin/Release/App.dll"], &["**/bin/Release/App.dll"], &[]),
            ["src/bin/Release"]
        );
    }

    #[test]
    fn a_glob_ending_in_a_file_below_any_folders_names_the_folder_before_them() {
        assert_eq!(folders(&["out/x/App.dll"], &["out/**/App.dll"], &[]), ["out"]);
        assert_eq!(
            folders(&["out/a/b/App.dll"], &["out/**/App.dll"], &[]),
            ["out"],
            "the `**` is no folder of its own"
        );
    }

    #[test]
    fn a_glob_ending_in_a_file_below_its_last_plain_folder_part_names_that_folder() {
        assert_eq!(folders(&["src/App/bin/Debug/x.dll"], &["**/bin/**/*.dll"], &[]), ["src/App/bin"]);
    }

    #[test]
    fn a_glob_with_no_plain_folder_part_names_no_folder() {
        assert_eq!(folders(&["a/b.dll"], &["**"], &[]), Vec::<String>::new());
        assert_eq!(folders(&["b.dll"], &["*.dll"], &[]), Vec::<String>::new());
        assert_eq!(folders(&["a/b.dll"], &["**/*.dll"], &[]), Vec::<String>::new());
        assert_eq!(folders(&["App.exe"], &["App.exe"], &[]), Vec::<String>::new());
    }

    #[test]
    fn a_folder_is_named_where_the_glob_matched_and_not_at_its_first_look_alike() {
        assert_eq!(folders(&["out/bin/bin/x.dll"], &["out/*/bin/**"], &[]), ["out/bin/bin"]);
    }

    #[test]
    fn a_run_that_brought_every_judging_glob_back_names_nothing() {
        // A closure the skip alone keeps from running: a walk here would say its error instead.
        let named = stale_folders(|_| Err(io::Error::other("the walk must not run")), &globs(&DEFAULTS), &globs(&DEFAULTS));

        assert_eq!(named.expect("every judging glob came back, so nothing is listed"), Vec::<String>::new());
    }

    #[test]
    fn a_line_names_every_folder_sorted() {
        let sorted: Vec<String> = (0..14).map(|number| format!("src/p{number:02}/bin")).collect();

        let line = stale_line(&sorted).expect("a line is said");

        assert_eq!(line, format!("gate: routed build left stale: {}", sorted.join(", ")));
    }

    #[test]
    fn a_folder_whose_name_holds_a_control_character_is_named_on_one_line() {
        let line = stale_line(&["src/a\nb/bin".to_owned()]).expect("a line is said");

        assert_eq!(line, r"gate: routed build left stale: src/a\nb/bin", "{line:?}");
    }

    #[test]
    fn no_matching_folder_says_no_line() {
        assert_eq!(folders(&["src/app/main.cs"], &DEFAULTS, &[]), Vec::<String>::new());
        assert_eq!(stale_line(&folders(&[], &DEFAULTS, &[])), None);
    }

    #[test]
    fn a_pair_s_sibling_that_cannot_be_listed_has_its_own_line() {
        let siblings = [("lib", "X:/dev/lib"), ("tools", "X:/dev/tools"), ("web", "X:/dev/web")];
        let listing = |root: &str| -> io::Result<Vec<String>> {
            match root {
                "X:/dev/app" => Ok(vec!["src/App/bin/Debug/x.dll".to_owned()]),
                "X:/dev/lib" => Err(io::Error::other("access\ndenied")),
                "X:/dev/tools" => Ok(vec!["bin/Debug/t.dll".to_owned(), "src/a.cs".to_owned()]),
                _ => Ok(vec!["out/bin/w.dll".to_owned()]),
            }
        };

        let lines = stale_lines("X:/dev/app", &siblings, &globs(&DEFAULTS), &globs(&[]), |root, _| listing(root));

        assert_eq!(
            lines,
            [
                "gate: routed build left stale: src/App/bin, ../tools/bin, ../web/out/bin",
                r"gate: routed build may have left stale output in ../lib; could not list it: access\ndenied",
            ],
            "the main's folders first, each listed sibling's after in order, the unlisted sibling on its own line"
        );
    }

    #[test]
    fn a_pair_whose_main_cannot_be_listed_still_names_its_siblings_folders() {
        let lines = stale_lines(
            "X:/dev/app",
            &[("lib", "X:/dev/lib")],
            &globs(&DEFAULTS),
            &globs(&["**/bin/**"]),
            |root, _| {
                if root == "X:/dev/app" {
                    Err(io::Error::other("gone"))
                } else {
                    Ok(vec!["bin/Debug/l.dll".to_owned()])
                }
            },
        );

        assert_eq!(
            lines,
            [
                "gate: routed build left stale: ../lib/bin",
                "gate: routed build may have left stale output; could not list it: gone",
            ],
            "the main brought its glob back, but no sibling's output ever comes back"
        );
    }

    #[test]
    fn an_obj_folder_no_judging_glob_matches_is_never_named() {
        assert_eq!(folders(&["src/App/obj/Debug/App.dll"], &DEFAULTS, &[]), Vec::<String>::new());
    }
}
