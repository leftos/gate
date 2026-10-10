//! `gate box preflight`, the first call of every routed run: readies the work folder and its bare repository as the
//! PowerShell wrapper's `Initialize-WorkFolder` does, then says the version, boot and SDK lines.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use crate::answer::USAGE_STATUS;
use crate::boxside::boot;
use crate::boxside::payload::Payload;
use crate::console::Console;
use crate::environment::Variables;
use crate::run::{ERROR_FILE_NOT_FOUND, find_program};

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
    fn without_status(line: String) -> Failed {
        Failed { line, status: FAILED_STATUS }
    }
}

/// The failure of git `step` that exited with `code`: the line shows the code signed, as `gate run` and the wrapper's
/// `$LASTEXITCODE` show it, and the gate exits with its whole 32 bits.
fn git_failed(step: &str, code: i32) -> Failed {
    Failed {
        line: format!("gate: remote {step} failed (status {code})"),
        status: code.cast_unsigned(),
    }
}

/// Readies the folders the payload's `Bare` and `Work` name, then says the three trailing lines; returns the status.
pub(crate) fn preflight(payload: &Payload, console: &mut Console) -> u32 {
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
    console.out(&format!("gate: remote version {}", env!("CARGO_PKG_VERSION")));
    boot::say(console);
    say_sdks(console);
    0
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
    if git.code(&["-C", work, "config", "--get", "remote.origin.url"], Stdio::null()) != Ok(0) {
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
        match self.code(args, Stdio::inherit()) {
            Ok(0) => Ok(()),
            Ok(code) => Err(git_failed(step, code)),
            Err(message) => Err(Failed::without_status(format!("gate: remote {step} failed: {message}"))),
        }
    }

    /// git's exit code for `args`, its stdout sent to `stdout` and its stderr passed through; else why it could not be
    /// started. git reads no standard input and none of the variables that would point it elsewhere.
    fn code(&self, args: &[&str], stdout: Stdio) -> Result<i32, String> {
        let program = self.program.as_ref().map_err(Clone::clone)?;
        let mut command = Command::new(program);
        for name in GIT_REDIRECTS {
            command.env_remove(name);
        }
        // Started under the spawn lock, so it inherits no launch's handles.
        let status = gate_win32::spawn(command.args(args).stdin(Stdio::null()).stdout(stdout))
            .and_then(|mut child| child.wait())
            .map_err(|error| error.to_string())?;
        Ok(exit_code(status))
    }
}

/// The exit code of a process that ran to its end; on Windows every such process has one.
pub(crate) fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(FAILED_STATUS.cast_signed())
}

/// The file `word` names, found as `gate run` finds a command's program: along the gate's own `PATH` and `PATHEXT`,
/// never in the working folder unless `PATH` names it. `None` when it is not found; else why it could not be looked
/// for.
fn locate(word: &str) -> Result<Option<PathBuf>, String> {
    let folder = std::env::current_dir().map_err(|error| format!("cannot read the working directory: {error}"))?;
    match find_program(word, &Variables::from_process(), &folder) {
        Ok(program) => Ok(Some(program)),
        // `find_program` says a word it does not find with this message, and every other failure with another.
        Err(message) if message == gate_win32::win32_message(ERROR_FILE_NOT_FOUND) => Ok(None),
        Err(message) => Err(message),
    }
}

/// Says the SDK line on stdout; a list that cannot be read is said on stderr instead, and fails nothing.
fn say_sdks(console: &mut Console) {
    match sdks() {
        Ok(list) => console.out(&format!("gate: remote sdks {list}")),
        Err(message) => console.err(&format!("gate: remote could not read the SDK list: {message}")),
    }
}

/// The SDK list of `dotnet --list-sdks`, `none` when no dotnet is found; else why it could not be read.
fn sdks() -> Result<String, String> {
    let Some(dotnet) = locate("dotnet")? else {
        return Ok(sdk_list(""));
    };
    // Started under the spawn lock, so it inherits no launch's handles.
    let output = gate_win32::spawn(
        Command::new(&dotnet)
            .arg("--list-sdks")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()),
    )
    .and_then(std::process::Child::wait_with_output)
    .map_err(|error| format!("cannot start {}: {error}", dotnet.display()))?;
    if !output.status.success() {
        return Err(format!("dotnet --list-sdks exited with status {}", exit_code(output.status)));
    }
    Ok(sdk_list(&String::from_utf8_lossy(&output.stdout)))
}

/// The first space-separated word of each non-empty line of `listing`, joined by `,`; `none` when there is none.
fn sdk_list(listing: &str) -> String {
    let versions: Vec<&str> = listing
        .lines()
        .filter_map(|line| line.trim().split(' ').next())
        .filter(|version| !version.is_empty())
        .collect();
    if versions.is_empty() { "none".to_owned() } else { versions.join(",") }
}

#[cfg(test)]
mod tests {
    use super::{git_failed, sdk_list};

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
