//! git as the routing decision and the remote client run it: the executable found once along the gate's own `PATH`,
//! asked about one folder alone, its output held in temporary files, and ended after a time limit or by the console
//! interrupt.

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use gate_args::TestSeams;
use gate_win32::Interrupt;

use crate::environment::Variables;
use crate::run::find_program;

/// How long one git call may take before git is killed.
pub(crate) const GIT_LIMIT: Duration = Duration::from_secs(60);
/// How often a git call is looked at while it runs.
const GIT_POLL: Duration = Duration::from_millis(5);
/// The variables that would point git at another repository, index, object store, config or pathspec reading; git is
/// asked about the gate's folder alone, so a gate inside a hook that exports them still routes. The indexed config
/// variables are removed too, by their prefixes.
const GIT_LOCATION: [&str; 16] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_GLOB_PATHSPECS",
    "GIT_NOGLOB_PATHSPECS",
    "GIT_ICASE_PATHSPECS",
    "GIT_LITERAL_PATHSPECS",
];
/// The prefixes of the config variables `GIT_CONFIG_COUNT` indexes: `GIT_CONFIG_KEY_<n>` and `GIT_CONFIG_VALUE_<n>`.
const GIT_CONFIG_INDEXED: [&str; 2] = ["GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"];
/// `FILE_FLAG_DELETE_ON_CLOSE`: the file of git's output is gone once its last handle closes.
const DELETE_ON_CLOSE: u32 = 0x0400_0000;

static NEXT_OUTPUT: AtomicU32 = AtomicU32::new(0);

/// How long each git call of a run may take: `--test-git-limit-ms` when given, else [`GIT_LIMIT`].
pub(crate) fn limit(seams: &TestSeams) -> Duration {
    seams.git_limit_ms.map_or(GIT_LIMIT, Duration::from_millis)
}

/// Why a git call did not finish: the console interrupt ended it, or it failed, with the failure in the words the
/// PowerShell client uses.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum GitError {
    /// The console interrupt was raised, and git was ended with it: the run is an interrupt, not a failure.
    Interrupted,
    /// The call failed: what could not be done, or, for a git that ran and refused, `git <args> failed in <root>
    /// (status <n>): <the lines git wrote>`.
    Failed(String),
}

/// A failed call's reason; an interrupt, which has no line of its own, says the run was interrupted.
impl From<GitError> for String {
    fn from(error: GitError) -> String {
        match error {
            GitError::Interrupted => "the run was interrupted".to_owned(),
            GitError::Failed(why) => why,
        }
    }
}

/// The git executable, found along the gate's own `PATH` once: a caller that asks git many times runs the one git it
/// found rather than searching for git again.
pub(crate) struct Git {
    program: PathBuf,
}

impl Git {
    /// git found along the gate's own `PATH`, read against `folder`; else why it cannot be found.
    pub(crate) fn find(folder: &Path) -> Result<Git, String> {
        find_program("git", &Variables::from_process(), folder)
            .map(|program| Git { program })
            .map_err(|why| format!("cannot run git: {why}"))
    }

    /// Runs git as `call` says, its standard error going to null: a caller that reads none of git's errors has none
    /// written down.
    pub(crate) fn run(&self, call: &Call<'_>) -> Result<Ran, GitError> {
        let (status, stdout) = self.spawned(call, None)?;
        Ok(Ran { status, stdout })
    }

    /// Runs git as [`Git::run`] does, its standard error held in a temporary file, and returns what it wrote on stdout;
    /// else why the call failed.
    #[cfg_attr(
        not(any(test, feature = "test-seams")),
        expect(
            dead_code,
            reason = "only the remote client asks git this way, and a gate without the test seams has none"
        )
    )]
    pub(crate) fn output(&self, call: &Call<'_>) -> Result<String, GitError> {
        let cannot_hold = |error: io::Error| GitError::Failed(format!("cannot make a file for git's output: {error}"));
        let mut stderr = output_file().map_err(cannot_hold)?;
        let (status, mut stdout) = self.spawned(call, Some(&stderr))?;
        let mut output = Vec::new();
        let mut errors = Vec::new();
        stdout
            .read_to_end(&mut output)
            .and_then(|_| stderr.read_to_end(&mut errors))
            .map_err(|error| GitError::Failed(format!("cannot read git's output: {error}")))?;
        if !status.success() {
            return Err(GitError::Failed(failed_line(call, status.code(), &errors, &output)));
        }
        String::from_utf8(output).map_err(|_| GitError::Failed("git's output is not valid UTF-8".to_owned()))
    }

    /// git asked about `folder` alone, as every call starts it: the location and config variables removed, `-C
    /// <folder>`, and no file-system monitor. Its arguments, streams and start are the caller's, through
    /// [`gate_win32::spawn`].
    pub(crate) fn command(&self, folder: &Path) -> Command {
        let mut command = Command::new(&self.program);
        for name in GIT_LOCATION {
            command.env_remove(name);
        }
        for (name, _) in std::env::vars_os() {
            if name.to_str().is_some_and(is_indexed_config) {
                command.env_remove(name);
            }
        }
        command.arg("-C").arg(folder).args(["-c", "core.fsmonitor=false"]);
        command
    }

    /// Runs git with `call`'s args and variables, its standard error going to the file `stderr` when one is given, else
    /// to null; its exit status and the file holding its standard output are returned, and both that file and `stderr`
    /// are left read from their start. A call made while the console interrupt is already raised starts no process.
    fn spawned(&self, call: &Call<'_>, stderr: Option<&File>) -> Result<(ExitStatus, File), GitError> {
        if call.interrupt.is_some_and(Interrupt::is_raised) {
            return Err(GitError::Interrupted);
        }
        let cannot_hold = |error: io::Error| GitError::Failed(format!("cannot make a file for git's output: {error}"));
        let mut stdout = output_file().map_err(cannot_hold)?;
        let mut command = self.command(call.folder);
        for name in call.unset {
            command.env_remove(*name);
        }
        command
            .envs(call.set.iter().copied())
            .args(call.args)
            .stdin(Stdio::null())
            .stdout(stdout.try_clone().map_err(cannot_hold)?);
        match stderr {
            Some(file) => command.stderr(Stdio::from(file.try_clone().map_err(cannot_hold)?)),
            None => command.stderr(Stdio::null()),
        };
        let mut child = gate_win32::spawn(&mut command).map_err(|error| GitError::Failed(format!("cannot run git: {error}")))?;
        let status = wait_for(&mut child, call.limit, call.interrupt)?;
        // git's writes moved the files, which it shares with the handles here, so each is read from its start again.
        let cannot_read = |error: io::Error| GitError::Failed(format!("cannot read git's output: {error}"));
        stdout.seek(SeekFrom::Start(0)).map_err(cannot_read)?;
        if let Some(file) = stderr {
            let mut held = file;
            held.seek(SeekFrom::Start(0)).map_err(cannot_read)?;
        }
        Ok((status, stdout))
    }
}

/// One git call: the folder git is asked about, its arguments, the variables it is given and the ones it is not, how
/// long it may take, and the console interrupt that ends it.
pub(crate) struct Call<'a> {
    /// The folder git is asked about with `-C`.
    pub folder: &'a Path,
    pub args: &'a [&'a str],
    /// The variables git is given, beside the ones every call is given.
    pub set: &'a [(&'a str, &'a OsStr)],
    /// The variables git is not given, beside [`GIT_LOCATION`].
    pub unset: &'a [&'a str],
    pub limit: Duration,
    /// The console interrupt a Ctrl+C raises: git is ended with it, and the call is [`GitError::Interrupted`].
    pub interrupt: Option<Interrupt>,
}

impl<'a> Call<'a> {
    /// A call that sets and removes no variables of its own and answers to no interrupt: the routing decision asks git
    /// this way, before a command is started.
    pub(crate) fn plain(folder: &'a Path, args: &'a [&'a str], limit: Duration) -> Call<'a> {
        Call {
            folder,
            args,
            set: &[],
            unset: &[],
            limit,
            interrupt: None,
        }
    }
}

/// How a git call ended: its exit status, and its stdout, held in a temporary file read from its start.
pub(crate) struct Ran {
    pub status: ExitStatus,
    pub stdout: File,
}

/// Whether `name` is one of the config variables `GIT_CONFIG_COUNT` indexes, its letter case aside as Windows reads a
/// variable's name.
fn is_indexed_config(name: &str) -> bool {
    GIT_CONFIG_INDEXED
        .iter()
        .any(|prefix| name.get(..prefix.len()).is_some_and(|start| start.eq_ignore_ascii_case(prefix)) && name.len() > prefix.len())
}

/// git's exit status once it ends; after `limit` it is killed and the call fails, and a console interrupt raised while
/// it runs ends it as [`GitError::Interrupted`].
fn wait_for(child: &mut Child, limit: Duration, interrupt: Option<Interrupt>) -> Result<ExitStatus, GitError> {
    let deadline = Instant::now() + limit;
    loop {
        if interrupt.is_some_and(Interrupt::is_raised) {
            return match child.kill().and_then(|()| child.wait()) {
                Ok(_) => Err(GitError::Interrupted),
                Err(error) => Err(GitError::Failed(format!(
                    "the run was interrupted, and git could not be stopped: {error}"
                ))),
            };
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| GitError::Failed(format!("cannot wait for git: {error}")))?
        {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let late = late_git_line(limit);
            return match child.kill().and_then(|()| child.wait()) {
                Ok(_) => Err(GitError::Failed(late)),
                Err(error) => Err(GitError::Failed(format!("{late}, and could not be stopped: {error}"))),
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

/// Why a git call with `call`'s arguments failed, in the words the PowerShell client uses: the folder git was asked
/// about, its status, and every line it wrote, standard error's first, joined by ` | `.
#[cfg_attr(
    not(any(test, feature = "test-seams")),
    expect(
        dead_code,
        reason = "only the remote client reads the lines of a failed git, and a gate without the test seams has none"
    )
)]
fn failed_line(call: &Call<'_>, status: Option<i32>, errors: &[u8], output: &[u8]) -> String {
    let status = status.map_or_else(|| "unknown".to_owned(), |code| code.to_string());
    let errors = String::from_utf8_lossy(errors);
    let output = String::from_utf8_lossy(output);
    let lines: Vec<&str> = errors.lines().chain(output.lines()).collect();
    format!(
        "git {} failed in {} (status {status}): {}",
        call.args.join(" "),
        call.folder.display(),
        lines.join(" | ")
    )
}

/// A new file in the temporary folder for one git call's output, deleted once its last handle closes.
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

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use std::ffi::OsStr;
    use std::path::PathBuf;

    use super::{Call, GIT_LIMIT, Git, failed_line, is_indexed_config, late_git_line};

    #[test]
    fn git_is_given_none_of_the_config_variables() {
        let command = Git {
            program: PathBuf::from("git.exe"),
        }
        .command(Path::new(r"C:\work\app"));
        let removed: Vec<&OsStr> = command.get_envs().filter(|(_, value)| value.is_none()).map(|(name, _)| name).collect();
        for name in [
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_PARAMETERS",
            "GIT_DIR",
        ] {
            assert!(removed.contains(&OsStr::new(name)), "{name} is removed: {removed:?}");
        }
        for name in ["GIT_CONFIG_KEY_0", "GIT_CONFIG_VALUE_0", "GIT_CONFIG_KEY_12", "git_config_value_3"] {
            assert!(is_indexed_config(name), "{name}");
        }
        for name in ["GIT_CONFIG_KEYS", "GIT_CONFIG_NOSYSTEM", "GIT_CONFIG_KEY", "XGIT_CONFIG_KEY_0", "PATH"] {
            assert!(!is_indexed_config(name), "{name}");
        }
    }

    #[test]
    fn a_late_git_names_its_limit_in_seconds_or_milliseconds() {
        assert_eq!(late_git_line(GIT_LIMIT), "git did not answer within 60 s", "the default limit");
        assert_eq!(late_git_line(Duration::from_secs(60)), "git did not answer within 60 s");
        assert_eq!(late_git_line(Duration::from_secs(10)), "git did not answer within 10 s");
        assert_eq!(late_git_line(Duration::from_millis(1)), "git did not answer within 1 ms");
    }

    #[test]
    fn a_failed_git_line_names_the_call_its_folder_its_status_and_git_s_lines() {
        let call = Call::plain(Path::new(r"C:\work\app"), &["write-tree"], GIT_LIMIT);
        assert_eq!(
            failed_line(&call, Some(128), b"warning: first\r\nfatal: last\n", b"then out\n"),
            r"git write-tree failed in C:\work\app (status 128): warning: first | fatal: last | then out"
        );
        assert_eq!(
            failed_line(&call, Some(1), b"", b""),
            r"git write-tree failed in C:\work\app (status 1): ",
            "git wrote nothing"
        );
        assert_eq!(
            failed_line(&call, None, b"fatal: x\n", b""),
            r"git write-tree failed in C:\work\app (status unknown): fatal: x",
            "no exit code"
        );
    }
}
