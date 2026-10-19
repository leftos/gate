//! The harness: a [`Case`] with its own state root, the programs under test, and order-only assertions.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::os::windows::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use std::{fs, process, thread};

use gate_win32::StoppableProcess;
use jiff::Timestamp;
use serde_json::{Value, json};
use windows_sys::Win32::Foundation::{ERROR_OPERATION_ABORTED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::IO::CancelIoEx;
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};

/// The gate under test.
pub const GATE: &str = env!("CARGO_BIN_EXE_gate");
/// The helper program the suite's commands run.
pub const FAKE_TOOL: &str = env!("CARGO_BIN_EXE_fake-tool");
/// The stand-in desktop helper a box case puts where the box looks for the installed one; it runs the real helper's
/// code and breaks the step `GATE_DESKTOP_TEST_FAULT` names.
pub const FAKE_DESKTOP_HELPER: &str = env!("CARGO_BIN_EXE_fake-desktop-helper");

/// The published usage text, copied here so a change to it fails a case.
pub const USAGE: &str = "\
usage: gate run --slot heavy|light|critical|none [--alone] [--reason \"<why>\"] --timeout <seconds>|auto:<seconds> --log <path> [options] -- <command> [args...]
       gate stop-tree <pid>
       gate cutover
       gate push
       gate check [--builder <host>]
       gate warm [--cd <folder>] [--background] [--state-dir <path>]
       gate burn [--cores <n>] --seconds <s>
       gate expect [--state-dir <path>] [--slot heavy|light|critical] -- <command> [args...]
       gate --version | --help

run options:
  --stall <seconds>    stall window, default 120
  --tail <lines>       log lines printed at the end, default 20
  --no-markers         the command's status is the verdict
  --local              never route this run to a builder
  --reason \"<why>\"     why this run is local, critical or alone; required with --local, --slot critical and --alone
  --alone              with --slot critical: hold the machine alone, every light slot included
  --state-dir <path>   use another state root (also GATE_STATE_DIR)
  --cd <folder>        run the command in <folder> and route from it
  --bring-tracked <glob>
                       install a routed run's outputs git tracks that match <glob>; repeatable
  --env NAME[=value]   set NAME for the command, local or routed; without =value, forward NAME's value from here, the form for secrets; repeatable

--slot, --timeout, --log and the command are all required; so is --reason \"<why>\" with --local, --slot critical or --alone.
The PowerShell form (gate -Log <path> -TimeoutSeconds <n> -Slot <kind> [...] -- <command>) is also accepted.
";

/// The git variables a caller's environment, such as a git hook around the suite, may hold that point git at another
/// repository, index, object store or config; no case's gate or git gets them, and a gate with `GIT_INDEX_FILE` set
/// routes only when `remote.json`'s `hooks` names its repo.
pub const GIT_REDIRECTS: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_CONFIG_PARAMETERS",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
];
/// The variables a real gate or box run around the suite may have set, which no case's gate may inherit.
const INHERITED: [&str; 4] = ["GATE_STATE_DIR", "GATE_SLOT_HELD", "GATE_BOX_JOB", "GATE_BOX_EXE"];
/// Only guards against a stuck case; no case asserts how long anything took. Ten minutes, since a loaded machine
/// (several heavy gates building at once) has kept a healthy case past a minute, and a real hang still ends.
const HANG_GUARD: Duration = Duration::from_secs(600);
/// How long the output readers get to end once the program has exited; a reader whose pipe a grandchild still holds
/// would otherwise keep its case waiting the whole hang guard.
const READER_GRACE: Duration = Duration::from_secs(60);
/// How often a wait looks again.
const LOOK_AGAIN: Duration = Duration::from_millis(10);
/// How long the `gate stop-tree` a drop runs is given to end before the drop kills that one too: a hang guard, never a
/// bound a case asserts on, so a stop-tree that cannot end never holds a case for the whole run.
const STOP_TREE_GUARD: Duration = Duration::from_secs(60);
/// The status `gate stop-tree` ends with when no live process has the pid: by the time it looked, the program had gone,
/// which a drop takes as nothing left to stop rather than a failure to report.
const STOP_TREE_GONE: i32 = 2;
/// The fallback poll of every gate a case starts, so a wake missed costs a case 20 ms, not 2 s.
pub const POLL: [&str; 2] = ["--test-poll-ms", "20"];
/// .NET ticks (100 ns since 0001-01-01) at the Unix epoch.
const UNIX_EPOCH_TICKS: i128 = 621_355_968_000_000_000;

static NEXT_CASE: AtomicU64 = AtomicU64::new(0);

/// One case: its own folder under the target directory, removed when the case ends. The folder is the gate's working
/// directory, where its logs go; the state root is the `state` folder inside it, so no log lands in the state root.
pub struct Case {
    folder: PathBuf,
    state: PathBuf,
    number: u64,
}

impl Case {
    pub fn new(name: &str) -> Case {
        let number = NEXT_CASE.fetch_add(1, Ordering::Relaxed);
        let folder = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}-{number}", process::id()));
        if folder.exists() {
            fs::remove_dir_all(&folder).expect("a case folder left by an earlier run can be removed");
        }
        let state = folder.join("state");
        fs::create_dir_all(&state).expect("the case folder and its state root can be made");
        Case { folder, state, number }
    }

    /// The case's state root, absolute as the gate reports it.
    pub fn root(&self) -> PathBuf {
        std::path::absolute(&self.state).expect("the state root has an absolute path")
    }

    /// The case's folder, absolute: the gate's working directory, holding the state root and the logs.
    pub fn folder(&self) -> PathBuf {
        std::path::absolute(&self.folder).expect("the case folder has an absolute path")
    }

    /// Readies `command` for this case: the inherited gate variables removed, git pointed at the suite's git home
    /// (`HOME` and `XDG_CONFIG_HOME`, [`git_home_for`]), LOCALAPPDATA under the case folder
    /// ([`Case::local_app_data`]), the working directory the case folder,
    /// and `args` given with `--state-dir <root>` after a `run`, `stop-tree` or `expect` subcommand.
    pub fn prepare(&self, command: &mut Command, args: &[&str]) {
        self.prepare_without_state_dir(command, &[]);
        let Some((first, rest)) = args.split_first() else {
            return;
        };
        command.arg(first);
        if matches!(*first, "run" | "stop-tree" | "expect") {
            command.arg("--state-dir").arg(&self.state);
        }
        command.args(rest);
    }

    /// As [`Case::prepare`], with `args` given as they are.
    pub fn prepare_without_state_dir(&self, command: &mut Command, args: &[&str]) {
        for name in INHERITED.iter().chain(&GIT_REDIRECTS) {
            command.env_remove(name);
        }
        git_home_for(command);
        command.env("LOCALAPPDATA", self.local_app_data()).current_dir(&self.folder).args(args);
    }

    /// The LOCALAPPDATA every case's gate sees, under the case folder, so no case reads the host's default root. The
    /// folder is not created.
    pub fn local_app_data(&self) -> PathBuf {
        self.folder().join("appdata")
    }

    /// The gate with `args`, `--state-dir` added where the form takes it.
    pub fn gate(&self, args: &[&str]) -> Command {
        let mut command = Command::new(GATE);
        self.prepare(&mut command, args);
        command
    }

    /// The gate with `args` as they are.
    pub fn gate_without_state_dir(&self, args: &[&str]) -> Command {
        let mut command = Command::new(GATE);
        self.prepare_without_state_dir(&mut command, args);
        command
    }

    /// The gate with `args` as [`Case::gate`] readies it, its working directory the case folder's `sub` folder.
    pub fn gate_in(&self, sub: &str, args: &[&str]) -> Command {
        let mut command = self.gate(args);
        command.current_dir(self.folder.join(sub));
        command
    }

    pub fn run(&self, args: &[&str]) -> Outcome {
        finish(self.gate(args))
    }

    /// Starts the gate with `args` as [`Case::gate`] readies it.
    pub fn start(&self, args: &[&str]) -> Running {
        start(self.gate(args))
    }

    /// The names of the entries the gate left in the state root.
    pub fn root_entries(&self) -> Vec<String> {
        fs::read_dir(&self.state)
            .expect("the state root can be listed")
            .map(|entry| entry.expect("a root entry can be read").file_name().to_string_lossy().into_owned())
            .collect()
    }

    /// The names of the entries in the case folder, less the state root the harness made there.
    pub fn folder_entries(&self) -> Vec<String> {
        fs::read_dir(&self.folder)
            .expect("the case folder can be listed")
            .map(|entry| entry.expect("a folder entry can be read").file_name().to_string_lossy().into_owned())
            .filter(|name| name != "state")
            .collect()
    }
}

impl Drop for Case {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.folder) {
            // A folder left under the target directory harms no later case, and a panic in drop would abort the run.
            let _ = writeln!(io::stderr(), "could not remove the case folder {}: {error}", self.folder.display());
        }
    }
}

/// What a program did: its status code (`None` when killed by the hang guard) and its output, read as UTF-8 with
/// U+FFFD for what is not, and stdout's own bytes.
#[derive(Debug)]
pub struct Outcome {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub raw_stdout: Vec<u8>,
}

impl Outcome {
    /// The one JSON line `--test-print-request` printed.
    pub fn request(&self) -> Value {
        assert_eq!(self.code, Some(0), "the request is printed with status 0: {self:?}");
        assert_eq!(self.stderr, "", "the request is printed with nothing on stderr");
        serde_json::from_str(self.stdout.trim_end()).expect("stdout is one JSON line")
    }
}

/// Runs `command` to its end, or kills it after the hang guard.
pub fn finish(command: Command) -> Outcome {
    start(command).finish()
}

/// What the gate at `exe` prints after `gate ` for `--version`: the package version and the build's own id.
pub fn version_of(exe: &Path) -> String {
    let mut command = Command::new(exe);
    command.arg("--version");
    after_gate(&finish(command))
}

/// What `case`'s gate prints after `gate ` for `--version`, run as [`Case::run`] runs it: read from the gate itself, so a
/// case need not know the id the build was made with.
pub fn build_version(case: &Case) -> String {
    after_gate(&case.run(&["--version"]))
}

/// The version a `--version` that exited 0 printed after `gate `.
fn after_gate(outcome: &Outcome) -> String {
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    outcome
        .stdout
        .trim_end()
        .strip_prefix("gate ")
        .expect("a gate prints its version after `gate `")
        .to_owned()
}

/// Starts `command` with stdin null and its output gathered as it arrives.
pub fn start(mut command: Command) -> Running {
    command.stdin(Stdio::null());
    gathered(command)
}

/// Starts `command` with its output gathered as [`start`] gathers it and its stdin a pipe, whose write end it returns:
/// the program reads what the case writes there, and the end of its input once the case drops it.
pub fn start_with_stdin(mut command: Command) -> (Running, ChildStdin) {
    command.stdin(Stdio::piped());
    let mut running = gathered(command);
    let stdin = running.child.stdin.take().expect("stdin is piped");
    (running, stdin)
}

/// Starts `command`, its stdin as the caller set it, both output streams piped and gathered as they arrive.
fn gathered(mut command: Command) -> Running {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let stop_tree = stop_tree_of(&command);
    let mut child = command.spawn().expect("the program starts");
    let (stdout, stdout_reader) = gather(child.stdout.take().expect("stdout is piped"));
    let (stderr, stderr_reader) = gather(child.stderr.take().expect("stderr is piped"));
    Running {
        child,
        stdout,
        stderr,
        readers: vec![stdout_reader, stderr_reader],
        stop_tree,
    }
}

/// Which of a program's output streams a caller stops reading at once.
#[derive(Clone, Copy)]
pub enum Closed {
    Stdout,
    Stderr,
}

/// Starts `command` with its stdin a pipe, whose write end it returns as [`start_with_stdin`] does, and both output
/// streams piped, dropping this process's read end of `closed` at once, so the program's next write to that stream
/// fails; the other stream is gathered as [`start`] gathers it.
pub fn start_with_closed(closed: Closed, mut command: Command) -> (Running, ChildStdin) {
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let stop_tree = stop_tree_of(&command);
    let mut child = command.spawn().expect("the program starts");
    let stdin = child.stdin.take().expect("stdin is piped");
    let out_pipe = child.stdout.take().expect("stdout is piped");
    let err_pipe = child.stderr.take().expect("stderr is piped");
    let (stdout, stderr, reader) = match closed {
        Closed::Stdout => {
            drop(out_pipe);
            let (stderr, reader) = gather(err_pipe);
            (Buffer::default(), stderr, reader)
        }
        Closed::Stderr => {
            drop(err_pipe);
            let (stdout, reader) = gather(out_pipe);
            (stdout, Buffer::default(), reader)
        }
    };
    let running = Running {
        child,
        stdout,
        stderr,
        readers: vec![reader],
        stop_tree,
    };
    (running, stdin)
}

/// How often [`StderrCloser::close`] asks its reader's read to give up until the reader has ended.
const CANCEL_STEP: Duration = Duration::from_millis(10);

/// Starts `command` with its stdin a pipe, whose write end it returns as [`start_with_stdin`] does, and both output
/// streams piped and gathered as [`start`] gathers them, stderr by a reader the case closes on demand
/// ([`StderrCloser::close`]): from then on the program's writes to stderr fail as a gone client's would make them.
pub fn start_with_closable_stderr(mut command: Command) -> (Running, ChildStdin, StderrCloser) {
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let stop_tree = stop_tree_of(&command);
    let mut child = command.spawn().expect("the program starts");
    let stdin = child.stdin.take().expect("stdin is piped");
    let (stdout, stdout_reader) = gather(child.stdout.take().expect("stdout is piped"));
    let open = Arc::new(Mutex::new(true));
    let pipe = SharedPipe {
        pipe: child.stderr.take().expect("stderr is piped"),
        open: Arc::clone(&open),
    };
    let handle = pipe.pipe.as_raw_handle();
    let stderr = Buffer::default();
    let closing = Arc::new(AtomicBool::new(false));
    let reader = {
        let stderr = Arc::clone(&stderr);
        let closing = Arc::clone(&closing);
        thread::spawn(move || read_until_closed(pipe, &stderr, &closing))
    };
    let running = Running {
        child,
        stdout,
        stderr,
        readers: vec![stdout_reader],
        stop_tree,
    };
    (
        running,
        stdin,
        StderrCloser {
            closing,
            handle,
            open,
            reader,
        },
    )
}

/// The stderr pipe a [`StderrCloser`]'s reader reads, and whether it is still open. Its drop, on the hand-back or on
/// the reader's panic alike, marks it shut under the lock [`StderrCloser::close`] holds while it gives up a read, and
/// only then closes the pipe, so no read is given up on a closed handle.
struct SharedPipe {
    pipe: ChildStderr,
    open: Arc<Mutex<bool>>,
}

impl Drop for SharedPipe {
    fn drop(&mut self) {
        *self.open.lock().unwrap_or_else(PoisonError::into_inner) = false;
    }
}

/// Reads `pipe` into `buffer` until it ends or `closing` is set, then hands it back for the caller to close. A read
/// given up for [`StderrCloser::close`] is not an end: the reader looks at `closing` again.
#[expect(clippy::panic, reason = "a stderr that cannot be read fails the case, as `gather`'s does")]
fn read_until_closed(mut pipe: SharedPipe, buffer: &Buffer, closing: &AtomicBool) -> SharedPipe {
    let aborted = i32::try_from(ERROR_OPERATION_ABORTED).ok();
    let mut chunk = [0; 4096];
    while !closing.load(Ordering::SeqCst) {
        match pipe.pipe.read(&mut chunk) {
            Ok(read) => match chunk.get(..read).filter(|bytes| !bytes.is_empty()) {
                Some(bytes) => buffer.lock().expect("no reader panicked holding the buffer").extend_from_slice(bytes),
                None => break,
            },
            Err(error) if error.raw_os_error() == aborted => {}
            Err(error) => panic!("the program's stderr cannot be read: {error}"),
        }
    }
    pipe
}

/// The reader of a program's stderr that [`start_with_closable_stderr`] started, which the case closes on demand.
pub struct StderrCloser {
    closing: Arc<AtomicBool>,
    /// The raw handle of the pipe the reader reads, open while `open` says so.
    handle: RawHandle,
    /// Whether `handle` is still open; [`SharedPipe`]'s drop shuts it.
    open: Arc<Mutex<bool>>,
    reader: JoinHandle<SharedPipe>,
}

impl StderrCloser {
    /// Stops the reader, giving up the read it waits in (`CancelIoEx` on the pipe, which std reads with overlapped
    /// I/O) until it has ended, then closes this process's read end of the program's stderr; fails after the hang
    /// guard.
    pub fn close(self) {
        self.closing.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + HANG_GUARD;
        while !self.reader.is_finished() {
            assert!(Instant::now() < deadline, "the stderr reader did not end within the hang guard");
            let open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
            if *open {
                // SAFETY: the pipe is open while `open` says so, and stays open while this lock is held, since the
                // pipe's drop takes it before the handle closes; with no overlapped structure named every read pending
                // on it is given up, and none pending leaves it as it is for the next turn to ask again.
                unsafe { CancelIoEx(self.handle, std::ptr::null()) };
            }
            drop(open);
            thread::sleep(CANCEL_STEP);
        }
        let pipe = self.reader.join();
        assert!(pipe.is_ok(), "the stderr reader panicked");
        drop(pipe);
    }
}

/// A stream's bytes so far, shared with the thread that reads it.
type Buffer = Arc<Mutex<Vec<u8>>>;

/// A program [`start`] started, and its output so far.
pub struct Running {
    child: Child,
    stdout: Buffer,
    stderr: Buffer,
    readers: Vec<JoinHandle<()>>,
    /// The `gate stop-tree` the drop runs for this program while it still runs, so the command the program left behind
    /// goes with it; its words and environment are ready but for the pid it is given then.
    stop_tree: Command,
}

impl Running {
    /// Returns once stderr contains `needle`; fails as [`Running::wait_for`] does.
    pub fn wait_for_stderr(&self, needle: &str) {
        self.wait_for_text(Stream::Stderr, needle);
    }

    /// Returns stderr so far once `check` holds for it; fails as [`Running::wait_for`] does, naming `what`.
    pub fn wait_for_stderr_where(&self, what: &str, check: impl Fn(&str) -> bool) -> String {
        self.wait_for(&Wanted {
            stream: Stream::Stderr,
            what,
            check: &check,
        })
    }

    /// Returns stderr so far once `check` holds for it and the program is seen running after that stderr was read, so
    /// it had not exited when it said what `check` looks for. Fails at once, saying `what` did not happen, when the
    /// program has exited, and after the hang guard.
    pub fn wait_for_stderr_while_running(&mut self, what: &str, check: impl Fn(&str) -> bool) -> String {
        let Running { child, stderr, .. } = self;
        wait_for_stderr_check(stderr, what, check, || {
            child.try_wait().expect("the program's status can be read").is_none()
        })
    }

    /// Returns stdout so far once it contains `needle`; fails as [`Running::wait_for`] does.
    pub fn wait_for_stdout(&self, needle: &str) -> String {
        self.wait_for_text(Stream::Stdout, needle)
    }

    /// Returns the text of `stream` so far once it contains `needle`; fails as [`Running::wait_for`] does.
    fn wait_for_text(&self, stream: Stream, needle: &str) -> String {
        let what = format!("{needle:?}");
        let check = |text: &str| text.contains(needle);
        self.wait_for(&Wanted {
            stream,
            what: &what,
            check: &check,
        })
    }

    /// Returns the text of the wanted stream so far once its check holds for it. Fails at once, with the exit status
    /// and the output so far, when the program has exited without saying it, after the readers have had
    /// [`READER_GRACE`] to drain what it wrote before it exited; fails after the hang guard while it still runs.
    #[expect(
        clippy::panic,
        reason = "a program that exited without saying what its case waits for fails the case with its status and output"
    )]
    fn wait_for(&self, wanted: &Wanted<'_>) -> String {
        let deadline = Instant::now() + HANG_GUARD;
        loop {
            let ended = self.exit_status();
            let mut said = self.said();
            let mut found = verdict(&said, wanted, ended);
            if matches!(found, Verdict::Failed(_)) {
                self.wait_for_readers();
                said = self.said();
                found = verdict(&said, wanted, ended);
            }
            match found {
                Verdict::Found => return said.of(wanted.stream).to_owned(),
                Verdict::Failed(message) => panic!("{message}"),
                Verdict::Waiting => {}
            }
            let stream = wanted.stream.name();
            assert!(
                Instant::now() < deadline,
                "{} did not appear on {stream}; {stream} so far:\n{}",
                wanted.what,
                said.of(wanted.stream)
            );
            thread::sleep(LOOK_AGAIN);
        }
    }

    /// Returns once `check` holds. Each look reads the program's exit status before it runs `check`, so a check that
    /// still fails after the program was seen exited never will: the wait then fails at once, naming `what`, with the
    /// exit status and the output so far, after the readers have had [`READER_GRACE`] to drain what it wrote. Fails
    /// after the hang guard, with stderr so far, while the program still runs.
    #[expect(
        clippy::panic,
        reason = "a program that exited before its case's check held fails the case with its status and output"
    )]
    pub fn wait_until(&self, what: &str, mut check: impl FnMut() -> bool) {
        let deadline = Instant::now() + HANG_GUARD;
        loop {
            let ended = self.exit_status();
            if check() {
                return;
            }
            if let Some(status) = ended {
                self.wait_for_readers();
                let said = self.said();
                panic!(
                    "{what} did not happen before the program exited with {status}; stdout so far:\n{}\nstderr so far:\n{}",
                    said.stdout, said.stderr
                );
            }
            assert!(
                Instant::now() < deadline,
                "{what} did not happen within the hang guard; stderr so far:\n{}",
                text(&self.stderr)
            );
            thread::sleep(LOOK_AGAIN);
        }
    }

    /// The program's exit status once it has exited, read through its handle so a wait that only borrows it can ask.
    fn exit_status(&self) -> Option<ExitStatus> {
        let process = self.child.as_raw_handle();
        // SAFETY: `process` is the program's handle, which `self.child` keeps open while it lives; a zero timeout
        // only asks whether it is signalled.
        let waited = unsafe { WaitForSingleObject(process, 0) };
        if waited == WAIT_TIMEOUT {
            return None;
        }
        assert_eq!(
            waited,
            WAIT_OBJECT_0,
            "the program's handle can be waited on: {}",
            io::Error::last_os_error()
        );
        let mut code = 0;
        // SAFETY: as above, and `code` is a valid place for the exit code.
        let read = unsafe { GetExitCodeProcess(process, &raw mut code) };
        assert_ne!(read, 0, "the program's exit code can be read: {}", io::Error::last_os_error());
        Some(ExitStatus::from_raw(code))
    }

    /// Both streams as they stand.
    fn said(&self) -> Said {
        Said {
            stdout: text(&self.stdout),
            stderr: text(&self.stderr),
        }
    }

    /// The program's pid.
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Kills the program, waits for it, and returns what it said before it died.
    pub fn kill(mut self) -> Outcome {
        self.child.kill().expect("the program can be killed");
        let status = self.child.wait().expect("a killed program can be waited for");
        self.let_readers_end();
        Outcome {
            code: status.code(),
            stdout: text(&self.stdout),
            stderr: text(&self.stderr),
            raw_stdout: bytes(&self.stdout),
        }
    }

    /// Whether the program has not exited yet.
    pub fn is_running(&mut self) -> bool {
        self.child.try_wait().expect("the program's status can be read").is_none()
    }

    /// Joins the readers that end within [`READER_GRACE`] of the program's exit. A reader whose pipe a grandchild still
    /// holds is left running; its buffer is taken as it stands.
    fn let_readers_end(&mut self) {
        self.wait_for_readers();
        for reader in self.readers.drain(..) {
            if reader.is_finished() {
                reader.join().expect("an output reader ends");
            }
        }
    }

    /// Waits up to [`READER_GRACE`] for the output readers to end, as they do once the program and everything holding
    /// its pipes have exited.
    fn wait_for_readers(&self) {
        let deadline = Instant::now() + READER_GRACE;
        while Instant::now() < deadline && !self.readers.iter().all(JoinHandle::is_finished) {
            thread::sleep(LOOK_AGAIN);
        }
    }

    /// Waits for the program to end, or kills it after the hang guard.
    pub fn finish(mut self) -> Outcome {
        let deadline = Instant::now() + HANG_GUARD;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("the program's status can be read") {
                break Some(status);
            }
            if Instant::now() >= deadline {
                self.child.kill().expect("a stuck program can be killed");
                self.child.wait().expect("a killed program can be waited for");
                break None;
            }
            thread::sleep(LOOK_AGAIN);
        };
        self.let_readers_end();
        Outcome {
            code: status.and_then(|status| status.code()),
            stdout: text(&self.stdout),
            stderr: text(&self.stderr),
            raw_stdout: bytes(&self.stdout),
        }
    }
}

impl Drop for Running {
    /// Kills a program its case left running, as when the case failed before finishing it, and the tree under it: a
    /// gate under test leaves its command running by design, so killing the gate's pid alone would leave that command
    /// holding the files of the build. A test declares its `Running` after its [`Case`], so the locals' reverse drop
    /// order ends the program before the case root goes. A drop never panics: a failure is reported on stderr and the
    /// pid is killed as before.
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(None)) {
            return;
        }
        stop_tree(&mut self.stop_tree, self.child.id());
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        if let Err(error) = self.child.kill().and_then(|()| self.child.wait().map(drop)) {
            // A panic in drop would abort the run; the program is left for the machine to end.
            let _ = writeln!(io::stderr(), "could not kill a case's program {}: {error}", self.child.id());
        }
    }
}

/// The `gate stop-tree` command for a program started with `command`: this suite's gate, the state root `command`'s
/// words name, and the environment it was given, so the stop reads the same root the program's own gate does. Only the
/// pid, the one word left, is added when the command runs.
fn stop_tree_of(command: &Command) -> Command {
    let mut stop = Command::new(GATE);
    stop.arg("stop-tree");
    if let Some(root) = state_dir_of(command) {
        stop.arg("--state-dir").arg(root);
    }
    for (name, value) in command.get_envs() {
        match value {
            Some(value) => {
                stop.env(name, value);
            }
            None => {
                stop.env_remove(name);
            }
        }
    }
    stop.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    stop
}

/// The root `command`'s words name after `--state-dir`, as [`Case::prepare`] places it, or none when they name none.
fn state_dir_of(command: &Command) -> Option<OsString> {
    let words: Vec<&OsStr> = command.get_args().collect();
    let at = words.iter().position(|word| *word == OsStr::new("--state-dir"))?;
    words.get(at + 1).copied().map(OsStr::to_os_string)
}

/// Ends the tree of the program `pid`, which still runs, by running the `stop-tree` command prepared for that program,
/// so the gate's command and everything under it go with the gate, and waits for that stop up to [`STOP_TREE_GUARD`],
/// killing it past that. Every failure is reported on stderr and none panics, so the drop goes on to kill the pid.
fn stop_tree(command: &mut Command, pid: u32) {
    let mut stop = match command.arg(pid.to_string()).spawn() {
        Ok(stop) => stop,
        Err(error) => {
            let _ = writeln!(io::stderr(), "could not run gate stop-tree for {pid}: {error}");
            return;
        }
    };
    let deadline = Instant::now() + STOP_TREE_GUARD;
    let status = loop {
        match stop.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(LOOK_AGAIN),
            Ok(None) => {
                let _ = stop.kill();
                let _ = stop.wait();
                break None;
            }
            Err(error) => {
                let _ = writeln!(io::stderr(), "could not wait for the gate stop-tree of {pid}: {error}");
                return;
            }
        }
    };
    let mut said = String::new();
    if let Some(mut stderr) = stop.stderr.take() {
        let _ = stderr.read_to_string(&mut said);
    }
    match status {
        Some(status) if status.success() || status.code() == Some(STOP_TREE_GONE) => {}
        Some(status) => {
            let _ = writeln!(io::stderr(), "gate stop-tree of {pid} ended with {status}: {}", said.trim());
        }
        None => {
            let _ = writeln!(io::stderr(), "gate stop-tree of {pid} did not end within {} s", STOP_TREE_GUARD.as_secs());
        }
    }
}

/// Reads `stream` on its own thread into the buffer returned.
fn gather(mut stream: impl Read + Send + 'static) -> (Buffer, JoinHandle<()>) {
    let buffer = Buffer::default();
    let shared = Arc::clone(&buffer);
    let reader = thread::spawn(move || {
        let mut chunk = [0; 4096];
        loop {
            let read = stream.read(&mut chunk).expect("the program's output can be read");
            if read == 0 {
                return;
            }
            shared
                .lock()
                .expect("no reader panicked holding the buffer")
                .extend_from_slice(&chunk[..read]);
        }
    });
    (buffer, reader)
}

/// Returns the text of `stderr`, a program's, once `check` holds for it and `running` then says the program has not
/// exited; fails saying `what` did not happen, with the text so far, at once when `running` says it has exited, and
/// after the hang guard.
fn wait_for_stderr_check(stderr: &Buffer, what: &str, check: impl Fn(&str) -> bool, mut running: impl FnMut() -> bool) -> String {
    let deadline = Instant::now() + HANG_GUARD;
    loop {
        let text = text(stderr);
        let alive = running();
        if alive && check(&text) {
            return text;
        }
        assert!(alive, "{what} did not happen on stderr before the program exited; stderr so far:\n{text}");
        assert!(Instant::now() < deadline, "{what} did not happen on stderr; stderr so far:\n{text}");
        thread::sleep(LOOK_AGAIN);
    }
}

/// One of a program's two output streams.
#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

impl Stream {
    fn name(self) -> &'static str {
        match self {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
        }
    }
}

/// A program's output so far, both streams read at one look.
struct Said {
    stdout: String,
    stderr: String,
}

impl Said {
    fn of(&self, stream: Stream) -> &str {
        match stream {
            Stream::Stdout => &self.stdout,
            Stream::Stderr => &self.stderr,
        }
    }
}

/// What a wait waits for: `check` holding for the text of `stream`, which the failure messages name as `what`.
struct Wanted<'a> {
    stream: Stream,
    what: &'a str,
    check: &'a dyn Fn(&str) -> bool,
}

/// What one look at a program's output makes of a wait.
#[derive(Debug, PartialEq)]
enum Verdict {
    /// The wanted text is there.
    Found,
    /// The text is not there yet and the program still runs, so it may yet say it.
    Waiting,
    /// The program has exited without saying it: the failure message, with the exit status and the output so far.
    Failed(String),
}

/// The verdict on `wanted`, given `said` and the program's exit status when it has `ended`.
fn verdict(said: &Said, wanted: &Wanted<'_>, ended: Option<ExitStatus>) -> Verdict {
    if (wanted.check)(said.of(wanted.stream)) {
        return Verdict::Found;
    }
    let Some(status) = ended else {
        return Verdict::Waiting;
    };
    Verdict::Failed(format!(
        "{} did not appear on {} before the program exited with {status}; stdout so far:\n{}\nstderr so far:\n{}",
        wanted.what,
        wanted.stream.name(),
        said.stdout,
        said.stderr
    ))
}

/// `ready` on stdout and `started` on stderr, the output the verdict cases look at.
fn said_ready_then_started() -> Said {
    Said {
        stdout: "ready\n".to_owned(),
        stderr: "started\n".to_owned(),
    }
}

fn has_started(text: &str) -> bool {
    text.contains("started")
}

#[test]
fn a_wait_finds_text_its_stream_holds_whether_or_not_the_program_has_exited() {
    let said = said_ready_then_started();
    let wanted = Wanted {
        stream: Stream::Stderr,
        what: "\"started\"",
        check: &has_started,
    };

    assert_eq!(verdict(&said, &wanted, None), Verdict::Found);
    assert_eq!(verdict(&said, &wanted, Some(ExitStatus::from_raw(3))), Verdict::Found);
}

#[test]
fn a_wait_goes_on_while_the_program_runs_without_the_text_on_its_stream() {
    let wanted = Wanted {
        stream: Stream::Stdout,
        what: "\"started\"",
        check: &has_started,
    };

    assert_eq!(verdict(&said_ready_then_started(), &wanted, None), Verdict::Waiting);
}

#[test]
fn a_wait_fails_at_once_when_the_program_has_exited_without_the_text_naming_its_status() {
    let wanted = Wanted {
        stream: Stream::Stdout,
        what: "\"started\"",
        check: &has_started,
    };

    let message = match verdict(&said_ready_then_started(), &wanted, Some(ExitStatus::from_raw(3))) {
        Verdict::Failed(message) => message,
        other => format!("{other:?}"),
    };

    for part in [
        "\"started\" did not appear on stdout",
        "exited with exit code: 3",
        "stdout so far:\nready\n",
        "stderr so far:\nstarted\n",
    ] {
        assert!(message.contains(part), "{part:?} is in the failure: {message:?}");
    }
}

#[test]
#[should_panic(expected = "\"started\" did not appear on stdout before the program exited with exit code: 3")]
fn a_wait_for_output_fails_at_once_when_the_program_exits_without_it() {
    let mut command = Command::new(FAKE_TOOL);
    command.args(["exit", "3"]);
    let running = start(command);

    running.wait_for_stdout("started");
}

#[test]
#[should_panic(expected = "the third call did not happen before the program exited with exit code: 3")]
fn a_wait_until_fails_at_once_when_the_program_exits_before_its_check_holds() {
    let mut command = Command::new(FAKE_TOOL);
    command.args(["exit", "3"]);
    let running = start(command);

    running.wait_until("the third call", || false);
}

fn text(buffer: &Buffer) -> String {
    String::from_utf8_lossy(&bytes(buffer)).into_owned()
}

fn bytes(buffer: &Buffer) -> Vec<u8> {
    buffer.lock().expect("no reader panicked holding the buffer").clone()
}

/// Returns once a line of `root`'s journal contains `needle`, a missing journal read as no lines yet; fails with the
/// journal so far after the hang guard.
pub fn wait_for_journal_line(root: &Path, needle: &str) {
    let journal = root.join("events.jsonl");
    let deadline = Instant::now() + HANG_GUARD;
    loop {
        let lines = match fs::read_to_string(&journal) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
            read => read.expect("the journal can be read"),
        };
        if lines.lines().any(|line| line.contains(needle)) {
            return;
        }
        assert!(Instant::now() < deadline, "no line has {needle:?}; the journal so far:\n{lines}");
        thread::sleep(LOOK_AGAIN);
    }
}

/// Returns once `check` holds; fails naming `what` after the hang guard.
pub fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + HANG_GUARD;
    while !check() {
        assert!(Instant::now() < deadline, "{what} did not happen within the hang guard");
        thread::sleep(LOOK_AGAIN);
    }
}

/// Makes a junction at `link` that leads to `target`, with `mklink /J`.
pub fn junction(link: &Path, target: &Path) {
    let made = Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .expect("cmd starts");
    assert!(made.success(), "the junction {} is made", link.display());
}

/// Returns once `path` exists.
pub fn wait_for_file(path: &Path) {
    wait_until(&format!("{} appearing", path.display()), || path.exists());
}

/// Writes `json` as the counts file of `root`, through a file beside it moved over it, so a gate never reads it torn.
pub fn write_counts(root: &Path, json: &str) {
    write_whole(&root.join("slot-counts.json"), json);
}

/// Asserts that `needles` occur in `haystack` in this order, each after the end of the one before.
pub fn assert_in_order(haystack: &str, needles: &[&str]) {
    let mut from = 0;
    for needle in needles {
        let found = haystack[from..].find(needle);
        assert!(found.is_some(), "{needle:?} does not follow byte {from} of:\n{haystack}");
        from += found.expect("checked above") + needle.len();
    }
}

/// The lines of `case`'s journal, in order.
pub fn events(case: &Case) -> Vec<Value> {
    fs::read_to_string(case.root().join("events.jsonl"))
        .expect("the journal was written")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("a journal line is JSON"))
        .collect()
}

/// The journal's events of the gate `pid`, in order.
pub fn events_of(case: &Case, pid: u32) -> Vec<String> {
    events(case)
        .into_iter()
        .filter(|event| event["pid"] == pid)
        .map(|event| event["event"].as_str().expect("a line names its event").to_owned())
        .collect()
}

/// The `take` line of the gate `pid`.
pub fn take_of(case: &Case, pid: u32) -> Value {
    events(case)
        .into_iter()
        .find(|event| event["event"] == "take" && event["pid"] == pid)
        .expect("the gate journalled its take")
}

/// The `release` line of `case`'s journal.
pub fn release(case: &Case) -> Value {
    fs::read_to_string(case.root().join("events.jsonl"))
        .expect("the journal was written")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("a journal line is JSON"))
        .find(|event| event["event"] == "release")
        .expect("the run journalled its release")
}

/// The pid `log` says on its first line starting `prefix`, as fake-tool's `pid` and `spawn-wait` steps write it.
pub fn said_pid(log: &str, prefix: &str) -> u32 {
    let pid: Option<u32> = log.lines().find_map(|text| text.strip_prefix(prefix)).and_then(|pid| pid.parse().ok());
    assert!(pid.is_some(), "no {prefix:?} line with a pid in the log:\n{log}");
    pid.expect("checked above")
}

/// `path` as the gate prints it in the request.
pub fn shown(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The names of the entries in `folder`, in name order.
pub(crate) fn names(folder: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(folder)
        .expect("the folder can be listed")
        .map(|entry| entry.expect("an entry can be read").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The path of `path` with `suffix` after its name, as fake-ssh names the files it keeps beside its steps file.
pub fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut beside = path.as_os_str().to_owned();
    beside.push(suffix);
    PathBuf::from(beside)
}

/// A junction beside the case folder, outside its worktree, made with `mklink /J`; removed, and not what it leads to,
/// when it drops.
pub struct Junction(pub PathBuf);

impl Junction {
    /// A junction that leads to `target`.
    pub fn to(case: &Case, target: &Path) -> Junction {
        let mut link = case.folder().into_os_string();
        link.push("-junction");
        let link = PathBuf::from(link);
        // A junction an earlier run of the case left would make `mklink` refuse.
        drop(fs::remove_dir(&link));
        junction(&link, target);
        Junction(link)
    }
}

impl Drop for Junction {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir(&self.0) {
            // A junction left beside the case harms no later case, which removes it first, and a panic in drop aborts.
            let _ = writeln!(io::stderr(), "could not remove the junction {}: {error}", self.0.display());
        }
    }
}

/// The reason a run the suite keeps here with `--local` gives, the two words a run's options take.
pub const LOCAL_REASON: [&str; 2] = ["--reason", "the suite keeps this run here"];

/// The words of a light run logging to `log`, `extra` before `--` and `command` after it. Gates that run at once each
/// need a log of their own, since a gate refuses a log another gate is writing. The ceiling is far above the hang
/// guard, so a run a case holds is never ended by it.
pub fn run_words<'a>(log: &'a str, extra: &[&'a str], command: &[&'a str]) -> Vec<&'a str> {
    let mut words = vec!["run", "--slot", "light", "--timeout", "3600", "--log", log];
    words.extend(extra);
    words.push("--");
    words.extend(command);
    words
}

/// The words of a heavy run, as [`run_words`] gives those of a light one.
pub fn heavy_words<'a>(log: &'a str, extra: &[&'a str], command: &[&'a str]) -> Vec<&'a str> {
    let mut words = run_words(log, extra, command);
    words[2] = "heavy";
    words
}

/// The words of a critical run, as [`run_words`] gives those of a light one, with the reason a critical run must state.
pub fn critical_words<'a>(log: &'a str, extra: &[&'a str], command: &[&'a str]) -> Vec<&'a str> {
    let mut before = vec!["--reason", "the suite's critical run"];
    before.extend(extra);
    let mut words = run_words(log, &before, command);
    words[2] = "critical";
    words
}

/// The words of a run that takes no slot, as [`run_words`] gives those of a light one.
pub fn none_words<'a>(log: &'a str, extra: &[&'a str], command: &[&'a str]) -> Vec<&'a str> {
    let mut words = run_words(log, extra, command);
    words[2] = "none";
    words
}

/// The global git config every case's git reads in place of the user's own: line ends as the developer's config sets
/// them over the system config's `core.autocrlf=true`, and no auto-maintenance or gc after a commit, push or fetch.
const GIT_CONFIG: &str = "\
[core]
\tautocrlf = false
\teol = lf
\tlongpaths = true
\tuntrackedCache = true
[maintenance]
\tauto = false
[receive]
\tautogc = false
[gc]
\tauto = 0
";
/// The global attributes every case's git reads in place of the user's own: LF everywhere but in batch files.
const GIT_ATTRIBUTES: &str = "* text=auto eol=lf\n*.bat text eol=crlf\n*.cmd text eol=crlf\n";

/// The variables that point git at another global or system config, or give it config of its own, which no suite git
/// inherits: the config variables of the gate's `GIT_LOCATION` (`crates/gate/src/remote/git.rs`) less
/// `GIT_CONFIG_PARAMETERS`, which [`GIT_REDIRECTS`] holds, and `GIT_CONFIG_NOSYSTEM`.
const GIT_CONFIG_REDIRECTS: [&str; 4] = ["GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM", "GIT_CONFIG_NOSYSTEM", "GIT_CONFIG_COUNT"];
/// The prefixes of the config variables `GIT_CONFIG_COUNT` indexes, as the gate's `GIT_CONFIG_INDEXED`
/// (`crates/gate/src/remote/git.rs`) names them: `GIT_CONFIG_KEY_<n>` and `GIT_CONFIG_VALUE_<n>`.
const GIT_CONFIG_INDEXED: [&str; 2] = ["GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"];

/// Points `command`'s git, and every git it starts, at [`git_home`] as its home and XDG config folder, so it reads
/// the suite's global config and attributes, not the user's or a caller's; the system config still applies, under
/// them. The config variables a caller may have set ([`GIT_CONFIG_REDIRECTS`], [`GIT_CONFIG_INDEXED`]) are removed, as
/// the gate removes them from every git it starts.
pub fn git_home_for(command: &mut Command) {
    for name in GIT_CONFIG_REDIRECTS {
        command.env_remove(name);
    }
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_some_and(is_indexed_config) {
            command.env_remove(name);
        }
    }
    let home = git_home();
    command.env("HOME", home).env("XDG_CONFIG_HOME", home);
}

/// Whether the variable `name` is one of the config variables `GIT_CONFIG_COUNT` indexes, its case ignored as Windows
/// ignores it.
fn is_indexed_config(name: &str) -> bool {
    GIT_CONFIG_INDEXED
        .iter()
        .any(|prefix| name.get(..prefix.len()).is_some_and(|start| start.eq_ignore_ascii_case(prefix)) && name.len() > prefix.len())
}

/// The suite's git home under the target's temporary folder, shared by every test process and written once by each:
/// [`GIT_CONFIG`] as its `.gitconfig`, [`GIT_ATTRIBUTES`] as `git/attributes`.
fn git_home() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let home = Path::new(env!("CARGO_TARGET_TMPDIR")).join("git-home");
        place(&home.join(".gitconfig"), GIT_CONFIG);
        place(&home.join("git").join("attributes"), GIT_ATTRIBUTES);
        home
    })
}

/// Makes the file `path` hold `content`, as many test processes may at once: one that holds it already is left alone,
/// so no git ever reads a file being written; otherwise `content` is written beside it and renamed into place. A rename
/// refused, as by another process's git reading the file, is fine when that file already holds `content`.
fn place(path: &Path, content: &str) {
    if fs::read_to_string(path).is_ok_and(|held| held == content) {
        return;
    }
    let folder = path.parent().expect("a file of the git home has a folder");
    fs::create_dir_all(folder).expect("the git home's folders can be made");
    let name = path.file_name().expect("a file of the git home has a name").to_string_lossy();
    let staged = folder.join(format!("{name}.{}.new", process::id()));
    fs::write(&staged, content).expect("a file of the git home can be written");
    if let Err(error) = fs::rename(&staged, path) {
        fs::remove_file(&staged).expect("a file of the git home that was not renamed can be removed");
        let held = fs::read_to_string(path);
        assert!(
            held.as_deref().is_ok_and(|held| held == content),
            "{} cannot be replaced ({error}) and holds {held:?}",
            path.display()
        );
    }
}

/// Makes the case folder a git worktree of its own, so a gate there is at the root of a checkout named as the folder.
pub fn git_init(case: &Case) {
    git(case, &["init", "--quiet"]);
}

/// Runs git with `args` in the case folder, as a committer of the suite's own, with no hook and no signing, and
/// returns what it printed on stdout, its trailing line end cut; fails the case, naming the arguments, the folder,
/// git's exit status and both its streams, when git fails.
pub fn git(case: &Case, args: &[&str]) -> String {
    let mut command = Command::new("git");
    for name in GIT_REDIRECTS {
        command.env_remove(name);
    }
    git_home_for(&mut command);
    let identity = ["-c", "user.name=gate suite", "-c", "user.email=suite@example.invalid"];
    let quiet = ["-c", "commit.gpgsign=false", "-c", "core.hooksPath=.no-hooks"];
    let output = command
        .args(identity)
        .args(quiet)
        .args(args)
        .current_dir(case.folder())
        .stdin(Stdio::null())
        .output()
        .expect("git starts");
    assert!(
        output.status.success(),
        "git {args:?} in {}: {} stdout: {:?} stderr: {:?}",
        case.folder().display(),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("git's stdout is UTF-8").trim_end().to_owned()
}

/// The suite's stand-in for ssh, which `--test-remote-ssh` names.
pub const FAKE_SSH: &str = env!("CARGO_BIN_EXE_fake-ssh");
/// The variable naming the stand-in's steps file; the stand-in logs each call's verb to `<steps>.log`.
pub const STEPS_VARIABLE: &str = "GATE_FAKE_SSH_STEPS";
/// The variable naming the gate the stand-in runs `box <verb> --stdin` with, feeding it the request on standard input.
pub const GATE_VARIABLE: &str = "GATE_FAKE_SSH_GATE";
/// The variable whose words the stand-in gives the box's `preflight` before its verb.
pub const PREFLIGHT_SEAMS_VARIABLE: &str = "GATE_FAKE_SSH_PREFLIGHT_SEAMS";
/// The preflight seams of a tool case whose probes and SDK list must answer: bounds no run reaches, so only an answer
/// ends them and no load changes what the run says.
pub const ANSWERING: &str = "--test-probe-limit-ms 3600000 --test-dotnet-limit-ms 3600000";

/// Writes `steps`, one a line, as the stand-in's steps file `name` in the case's `ssh` folder, which must exist; its
/// path.
pub fn ssh_steps(case: &Case, name: &str, steps: &[&str]) -> PathBuf {
    let path = case.folder().join("ssh").join(format!("{name}.steps"));
    fs::write(&path, steps.join("\n")).expect("the steps file can be written");
    path
}

/// The verbs the stand-in of the steps file `steps` was called with, in order; none before its first call.
pub fn calls(steps: &Path) -> Vec<String> {
    fs::read_to_string(beside(steps, ".log")).map_or_else(|_| Vec::new(), |text| text.lines().map(str::to_owned).collect())
}

/// Writes `json` as the routing file the case's gates read, `<LOCALAPPDATA>\gate\remote.json`; its path.
pub fn write_remote_json(case: &Case, json: &str) -> PathBuf {
    let folder = case.local_app_data().join("gate");
    fs::create_dir_all(&folder).expect("the routing file's folder can be made");
    let path = folder.join("remote.json");
    fs::write(&path, json).expect("the routing file can be written");
    path
}

/// The case's nothing-back record, the one `*.nothing-back` entry of the state root's `remote` folder; `None` when
/// there is none.
pub fn record_path(case: &Case) -> Option<PathBuf> {
    let entries = fs::read_dir(case.root().join("remote")).ok()?;
    let found: Vec<PathBuf> = entries
        .map(|entry| entry.expect("a remote entry can be read").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "nothing-back"))
        .collect();
    assert!(found.len() <= 1, "one worktree leaves one record: {found:?}");
    found.into_iter().next()
}

/// The text of the case's nothing-back record ([`record_path`]); `None` when there is none.
pub fn nothing_back(case: &Case) -> Option<String> {
    record_path(case).map(|path| fs::read_to_string(path).expect("the record can be read"))
}

/// The PowerShell gate's folder under `case`'s LOCALAPPDATA, `%LOCALAPPDATA%\gate`.
pub fn powershell_folder(case: &Case) -> PathBuf {
    case.local_app_data().join("gate")
}

/// `at` in .NET ticks, as the PowerShell gate writes a ticket's times.
fn dotnet_ticks(at: Timestamp) -> i64 {
    i64::try_from(at.as_nanosecond().div_euclid(100) + UNIX_EPOCH_TICKS).expect("a test moment fits .NET ticks")
}

/// Writes `text` at `path`, a `.json` file in a folder that exists, through a file beside it moved over it, so a gate
/// never reads it torn.
fn write_whole(path: &Path, text: &str) {
    let staged = path.with_extension("json.staged");
    fs::write(&staged, text).expect("the file can be staged");
    fs::rename(&staged, path).expect("the file can be moved into place");
}

/// Writes the claim a PowerShell gate holding the heavy or light slot `name` (`gate-slot-<i>` or `gate-light-slot-<i>`)
/// writes, `%LOCALAPPDATA%\gate\slots\<name>.json` under `case`, and returns its path: the gate `pid` whose process
/// started at `start`, running `command`, took the slot at `started`.
pub fn write_powershell_claim(case: &Case, name: &str, (pid, start): (u32, Timestamp), command: &str, started: Timestamp) -> PathBuf {
    let (kind, index) = name
        .strip_prefix("gate-light-slot-")
        .map(|index| ("light", index))
        .or_else(|| name.strip_prefix("gate-slot-").map(|index| ("heavy", index)))
        .expect("a heavy or light slot's claim");
    let index: u32 = index.parse().expect("the slot's index is a number");
    let claim = json!({
        "kind": kind,
        "index": index,
        "pid": pid,
        "processStart": start.to_string(),
        "started": started.to_string(),
        "cwd": shown(&case.folder()),
        "command": command,
        "log": "out.log",
        "timeoutSeconds": 3600.0,
    });
    let folder = powershell_folder(case).join("slots");
    fs::create_dir_all(&folder).expect("the claims folder can be made");
    let path = folder.join(format!("{name}.json"));
    write_whole(&path, &claim.to_string());
    path
}

/// Writes the arrival ticket a PowerShell gate waiting for a `kind` slot writes, `%LOCALAPPDATA%\gate\waiting\<pid>-<start
/// ticks>.json` under `case`, and returns its path: the gate `pid` whose process started at `start` arrived at `arrived`.
pub fn write_powershell_ticket(case: &Case, (pid, start): (u32, Timestamp), kind: &str, arrived: Timestamp) -> PathBuf {
    let start_ticks = dotnet_ticks(start);
    let ticket = json!({
        "kind": kind,
        "arrived": dotnet_ticks(arrived),
        "pid": pid,
        "startTicks": start_ticks,
        "holds": false,
    });
    let folder = powershell_folder(case).join("waiting");
    fs::create_dir_all(&folder).expect("the tickets folder can be made");
    let path = folder.join(format!("{pid}-{start_ticks}.json"));
    write_whole(&path, &ticket.to_string());
    path
}

/// Copies fake-tool into the case folder as `name`, so a command can name it inside the case's worktree.
pub fn copy_tool(case: &Case, name: &str) {
    fs::copy(FAKE_TOOL, case.folder().join(name)).expect("fake-tool can be copied into the case folder");
}

/// An entry in a folder's access list that denies this user the icacls `rights` given, such as `(X)` (traverse the
/// folder itself) or `(OI)(IO)(X)` (run any file put in it); dropping the guard takes the entry away.
pub struct DenyEntry(PathBuf);

impl DenyEntry {
    pub fn new(folder: &Path, rights: &str) -> DenyEntry {
        let denied = DenyEntry(folder.to_path_buf());
        let status = Command::new("icacls")
            .arg(folder)
            .args(["/deny", &format!("{}:{rights}", this_user())])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .expect("icacls starts");
        assert!(status.success(), "the deny entry is added");
        denied
    }
}

/// This user, as `whoami` names one and icacls takes it.
fn this_user() -> String {
    let said = Command::new("whoami").stdin(Stdio::null()).output().expect("whoami starts");
    assert!(said.status.success(), "whoami names this user: {said:?}");
    String::from_utf8_lossy(&said.stdout).trim().to_owned()
}

impl Drop for DenyEntry {
    /// Takes the deny entry away; one that stays is said on stderr with its folder, since the test cannot fail here.
    #[expect(clippy::print_stderr, reason = "a deny entry that cannot be taken away is said without failing the case")]
    fn drop(&mut self) {
        let removed = Command::new("icacls")
            .arg(&self.0)
            .args(["/remove:d", &this_user()])
            .stdin(Stdio::null())
            .output();
        match removed {
            Ok(output) if output.status.success() => {}
            Ok(output) => eprintln!(
                "the deny entry on {} is left: icacls said {}{}",
                shown(&self.0),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
            Err(error) => eprintln!("the deny entry on {} is left: icacls cannot start: {error}", shown(&self.0)),
        }
    }
}

/// Copies fake-tool into the case folder as `name` and returns its absolute path, so a command or a process the case
/// starts itself names it inside the case's working directory.
pub fn tool(case: &Case, name: &str) -> String {
    copy_tool(case, name);
    shown(&case.folder().join(name))
}

/// The go file of a case that parks a stand-in or a stub until it exists: writing it when it drops releases what waits
/// for it even when the case panics before its own write, and writing one the case already made is harmless. A write
/// that fails is said unless the file is there after all.
pub struct Go(pub PathBuf);

impl Go {
    /// The path of the go file the case writes and what it parked waits for.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Go {
    fn drop(&mut self) {
        if let Err(error) = fs::write(&self.0, "")
            && !self.0.exists()
        {
            let _ = writeln!(io::stderr(), "the go file {} cannot be made: {error}", self.0.display());
        }
    }
}

/// Returns once every handle on `path` is closed: an open that shares nothing succeeds only then. A command a killed
/// gate left running holds its log open until it ends, so this waits until no process of that command has the log
/// open.
pub fn wait_until_closed(path: &Path) {
    wait_until(&format!("every handle on {} closing", path.display()), || {
        fs::OpenOptions::new().read(true).share_mode(0).open(path).is_ok()
    });
}

/// The payload of a `gate box run` of `command` for the work folder `key`, as the remote client sends it, laid out as
/// the client lays out a builder's root: under the case folder's `remote` folder, the bare repository `<key>.git`, the
/// work folder `work\<key>` with its lock and index beside it, and the `outputs` folder. A job name unique to the case
/// and the key, its 12-hex nonce ending in the run's number `0000`, and a 32-hex outputs nonce, unique the same way and
/// ending in the same `0000`.
pub fn box_payload(case: &Case, key: &str, command: &[&str]) -> Value {
    let remote = case.folder().join("remote");
    let path = |name: String| remote.join(name).to_str().expect("the case folder is Unicode").to_owned();
    let work = path(format!(r"work\{key}"));
    let unique = format!("{:04x}{:04x}", process::id() & 0xffff, case.number & 0xffff);
    json!({
        "Key": key,
        "Bare": path(format!("{key}.git")),
        "Lock": format!("{work}.lock"),
        "Index": format!("{work}.index"),
        "Work": work,
        "OutputFolder": path("outputs".to_owned()),
        "Job": format!(r"Local\gate-remote-{key}-{unique}0000"),
        "Command": command,
        "Subfolder": "",
        "Identity": {
            "GIT_AUTHOR_NAME": "gate suite",
            "GIT_AUTHOR_EMAIL": "suite@example.invalid",
            "GIT_COMMITTER_NAME": "gate suite",
            "GIT_COMMITTER_EMAIL": "suite@example.invalid",
        },
        "Outputs": [],
        "Nonce": format!("{unique}{}0000", "0".repeat(20)),
        "OutputsFull": false,
    })
}

/// The unique part of the case's own desktop namespace and lock, which the box's `--test-box-desktop-mutex` takes: the
/// suite's process and the case's number, so no two cases or runs share one.
pub fn desktop_unique(case: &Case) -> String {
    format!("{:x}-{}", process::id(), case.number)
}

/// [`box_payload`] of a run that asks for the builder's desktop and may wait `wait_secs` for it.
pub fn desktop_payload(case: &Case, key: &str, command: &[&str], wait_secs: u64) -> Value {
    let mut payload = box_payload(case, key, command);
    payload["Desktop"] = json!(true);
    payload["DesktopWait"] = json!(wait_secs);
    payload
}

/// The box's desktop seams of a case: the fault `mode`, the console session `session`, the case's own desktop lock
/// ([`desktop_unique`]) and the cap `ms` on the run's desktop waits.
pub fn desktop_seams(case: &Case, mode: &str, session: &str, ms: &str) -> Vec<String> {
    [
        "--test-box-desktop",
        mode,
        "--test-box-desktop-session",
        session,
        "--test-box-desktop-mutex",
        &desktop_unique(case),
        "--test-box-desktop-ms",
        ms,
    ]
    .map(str::to_owned)
    .to_vec()
}

/// The case's own desktop lock, as the box opens it under [`desktop_seams`].
fn desktop_lock(case: &Case) -> gate_win32::PrivateName {
    let sid = gate_win32::current_user_sid().expect("the suite's user is read");
    gate_win32::PrivateName::for_user(&sid, &desktop_unique(case)).expect("the case's desktop lock has a name")
}

/// The case's own desktop lock held by a thread of the suite until dropped, as another run holding the builder's
/// desktop holds it. The lock is owned by a thread, so that one thread takes and releases it.
pub struct HeldDesktop {
    release: Option<std::sync::mpsc::Sender<()>>,
    holder: Option<JoinHandle<()>>,
}

impl HeldDesktop {
    /// Takes the case's desktop lock on a thread of its own; returns once it is held.
    pub fn hold(case: &Case) -> HeldDesktop {
        let name = desktop_lock(case);
        let (taken, held) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let holder = thread::spawn(move || {
            let mutex = gate_win32::PrivateMutex::open(&name).expect("the case's desktop lock opens");
            let guard = mutex.acquire(Duration::ZERO).expect("the case's desktop lock is free");
            taken.send(()).expect("the case waits for the lock to be held");
            let _ = released.recv();
            drop(guard);
        });
        held.recv().expect("the holder took the case's desktop lock");
        HeldDesktop {
            release: Some(release),
            holder: Some(holder),
        }
    }

    /// Takes the case's desktop lock on a thread that ends without releasing it, so the next taker finds it abandoned.
    /// The thread's handle on the lock is left open for the rest of the suite's process, as an open handle is what
    /// keeps the abandoned lock in being; the lock cannot leave its thread.
    pub fn abandon(case: &Case) {
        let name = desktop_lock(case);
        thread::spawn(move || {
            let mutex: &'static gate_win32::PrivateMutex =
                Box::leak(Box::new(gate_win32::PrivateMutex::open(&name).expect("the case's desktop lock opens")));
            let _never_released = std::mem::ManuallyDrop::new(mutex.acquire(Duration::ZERO).expect("the case's desktop lock is free"));
        })
        .join()
        .expect("the abandoning thread ends");
    }
}

impl Drop for HeldDesktop {
    fn drop(&mut self) {
        drop(self.release.take());
        if let Some(holder) = self.holder.take() {
            holder.join().expect("the holder of the case's desktop lock released it");
        }
    }
}

/// The case's own desktop lock kept open, not taken, by the suite for as long as this lives, so the lock outlives each
/// box that takes it: a box that ended without releasing it leaves it abandoned for the next.
pub struct KeptDesktop(#[expect(dead_code, reason = "held only to keep the lock open")] gate_win32::PrivateMutex);

impl KeptDesktop {
    /// Opens the case's desktop lock without taking it.
    pub fn open(case: &Case) -> KeptDesktop {
        KeptDesktop(gate_win32::PrivateMutex::open(&desktop_lock(case)).expect("the case's desktop lock opens"))
    }
}

/// Puts the suite's process in a fresh job that allows breakaway, nested in the job it already runs in, once. A box
/// run creates its command with `CREATE_BREAKAWAY_FROM_JOB`, which fails while the immediate job refuses breakaway, and
/// the gate's own job refuses it; from this inner job the command breaks away and stays in the gate's job, as the
/// PowerShell wrapper's self-test arranges it. Every program the suite starts afterwards starts in this job too.
pub fn allow_breakaway() {
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    static ALLOWED: OnceLock<()> = OnceLock::new();
    ALLOWED.get_or_init(|| {
        // SAFETY: no security attributes and no name are allowed; a null handle is a failure.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        assert!(!job.is_null(), "the breakaway job is created: {}", io::Error::last_os_error());
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_BREAKAWAY_OK;
        let size = u32::try_from(size_of_val(&limits)).expect("the limits fit a u32");
        // SAFETY: the job is open and `limits` is a whole extended limit structure of `size` bytes.
        let set = unsafe { SetInformationJobObject(job, JobObjectExtendedLimitInformation, (&raw const limits).cast(), size) };
        assert_ne!(set, 0, "breakaway is allowed: {}", io::Error::last_os_error());
        // SAFETY: the job is open and the pseudo-handle of this process is always open. The job's handle is never
        // closed: the job lasts as long as this process, which it holds.
        let assigned = unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) };
        assert_ne!(assigned, 0, "the suite enters the job: {}", io::Error::last_os_error());
    });
}

/// The words of a valid run that prints its request, the command after `--` last.
pub fn print_run(command: &[&'static str]) -> Vec<&'static str> {
    let mut words = vec![
        "run",
        "--slot",
        "heavy",
        "--timeout",
        "5",
        "--log",
        "out.log",
        "--test-print-request",
        "--",
    ];
    words.extend(command);
    words
}

/// How long a command the gate is meant to end sleeps, as fake-tool's `sleep` takes it in milliseconds: past any case
/// and just inside the suite's 600 s hang guard, so only the gate's kill or interrupt ends it. A starved watch delays
/// that end, but the command never ends first on a timer of its own and turns the case's 124 into a pass.
pub const UNTIL_KILLED: &str = "590000";

/// 100 ns ticks in one core-second.
pub const CORE_SECOND: u64 = 10_000_000;

/// A process a case's command starts outside its job with fake-tool's `spawn-away`, spinning until its stop file
/// exists, or, idle, sleeping without ever looking at it. It is ended on every path out of the case, a panic included:
/// its stop file is written, then the process the pid file names, checked to be the one that started then, is
/// terminated, which is what ends an idle one.
pub struct Escapee {
    pid_file: PathBuf,
    stop: PathBuf,
}

impl Escapee {
    /// The escapee whose pid file and stop file are `away.pid` and `stop` in `case`'s folder.
    pub fn new(case: &Case) -> Escapee {
        Escapee {
            pid_file: case.folder().join("away.pid"),
            stop: case.folder().join("stop"),
        }
    }

    /// The words of a command that starts the escapee spinning on `threads` threads (`0`: sleeping, its stop file never
    /// read), then waits for the escapee's stop file itself.
    pub fn command(&self, threads: &str) -> Vec<String> {
        let (pid_file, stop) = (shown(&self.pid_file), shown(&self.stop));
        [FAKE_TOOL, "spawn-away", &pid_file, threads, &stop, "wait", &stop]
            .map(str::to_owned)
            .to_vec()
    }

    /// The words of a command that starts the escapee sleeping, then sleeps itself until just inside the hang guard, so
    /// only the gate's kill ends it: neither process polls a file or uses a tick of CPU, so nothing they do is progress.
    pub fn idle_command(&self) -> Vec<String> {
        let (pid_file, stop) = (shown(&self.pid_file), shown(&self.stop));
        [FAKE_TOOL, "spawn-away", &pid_file, "0", &stop, "sleep", UNTIL_KILLED]
            .map(str::to_owned)
            .to_vec()
    }

    /// The words of a command whose child, the launcher, starts the escapee sleeping and ends a second later, while the
    /// command holds the launcher's handle until a release file no case writes: from then on no process of the job is
    /// the escapee's parent, so only what the watch recorded of it reaches it. Only `spawn-pid`'s look for its release
    /// file every 20 ms uses CPU, far under the stall floor.
    pub fn launched_command(&self) -> Vec<String> {
        let (pid_file, stop) = (shown(&self.pid_file), shown(&self.stop));
        let launcher = shown(&self.pid_file.with_file_name("launcher.pid"));
        let release = shown(&self.pid_file.with_file_name("release"));
        [
            FAKE_TOOL,
            "spawn-pid",
            &launcher,
            &release,
            FAKE_TOOL,
            "spawn-away",
            &pid_file,
            "0",
            &stop,
            "sleep",
            "1000",
        ]
        .map(str::to_owned)
        .to_vec()
    }

    /// The words of [`Escapee::launched_command`] with a launcher that ends as soon as it has started the escapee, so
    /// no sample of a watch that samples every few seconds sees the escapee under it: only the job's memory of the
    /// launcher reaches it.
    pub fn instant_launched_command(&self) -> Vec<String> {
        let mut words = self.launched_command();
        words.truncate(words.len() - 2);
        words
    }

    /// The exit code of the escapee `process`, once it has ended, waited for at most half the hang guard: an idle
    /// escapee ends on its own only after its 600 s sleep, so on a broken run the wait ends first and says it still
    /// runs, and a passing run, whose kill ended it before the gate returned, never waits. `None` when it still runs.
    pub fn exit_code_once_ended(process: &StoppableProcess) -> Option<u32> {
        let ended = matches!(process.wait(HANG_GUARD / 2), Ok(true));
        ended.then(|| process.exit_code().expect("an ended escapee's exit code is read"))
    }

    /// The escapee's pid and the process, once the pid file holds its whole line, checked to be the process that
    /// started then. Fails at once, rather than at the hang guard, when `ended` says the command that would write the
    /// file has ended without it.
    #[expect(clippy::panic, reason = "a command that ended before it wrote the pid file fails the case saying so")]
    pub fn open(&self, mut ended: impl FnMut() -> bool) -> (u32, StoppableProcess) {
        let mut said = None;
        wait_until(
            &format!("{} holding a pid and a start, or its writer ending", self.pid_file.display()),
            || {
                said = self.said();
                said.is_some() || ended()
            },
        );
        let Some((pid, start)) = said else {
            panic!("the command ended before it wrote {}", self.pid_file.display());
        };
        let process = StoppableProcess::open(pid)
            .expect("the escapee is opened")
            .expect("it runs: it spins until its stop file exists, or sleeps");
        let created = process.created().expect("the escapee's start is read");
        assert_eq!(
            Some(created),
            proc_table::filetime_to_timestamp(start),
            "pid {pid} is the escapee spawn-away started"
        );
        (pid, process)
    }

    /// The launcher [`Escapee::launched_command`] starts, once it has ended on its own: opened by the pid `spawn-pid`
    /// wrote, which goes on naming the launcher while `spawn-pid` holds its handle, then waited for within the hang
    /// guard. Fails unless it exited 0, its own end after its sleep rather than a kill's.
    pub fn launcher_ended(&self) -> StoppableProcess {
        let path = self.pid_file.with_file_name("launcher.pid");
        let mut pid = None;
        wait_until(&format!("{} holding the launcher's pid", path.display()), || {
            pid = fs::read_to_string(&path).ok().and_then(|text| text.parse::<u32>().ok());
            pid.is_some()
        });
        let pid = pid.expect("the wait returns once the launcher's pid is read");
        let launcher = StoppableProcess::open(pid)
            .expect("the launcher is opened")
            .expect("spawn-pid holds its handle, so its pid still names it");
        assert!(
            matches!(launcher.wait(HANG_GUARD), Ok(true)),
            "the launcher {pid} ended within the hang guard"
        );
        assert_eq!(launcher.exit_code().ok(), Some(0), "the launcher {pid} ended on its own, not by a kill");
        launcher
    }

    /// Ends the escapee's `spin` by writing its stop file.
    pub fn stop(&self) {
        fs::write(&self.stop, "").expect("the escapee's stop file is written");
    }

    /// The pid and the start, in 100 ns ticks since 1601, of the whole line `spawn-away` wrote; `None` until it is
    /// whole.
    fn said(&self) -> Option<(u32, i64)> {
        let text = fs::read_to_string(&self.pid_file).ok()?;
        let (pid, start) = text.strip_suffix('\n')?.split_once(' ')?;
        Some((pid.parse().ok()?, start.parse().ok()?))
    }
}

impl Drop for Escapee {
    fn drop(&mut self) {
        // Each fails only when there is nothing left to end: the escapee has ended or never started, or the case's
        // folder is gone.
        let _ = fs::write(&self.stop, "");
        let Some((pid, start)) = self.said() else {
            return;
        };
        if let Ok(Some(process)) = StoppableProcess::open(pid)
            && process.created().ok() == proc_table::filetime_to_timestamp(start)
        {
            let _ = process.terminate(1);
        }
    }
}

/// The CPU time the running process `pid` has used, in 100 ns ticks; 0 once it has ended.
pub fn cpu_ticks(pid: u32) -> u64 {
    gate_win32::process_times(pid).ok().flatten().map_or(0, |times| times.cpu)
}

/// A key under `HKEY_CURRENT_USER` that stands in for the machine's list of installed .NET Framework SDKs, which the seam
/// `--test-netfx-key` points the netfx-sdk probe at, so no case reads this machine's own list; one per test process,
/// removed when dropped. Until an SDK is listed in it, it does not exist, and a probe of it finds no SDK.
pub struct NetfxKey {
    /// The key's path under `HKEY_CURRENT_USER`.
    path: String,
}

impl NetfxKey {
    /// The key of this test process, emptied of what an earlier process of the same id left.
    pub fn new() -> NetfxKey {
        let key = NetfxKey {
            path: format!(r"Software\gate-tests\netfx-{}", process::id()),
        };
        key.removed();
        key
    }

    /// The key with an SDK of `version` listed, its `KitsInstallationFolder` the folder `folder`, which is made.
    pub fn with_sdk(self, version: &str, folder: &Path) -> NetfxKey {
        fs::create_dir_all(folder).expect("the SDK's folder can be made");
        let folder = folder.display().to_string();
        let subkey = format!(r"{}\{version}", self.full());
        reg(&["add", &subkey, "/v", "KitsInstallationFolder", "/t", "REG_SZ", "/d", &folder, "/f"]);
        self
    }

    /// The seam that points a probe at the key, as the words `--test-netfx-key <path>`.
    pub fn seam(&self) -> String {
        format!("--test-netfx-key {}", self.path)
    }

    /// The key's path under `HKEY_CURRENT_USER`, the value the seam takes.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The key's full name, as `reg.exe` takes it.
    fn full(&self) -> String {
        format!(r"HKCU\{}", self.path)
    }

    /// Removes the key and all it holds; a key that is not there is no failure.
    fn removed(&self) {
        let _ = reg_status(&["delete", &self.full(), "/f"]);
    }
}

impl Drop for NetfxKey {
    fn drop(&mut self) {
        self.removed();
    }
}

/// Runs the system's `reg.exe` with `args`, which must succeed.
fn reg(args: &[&str]) {
    let status = reg_status(args);
    assert!(status.as_ref().is_ok_and(ExitStatus::success), "reg.exe {args:?} failed: {status:?}");
}

/// The exit status of the system's `reg.exe` run with `args`, its output dropped.
fn reg_status(args: &[&str]) -> io::Result<ExitStatus> {
    let system = std::env::var_os("SystemRoot").expect("SystemRoot is set");
    Command::new(Path::new(&system).join("System32").join("reg.exe"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
}
