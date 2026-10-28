//! git as the routing decision and the remote client run it: the executable found once along the gate's own `PATH`,
//! asked about one folder alone, started suspended in an anonymous kill-on-close job of its own on this process's
//! console or a hidden one of its own, its output read from pipes by a thread per stream, and ended with its whole job
//! after a time limit or by the console interrupt. Other programs are started, bounded and ended the same way
//! ([`program_output`]): `dotnet --list-sdks`, which this PC's parity check and the builder's preflight read
//! ([`sdk_listing`]), and the builder's tool probes.

use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::process::ExitStatusExt;
use std::path::{Component, Path, PathBuf, Prefix};
use std::process::{Command, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use gate_args::{TestSeams, command_line, is_batch_target};
use gate_router::escaped;
use gate_win32::{Interrupt, Job, PipedConsole, PipedLaunch, Running};

use super::launch::{DRAIN, KILL_WAIT, KILLED, call_job, launched, piped_launch};
use crate::environment::Variables;
use crate::run::{CONTROL_C_EXIT, ERROR_FILE_NOT_FOUND, Written, find_program, is_within, resolved, written};

/// How long one git call may take before git is killed.
pub(crate) const GIT_LIMIT: Duration = Duration::from_secs(60);
/// How long a `dotnet --list-sdks` may take before it is killed and its list counts as unreadable.
pub(crate) const DOTNET_LIMIT: Duration = Duration::from_secs(30);
/// How often a git call is looked at while it runs.
const GIT_POLL: Duration = Duration::from_millis(5);
/// The most of its standard output a call through [`Git::output`] is read for, as the sync-back's calls read the
/// builder's file lists: one that writes more fails.
pub(crate) const OUTPUT_CAP: usize = 16 * 1024 * 1024;
/// The most of the end of its standard error a call through [`Git::output`] keeps for its failure line, which carries
/// at most [`FAILURE_LINES`] and [`FAILURE_BYTES`] of it.
const ERRORS_KEPT: usize = 64 * 1024;
/// How much of a stream a reader takes from its pipe at a time.
const CHUNK: usize = 8 * 1024;
/// The variables that would point git at another repository, index, object store, config or pathspec reading; git is
/// asked about the gate's folder alone, so a gate inside a hook that exports them still routes. The indexed config
/// variables are removed too, by their prefixes. So are the ones that would change what git reports of the history it
/// is asked about (a graft file, a replace ref base, no-replace, a shallow file), the quarantine a receive hook sets,
/// which makes ref updates fail, and how git reaches the builder or what patch the sync-back applies (the ssh command
/// and proxy and their variant, and the external diff).
const GIT_LOCATION: [&str; 26] = [
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
    "GIT_SSH_VARIANT",
    "GIT_EXTERNAL_DIFF",
    "GIT_GRAFT_FILE",
    "GIT_REPLACE_REF_BASE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_SHALLOW_FILE",
    "GIT_QUARANTINE_PATH",
    "GIT_SSH_COMMAND",
    "GIT_SSH",
    "GIT_PROXY_COMMAND",
];
/// The variable that, set, keeps the working folder out of a program's search for another program by name, as
/// `CreateProcess` and `SearchPath` make it.
pub(crate) const NO_CURRENT_FOLDER: &str = "NoDefaultCurrentDirectoryInExePath";
/// The prefixes of the config variables `GIT_CONFIG_COUNT` indexes: `GIT_CONFIG_KEY_<n>` and `GIT_CONFIG_VALUE_<n>`.
const GIT_CONFIG_INDEXED: [&str; 2] = ["GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"];
/// `FILE_FLAG_DELETE_ON_CLOSE`: the file of git's output is gone once its last handle closes.
const DELETE_ON_CLOSE: u32 = 0x0400_0000;

static NEXT_OUTPUT: AtomicU32 = AtomicU32::new(0);

/// How long each git call of a run may take: `--test-git-limit-ms` when given, else [`GIT_LIMIT`].
pub(crate) fn limit(seams: &TestSeams) -> Duration {
    seams.git_limit_ms.map_or(GIT_LIMIT, Duration::from_millis)
}

/// How long the readers of each git call of a run, and of the remote client's read of this PC's `dotnet --list-sdks`,
/// get to end once the program has exited or the call ended otherwise: `--test-git-drain-ms` when given, else [`DRAIN`].
/// A placement call's readers get [`placing_drain`] instead when git exits by itself ([`Call::placing`]).
pub(crate) fn drain(seams: &TestSeams) -> Duration {
    seams.git_drain_ms.map_or(DRAIN, Duration::from_millis)
}

/// How long the readers of a call that places a folder get once git has exited by itself: as long as git itself may
/// take, or the call's own drain when that is longer. A starved gate may not run a reader within [`DRAIN`] of git's
/// exit, and the place is then lost. The long drain costs time only when a process outside git's job holds a pipe — a
/// Store-installed program git starts runs outside the job — and `rev-parse` with [`PLACE_ARGS`](crate::route::PLACE_ARGS)
/// starts none, since the job is ended at git's exit and its own readers end as soon as they get CPU.
pub(crate) fn placing_drain(limit: Duration, drain: Duration) -> Duration {
    limit.max(drain)
}

/// The bounds of one git call: how long it may take, and how long its readers get to reach their pipes' ends once git
/// has exited. One value, so no call takes one bound and misses the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GitBounds {
    pub limit: Duration,
    pub drain: Duration,
}

impl GitBounds {
    /// The bounds of a run given neither git seam: [`GIT_LIMIT`] and [`DRAIN`].
    pub(crate) const DEFAULT: GitBounds = GitBounds {
        limit: GIT_LIMIT,
        drain: DRAIN,
    };

    /// The bounds of each git call of a run: [`limit`] and [`drain`] of `seams`, [`GitBounds::DEFAULT`] with neither.
    pub(crate) fn of(seams: &TestSeams) -> GitBounds {
        GitBounds {
            limit: limit(seams),
            drain: drain(seams),
        }
    }
}

/// A unit test's bound on a git call, its readers' drain or a lock wait: a hang guard no passing call reaches, however
/// loaded the machine.
#[cfg(test)]
pub(crate) const TEST_HANG_GUARD: Duration = Duration::from_hours(1);

/// The bounds of a unit test's git calls: [`TEST_HANG_GUARD`] for both.
#[cfg(test)]
pub(crate) const GIT_BOUNDS: GitBounds = GitBounds {
    limit: TEST_HANG_GUARD,
    drain: TEST_HANG_GUARD,
};

/// Why a git call did not finish: the console interrupt ended it, or it failed, with the failure in the words the
/// PowerShell client uses.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum GitError {
    /// The console interrupt was raised, and git was ended with it: the run is an interrupt, not a failure.
    Interrupted,
    /// The call failed: what could not be done, or, for a git that ran and refused, `git <args> failed in <root>
    /// (status <n>): <the lines git wrote>`.
    Failed(String),
    /// The call wrote more than its reader's cap on standard output and was ended as soon as the reader saw it: its
    /// line names the call, its folder and the cap, as a failure does.
    OverCap(String),
}

/// A failed call's reason; an interrupt, which has no line of its own, says the run was interrupted.
impl From<GitError> for String {
    fn from(error: GitError) -> String {
        match error {
            GitError::Interrupted => "the run was interrupted".to_owned(),
            GitError::Failed(why) | GitError::OverCap(why) => why,
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

    /// Runs git as `call` says, its standard output copied whole into a temporary file and its standard error read and
    /// dropped: a caller that reads none of git's errors has none written down.
    pub(crate) fn run(&self, call: &Call<'_>) -> Result<Ran, GitError> {
        let mut file = output_file().map_err(|error| GitError::Failed(format!("cannot make a file for git's output: {error}")))?;
        let into_file = move |mut pipe: File| -> io::Result<File> {
            io::copy(&mut pipe, &mut file)?;
            file.seek(SeekFrom::Start(0))?;
            Ok(file)
        };
        let (status, stdout, ()) = self.spawned(call, into_file, dropped, None)?;
        Ok(Ran { status, stdout })
    }

    /// Runs git as [`Git::run`] does, with the last [`ERRORS_KEPT`] bytes of its standard error kept beside the call's
    /// end, for a caller that judges git's status itself and says its errors when it fails.
    pub(crate) fn run_kept(&self, call: &Call<'_>) -> Result<(Ran, Vec<u8>), GitError> {
        let mut file = output_file().map_err(|error| GitError::Failed(format!("cannot make a file for git's output: {error}")))?;
        let into_file = move |mut pipe: File| -> io::Result<File> {
            io::copy(&mut pipe, &mut file)?;
            file.seek(SeekFrom::Start(0))?;
            Ok(file)
        };
        let (status, stdout, errors) = self.spawned(call, into_file, |pipe: File| tail(pipe, ERRORS_KEPT), None)?;
        Ok((Ran { status, stdout }, errors))
    }

    /// Runs git as [`Git::run`] does and returns what it wrote on standard output, read up to [`OUTPUT_CAP`]; else why
    /// the call failed, from the last [`ERRORS_KEPT`] of its standard error.
    pub(crate) fn output(&self, call: &Call<'_>) -> Result<String, GitError> {
        let output = self.output_bytes(call)?;
        String::from_utf8(output).map_err(|_| GitError::Failed("git's output is not valid UTF-8".to_owned()))
    }

    /// [`Git::output`] with the bytes git wrote and no check that they are UTF-8: a caller that reads a tree's own
    /// bytes, a path that need not be text or a file of any encoding, asks this way.
    pub(crate) fn output_bytes(&self, call: &Call<'_>) -> Result<Vec<u8>, GitError> {
        self.output_within(call, OUTPUT_CAP, ERRORS_KEPT)
    }

    /// [`Git::output_bytes`] with standard output read up to `cap` bytes and the last [`ERRORS_KEPT`] bytes of standard
    /// error kept: a caller whose own cap bounds an answer it asked for, as a batch of object ids is.
    pub(crate) fn output_bytes_within(&self, call: &Call<'_>, cap: usize) -> Result<Vec<u8>, GitError> {
        self.output_within(call, cap, ERRORS_KEPT)
    }

    /// git run as [`Git::output_bytes_within`] runs it, with its exit status beside what it wrote on standard output, read
    /// up to `cap` bytes, and the last [`ERRORS_KEPT`] bytes of its standard error, whatever the status: for a caller
    /// that judges the status itself and says [`failed_line`] when it fails. A call that writes more than `cap` is ended
    /// as soon as its reader sees it, and is an over-cap call.
    pub(crate) fn status_within(&self, call: &Call<'_>, cap: usize) -> Result<(ExitStatus, Vec<u8>, Vec<u8>), GitError> {
        self.capped(call, cap, ERRORS_KEPT)
    }

    /// git run with standard output read up to `cap` bytes and the last `kept` bytes of standard error kept, and its
    /// exit status: a call that writes more than `cap` is ended as soon as its reader sees it, and is an over-cap call.
    fn capped(&self, call: &Call<'_>, cap: usize, kept: usize) -> Result<(ExitStatus, Vec<u8>, Vec<u8>), GitError> {
        let (over, read_capped) = capped_reader(cap, over_line(call, cap));
        let (status, output, errors) = self.spawned(call, read_capped, move |pipe: File| tail(pipe, kept), Some(&over))?;
        let Some(output) = output else {
            return Err(GitError::OverCap(over.line));
        };
        Ok((status, output, errors))
    }

    /// [`Git::output_bytes`] with standard output read up to `cap` bytes and the last `kept` bytes of standard error
    /// kept: a call that writes more than `cap` is ended as soon as its reader sees it, and is an over-cap call.
    fn output_within(&self, call: &Call<'_>, cap: usize, kept: usize) -> Result<Vec<u8>, GitError> {
        let (status, output, errors) = self.capped(call, cap, kept)?;
        if !status.success() {
            return Err(GitError::Failed(failed_line(call, status.code(), &errors, &output)));
        }
        Ok(output)
    }

    /// git asked about `folder` alone, as every call starts it: the location and config variables removed,
    /// [`NO_CURRENT_FOLDER`] set, so the ssh git starts finds no program by name in the gate's working folder, `-C
    /// <folder>`, no file-system monitor, and its hooks read from a folder that cannot exist, inside the git program's
    /// own file, so no hook a repository's config names runs. Its arguments, streams and start are the caller's.
    pub(crate) fn command(&self, folder: &Path) -> Command {
        let mut command = Command::new(&self.program);
        command.env(NO_CURRENT_FOLDER, "1");
        for name in GIT_LOCATION {
            command.env_remove(name);
        }
        for (name, _) in std::env::vars_os() {
            if name.to_str().is_some_and(is_indexed_config) {
                command.env_remove(name);
            }
        }
        let mut hooks = OsString::from("core.hooksPath=");
        hooks.push(self.program.join("hooks"));
        command.arg("-C").arg(folder).args(["-c", "core.fsmonitor=false", "-c"]).arg(hooks);
        command
    }

    /// The git command `call` runs: [`Git::command`] asked about its folder, its variables unset and set, then its args.
    pub(crate) fn prepared(&self, call: &Call<'_>) -> Command {
        let mut command = self.command(call.folder);
        for name in call.unset {
            command.env_remove(*name);
        }
        command.envs(call.set.iter().copied()).args(call.args);
        command
    }

    /// Runs git with `call`'s args and variables on the console `call` names, through [`bounded`].
    fn spawned<O, E>(
        &self,
        call: &Call<'_>,
        stdout: impl FnOnce(File) -> io::Result<O> + Send + 'static,
        stderr: impl FnOnce(File) -> io::Result<E> + Send + 'static,
        over: Option<&Over>,
    ) -> Result<(ExitStatus, O, E), GitError>
    where
        O: Send + 'static,
        E: Send + 'static,
    {
        let command = self.prepared(call);
        let spawn = Spawn {
            name: "git",
            late: late_git_line(call.limit),
            launch: piped_launch(&command, call.console),
        };
        bounded(&spawn, call, stdout, stderr, over)
    }
}

/// A process a bounded call starts: the name its failure lines call it by, the line it is killed under once it outlives
/// its call's limit, and its launch, or why none could be made.
struct Spawn<'a> {
    name: &'a str,
    late: String,
    launch: io::Result<PipedLaunch>,
}

/// A program other than git started, bounded and ended as the client's git calls are ([`program_output`]): its name,
/// looked for along a `PATH`, its arguments, how long it may take, how long its readers get to end once it has exited,
/// the console interrupt that ends it, the worktrees it must not be found inside, none for no such check, the console
/// it is created on: this process's, or a hidden one of its own; and the bytes fed on its standard input, which is then
/// closed, `None` for none.
pub(crate) struct ProgramCall<'a> {
    pub program: &'a str,
    pub args: &'a [&'a str],
    pub limit: Duration,
    pub drain: Duration,
    pub interrupt: Option<Interrupt>,
    pub worktrees: &'a [&'a Path],
    pub console: PipedConsole,
    pub stdin: Option<&'a [u8]>,
}

/// Where [`program_output`] runs a program: the system folder, and an environment with
/// `NoDefaultCurrentDirectoryInExePath=1` set, so no program is found by name in the working folder. Read once, it
/// serves every program of a batch.
pub(crate) struct ProgramPlace {
    folder: PathBuf,
    variables: Variables,
}

impl ProgramPlace {
    /// The system folder and `variables` with `NoDefaultCurrentDirectoryInExePath=1`; else why the system folder
    /// cannot be read.
    pub(crate) fn new(variables: Variables) -> Result<ProgramPlace, String> {
        let folder = gate_win32::system_directory().map_err(|error| format!("cannot read the system folder: {error}"))?;
        Ok(ProgramPlace {
            folder,
            variables: variables.with(NO_CURRENT_FOLDER, "1"),
        })
    }
}

impl ProgramCall<'_> {
    /// `<program> <args>`, as the call's failure lines name it.
    fn named(&self) -> String {
        std::iter::once(self.program)
            .chain(self.args.iter().copied())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The line the call fails with once it outlives its limit, the limit as [`spelled_limit`] says it:
    /// `<program> <args> did not answer in <limit>`.
    pub(crate) fn late_line(&self) -> String {
        format!("{} did not answer in {}", self.named(), spelled_limit(self.limit))
    }
}

/// How a program that ran to its exit ended: its exit status, what it wrote on standard output, and the end of what it
/// wrote on standard error.
pub(crate) struct ProgramOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Runs `call`'s program, `None` when it is not found along the `PATH` of `place`'s environment; the search never looks
/// in the working folder, and when the call gives a worktree a program found is followed through links, refused when
/// that leads inside it and else launched as the file it leads to, its command line's first word still the name found
/// along `PATH`. The program runs in `place`'s system folder with its environment, a batch file through the system
/// folder's cmd.exe, as the client's git calls run: in a kill-on-close job of its own, on the call's console, ended past
/// the call's limit (failing with [`ProgramCall::late_line`], followed by why it could not be stopped when it could not)
/// or by its console interrupt, its standard output read up to [`OUTPUT_CAP`] and the last [`ERRORS_KEPT`] bytes of its
/// standard error kept. Else why it could not be run (`cannot run <program>: <why>`), or [`GitError::Interrupted`].
pub(crate) fn program_output(call: &ProgramCall<'_>, place: &ProgramPlace) -> Result<Option<ProgramOutput>, GitError> {
    let program = call.program;
    let cannot_run = |why: String| GitError::Failed(format!("cannot run {program}: {why}"));
    let (folder, variables) = (&place.folder, &place.variables);
    let found = match find_program(program, variables, folder) {
        Ok(found) => found,
        // `find_program` says a word it does not find with this message, and every other failure with another.
        Err(message) if message == gate_win32::win32_message(ERROR_FILE_NOT_FOUND) => return Ok(None),
        Err(message) => return Err(cannot_run(message)),
    };
    let target = if call.worktrees.is_empty() {
        found.clone()
    } else {
        judged_among(&found, call.worktrees).map_err(cannot_run)?
    };
    let bounds = program_call(folder, call);
    let spawn = Spawn {
        name: program,
        late: call.late_line(),
        launch: program_launch(&found, &target, call, place),
    };
    let over = format!("{} wrote more than {} MiB", call.named(), OUTPUT_CAP / (1024 * 1024));
    let (status, stdout, stderr) = output_of(&spawn, &bounds, over)?;
    Ok(Some(ProgramOutput { status, stdout, stderr }))
}

/// The git call the program of [`program_output`] is run as: [`ProgramCall`]'s limit, drain, interrupt, console and
/// standard input, in `folder`.
fn program_call<'a>(folder: &'a Path, call: &ProgramCall<'a>) -> Call<'a> {
    Call {
        drain: call.drain,
        interrupt: call.interrupt,
        console: call.console,
        stdin: call.stdin,
        ..Call::plain(folder, call.args, call.limit)
    }
}

/// What `dotnet --list-sdks` lists, empty when no dotnet is found; dotnet is run as [`program_output`] runs a program,
/// with `variables`, `limit`, `drain`, `interrupt` and `worktrees`, and a failed read's reason carries the last lines of its
/// standard error. This PC's parity check and the builder's preflight both read the list this way. Else why the list
/// cannot be read, or [`GitError::Interrupted`].
pub(crate) fn sdk_listing(
    variables: Variables,
    limit: Duration,
    drain: Duration,
    interrupt: Option<Interrupt>,
    worktrees: &[&Path],
) -> Result<String, GitError> {
    let call = ProgramCall {
        program: "dotnet",
        args: &["--list-sdks"],
        limit,
        drain,
        interrupt,
        worktrees,
        console: PipedConsole::Attached,
        stdin: None,
    };
    let place = ProgramPlace::new(variables).map_err(|why| GitError::Failed(format!("cannot run dotnet: {why}")))?;
    let Some(ProgramOutput {
        status,
        stdout: output,
        stderr: errors,
    }) = program_output(&call, &place)?
    else {
        return Ok(String::new());
    };
    if !status.success() {
        let status = status.code().map_or_else(|| "unknown".to_owned(), |code| code.to_string());
        let errors = String::from_utf8_lossy(&errors);
        let lines: Vec<&str> = errors.lines().filter(|line| !line.trim().is_empty()).collect();
        let said = if lines.is_empty() {
            String::new()
        } else {
            format!(": {}", failure_text(&lines))
        };
        return Err(GitError::Failed(format!("dotnet --list-sdks exited with status {status}{said}")));
    }
    Ok(String::from_utf8_lossy(&output).into_owned())
}

/// The file `program`, found along a `PATH`, leads to, followed through links, which is what is then launched, so a
/// link retargeted after the check cannot change what runs; else why it is refused, when that file lies inside
/// `worktree`: a folder outside the worktree may still hold a link that leads into it. dotnet and the client's ssh are
/// judged so. The refusal covers only the file launched: a program it starts in turn (a multi-call proxy such as
/// rustup's, a shim) is not judged.
pub(crate) fn judged(program: &Path, worktree: &Path) -> Result<PathBuf, String> {
    judged_among(program, &[worktree])
}

/// The file `program` leads to, read once as [`judged`] reads it, refused when it lies inside any of `worktrees`: a
/// sibling pair's repositories are each a worktree no program of the run may come from.
fn judged_among(program: &Path, worktrees: &[&Path]) -> Result<PathBuf, String> {
    let target = resolved(program);
    if worktrees.iter().any(|worktree| is_within(&target, &resolved(worktree))) {
        return Err(format!("{} lies inside the worktree", program.display()));
    }
    Ok(without_verbatim(&target))
}

/// `path` without the `\\?\` prefix a canonical path carries, which cmd.exe does not take: `\\?\C:\a` as `C:\a`, and
/// `\\?\UNC\server\share\a` as `\\server\share\a`. Any other path is kept as it is.
pub(crate) fn without_verbatim(path: &Path) -> PathBuf {
    let mut parts = path.components();
    let Some(Component::Prefix(prefix)) = parts.next() else {
        return path.to_owned();
    };
    let mut root = OsString::new();
    match prefix.kind() {
        Prefix::VerbatimDisk(disk) => root.push(format!("{}:\\", char::from(disk))),
        Prefix::VerbatimUNC(server, share) => {
            root.push(r"\\");
            root.push(server);
            root.push(r"\");
            root.push(share);
            root.push(r"\");
        }
        _ => return path.to_owned(),
    }
    parts
        .filter(|part| !matches!(part, Component::RootDir))
        .fold(PathBuf::from(root), |joined, part| joined.join(part))
}

/// The launch of `target`, the file the program found along `PATH` at `found` leads to, with `call`'s arguments, in
/// `place`'s folder, on `call`'s console, with `place`'s variables as its environment. The image started is `target`
/// and its command line's first word is `found`, the name the program was found by, since a multi-call program such as
/// rustup's proxies picks what to be from that word; a batch target runs through the system folder's cmd.exe given
/// `target`, as `gate run` starts one.
fn program_launch(found: &Path, target: &Path, call: &ProgramCall<'_>, place: &ProgramPlace) -> io::Result<PipedLaunch> {
    let (folder, variables) = (&place.folder, &place.variables);
    let args: Vec<OsString> = call.args.iter().map(OsString::from).collect();
    let (application, command_line) = if is_batch_target(&target.to_string_lossy()) {
        match written(target, &args, variables) {
            Written::Line(application, command_line) => (application, command_line),
            Written::Refused(why) | Written::NoShell(why) => return Err(io::Error::other(why)),
        }
    } else {
        let mut words: Vec<Vec<u16>> = vec![found.as_os_str().encode_wide().collect()];
        words.extend(args.iter().map(|word| word.encode_wide().collect()));
        (target.to_owned(), command_line(&words))
    };
    Ok(PipedLaunch {
        application,
        command_line,
        directory: folder.clone(),
        environment: variables.block()?,
        console: call.console,
        token: None,
    })
}

/// Runs `spawn`'s process as `call` bounds it, as [`Git::output_bytes`] runs git: its exit status, what it wrote on
/// standard output, read up to [`OUTPUT_CAP`], and the last [`ERRORS_KEPT`] bytes of its standard error. One that
/// writes more on standard output is ended as soon as its reader sees it, and the call is [`GitError::OverCap`] with
/// the line `over`.
fn output_of(spawn: &Spawn<'_>, call: &Call<'_>, over: String) -> Result<(ExitStatus, Vec<u8>, Vec<u8>), GitError> {
    let (over, read_capped) = capped_reader(OUTPUT_CAP, over);
    let (status, output, errors) = bounded(spawn, call, read_capped, |pipe: File| tail(pipe, ERRORS_KEPT), Some(&over))?;
    let Some(output) = output else {
        return Err(GitError::OverCap(over.line));
    };
    Ok((status, output, errors))
}

/// Runs `spawn`'s process, created suspended with no standard input, or with `call`'s input fed by a thread of its own
/// and then closed, and put in an anonymous kill-on-close job of its own before it runs; a thread per stream reads its
/// standard output with `stdout` and its standard error with `stderr`, and what they read is returned with its exit
/// status once it has exited. Whatever it left running then is ended with its job. A call made while the console
/// interrupt is already raised starts no process; one whose wait ends otherwise than in the process's exit (it outlived
/// `call`'s limit, was interrupted, wrote more than `over` allows, or could not be waited for) has its whole job ended.
fn bounded<O, E>(
    spawn: &Spawn<'_>,
    call: &Call<'_>,
    stdout: impl FnOnce(File) -> io::Result<O> + Send + 'static,
    stderr: impl FnOnce(File) -> io::Result<E> + Send + 'static,
    over: Option<&Over>,
) -> Result<(ExitStatus, O, E), GitError>
where
    O: Send + 'static,
    E: Send + 'static,
{
    if call.interrupt.is_some_and(Interrupt::is_raised) {
        return Err(GitError::Interrupted);
    }
    let name = spawn.name;
    let stops = Stops {
        deadline: Instant::now() + call.limit,
        over,
    };
    let cannot_run = |error: &dyn std::fmt::Display| GitError::Failed(format!("cannot run {name}: {error}"));
    let job = call_job(false).map_err(|error| cannot_run(&error))?;
    let launch = spawn.launch.as_ref().map_err(|error| cannot_run(error))?;
    let (suspended, out_pipe, err_pipe, in_pipe) = launched(launch, &job, call.stdin.is_some()).map_err(|error| cannot_run(&error))?;
    let running = suspended.assign_and_resume(&job).map_err(|error| cannot_run(&error))?;
    let output = reading(out_pipe, stdout);
    let errors = reading(err_pipe, stderr);
    // The feed writes on a thread of its own so this thread keeps watching the limit, the interrupt and the output
    // cap: a process that stops reading its input would block the write forever, and its limit would never fire.
    let fed = in_pipe.zip(call.stdin).map(|(pipe, bytes)| {
        let bytes = bytes.to_vec();
        reading(pipe, move |mut pipe: File| pipe.write_all(&bytes))
    });
    let code = match waited(&running, &job, spawn, call, &stops) {
        Ok(code) => code,
        Err(error) => {
            // Whatever ended the wait, the job is ended now, so the readers reach their pipes' ends; what they
            // read is no longer needed, and they are given a short while to end before they are left to it.
            let error = with_job_ended(error, &job, name);
            let until = Instant::now() + call.drain;
            drop((received(&output, until), received(&errors, until)));
            return Err(error);
        }
    };
    // The process has exited; whatever it left running, which would hold its pipes open, ends with the job, and the
    // readers then reach their pipes' ends within a short while.
    drop(job);
    let drain = call.drain_once_exited();
    let until = Instant::now() + drain;
    let not_read = || {
        if call.interrupt.is_some_and(Interrupt::is_raised) {
            return GitError::Interrupted;
        }
        GitError::Failed(format!(
            "cannot read {name}'s output: it did not end within {} of {name}'s exit",
            spelled_limit(drain)
        ))
    };
    let output = received_or_interrupted(&output, until, call.interrupt).ok_or_else(not_read)?;
    let errors = received_or_interrupted(&errors, until, call.interrupt).ok_or_else(not_read)?;
    let cannot_read = |error: io::Error| GitError::Failed(format!("cannot read {name}'s output: {error}"));
    let status = ExitStatus::from_raw(code);
    fed_whole(fed.as_ref(), status, until, call, name)?;
    Ok((status, output.map_err(cannot_read)?, errors.map_err(cannot_read)?))
}

/// Reads `pipe` to its end and drops what it read: the standard error of a call that reads none of it.
fn dropped(mut pipe: File) -> io::Result<()> {
    io::copy(&mut pipe, &mut io::sink()).map(drop)
}

/// Whether the process `name`, which exited with `status`, was fed its whole input by `fed`, waited for until `until`:
/// one that failed says why in its own lines, so a feed it cut short by exiting is no failure of its own, and a call
/// with no input has nothing to check. One that succeeded while its feed failed, or whose feed did not end within the
/// call's drain once it exited by itself, fails the call; an interrupt raised during the wait ends it as
/// [`GitError::Interrupted`].
fn fed_whole(fed: Option<&Receiver<io::Result<()>>>, status: ExitStatus, until: Instant, call: &Call<'_>, name: &str) -> Result<(), GitError> {
    let Some(fed) = fed.filter(|_| status.success()) else {
        return Ok(());
    };
    match received_or_interrupted(fed, until, call.interrupt) {
        Some(Ok(())) => Ok(()),
        Some(Err(error)) => Err(GitError::Failed(format!("cannot feed {name} its input: {error}"))),
        None if call.interrupt.is_some_and(Interrupt::is_raised) => Err(GitError::Interrupted),
        None => Err(GitError::Failed(format!(
            "cannot feed {name} its input: it did not end within {} of {name}'s exit",
            spelled_limit(call.drain_once_exited())
        ))),
    }
}

/// A call whose standard output is read up to a cap: whether its reader has seen more, and the line it then fails with.
struct Over {
    seen: Arc<AtomicBool>,
    line: String,
}

/// A reader of standard output that reads up to `cap` bytes, and the [`Over`] it tells when it saw more, which fails
/// with `line`.
fn capped_reader(cap: usize, line: String) -> (Over, impl FnOnce(File) -> io::Result<Option<Vec<u8>>> + Send + 'static) {
    let over = Over {
        seen: Arc::new(AtomicBool::new(false)),
        line,
    };
    let seen = Arc::clone(&over.seen);
    let read_capped = move |pipe: File| {
        let held = capped(pipe, cap)?;
        if held.is_none() {
            seen.store(true, Ordering::Release);
        }
        Ok(held)
    };
    (over, read_capped)
}

/// What ends a running call's wait besides its exit: its deadline, and its reader seeing more output than its cap.
struct Stops<'s> {
    deadline: Instant,
    over: Option<&'s Over>,
}

/// Reads `pipe` with `read` on a thread of its own; its result arrives on the channel returned.
fn reading<T: Send + 'static>(pipe: File, read: impl FnOnce(File) -> io::Result<T> + Send + 'static) -> Receiver<io::Result<T>> {
    let (sent, received) = mpsc::channel();
    thread::spawn(move || {
        // The receiver is gone only once the call stopped waiting on this stream, when what it read is not needed.
        drop(sent.send(read(pipe)));
    });
    received
}

/// What a reader read, once it ended before `until`; `None` when it had not. A reader that ended with no result is a
/// read that failed.
fn received<T>(from: &Receiver<io::Result<T>>, until: Instant) -> Option<io::Result<T>> {
    match from.recv_timeout(until.saturating_duration_since(Instant::now())) {
        Ok(result) => Some(result),
        Err(RecvTimeoutError::Timeout) => None,
        Err(RecvTimeoutError::Disconnected) => Some(Err(io::Error::other("its reader ended with no result"))),
    }
}

/// How long a wait between looks at a raised interrupt: short enough that a Ctrl+C during a drain as long as a call's
/// limit ends the call at once.
const INTERRUPT_SLICE: Duration = Duration::from_millis(50);

/// What [`received`] reads, unless the call's `interrupt` is raised before a result arrives: `None` when neither came
/// before `until`. As [`waited`] gives a signalled handle the win over a raised interrupt, a result already in the
/// channel is taken first, and a drain as long as the call's limit is waited for in slices so an interrupt ends it.
fn received_or_interrupted<T>(from: &Receiver<io::Result<T>>, until: Instant, interrupt: Option<Interrupt>) -> Option<io::Result<T>> {
    let Some(interrupt) = interrupt else {
        return received(from, until);
    };
    loop {
        if let Some(read) = received(from, Instant::now()) {
            return Some(read);
        }
        if interrupt.is_raised() {
            return None;
        }
        let now = Instant::now();
        if now >= until {
            return None;
        }
        if let Some(read) = received(from, now + (until - now).min(INTERRUPT_SLICE)) {
            return Some(read);
        }
    }
}

/// What `reader` sends up to `cap` bytes; `None` once it sends more, which is read only to its first byte past the cap.
fn capped(reader: impl Read, cap: usize) -> io::Result<Option<Vec<u8>>> {
    let mut held = Vec::new();
    let limit = u64::try_from(cap).unwrap_or(u64::MAX).saturating_add(1);
    reader.take(limit).read_to_end(&mut held)?;
    Ok((held.len() <= cap).then_some(held))
}

/// The last `kept` bytes `reader` sends, read to its end while at most twice that is held; when the stream was longer,
/// the cut line at their start is dropped when a line break follows it before their last byte, so they start at a
/// whole line when they hold one, and keep their last line whole when it is all they hold.
fn tail(mut reader: impl Read, kept: usize) -> io::Result<Vec<u8>> {
    let mut held = Vec::new();
    let mut chunk = vec![0; CHUNK];
    let mut cut = false;
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        held.extend_from_slice(chunk.get(..read).unwrap_or_default());
        if held.len() > kept.saturating_mul(2) {
            held.drain(..held.len() - kept);
            cut = true;
        }
    }
    if held.len() > kept {
        held.drain(..held.len() - kept);
        cut = true;
    }
    if cut
        && let Some(end) = held.iter().position(|byte| *byte == b'\n')
        && end + 1 < held.len()
    {
        held.drain(..=end);
    }
    Ok(held)
}

/// The line a call that wrote more than `cap` bytes on standard output fails with: the cap in whole MiB when it is a
/// whole number of them, else in bytes.
fn over_line(call: &Call<'_>, cap: usize) -> String {
    const MIB: usize = 1024 * 1024;
    let size = match cap % MIB {
        0 => format!("{} MiB", cap / MIB),
        _ => format!("{cap} bytes"),
    };
    format!("{}: its output passed {size}", failed_in(call))
}

/// `git <args> failed in <folder>`, how a failed call's line starts, its args and folder escaped, as either may name
/// what the builder sent.
fn failed_in(call: &Call<'_>) -> String {
    format!(
        "git {} failed in {}",
        escaped(&call.args.join(" ")),
        escaped(&call.folder.to_string_lossy())
    )
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
    /// How long git's readers get to reach their pipes' ends once the call ended otherwise than by git's own exit: its
    /// interrupt raised, or its limit outlived.
    pub drain: Duration,
    /// How long git's readers get to reach their pipes' ends once git has exited by itself and its job is dropped, when
    /// that is not `drain`: `None` for every call but a placement call, whose readers get [`placing_drain`]
    /// ([`Call::placing`]), and set by [`Call::drain_once_exited`].
    pub exit_drain: Option<Duration>,
    /// The console interrupt a Ctrl+C raises: git is ended with it, and the call is [`GitError::Interrupted`].
    pub interrupt: Option<Interrupt>,
    /// The console git is created on: this process's, so a Ctrl+C reaches git as it reaches the gate, or a hidden one
    /// of its own, which no Ctrl+C reaches.
    pub console: PipedConsole,
    /// What git reads on its standard input, written whole and then closed so git reaches its end; with none, its
    /// standard input is `NUL`.
    pub stdin: Option<&'a [u8]>,
}

impl<'a> Call<'a> {
    /// A call as [`Call::bounded`] makes it, ended after `limit`, its readers given [`GitBounds::DEFAULT`]'s drain.
    pub(crate) fn plain(folder: &'a Path, args: &'a [&'a str], limit: Duration) -> Call<'a> {
        Call::bounded(folder, args, GitBounds { limit, ..GitBounds::DEFAULT })
    }

    /// A call on this process's console that sets and removes no variables of its own, answers to no interrupt, reads
    /// nothing on its standard input, is ended after `bounds`' limit and gives its readers `bounds`' drain once git has
    /// exited: the remote client asks git this way, with the run's bounds.
    pub(crate) fn bounded(folder: &'a Path, args: &'a [&'a str], bounds: GitBounds) -> Call<'a> {
        Call {
            folder,
            args,
            set: &[],
            unset: &[],
            limit: bounds.limit,
            drain: bounds.drain,
            exit_drain: None,
            interrupt: None,
            console: PipedConsole::Attached,
            stdin: None,
        }
    }

    /// A call that places a folder ([`PLACE_ARGS`](crate::route::PLACE_ARGS) asked): ended after `limit`, or when
    /// `interrupt` is raised, its readers given `drain` when the call ends otherwise than by git's own exit and
    /// [`placing_drain`] when git exits by itself.
    pub(crate) fn placing(folder: &'a Path, args: &'a [&'a str], limit: Duration, drain: Duration, interrupt: Option<Interrupt>) -> Call<'a> {
        Call {
            interrupt,
            exit_drain: Some(placing_drain(limit, drain)),
            ..Call::bounded(folder, args, GitBounds { limit, drain })
        }
    }

    /// How long git's readers get to reach their pipes' ends once git has exited by itself and its job is dropped: the
    /// call's own `drain`, or the longer one a placement call sets.
    pub(crate) fn drain_once_exited(&self) -> Duration {
        self.exit_drain.unwrap_or(self.drain)
    }

    /// A call as [`Call::bounded`] makes it, but on a hidden console of its own, which no Ctrl+C reaches: a call that
    /// takes a lock in the git folder and lets it go ends only on its own or at its limit, so no Ctrl+C kills git
    /// while it holds the lock.
    pub(crate) fn locked(folder: &'a Path, args: &'a [&'a str], bounds: GitBounds) -> Call<'a> {
        Call {
            console: PipedConsole::Hidden,
            ..Call::bounded(folder, args, bounds)
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

/// The exit code of `spawn`'s process once it ends, or [`GitError::Interrupted`] when that is a Ctrl+C's with the
/// console interrupt raised. Past the deadline of `stops` its job is ended and the call fails with `spawn`'s late line;
/// once its reader saw more of its output than the cap of `stops` allows, its job is ended and the call is an over-cap
/// call, whether the process had ended of it or not; a console interrupt raised while it runs ends its job as
/// [`GitError::Interrupted`].
fn waited(running: &Running, job: &Job, spawn: &Spawn<'_>, call: &Call<'_>, stops: &Stops<'_>) -> Result<u32, GitError> {
    let name = spawn.name;
    loop {
        if call.interrupt.is_some_and(Interrupt::is_raised) {
            return Err(match ended(running, job) {
                Ok(()) => GitError::Interrupted,
                Err(error) => GitError::Failed(format!("the run was interrupted, and {name} could not be stopped: {error}")),
            });
        }
        if let Some(over) = stops.over.filter(|over| over.seen.load(Ordering::Acquire)) {
            return Err(stopped(over.line.clone(), GitError::OverCap, running, job));
        }
        if let Some(code) = running
            .wait(GIT_POLL)
            .map_err(|error| GitError::Failed(format!("cannot wait for {name}: {error}")))?
        {
            return exited(code, call);
        }
        if Instant::now() >= stops.deadline {
            return Err(stopped(spawn.late.clone(), GitError::Failed, running, job));
        }
    }
}

/// The process's exit `code`, or [`GitError::Interrupted`] when it exited of a Ctrl+C and the console interrupt is
/// raised by now: a Ctrl+C reaches it as it reaches the gate, so it may exit of it before the wait saw the interrupt.
fn exited(code: u32, call: &Call<'_>) -> Result<u32, GitError> {
    if code == CONTROL_C_EXIT && call.interrupt.is_some_and(Interrupt::is_raised) {
        Err(GitError::Interrupted)
    } else {
        Ok(code)
    }
}

/// `error`, the end of a call whose wait did not end in the exit of its process `name`, once its whole job is ended,
/// so nothing of it keeps running or holds its pipes open; with why the job could not be ended when it could not.
fn with_job_ended(error: GitError, job: &Job, name: &str) -> GitError {
    match job.terminate(KILLED.cast_unsigned()) {
        Ok(()) => error,
        Err(kill) => GitError::Failed(format!("{}, and {name} could not be stopped: {kill}", String::from(error))),
    }
}

/// The end `why` of a call whose job is ended now, built by `end`: [`GitError::OverCap`] for a call whose reader saw
/// it pass its cap, [`GitError::Failed`] for one that outlived its limit. A job that cannot be ended says so beside
/// `why`, whatever `end` is.
fn stopped(why: String, end: fn(String) -> GitError, running: &Running, job: &Job) -> GitError {
    match ended(running, job) {
        Ok(()) => end(why),
        Err(error) => GitError::Failed(format!("{why}, and could not be stopped: {error}")),
    }
}

/// Ends every process of a call's job and waits [`KILL_WAIT`] for its own process to exit; else why it could not.
fn ended(running: &Running, job: &Job) -> Result<(), String> {
    job.terminate(KILLED.cast_unsigned()).map_err(|error| error.to_string())?;
    match running.wait(KILL_WAIT) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(format!("it did not end within {} s of its kill", KILL_WAIT.as_secs())),
        Err(error) => Err(error.to_string()),
    }
}

/// The line a git that outlived `limit` is killed under, the limit as [`spelled_limit`] says it.
pub(crate) fn late_git_line(limit: Duration) -> String {
    format!("git did not answer within {}", spelled_limit(limit))
}

/// A call's `limit` as its late line says it: in whole seconds when it is a whole number of seconds, else in whole
/// milliseconds.
pub(crate) fn spelled_limit(limit: Duration) -> String {
    match limit.as_millis() % 1000 {
        0 => format!("{} s", limit.as_secs()),
        _ => format!("{} ms", limit.as_millis()),
    }
}

/// Why a git call with `call`'s arguments failed, in the words the PowerShell client uses: the folder git was asked
/// about, its status, and what [`failure_text`] keeps of the lines it wrote, standard output's first and standard
/// error's last, so that its last error lines are the ones kept. The args, the folder and the lines are each escaped
/// once, as any of them may name what the builder sent.
pub(crate) fn failed_line(call: &Call<'_>, status: Option<i32>, errors: &[u8], output: &[u8]) -> String {
    let status = status.map_or_else(|| "unknown".to_owned(), |code| code.to_string());
    let errors = String::from_utf8_lossy(errors);
    let output = String::from_utf8_lossy(output);
    let lines: Vec<&str> = output.lines().chain(errors.lines()).collect();
    format!("{} (status {status}): {}", failed_in(call), failure_text(&lines))
}

/// The most lines, the last, and the most bytes of them that a failure line carries of a call's lines.
const FAILURE_LINES: usize = 10;
pub(crate) const FAILURE_BYTES: usize = 4096;

/// `text` cut to its first `most` bytes, at a character's boundary.
pub(crate) fn first_bytes(text: &str, most: usize) -> &str {
    let mut end = text.len().min(most);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default()
}

/// Why git failed with the exit `code`, `None` when it had none, having written `errors` on standard error: `status
/// <n>`, then its last error lines when it wrote any ([`failure_text`]).
pub(crate) fn git_failure(code: Option<i32>, errors: &[u8]) -> String {
    let status = code.map_or_else(|| "unknown".to_owned(), |code| code.to_string());
    let errors = String::from_utf8_lossy(errors);
    let lines: Vec<&str> = errors.lines().filter(|line| !line.trim().is_empty()).collect();
    if lines.is_empty() {
        format!("status {status}")
    } else {
        format!("status {status}: {}", failure_text(&lines))
    }
}

/// The last [`FAILURE_LINES`] of a failed call's `lines`, each as its last carriage-return update leaves it, joined by
/// ` | `, escaped and cut to their last [`FAILURE_BYTES`]: what a failure line carries of them. The characters are
/// counted from the end by the width of their escapes and only those that fit are escaped, so the escaped text stays
/// within the bytes and the cut never splits a character or an escape.
pub(crate) fn failure_text<S: AsRef<str>>(lines: &[S]) -> String {
    let last = lines.get(lines.len().saturating_sub(FAILURE_LINES)..).unwrap_or_default();
    let shown: Vec<&str> = last.iter().map(|line| last_update(line.as_ref())).collect();
    let joined = shown.join(" | ");
    let mut start = joined.len();
    let mut width = 0;
    for (at, character) in joined.char_indices().rev() {
        width += escaped(character.encode_utf8(&mut [0; 4])).len();
        if width > FAILURE_BYTES {
            break;
        }
        start = at;
    }
    escaped(joined.get(start..).unwrap_or_default())
}

/// What a progress line shows once its carriage-return updates are done: the text after its last carriage return.
/// Preflight and failure text are read this way on purpose, each line keeping its last update and dropping what came
/// before it.
pub(crate) fn last_update(line: &str) -> &str {
    line.rsplit('\r').next().unwrap_or(line)
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
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a test whose git cannot be found or whose bytes cannot be read must fail loudly"
)]
mod tests {
    use std::ffi::OsStr;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;
    use std::thread::{self, ScopedJoinHandle};
    use std::time::{Duration, Instant};

    use gate_win32::{Interrupt, PipedConsole};
    use jiff::Timestamp;

    use super::{
        Call, DRAIN, ERRORS_KEPT, FAILURE_BYTES, GIT_LIMIT, Git, GitError, NO_CURRENT_FOLDER, OUTPUT_CAP, ProgramCall, ProgramPlace, capped,
        failed_line, failure_text, is_indexed_config, late_git_line, program_call, program_output, received_or_interrupted, sdk_listing, tail,
        without_verbatim,
    };
    use crate::environment::Variables;
    use crate::remote::snapshot::repository;
    use gate_router::escaped;

    /// How long a case waits for what a passing run always sees: only a broken product reaches it.
    const HANG_GUARD: Duration = Duration::from_secs(600);
    /// How often a case looks again for what it waits for.
    const POLL: Duration = Duration::from_millis(50);
    /// Set for the process of its own the interrupt case runs in.
    const OWN_PROCESS: &str = "GATE_TEST_GIT_INTERRUPT_OWN_PROCESS";

    /// Git's trust for this checkout, which on a machine whose folder is owned by another account git otherwise refuses
    /// with "detected dubious ownership": `safe.directory` set to `*` through a call's own variables, which
    /// `Git::command` gives to git after stripping the ones this process inherited.
    fn trust() -> [(&'static str, &'static OsStr); 3] {
        [
            ("GIT_CONFIG_COUNT", OsStr::new("1")),
            ("GIT_CONFIG_KEY_0", OsStr::new("safe.directory")),
            ("GIT_CONFIG_VALUE_0", OsStr::new("*")),
        ]
    }

    /// This crate's folder, inside the repository's own checkout, and git found along `PATH` for it.
    fn this_checkout() -> (&'static Path, Git) {
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        (here, Git::find(here).expect("git is found along PATH"))
    }

    /// A new repository for the case `name` whose `held.slow` git runs through the clean filter `slow`: its folder, the
    /// file the filter writes its pid to, and the go file the filter waits for, which no case writes. Both files are in
    /// the git folder, where `git add -A` never looks.
    fn slow_repository(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = repository(name);
        fs::write(root.join(".gitattributes"), "*.slow filter=slow\n").expect("the attributes can be written");
        fs::write(root.join("held.slow"), "held\n").expect("a filtered file can be written");
        let pid_file = root.join(".git").join("filter.pid");
        let go = root.join(".git").join("go");
        (root, pid_file, go)
    }

    /// The `-c` value that makes `slow` a clean filter running Windows PowerShell, which writes `pid <its pid>` to
    /// `pid_file` and then waits for `go`: a git call that runs it cannot finish on its own. Not pwsh: a pwsh installed
    /// from the Store starts outside the job of the process that starts it, so no job's kill reaches it.
    fn waiting_filter(pid_file: &Path, go: &Path) -> String {
        let slashed = |path: &Path| path.display().to_string().replace('\\', "/");
        format!(
            "filter.slow.clean=powershell -NoProfile -NonInteractive -Command \"Set-Content -LiteralPath '{}' -Value ('pid ' + \\$PID); \
             while (-not (Test-Path -LiteralPath '{}')) {{ Start-Sleep -Milliseconds 100 }}\"",
            slashed(pid_file),
            slashed(go)
        )
    }

    /// Writes the go file when dropped, so a filter a failing case leaves running ends.
    struct LetGo<'a>(&'a Path);

    impl Drop for LetGo<'_> {
        fn drop(&mut self) {
            // A filter that cannot see the file is one the case already saw end, or one left to its folder's cleanup.
            drop(fs::write(self.0, ""));
        }
    }

    /// Removes a case's repository when dropped; a case declares it before its [`LetGo`], so its filter is let go
    /// first, and a pass or a panic leaves no folder behind.
    struct Removed(PathBuf);

    impl Drop for Removed {
        fn drop(&mut self) {
            // A filter a failing case could not end may still hold its folder; that folder is left to the next run of
            // the case, which removes it before making the repository again.
            drop(fs::remove_dir_all(&self.0));
        }
    }

    /// The pid the filter wrote to `pid_file`, once it has written its whole line.
    fn written_pid(pid_file: &Path) -> Option<u32> {
        let text = fs::read_to_string(pid_file).ok()?;
        text.strip_suffix('\n')?.trim_end().strip_prefix("pid ")?.parse().ok()
    }

    /// The pid the filter wrote, waited for while `call` runs: a call that ended before the filter wrote it fails the
    /// case at once, and one whose filter never writes it fails at the hang guard.
    fn pid_while_running<T>(pid_file: &Path, call: &ScopedJoinHandle<'_, T>) -> u32 {
        let deadline = Instant::now() + HANG_GUARD;
        loop {
            if let Some(pid) = written_pid(pid_file) {
                return pid;
            }
            assert!(!call.is_finished(), "git ended before its filter wrote its pid");
            assert!(Instant::now() < deadline, "the filter wrote no pid within the hang guard");
            thread::sleep(POLL);
        }
    }

    /// Waits until the process `pid`, which started at `started` when it was last seen, has ended: gone, or its pid
    /// taken by a later process.
    fn wait_until_gone(pid: u32, started: Option<Timestamp>) {
        let deadline = Instant::now() + HANG_GUARD;
        while started.is_some() && gate_win32::process_start_of(pid).expect("the process can be looked for") == started {
            assert!(Instant::now() < deadline, "the filter's process {pid} did not end within the hang guard");
            thread::sleep(POLL);
        }
    }

    #[test]
    fn an_interrupt_while_git_runs_ends_as_interrupted() {
        // The console interrupt is the process's own and never lowered, and other cases here raise it: this case runs
        // in a process of its own, the test binary run on this case alone, where only it raises the interrupt.
        if std::env::var_os(OWN_PROCESS).is_none() {
            let name = "remote::git::tests::an_interrupt_while_git_runs_ends_as_interrupted";
            let mut own = Command::new(std::env::current_exe().expect("the test binary has a path"));
            own.args(["--exact", name, "--test-threads=1"])
                .env(OWN_PROCESS, "1")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let ran = gate_win32::spawn(&mut own)
                .and_then(Child::wait_with_output)
                .expect("the test binary runs the case in a process of its own");
            let said = format!("{}{}", String::from_utf8_lossy(&ran.stdout), String::from_utf8_lossy(&ran.stderr));
            assert!(ran.status.success() && said.contains("1 passed"), "{}: {said}", ran.status);
            return;
        }
        let (root, pid_file, go) = slow_repository("git-interrupt");
        let _removed = Removed(root.clone());
        let _let_go = LetGo(&go);
        let clean = waiting_filter(&pid_file, &go);
        let args = ["-c", clean.as_str(), "add", "-A"];
        let git = Git::find(&root).expect("git is found along PATH");
        let interrupt = Interrupt::install().expect("the console handler is installed");
        // The hang guard as limit: in a passing run only the interrupt ends the call.
        let call = Call {
            interrupt: Some(interrupt),
            ..Call::plain(&root, &args, HANG_GUARD)
        };
        let (ended, pid, started) = thread::scope(|scope| {
            let running = scope.spawn(|| git.output(&call));
            let pid = pid_while_running(&pid_file, &running);
            let started = gate_win32::process_start_of(pid).expect("the filter can be looked for");
            interrupt.raise();
            (running.join().expect("the call returns"), pid, started)
        });
        assert_eq!(ended, Err(GitError::Interrupted));
        wait_until_gone(pid, started);
    }

    /// A folder of its own in the unit tests' temp folder for the case `name` of this process, holding a `dotnet.cmd`
    /// that runs `body` after `@echo off`, and the `PATH` that names it alone.
    fn dotnet_folder(name: &str, body: &str) -> (PathBuf, Variables) {
        let folder = crate::test_support::temp_dir().join(format!("gate-sdk-{name}-{}", std::process::id()));
        drop(fs::remove_dir_all(&folder));
        fs::create_dir_all(&folder).expect("the stub's folder can be made");
        fs::write(folder.join("dotnet.cmd"), format!("@echo off\r\n{body}")).expect("the stub dotnet can be written");
        let variables = Variables::from_process().with("PATH", &folder.to_string_lossy());
        (folder, variables)
    }

    #[test]
    fn an_interrupt_while_dotnet_runs_ends_as_interrupted() {
        // The console interrupt is the process's own and never lowered, and other cases here raise it: this case runs
        // in a process of its own, the test binary run on this case alone, where only it raises the interrupt.
        if std::env::var_os(OWN_PROCESS).is_none() {
            let name = "remote::git::tests::an_interrupt_while_dotnet_runs_ends_as_interrupted";
            let mut own = Command::new(std::env::current_exe().expect("the test binary has a path"));
            own.args(["--exact", name, "--test-threads=1"])
                .env(OWN_PROCESS, "1")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let ran = gate_win32::spawn(&mut own)
                .and_then(Child::wait_with_output)
                .expect("the test binary runs the case in a process of its own");
            let said = format!("{}{}", String::from_utf8_lossy(&ran.stdout), String::from_utf8_lossy(&ran.stderr));
            assert!(ran.status.success() && said.contains("1 passed"), "{}: {said}", ran.status);
            return;
        }
        let (folder, variables) = dotnet_folder("interrupt", "");
        let _removed = Removed(folder.clone());
        let (pid_file, go) = (folder.join("dotnet.pid"), folder.join("go"));
        let _let_go = LetGo(&go);
        // Windows PowerShell, named by its full path as the stub's PATH names its own folder alone, writes its pid and
        // then waits for the go file no case writes.
        let slashed = |path: &Path| path.display().to_string().replace('\\', "/");
        let waits = format!(
            "\"%SystemRoot%\\System32\\WindowsPowerShell\\v1.0\\powershell.exe\" -NoProfile -NonInteractive -Command \
             \"Set-Content -LiteralPath '{}' -Value ('pid ' + $PID); \
             while (-not (Test-Path -LiteralPath '{}')) {{ Start-Sleep -Milliseconds 100 }}\"\r\n",
            slashed(&pid_file),
            slashed(&go)
        );
        fs::write(folder.join("dotnet.cmd"), format!("@echo off\r\n{waits}")).expect("the stub dotnet can be written");
        let interrupt = Interrupt::install().expect("the console handler is installed");
        // The hang guard as limit: in a passing run only the interrupt ends the call.
        let (ended, pid, started) = thread::scope(|scope| {
            let running = scope.spawn(|| sdk_listing(variables, HANG_GUARD, DRAIN, Some(interrupt), &[]));
            let pid = pid_while_running(&pid_file, &running);
            let started = gate_win32::process_start_of(pid).expect("the stub can be looked for");
            interrupt.raise();
            (running.join().expect("the read returns"), pid, started)
        });
        assert_eq!(ended, Err(GitError::Interrupted));
        wait_until_gone(pid, started);
    }

    #[test]
    fn a_failing_dotnet_says_its_last_error_lines() {
        let (folder, variables) = dotnet_folder("failing", ">&2 echo first\r\n>&2 echo the SDK resolver failed\r\nexit /b 3\r\n");
        let _removed = Removed(folder);

        let read = sdk_listing(variables, HANG_GUARD, DRAIN, None, &[]);

        let said = "dotnet --list-sdks exited with status 3: first | the SDK resolver failed";
        assert_eq!(read, Err(GitError::Failed(said.to_owned())));
    }

    #[test]
    fn a_failing_dotnet_that_says_nothing_says_its_status_alone() {
        let (folder, variables) = dotnet_folder("failing-quiet", "exit /b 3\r\n");
        let _removed = Removed(folder);

        let read = sdk_listing(variables, HANG_GUARD, DRAIN, None, &[]);

        assert_eq!(read, Err(GitError::Failed("dotnet --list-sdks exited with status 3".to_owned())));
    }

    #[test]
    fn a_verbatim_path_loses_its_prefix_for_cmd() {
        for (path, plain) in [
            (r"\\?\C:\Program Files\dotnet\dotnet.exe", r"C:\Program Files\dotnet\dotnet.exe"),
            (r"\\?\UNC\server\share\dotnet\dotnet.cmd", r"\\server\share\dotnet\dotnet.cmd"),
            (r"C:\dotnet\dotnet.exe", r"C:\dotnet\dotnet.exe"),
            (r"\\server\share\dotnet.cmd", r"\\server\share\dotnet.cmd"),
        ] {
            assert_eq!(without_verbatim(Path::new(path)), PathBuf::from(plain), "{path}");
        }
    }

    #[test]
    fn a_dotnet_that_lies_inside_the_worktree_is_not_run() {
        let (folder, variables) = dotnet_folder("inside", "");
        let _removed = Removed(folder.clone());
        let marker = folder.join("ran");
        let stub = format!("@echo off\r\n>\"{}\" echo ran\r\necho 9.0.100 [C:\\stub]\r\n", marker.display());
        fs::write(folder.join("dotnet.cmd"), stub).expect("the stub dotnet can be written");

        // The dotnet lies inside the second of two worktrees, as one inside a pair's sibling does.
        let refused = sdk_listing(variables, HANG_GUARD, DRAIN, None, &[Path::new(env!("CARGO_MANIFEST_DIR")), &folder]);
        let ran_inside = marker.exists();
        let again = Variables::from_process().with("PATH", &folder.to_string_lossy());
        let elsewhere = sdk_listing(again, HANG_GUARD, DRAIN, None, &[Path::new(env!("CARGO_MANIFEST_DIR"))]);

        let Err(GitError::Failed(why)) = refused else {
            panic!("a dotnet inside the worktree was read: {refused:?}");
        };
        assert!(
            why.starts_with("cannot run dotnet: ") && why.ends_with(" lies inside the worktree"),
            "{why}"
        );
        assert!(!ran_inside, "the dotnet inside the worktree ran");
        assert_eq!(
            elsewhere,
            Ok("9.0.100 [C:\\stub]\r\n".to_owned()),
            "the same dotnet outside the worktree is read"
        );
    }

    #[test]
    fn a_git_call_past_its_limit_says_it_did_not_answer() {
        let (root, pid_file, go) = slow_repository("git-limit");
        let _removed = Removed(root.clone());
        let _let_go = LetGo(&go);
        let clean = waiting_filter(&pid_file, &go);
        let args = ["-c", clean.as_str(), "add", "-A"];
        let git = Git::find(&root).expect("git is found along PATH");
        // git cannot finish while its filter waits, so only the limit ends the call, however slow the machine.
        let call = Call::plain(&root, &args, Duration::from_millis(1));
        assert_eq!(git.output(&call), Err(GitError::Failed("git did not answer within 1 ms".to_owned())));
        // A 1 ms limit mostly ends git before its filter starts; a filter that did start ended with git's job.
        if let Some(pid) = written_pid(&pid_file) {
            wait_until_gone(pid, gate_win32::process_start_of(pid).expect("the filter can be looked for"));
        }
    }

    #[test]
    fn a_batch_file_git_says_why_it_cannot_run() {
        let program = crate::test_support::temp_dir().join("gate-git-batch").join("git.cmd");
        let git = Git { program: program.clone() };
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        let Err(GitError::Failed(line)) = git.run(&Call::plain(here, &["version"], GIT_LIMIT)) else {
            panic!("a batch file is never run as git");
        };
        assert_eq!(
            line,
            format!(
                "cannot run git: check the launch: the application {} is a batch file, which runs only through cmd.exe",
                program.display()
            )
        );
    }

    #[test]
    fn a_failure_line_cuts_its_escaped_text_to_4_kib() {
        let line = ["\u{1b}".repeat(1000) + "end"];
        let text = failure_text(&line);
        assert!(text.len() <= FAILURE_BYTES, "{} bytes", text.len());
        assert!(text.ends_with("end"), "{text}");
        let start = text.get(..24).unwrap_or(&text);
        assert!(text.starts_with(&escaped("\u{1b}")), "the cut starts at a whole escape: {start}");
    }

    /// The over branch of the wait, the one that is not git's own exit, ends a call as over-cap too: here git writes
    /// past a 1-byte cap, then blocks in a clean filter waiting for its go file, so the wait ends the call while git is
    /// still running. `GIT_FLUSH` is what makes git write those lines before it blocks.
    #[test]
    fn an_over_cap_call_that_keeps_running_is_over_cap() {
        let (root, pid_file, go) = slow_repository("git-over-cap");
        let _removed = Removed(root.clone());
        let _let_go = LetGo(&go);
        let clean = waiting_filter(&pid_file, &go);
        let args = ["-c", clean.as_str(), "add", "-A", "-v"];
        let git = Git::find(&root).expect("git is found along PATH");
        let call = Call {
            set: &[("GIT_FLUSH", OsStr::new("1"))],
            ..Call::plain(&root, &args, HANG_GUARD)
        };

        match git.output_within(&call, 1, ERRORS_KEPT) {
            Err(GitError::OverCap(why)) => assert!(why.ends_with(": its output passed 1 bytes"), "{why}"),
            other => panic!("a call whose reader saw it pass its cap is an over-cap call, whether or not git had ended: {other:?}"),
        }
    }

    /// git reads what a call feeds it to its end: one object written from a short input and one from an input larger
    /// than a pipe holds, then both sizes asked in one `cat-file` fed their two ids, one per line.
    #[test]
    fn a_call_fed_its_standard_input_reads_it_to_its_end() {
        let root = repository("git-fed");
        let _removed = Removed(root.clone());
        let git = Git::find(&root).expect("git is found along PATH");
        let written = |content: &[u8]| {
            let call = Call {
                stdin: Some(content),
                ..Call::plain(&root, &["hash-object", "-w", "--stdin"], GIT_LIMIT)
            };
            git.output(&call).expect("git writes the object it is fed").trim_end().to_owned()
        };
        let short = written(b"one\n");
        let long = written(&vec![b'x'; 100_000]);
        let ids = format!("{short}\n{long}\n");
        let call = Call {
            stdin: Some(ids.as_bytes()),
            ..Call::plain(&root, &["cat-file", "--batch-check=%(objectsize)"], GIT_LIMIT)
        };

        assert_eq!(git.output(&call).expect("git reads both ids"), "4\n100000\n");
    }

    #[test]
    fn output_over_the_cap_is_over_cap_instead_of_reading_it_whole() {
        let (here, git) = this_checkout();
        let trust = trust();
        let call = Call {
            set: &trust,
            ..Call::plain(here, &["ls-files"], GIT_LIMIT)
        };
        assert_eq!(
            git.output_bytes(&call).expect("git lists its files"),
            git.output(&call).expect("git lists its files, as text").into_bytes(),
            "the bytes are what the text reads"
        );
        assert_eq!(
            git.output_within(&call, 64, ERRORS_KEPT),
            Err(GitError::OverCap(format!(
                "git ls-files failed in {}: its output passed 64 bytes",
                here.display()
            )))
        );
        assert_eq!(
            capped(io::repeat(b'x'), 64).expect("an endless stream can be read"),
            None,
            "an endless stream is read only to one byte past the cap, so the read ends"
        );
        assert_eq!(
            capped(&b"abcd"[..], 4).expect("bytes can be read"),
            Some(b"abcd".to_vec()),
            "output at the cap is whole"
        );
        assert_eq!(capped(&b"abcde"[..], 4).expect("bytes can be read"), None, "one byte past the cap");
    }

    #[test]
    fn a_failed_git_reads_only_the_tail_of_its_standard_error() {
        let (here, git) = this_checkout();
        let missing: Vec<String> = (1..=40).map(|number| format!("missing-{number:02}")).collect();
        let mut args = vec!["ls-files", "--error-unmatch", "--"];
        args.extend(missing.iter().map(String::as_str));
        let trust = trust();
        let call = Call {
            set: &trust,
            ..Call::plain(here, &args, GIT_LIMIT)
        };
        let Err(GitError::Failed(line)) = git.output_within(&call, OUTPUT_CAP, 512) else {
            panic!("git ls-files --error-unmatch fails on a missing path");
        };
        assert!(line.contains(" (status 1): "), "{line}");
        assert!(line.contains("pathspec 'missing-40'"), "the last error line is kept: {line}");

        let text = (1..=10_000).fold(String::new(), |text, number| text + "line " + &number.to_string() + "\n");
        let kept = tail(text.as_bytes(), 512).expect("bytes can be read");
        assert!(kept.len() <= 512, "only the tail is held: {} bytes", kept.len());
        assert!(kept.starts_with(b"line "), "the tail starts at a whole line");
        assert!(kept.ends_with(b"line 9999\nline 10000\n"), "the tail is the stream's end");
        assert_eq!(
            tail(&b"a\nb\n"[..], 512).expect("bytes can be read"),
            b"a\nb\n",
            "a short stream is kept whole"
        );

        let mut progress = b"x\r".repeat(512);
        progress.extend_from_slice(b"fatal: x\n");
        let kept = tail(progress.as_slice(), 512).expect("bytes can be read");
        assert!(
            kept.ends_with(b"fatal: x\n"),
            "a cut tail whose one line break is its last byte keeps its reason: {}",
            String::from_utf8_lossy(&kept)
        );
    }

    #[test]
    fn a_failure_line_shows_each_kept_line_s_last_progress_update() {
        let lines = [
            "Receiving objects:  1% (1/90)\rReceiving objects: 50% (45/90)",
            "fatal: the remote end hung up unexpectedly",
        ];
        assert_eq!(
            failure_text(&lines),
            "Receiving objects: 50% (45/90) | fatal: the remote end hung up unexpectedly"
        );
    }

    #[test]
    fn a_failure_line_carries_the_last_10_lines_and_4_kib_of_them_at_most() {
        let lines: Vec<String> = (1..=12).map(|number| format!("line {number}")).collect();
        assert_eq!(
            failure_text(&lines),
            "line 3 | line 4 | line 5 | line 6 | line 7 | line 8 | line 9 | line 10 | line 11 | line 12"
        );
        let wide = ["\u{e9}".repeat(3000)];
        let text = failure_text(&wide);
        assert_eq!(text.len(), FAILURE_BYTES, "the last 4 KiB, which start at a character's boundary here");
        let cut = [format!("{}x", "\u{e9}".repeat(3000))];
        let text = failure_text(&cut);
        assert_eq!(text.len(), FAILURE_BYTES - 1, "a character the cut would split is left out");
        assert!(text.starts_with('\u{e9}') && text.ends_with('x'), "{text}");
        assert_eq!(failure_text::<&str>(&[]), "");
    }

    #[test]
    fn a_failed_git_s_standard_error_comes_last_so_its_fatal_line_is_kept() {
        let call = Call::plain(Path::new(r"C:\work\app"), &["fetch"], GIT_LIMIT);
        let output = (1..=12).fold(String::new(), |text, number| text + "out " + &number.to_string() + "\n");
        assert_eq!(
            failed_line(&call, Some(128), b"fatal: the remote end hung up\n", output.as_bytes()),
            r"git fetch failed in C:\work\app (status 128): out 4 | out 5 | out 6 | out 7 | out 8 | out 9 | out 10 | out 11 | out 12 | fatal: the remote end hung up"
        );
    }

    #[test]
    fn a_failed_git_line_escapes_what_git_wrote() {
        let call = Call::plain(Path::new(r"C:\work\app"), &["write-tree"], GIT_LIMIT);
        assert_eq!(
            failed_line(&call, Some(128), b"fatal: \x1b[31mred\x1b]8;;http://x\x07link\n", b""),
            r"git write-tree failed in C:\work\app (status 128): fatal: \u{1b}[31mred\u{1b}]8;;http://x\u{7}link"
        );
    }

    #[test]
    fn git_is_given_none_of_the_config_variables() {
        let git = Git {
            program: PathBuf::from("git.exe"),
        };
        let command = git.command(Path::new(r"C:\work\app"));
        let removed: Vec<&OsStr> = command.get_envs().filter(|(_, value)| value.is_none()).map(|(name, _)| name).collect();
        for name in [
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_PARAMETERS",
            "GIT_DIR",
            "GIT_GRAFT_FILE",
            "GIT_REPLACE_REF_BASE",
            "GIT_NO_REPLACE_OBJECTS",
            "GIT_SHALLOW_FILE",
            "GIT_QUARANTINE_PATH",
            "GIT_SSH_COMMAND",
            "GIT_SSH",
            "GIT_PROXY_COMMAND",
        ] {
            assert!(removed.contains(&OsStr::new(name)), "{name} is removed: {removed:?}");
        }
        for name in ["GIT_CONFIG_KEY_0", "GIT_CONFIG_VALUE_0", "GIT_CONFIG_KEY_12", "git_config_value_3"] {
            assert!(is_indexed_config(name), "{name}");
        }
        for name in ["GIT_CONFIG_KEYS", "GIT_CONFIG_NOSYSTEM", "GIT_CONFIG_KEY", "XGIT_CONFIG_KEY_0", "PATH"] {
            assert!(!is_indexed_config(name), "{name}");
        }
        // A call's own variables are given to git after the removal, as `Git::spawned` does it: the gate's own ssh
        // command reaches git although every call strips the one it inherited.
        let call = Call {
            set: &[("GIT_SSH_COMMAND", OsStr::new("ssh -o BatchMode=yes"))],
            ..Call::plain(Path::new(r"C:\work\app"), &["ls-remote"], GIT_LIMIT)
        };
        let mut command = git.command(call.folder);
        for name in call.unset {
            command.env_remove(*name);
        }
        command.envs(call.set.iter().copied()).args(call.args);
        let ssh: Vec<Option<&OsStr>> = command
            .get_envs()
            .filter(|(name, _)| *name == OsStr::new("GIT_SSH_COMMAND"))
            .map(|(_, value)| value)
            .collect();

        assert_eq!(ssh, [Some(OsStr::new("ssh -o BatchMode=yes"))], "a call's own ssh command reaches git");
    }

    #[test]
    fn git_and_the_ssh_it_starts_find_no_program_in_the_working_folder() {
        let git = Git {
            program: PathBuf::from("git.exe"),
        };

        let command = git.command(Path::new(r"C:\work\app"));

        let set: Vec<Option<&OsStr>> = command
            .get_envs()
            .filter(|(name, _)| *name == OsStr::new(NO_CURRENT_FOLDER))
            .map(|(_, value)| value)
            .collect();
        assert_eq!(set, [Some(OsStr::new("1"))], "git's children inherit it");
    }

    #[test]
    fn a_late_git_names_its_limit_in_seconds_or_milliseconds() {
        assert_eq!(late_git_line(GIT_LIMIT), "git did not answer within 60 s", "the default limit");
        assert_eq!(late_git_line(Duration::from_secs(60)), "git did not answer within 60 s");
        assert_eq!(late_git_line(Duration::from_secs(10)), "git did not answer within 10 s");
        assert_eq!(late_git_line(Duration::from_millis(1)), "git did not answer within 1 ms");
    }

    #[test]
    fn a_placement_call_keeps_the_ordinary_drain_and_its_interrupt() {
        let interrupt = Interrupt::install().expect("the console handler can be installed");
        let call = Call::placing(Path::new(r"C:\work\app"), &["rev-parse"], GIT_LIMIT, DRAIN, Some(interrupt));
        assert_eq!(
            call.drain, DRAIN,
            "a call its interrupt ends gives its readers the ordinary drain, not the limit"
        );
        assert_eq!(call.drain_once_exited(), GIT_LIMIT, "git's own exit gives its readers the whole limit");
    }

    #[test]
    fn a_program_call_keeps_the_drain_it_was_given_once_the_program_exits_by_itself() {
        let call = ProgramCall {
            program: "tool",
            args: &["version"],
            limit: GIT_LIMIT,
            drain: Duration::from_secs(600),
            interrupt: None,
            worktrees: &[],
            console: PipedConsole::Hidden,
            stdin: None,
        };
        let bounds = program_call(Path::new(r"C:\work\app"), &call);
        assert_eq!(bounds.drain, Duration::from_secs(600));
        assert_eq!(
            bounds.drain_once_exited(),
            Duration::from_secs(600),
            "a program call's readers keep the drain it was given once the program exits by itself"
        );
    }

    #[test]
    fn an_arrived_result_wins_over_a_raised_interrupt() {
        let interrupt = Interrupt::install().expect("the console handler can be installed");
        interrupt.raise();
        let (sent, read) = mpsc::channel();
        drop(sent.send(Ok("read")));
        let got = received_or_interrupted(&read, Instant::now() + Duration::from_secs(30), Some(interrupt));
        assert!(
            matches!(got, Some(Ok("read"))),
            "a result already in the channel wins over a raised interrupt, as it wins over the wait's end"
        );
    }

    #[test]
    fn a_raised_interrupt_ends_the_wait_after_a_clean_exit_at_once() {
        let interrupt = Interrupt::install().expect("the console handler can be installed");
        let (sent, read) = mpsc::channel::<io::Result<&'static str>>();
        let reader = thread::spawn(move || {
            interrupt.raise();
            // The sender stays alive, so the channel is neither closed nor answered: only the interrupt ends the wait.
            sent
        });
        let got = received_or_interrupted(&read, Instant::now() + Duration::from_secs(30), Some(interrupt));
        let _sent = reader.join().expect("the reader's thread ends");
        assert!(got.is_none(), "a raised interrupt ends the drain when no result arrives");
    }

    #[test]
    fn a_failed_git_line_names_the_call_its_folder_its_status_and_git_s_lines() {
        let call = Call::plain(Path::new(r"C:\work\app"), &["write-tree"], GIT_LIMIT);
        assert_eq!(
            failed_line(&call, Some(128), b"warning: first\r\nfatal: last\n", b"then out\n"),
            r"git write-tree failed in C:\work\app (status 128): then out | warning: first | fatal: last"
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

    /// What Windows PowerShell says its command line is when it is run as `tool`, found along a `PATH` holding only a
    /// link named `link` that leads to it, in a folder of its own named after `tag`.
    fn command_line_through_link(link: &str, tag: &str) -> String {
        let folder = crate::test_support::temp_dir().join(format!("gate-link-{tag}-{}", std::process::id()));
        drop(fs::remove_dir_all(&folder));
        fs::create_dir_all(&folder).expect("the link's folder can be made");
        let _removed = Removed(folder.clone());
        let system_root = std::env::var_os("SystemRoot").expect("SystemRoot is set");
        let powershell = Path::new(&system_root).join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        std::os::windows::fs::symlink_file(&powershell, folder.join(link)).expect("this machine lets a test make a symbolic link");
        let place = ProgramPlace::new(Variables::from_process().with("PATH", &folder.to_string_lossy())).expect("the system folder can be read");
        let call = ProgramCall {
            program: "tool",
            args: &["-NoProfile", "-NonInteractive", "-Command", "[Environment]::CommandLine"],
            limit: HANG_GUARD,
            drain: DRAIN,
            interrupt: None,
            worktrees: &[Path::new(env!("CARGO_MANIFEST_DIR"))],
            console: PipedConsole::Hidden,
            stdin: None,
        };

        let output = program_output(&call, &place).expect("the link runs").expect("the link is found");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Whether the first word of the command line `line`, quoted or not, names a file called `name`.
    fn first_word_names(line: &str, name: &str) -> bool {
        let line_start = line.trim_start();
        let first_word = match line_start.strip_prefix('"') {
            Some(quoted) => quoted.split('"').next(),
            None => line_start.split_whitespace().next(),
        }
        .unwrap_or_default();
        Path::new(first_word).file_name().is_some_and(|file| file.eq_ignore_ascii_case(name))
    }

    #[test]
    fn a_program_found_through_a_link_is_started_by_the_name_it_was_found_by() {
        let line = command_line_through_link("tool.exe", "name");

        assert!(
            first_word_names(&line, "tool.exe"),
            "a multi-call program such as rustup's proxies must see the name it was found by: {line}"
        );
    }

    #[test]
    fn a_batch_named_link_to_a_program_is_started_as_the_program_it_leads_to() {
        let line = command_line_through_link("tool.cmd", "batch-name");

        assert!(
            line.contains("[Environment]::CommandLine"),
            "the link's target, not cmd.exe, must run and print its command line: {line}"
        );
        assert!(
            first_word_names(&line, "tool.cmd"),
            "the target must see the name it was found by: {line}"
        );
    }
}
