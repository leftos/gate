//! Where a run's command runs: the facts [`gate_router::decide`] asks for, read from the routing file, git and the
//! drive table, and the lines its decision says. git runs on the run thread, before any command is started.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use gate_args::{RunRequest, Slot};
use gate_router::{Config, ConfigProblem, Facts, Inputs, LocalReason, Place, Route, decide};

use crate::console::Console;
use crate::environment::Variables;
use crate::resolve::Environment;
use crate::run::find_program;

/// The `notRouted` value of a heavy gate its caller kept here with `--local`.
const LOCAL_FLAG: &str = "--local";
/// Why a command that would route runs here when one of its words is not valid Unicode.
const NOT_UNICODE: &str = "a word of the command is not valid Unicode";
/// How long one of the decision's git calls may take before git is killed and the run goes ahead here.
const GIT_LIMIT: Duration = Duration::from_secs(60);
/// How often a git call is looked at while it runs.
const GIT_POLL: Duration = Duration::from_millis(5);
/// The variables that would point git at another repository, index, object store, config or pathspec reading; git is
/// asked about the gate's folder alone, so a gate inside a hook that exports them still routes.
const GIT_LOCATION: [&str; 13] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_CONFIG_PARAMETERS",
    "GIT_GLOB_PATHSPECS",
    "GIT_NOGLOB_PATHSPECS",
    "GIT_ICASE_PATHSPECS",
    "GIT_LITERAL_PATHSPECS",
];
/// `FILE_FLAG_DELETE_ON_CLOSE`: the file of git's output is gone once its last handle closes.
const DELETE_ON_CLOSE: u32 = 0x0400_0000;

static NEXT_OUTPUT: AtomicU32 = AtomicU32::new(0);

/// Decides where `request`'s command runs and says the decision's lines, on stderr and in the log: why a command runs
/// here when there is a reason to say, or the lines of a routed run. A command that would route but has a word that is
/// not valid Unicode runs here, saying so.
pub(crate) fn route(request: &RunRequest, environment: &Environment, console: &mut Console) -> Route {
    let words: Vec<String> = request.command.iter().map(|word| word.to_string_lossy().into_owned()).collect();
    let unicode = request.command.iter().all(|word| word.to_str().is_some());
    let inputs = Inputs {
        slot: slot(request.slot),
        covering_parent: false,
        local: request.local,
        git_hook: environment.git_index_file.as_deref().is_some_and(|value| !value.is_empty()),
        words: &words,
    };
    let machine = Machine {
        folder: std::env::current_dir(),
        local_app_data: environment.local_app_data.as_deref(),
        git_limit: request.test_seams.git_limit_ms.map_or(GIT_LIMIT, Duration::from_millis),
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
        let path = folder.join("gate").join("remote.json");
        let problem = |reason: String| ConfigProblem { path: path.clone(), reason };
        match fs::metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(problem(error.to_string())),
            Ok(metadata) if !metadata.is_file() => return Err(problem("not a file".to_owned())),
            Ok(_) => {}
        }
        let bytes = fs::read(&path).map_err(|error| problem(error.to_string()))?;
        Config::parse(&String::from_utf8_lossy(&bytes)).map(Some).map_err(problem)
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
        let (status, mut output) = git(folder, &args, self.git_limit)?;
        if !status.success() {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        output
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
        let (status, output) = git(root, &args, self.git_limit)?;
        if !status.success() {
            let code = status.code().map_or_else(|| "unknown".to_owned(), |code| code.to_string());
            return Err(format!("git ls-files failed with status {code}"));
        }
        holds_existing_path(BufReader::new(output), root).map_err(|error| format!("cannot read git ls-files' output: {error}"))
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

/// Runs git, found along the gate's own `PATH` and started by its full path, in `folder` with `args`: no input, none
/// of [`GIT_LOCATION`], no fsmonitor program, and killed after `limit`. Its exit status and its stdout, held in
/// a temporary file read from its start, since a pipe nobody reads while git runs would stop a git that prints much.
/// git runs before the command is started, while the gate holds no handle a child should not inherit.
fn git(folder: &Path, args: &[&str], limit: Duration) -> Result<(ExitStatus, File), String> {
    let program = find_program("git", &Variables::from_process(), folder).map_err(|why| format!("cannot run git: {why}"))?;
    let cannot_hold = |error: io::Error| format!("cannot make a file for git's output: {error}");
    let mut output = output_file().map_err(cannot_hold)?;
    let stdout = output.try_clone().map_err(cannot_hold)?;
    let mut command = Command::new(&program);
    for name in GIT_LOCATION {
        command.env_remove(name);
    }
    command
        .arg("-C")
        .arg(folder)
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::null());
    let mut child = gate_win32::spawn(&mut command).map_err(|error| format!("cannot run git: {error}"))?;
    let status = wait_for(&mut child, limit)?;
    output
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("cannot read git's output: {error}"))?;
    Ok((status, output))
}

/// A new file in the temporary folder for one git call's stdout, deleted once its last handle closes.
fn output_file() -> io::Result<File> {
    let number = NEXT_OUTPUT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("gate-git-{}-{number}.out", std::process::id()));
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .custom_flags(DELETE_ON_CLOSE)
        .open(path)
}

/// git's exit status once it ends; after `limit` it is killed and the call fails.
fn wait_for(child: &mut Child, limit: Duration) -> Result<ExitStatus, String> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().map_err(|error| format!("cannot wait for git: {error}"))? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let late = late_git_line(limit);
            return match child.kill().and_then(|()| child.wait()) {
                Ok(_) => Err(late),
                Err(error) => Err(format!("{late}, and could not be stopped: {error}")),
            };
        }
        thread::sleep(GIT_POLL);
    }
}

/// The line a git that outlived `limit` is killed under: the limit in whole seconds when it is a whole number of
/// seconds, else in whole milliseconds.
fn late_git_line(limit: Duration) -> String {
    match limit.as_millis() % 1000 {
        0 => format!("git did not answer within {} s", limit.as_secs()),
        _ => format!("git did not answer within {} ms", limit.as_millis()),
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
    use std::time::Duration;

    use gate_router::{LocalReason, Place, Route};

    use super::{GIT_LIMIT, holds_existing_path, late_git_line, not_routed, parse_place};

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

    #[test]
    fn a_late_git_names_its_limit_in_seconds_or_milliseconds() {
        assert_eq!(late_git_line(GIT_LIMIT), "git did not answer within 60 s", "the default limit");
        assert_eq!(late_git_line(Duration::from_secs(60)), "git did not answer within 60 s");
        assert_eq!(late_git_line(Duration::from_secs(10)), "git did not answer within 10 s");
        assert_eq!(late_git_line(Duration::from_millis(1)), "git did not answer within 1 ms");
    }
}
