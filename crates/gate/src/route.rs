//! Where a run's command runs: the facts [`gate_router::decide`] asks for, read from the routing file, git and the
//! drive table, and the lines its decision says. git runs on the run thread, before any command is started.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use gate_args::{RunRequest, Slot};
use gate_router::{Config, ConfigProblem, Facts, Inputs, LocalReason, Place, Route, decide};

use crate::console::Console;
use crate::remote::git;
use crate::resolve::Environment;

/// The `notRouted` value of a heavy gate its caller kept here with `--local`.
const LOCAL_FLAG: &str = "--local";
/// Why a command that would route runs here when one of its words is not valid Unicode.
const NOT_UNICODE: &str = "a word of the command is not valid Unicode";

/// Decides where `request`'s command runs and says the decision's lines, on stderr and in the log: why a command runs
/// here when there is a reason to say, or the lines of a routed run. A command that would route but has a word that is
/// not valid Unicode runs here, saying so. A gate under a `covering_parent`'s slot runs here, saying nothing.
pub(crate) fn route(request: &RunRequest, environment: &Environment, covering_parent: bool, console: &mut Console) -> Route {
    let words: Vec<String> = request.command.iter().map(|word| word.to_string_lossy().into_owned()).collect();
    let unicode = request.command.iter().all(|word| word.to_str().is_some());
    let inputs = Inputs {
        slot: slot(request.slot),
        covering_parent,
        local: request.local,
        git_hook: environment.git_index_file.as_deref().is_some_and(|value| !value.is_empty()),
        words: &words,
    };
    let machine = Machine {
        folder: std::env::current_dir(),
        local_app_data: environment.local_app_data.as_deref(),
        git_limit: git::limit(&request.test_seams),
    };
    let route = match decide(&inputs, &machine) {
        Route::Routed(_) if !unicode => Route::Local {
            reason: Some(LocalReason::Failed {
                reason: NOT_UNICODE.to_owned(),
            }),
        },
        route => route,
    };
    match &route {
        Route::Local { reason: Some(reason) } => console.err(&reason.line()),
        Route::Local { reason: None } => {}
        Route::Routed(run) => run.lines().into_iter().for_each(|line| console.err(line)),
    }
    route
}

/// The queue line's `notRouted`: `--local` for a heavy gate its caller kept here with `--local`, else none.
pub(crate) fn not_routed(route: &Route) -> Option<String> {
    matches!(
        route,
        Route::Local {
            reason: Some(LocalReason::LocalFlag)
        }
    )
    .then(|| LOCAL_FLAG.to_owned())
}

fn slot(slot: Slot) -> gate_router::Slot {
    match slot {
        Slot::Heavy => gate_router::Slot::Heavy,
        Slot::Light => gate_router::Slot::Light,
        Slot::Critical => gate_router::Slot::Critical,
    }
}

/// The routing file at `path`: `None` when there is none; else why it cannot be used: not a file, unreadable, or
/// refused by [`Config::parse`]. Routing and `gate push` read it through this.
pub(crate) fn read_remote_json(path: &Path) -> Result<Option<Config>, ConfigProblem> {
    let problem = |reason: String| ConfigProblem {
        path: path.to_path_buf(),
        reason,
    };
    match fs::metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(problem(error.to_string())),
        Ok(metadata) if !metadata.is_file() => return Err(problem("not a file".to_owned())),
        Ok(_) => {}
    }
    let bytes = fs::read(path).map_err(|error| problem(error.to_string()))?;
    Config::parse(&String::from_utf8_lossy(&bytes)).map(Some).map_err(problem)
}

/// This machine's facts: the gate's working folder, or why it cannot be read, the LOCALAPPDATA folder that holds the
/// routing file, and how long each of the decision's git calls may take.
struct Machine<'a> {
    folder: io::Result<PathBuf>,
    local_app_data: Option<&'a OsStr>,
    git_limit: Duration,
}

impl Facts for Machine<'_> {
    /// `<LOCALAPPDATA>\gate\remote.json`; with LOCALAPPDATA unset, empty or relative there is no routing file.
    fn config(&self) -> Result<Option<Config>, ConfigProblem> {
        let Some(folder) = self.local_app_data.map(Path::new).filter(|folder| folder.is_absolute()) else {
            return Ok(None);
        };
        read_remote_json(&folder.join("gate").join("remote.json"))
    }

    fn place(&self) -> Result<Option<Place>, String> {
        let folder = self
            .folder
            .as_ref()
            .map_err(|error| format!("cannot read the working directory: {error}"))?;
        let args = [
            "rev-parse",
            "--show-toplevel",
            "--show-prefix",
            "--path-format=absolute",
            "--git-common-dir",
        ];
        let mut ran = git::Git::find(folder)?.run(&git::Call::plain(folder, &args, self.git_limit))?;
        if !ran.status.success() {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        ran.stdout
            .read_to_end(&mut bytes)
            .map_err(|error| format!("cannot read git rev-parse's output: {error}"))?;
        let text = String::from_utf8(bytes).map_err(|_| "git rev-parse's output is not valid UTF-8".to_owned())?;
        parse_place(&text).map(Some)
    }

    /// The first path `git ls-files` lists, tracked or untracked and not ignored, that is still on disk: an unstaged
    /// delete sends nothing. The prefix is a literal path, its spaces and wildcards as written.
    fn folder_has_files(&self, place: &Place) -> Result<bool, String> {
        let root = Path::new(&place.root);
        let args = [
            "--literal-pathspecs",
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            &place.prefix,
        ];
        let ran = git::Git::find(root)?.run(&git::Call::plain(root, &args, self.git_limit))?;
        if !ran.status.success() {
            let code = ran.status.code().map_or_else(|| "unknown".to_owned(), |code| code.to_string());
            return Err(format!("git ls-files failed with status {code}"));
        }
        holds_existing_path(BufReader::new(ran.stdout), root).map_err(|error| format!("cannot read git ls-files' output: {error}"))
    }

    /// The root's `HEAD` sha, `Ok(None)` when the worktree has none: a `git rev-parse --verify -q HEAD` that fails (an
    /// unborn branch among them) or writes anything but one sha is no sha.
    fn head_sha(&self, place: &Place) -> Result<Option<String>, String> {
        let root = Path::new(&place.root);
        let args = ["rev-parse", "--verify", "-q", "HEAD"];
        let mut ran = git::Git::find(root)?.run(&git::Call::plain(root, &args, self.git_limit))?;
        if !ran.status.success() {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        ran.stdout
            .read_to_end(&mut bytes)
            .map_err(|error| format!("cannot read git rev-parse's output: {error}"))?;
        let text = String::from_utf8(bytes).map_err(|_| "git rev-parse's output is not valid UTF-8".to_owned())?;
        Ok(parse_head(&text))
    }

    fn file_in_folder(&self, name: &str) -> bool {
        self.folder.as_ref().is_ok_and(|folder| folder.join(name).is_file())
    }

    fn subst_target(&self, letter: char) -> Option<String> {
        // The decision takes no error here: a letter whose device cannot be read is no subst drive, and a word on it
        // is judged as written.
        gate_win32::device_target(letter).ok().flatten()
    }
}

/// The place `git rev-parse --show-toplevel --show-prefix --path-format=absolute --git-common-dir` printed, in exactly
/// three lines: the root, the prefix (empty at the root) and the common git folder, whose parent's name is the
/// checkout's.
fn parse_place(text: &str) -> Result<Place, String> {
    let lines: Vec<&str> = text.lines().collect();
    let [root, prefix, common] = lines.as_slice() else {
        return Err(format!(
            "cannot read git rev-parse's output (routing needs git 2.31 or later): {}",
            text.trim()
        ));
    };
    let checkout = Path::new(common)
        .parent()
        .and_then(Path::file_name)
        .ok_or_else(|| format!("cannot name the checkout of the git folder {common}"))?;
    Ok(Place {
        root: (*root).to_owned(),
        prefix: (*prefix).to_owned(),
        checkout: checkout.to_string_lossy().into_owned(),
    })
}

/// The one sha `git rev-parse HEAD` wrote: 40 lower-case hex digits on a line of their own, nothing else.
fn parse_head(text: &str) -> Option<String> {
    let sha = text.trim();
    (sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))).then(|| sha.to_owned())
}

/// Whether a NUL-separated `git ls-files -z` listing, its paths relative to `root`, names a path still on disk; it
/// reads no further than the first that does.
fn holds_existing_path(listing: impl BufRead, root: &Path) -> io::Result<bool> {
    for path in listing.split(0) {
        let path = path?;
        if !path.is_empty() && fs::symlink_metadata(root.join(String::from_utf8_lossy(&path).as_ref())).is_ok() {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its folders must fail loudly")]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use gate_router::{LocalReason, Place, Route};

    use super::{holds_existing_path, not_routed, parse_head, parse_place};

    fn place(root: &str, prefix: &str, checkout: &str) -> Place {
        Place {
            root: root.to_owned(),
            prefix: prefix.to_owned(),
            checkout: checkout.to_owned(),
        }
    }

    #[test]
    fn rev_parse_output_gives_the_root_the_prefix_and_the_checkout() {
        assert_eq!(
            parse_place("C:/work/app\n\nC:/work/app/.git\n"),
            Ok(place("C:/work/app", "", "app")),
            "at the root the prefix is empty"
        );
        assert_eq!(
            parse_place("C:/work/app-wt\r\nsrc/my dir/\r\nC:/work/app/.git\r\n"),
            Ok(place("C:/work/app-wt", "src/my dir/", "app")),
            "a worktree below a folder names the main checkout"
        );
        assert!(parse_place("C:/work/app\n\n").is_err(), "a missing common folder");
        assert!(
            parse_place("C:/work/app\n\n--path-format=absolute\nC:/work/app/.git\n").is_err(),
            "a git too old for --path-format prints a fourth line"
        );
        assert!(parse_place("C:/work/app\n\n/\n").is_err(), "a common folder with no parent");
    }

    #[test]
    fn a_head_is_one_line_of_forty_lower_case_hex_digits() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(parse_head(&format!("{sha}\n")), Some(sha.to_owned()), "an LF ending");
        assert_eq!(parse_head(&format!("{sha}\r\n")), Some(sha.to_owned()), "a CRLF ending");
        assert_eq!(parse_head(&sha.to_uppercase()), None, "upper case");
        assert_eq!(parse_head(&format!("{sha}abcdef0123456789abcdef")), None, "a sha-256's 64 digits");
        assert_eq!(parse_head(&format!("{sha}\n{sha}\n")), None, "two lines");
        assert_eq!(parse_head(""), None, "nothing");
        assert_eq!(parse_head("fatal: not a git repository (or any parent)\n"), None, "junk");
    }

    #[test]
    fn only_a_listed_path_still_on_disk_counts() {
        let root: PathBuf = std::env::temp_dir().join(format!("gate-route-listing-{}", std::process::id()));
        fs::create_dir_all(root.join("sub")).expect("the test folder can be made");
        fs::write(root.join("sub").join("here.txt"), "x").expect("a file can be written");

        let found = |listing: &[u8]| holds_existing_path(listing, &root).expect("a listing in memory can be read");
        assert!(found(b"sub/gone.txt\0sub/here.txt\0"), "one file still there");
        assert!(!found(b"sub/gone.txt\0"), "an unstaged delete");
        assert!(!found(b""), "nothing listed");
        fs::remove_dir_all(&root).expect("the test folder can be removed");
    }

    #[test]
    fn only_the_local_flag_writes_not_routed() {
        let flag = Route::Local {
            reason: Some(LocalReason::LocalFlag),
        };
        let failed = Route::Local {
            reason: Some(LocalReason::Failed { reason: "x".to_owned() }),
        };

        assert_eq!(not_routed(&flag).as_deref(), Some("--local"));
        assert_eq!(not_routed(&failed), None);
        assert_eq!(not_routed(&Route::Local { reason: None }), None);
    }
}
