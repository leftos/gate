//! `gate box preflight`, the first call of every routed run: readies the work folder and its bare repository as the
//! PowerShell wrapper's `Initialize-WorkFolder` does, then says the version, boot and SDK lines, and a line for each
//! tool the payload's `Needs` names.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::PoisonError;
use std::thread;
use std::time::Duration;

use gate_args::BoxSeams;

use crate::answer::USAGE_STATUS;
use crate::boxside::boot;
use crate::boxside::forward::{ChildPipes, DRAIN, Drained, OutputLock};
use crate::boxside::payload::Payload;
use crate::boxside::probe;
use crate::console::Console;
use crate::environment::Variables;
use crate::remote::git;
use crate::run::{ERROR_FILE_NOT_FOUND, find_program};
use crate::version::build_version;

/// The status of a step that failed with no status of its own: git or a folder that could not be made or written.
const FAILED_STATUS: u32 = 1;

/// The git variables that would point git at another repository, index, object store or config than the work folder's.
pub(crate) const GIT_REDIRECTS: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
];

/// A preflight step that failed: the line saying so, and the status the gate exits with.
pub(crate) struct Failed {
    pub(crate) line: String,
    pub(crate) status: u32,
}

impl Failed {
    pub(crate) fn without_status(line: String) -> Failed {
        Failed { line, status: FAILED_STATUS }
    }
}

/// The failure of git `step` that exited with `code`: the line shows the code signed, as `gate run` and the wrapper's
/// `$LASTEXITCODE` show it, and the gate exits with its whole 32 bits.
pub(crate) fn git_failed(step: &str, code: i32) -> Failed {
    Failed {
        line: format!("gate: remote {step} failed (status {code})"),
        status: code.cast_unsigned(),
    }
}

/// Readies the folders the payload's `Bare` and `Work` name, then says the three trailing lines, the SDK list read
/// within `--test-dotnet-limit-ms` when `seams` gives it, else [`git::DOTNET_LIMIT`], and the tool lines, probed within
/// `--test-probe-limit-ms` when `seams` gives it, else [`probe::PROBE_LIMIT`]; returns the status.
pub(crate) fn preflight(payload: &Payload, seams: &BoxSeams, console: &mut Console) -> u32 {
    let (bare, work) = match payload.path("Bare").and_then(|bare| Ok((bare, payload.work_folder()?.1))) {
        Ok(folders) => folders,
        Err(line) => {
            console.err(&line);
            return u32::from(USAGE_STATUS);
        }
    };
    if let Err(failed) = ready(bare, work, console) {
        console.err(&failed.line);
        return failed.status;
    }
    console.out(&format!("gate: remote version {}", build_version()));
    boot::say(console);
    say_sdks_and_tools(payload, seams, console);
    0
}

/// Says the SDK line, the list read within `--test-dotnet-limit-ms` when `seams` gives it, else
/// [`git::DOTNET_LIMIT`], then one line on stdout for each tool the payload's `Needs` names, in its order. The tools are
/// probed within `--test-probe-limit-ms` when `seams` gives it, else [`probe::PROBE_LIMIT`], while dotnet lists the
/// SDKs. A `Needs` that is not an object of texts is said on stderr, probes nothing, and fails nothing. The minimum each
/// need carries is not read: the box reports, the client compares.
fn say_sdks_and_tools(payload: &Payload, seams: &BoxSeams, console: &mut Console) {
    let names: Result<Vec<String>, String> = payload
        .pairs("Needs")
        .map(|needs| needs.into_iter().map(|(name, _)| name).collect())
        .map_err(|line| format!("{line}; probing nothing"));
    let probe_limit = seams.probe_limit_ms.map_or(probe::PROBE_LIMIT, Duration::from_millis);
    thread::scope(|scope| {
        let probes = names.as_ref().ok().filter(|names| !names.is_empty()).map(|names| {
            (
                names,
                scope.spawn(move || probe::probe(names, probe_limit, Ok(Variables::from_process()), None)),
            )
        });
        say_sdks(seams.dotnet_limit_ms.map_or(git::DOTNET_LIMIT, Duration::from_millis), console);
        if let Err(line) = &names {
            console.err(line);
        }
        let Some((names, probes)) = probes else {
            return;
        };
        let found = probes.join().unwrap_or_else(|panic| {
            let why = probe::panic_reason(&*panic);
            names.iter().map(|name| (name.clone(), probe::Probed::Skipped(why.clone()))).collect()
        });
        for (name, probed) in found {
            console.out(&probe::tool_line(&name, &probed));
        }
    });
}

/// The folder above `work`; the bare repository, said as created; the work folder's repository; its `origin`; its
/// line-ending config; and its alternates file pointing at the bare repository's objects.
fn ready(bare: &str, work: &str, console: &mut Console) -> Result<(), Failed> {
    let git = Git::find();
    if let Some(parent) = Path::new(work).parent().filter(|parent| !parent.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|error| Failed::without_status(format!("gate: remote could not create the folder {}: {error}", parent.display())))?;
    }
    if !Path::new(bare).exists() {
        git.step("git init --bare", &["init", "--bare", "-q", "--", bare])?;
        console.out(&format!("gate: remote created {bare}"));
    }
    if !Path::new(work).join(".git").exists() {
        git.step("git init", &["init", "-q", "--", work])?;
    }
    // Checked apart from .git, so a preflight cut short between the two steps is finished by the next one. A git that
    // cannot be started here fails the next step, which names it.
    if git.code(&["-C", work, "config", "--get", "remote.origin.url"], Stdio::null(), Stdio::inherit()) != Ok(0) {
        git.step("git remote add", &["-C", work, "remote", "add", "--", "origin", bare])?;
    }
    // The work folder's own config, so the checkout and the sync-back keep the snapshot's bytes whatever the builder's
    // system and global git config say.
    git.step("git config core.autocrlf", &["-C", work, "config", "core.autocrlf", "false"])?;
    git.step("git config core.eol", &["-C", work, "config", "core.eol", "lf"])?;
    write_alternates(bare, work)
}

/// Writes `<bare with / separators>/objects` and LF, UTF-8 without a BOM, as the work folder's alternates file when
/// that file is missing or empty.
fn write_alternates(bare: &str, work: &str) -> Result<(), Failed> {
    let info = Path::new(work).join(".git").join("objects").join("info");
    let alternates = info.join("alternates");
    if fs::metadata(&alternates).is_ok_and(|file| file.len() > 0) {
        return Ok(());
    }
    let text = format!("{}/objects\n", bare.replace('\\', "/"));
    fs::create_dir_all(&info)
        .and_then(|()| fs::write(&alternates, text))
        .map_err(|error| Failed::without_status(format!("gate: remote could not write {}: {error}", alternates.display())))
}

/// git, found once along the gate's own `PATH` and `PATHEXT`.
pub(crate) struct Git {
    pub(crate) program: Result<PathBuf, String>,
}

impl Git {
    pub(crate) fn find() -> Git {
        let program = locate("git").and_then(|found| found.ok_or_else(|| gate_win32::win32_message(ERROR_FILE_NOT_FOUND)));
        Git { program }
    }

    /// Runs git with `args`, its output passed through; a non-zero status, or a git that cannot be started, fails
    /// `step`, named as the wrapper names it.
    pub(crate) fn step(&self, step: &str, args: &[&str]) -> Result<(), Failed> {
        match self.code(args, Stdio::inherit(), Stdio::inherit()) {
            Ok(0) => Ok(()),
            Ok(code) => Err(git_failed(step, code)),
            Err(message) => Err(Failed::without_status(format!("gate: remote {step} failed: {message}"))),
        }
    }

    /// git's exit code for `args`, its stdout sent to `stdout` and its stderr to `stderr`; else why it could not be
    /// started.
    pub(crate) fn code(&self, args: &[&str], stdout: Stdio, stderr: Stdio) -> Result<i32, String> {
        let mut command = self.command(args)?;
        // Started under the spawn lock, so it inherits no launch's handles.
        let status = gate_win32::spawn(command.stdout(stdout).stderr(stderr))
            .and_then(|mut child| child.wait())
            .map_err(|error| error.to_string())?;
        Ok(exit_code(status))
    }

    /// git with `args`, reading no standard input and none of the variables that would point it elsewhere; else why
    /// it cannot be started.
    pub(crate) fn command(&self, args: &[&str]) -> Result<Command, String> {
        let program = self.program.as_ref().map_err(Clone::clone)?;
        let mut command = Command::new(program);
        for name in GIT_REDIRECTS {
            command.env_remove(name);
        }
        command.args(args).stdin(Stdio::null());
        Ok(command)
    }

    /// Starts the git `command` under the spawn lock, its stderr, and its stdout unless `read_stdout`, forwarded line by
    /// line under `lock` ([`ChildPipes`]), so no heartbeat lands inside a line of git's, and waits for it to end. A
    /// process still holding git's output once the readers have drained for [`DRAIN`] is said under `lock`, and the
    /// run goes on. git's output, its stdout empty unless read; else why it could not be started or waited for, or that
    /// a line of git's could not be written to the box's own output: the client closed the connection.
    pub(crate) fn forwarded(command: Command, read_stdout: bool, lock: &OutputLock, console: &mut Console) -> Result<Output, GitFailure> {
        let (output, drained) = run_forwarded(command, read_stdout, lock).map_err(GitFailure::Failed)?;
        if drained.pipe_closed {
            return Err(GitFailure::ClientGone);
        }
        if !drained.all_ended {
            let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
            console.err(GIT_LEFTOVER);
        }
        Ok(output)
    }
}

/// The line said when a leftover process still holds git's output open once git exited.
const GIT_LEFTOVER: &str = "gate: remote git's output is still held open by a leftover process; stopped reading";

/// Why a git whose output the box forwards gave no output.
pub(crate) enum GitFailure {
    /// It could not be started or waited for, for this reason.
    Failed(String),
    /// A line of its could not be written to the box's own output: the client closed the connection.
    ClientGone,
}

/// [`Git::forwarded`] up to the drain: git's output, and what the readers' drain found; else why it could not be
/// started or waited for. `command` is dropped once git is started, so the readers see the end of its pipes once git
/// and whatever it started have closed them.
fn run_forwarded(mut command: Command, read_stdout: bool, lock: &OutputLock) -> Result<(Output, Drained), String> {
    let ChildPipes { stdout, stderr, forwarders } = ChildPipes::open("git's", !read_stdout, lock).map_err(|error| error.to_string())?;
    // Started under the spawn lock, so it inherits no launch's handles.
    let started = gate_win32::spawn(command.stdout(stdout).stderr(stderr));
    drop(command);
    let output = started.and_then(Child::wait_with_output);
    let drained = forwarders.drain(DRAIN);
    Ok((output.map_err(|error| error.to_string())?, drained))
}

/// The exit code of a process that ran to its end; on Windows every such process has one.
pub(crate) fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(FAILED_STATUS.cast_signed())
}

/// The file `word` names, found as `gate run` finds a command's program: along the gate's own `PATH` and `PATHEXT`,
/// never in the working folder unless `PATH` names it. `None` when it is not found; else why it could not be looked
/// for.
pub(crate) fn locate(word: &str) -> Result<Option<PathBuf>, String> {
    let folder = std::env::current_dir().map_err(|error| format!("cannot read the working directory: {error}"))?;
    match find_program(word, &Variables::from_process(), &folder) {
        Ok(program) => Ok(Some(program)),
        // `find_program` says a word it does not find with this message, and every other failure with another.
        Err(message) if message == gate_win32::win32_message(ERROR_FILE_NOT_FOUND) => Ok(None),
        Err(message) => Err(message),
    }
}

/// Says the SDK line on stdout, the list read within `limit`; a list that cannot be read is said on stderr instead, and
/// fails nothing.
fn say_sdks(limit: Duration, console: &mut Console) {
    match sdks(limit) {
        Ok(list) => console.out(&format!("gate: remote sdks {list}")),
        Err(message) => console.err(&format!("gate: remote could not read the SDK list: {message}")),
    }
}

/// The SDK list of `dotnet --list-sdks`, `none` when no dotnet is found along the gate's own `PATH`; else why it could
/// not be read. dotnet is run as [`git::sdk_listing`] runs it, with no console interrupt and no worktree to keep it out
/// of, ended with its job past `limit`.
fn sdks(limit: Duration) -> Result<String, String> {
    let listing = git::sdk_listing(Variables::from_process(), limit, None, None)?;
    Ok(sdk_list(&listing))
}

/// The first space-separated word of each non-empty line of `listing`, joined by `,`; `none` when there is none.
fn sdk_list(listing: &str) -> String {
    let versions = crate::remote::parity::sdk_versions(listing);
    if versions.is_empty() { "none".to_owned() } else { versions.join(",") }
}

#[cfg(test)]
mod tests {
    use std::sync::PoisonError;

    use super::{Git, git_failed, run_forwarded, sdk_list};
    use crate::boxside::forward::OutputLock;

    #[test]
    #[expect(clippy::expect_used, reason = "a test whose git cannot start must fail loudly")]
    fn git_writes_its_stderr_through_the_output_lock() {
        let output = OutputLock::default();
        let missing = std::env::temp_dir().join(format!("gate-preflight-no-such-repository-{}", std::process::id()));
        let git_dir = format!("--git-dir={}", missing.display());
        let command = Git::find()
            .command(&[&git_dir, "rev-parse", "HEAD"])
            .expect("git is on PATH; the unit tests need it");
        let held = output.lock().unwrap_or_else(PoisonError::into_inner);

        // git says on stderr that the repository does not exist, then exits.
        let ran = run_forwarded(command, false, &output);
        drop(held);

        let (ran, drained) = ran.expect("git starts");
        assert_eq!(ran.status.code(), Some(128), "git's own status");
        assert!(
            !drained.all_ended,
            "the reader of git's stderr waits for the held output lock, so it cannot end within the drain"
        );
    }

    #[test]
    fn a_git_status_is_said_signed_and_exited_with_whole() {
        let failed = git_failed("git init", -1_073_741_819);
        assert_eq!(failed.line, "gate: remote git init failed (status -1073741819)");
        assert_eq!(failed.status, 3_221_225_477);
        let failed = git_failed("git config core.eol", 128);
        assert_eq!(failed.line, "gate: remote git config core.eol failed (status 128)");
        assert_eq!(failed.status, 128);
    }

    #[test]
    fn the_sdk_list_is_the_first_word_of_each_line() {
        let listing = "8.0.404 [C:\\Program Files\\dotnet\\sdk]\r\n\r\n  9.0.100 [C:\\Program Files\\dotnet\\sdk]\r\n";
        assert_eq!(sdk_list(listing), "8.0.404,9.0.100");
        assert_eq!(sdk_list(""), "none");
        assert_eq!(sdk_list("\r\n  \n"), "none");
    }
}
