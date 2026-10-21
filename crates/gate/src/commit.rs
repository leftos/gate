//! A commit run: a gated `git commit`, whose hooks run inside its command. It takes the heavy or light slot its caller
//! asked for, a light one when it asked for none, for the whole command, at the priority any run of its kind gets. Its
//! slot covers the gates its hooks start as any slot of its kind does; a gate it covers still routes as one under no
//! parent would, and a critical or `--alone` gate in its hooks is refused.

use std::borrow::Cow;
use std::ffi::OsString;

use gate_args::{RunRequest, Slot};

/// The line a commit run asked for on no slot says once, before it waits for the light slot it takes in its place.
pub(crate) const NO_SLOT_LINE: &str = "gate: a commit's hooks take a slot: pass --slot heavy or --slot light";

/// The line a critical or `--alone` gate inside a commit run's hooks says as it is refused.
pub(crate) const IN_COMMIT_LINE: &str = "gate: inside a git commit's hooks; a critical or --alone gate cannot run there";

/// Whether `request` is a commit run: a `git commit` ([`is_commit`]) asked for on any slot but critical, which keeps
/// its own meaning.
pub(crate) fn is_commit_run(request: &RunRequest) -> bool {
    request.slot != Slot::Critical && is_commit(&request.command)
}

/// `request` as the gate runs it, and whether it is a commit run asked for on no slot: that one runs on a light slot,
/// and says [`NO_SLOT_LINE`] once it is known to take that slot itself; any other runs as it is.
pub(crate) fn as_run(request: &RunRequest) -> (Cow<'_, RunRequest>, bool) {
    if is_commit_run(request) && request.slot == Slot::None {
        let light = RunRequest {
            slot: Slot::Light,
            ..request.clone()
        };
        (Cow::Owned(light), true)
    } else {
        (Cow::Borrowed(request), false)
    }
}

/// The git options before the subcommand that take their value as the next word when it is not joined by `=`.
const VALUED_OPTIONS: [&str; 8] = [
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--super-prefix",
    "--config-env",
    "--attr-source",
];

/// The git options before the subcommand that make git say its help, version or a path and run no subcommand at all.
const SAYING_OPTIONS: [&str; 7] = ["--help", "-h", "--version", "--exec-path", "--html-path", "--man-path", "--info-path"];

/// Whether `words` are a commit run's command: the first word names `git` ([`gate_router::names_program`]), and the
/// first word after it that is neither an option nor the value of one ([`VALUED_OPTIONS`]) is `commit`, so
/// `git -C x commit` is one and `git commit-tree` is not. An option before it that makes git run no subcommand
/// ([`SAYING_OPTIONS`]) makes it none.
pub(crate) fn is_commit(words: &[OsString]) -> bool {
    let Some((first, rest)) = words.split_first() else {
        return false;
    };
    gate_router::names_program(&first.to_string_lossy(), "git") && subcommand(rest).is_some_and(|word| word == "commit")
}

/// The first word of `rest`, the words after `git`, that is neither an option nor the value of one; `None` when an
/// option makes git run no subcommand.
fn subcommand(rest: &[OsString]) -> Option<&OsString> {
    let mut words = rest.iter();
    while let Some(word) = words.next() {
        let text = word.to_string_lossy();
        if !text.starts_with('-') {
            return Some(word);
        }
        if SAYING_OPTIONS.contains(&text.as_ref()) {
            return None;
        }
        if VALUED_OPTIONS.contains(&text.as_ref()) {
            words.next();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::is_commit;

    fn commit(words: &[&str]) -> bool {
        is_commit(&words.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn a_git_commit_however_git_is_spelled_is_a_commit_run() {
        assert!(commit(&["git", "commit", "-F", "m"]), "git commit -F m");
        assert!(commit(&["git", "-C", "x", "commit"]), "git -C x commit");
        assert!(commit(&["git.exe", "commit"]), "git.exe commit");
        assert!(
            commit(&[r"C:\Program Files\Git\cmd\GIT.EXE", "commit", "-m", "x"]),
            "a full path in capitals"
        );
        assert!(
            commit(&["git", "-c", "core.hooksPath=h", "--no-pager", "commit"]),
            "options before the subcommand"
        );
        assert!(commit(&["git", "--git-dir=.git", "commit"]), "an option joined to its value");
        assert!(
            commit(&["git", "--attr-source", "HEAD", "commit", "-m", "x"]),
            "--attr-source and its value"
        );
        assert!(commit(&["git", "--exec-path=C:/git", "commit"]), "--exec-path given a value");
    }

    #[test]
    fn a_git_that_only_says_its_help_version_or_paths_is_no_commit_run() {
        for option in ["--help", "-h", "--version", "--exec-path", "--html-path", "--man-path", "--info-path"] {
            assert!(!commit(&["git", option, "commit"]), "git {option} commit");
            assert!(!commit(&["git", "-C", "x", option, "commit"]), "git -C x {option} commit");
        }
    }

    #[test]
    fn another_git_subcommand_or_program_is_no_commit_run() {
        assert!(!commit(&["git", "commit-tree", "abc"]), "git commit-tree");
        assert!(!commit(&["git", "log"]), "git log");
        assert!(!commit(&["prek", "run"]), "prek run");
        assert!(!commit(&["git", "-C", "commit", "log"]), "commit as the value of -C");
        assert!(!commit(&["git"]), "git alone");
        assert!(!commit(&["gitk", "commit"]), "another program");
        assert!(!commit(&[]), "no words");
    }
}
