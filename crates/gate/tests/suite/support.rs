//! The harness: a [`Case`] with its own state root, the programs under test, and order-only assertions.

use std::io::{self, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use std::{fs, process, thread};

use serde_json::{Value, json};

/// The gate under test.
pub const GATE: &str = env!("CARGO_BIN_EXE_gate");
/// The helper program the suite's commands run.
pub const FAKE_TOOL: &str = env!("CARGO_BIN_EXE_fake-tool");

/// The published usage text, copied here so a change to it fails a case.
pub const USAGE: &str = "\
usage: gate run --slot heavy|light|critical --timeout <seconds> --log <path> [options] -- <command> [args...]
       gate stop-tree <pid>
       gate --version | --help

run options:
  --stall <seconds>    stall window, default 120
  --tail <lines>       log lines printed at the end, default 20
  --no-markers         the command's status is the verdict
  --local              never route this run to a builder
  --state-dir <path>   use another state root (also GATE_STATE_DIR)

--slot, --timeout, --log and the command are all required.
The PowerShell form (gate -Log <path> -TimeoutSeconds <n> -Slot <kind> [...] -- <command>) is also accepted.
";

/// The git variables a caller's environment, such as a git hook around the suite, may hold that point git at another
/// repository, index, object store or config; no case's gate or git gets them, and a gate with `GIT_INDEX_FILE` set
/// never routes.
pub const GIT_REDIRECTS: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_CONFIG_PARAMETERS",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
];
/// The variables a real gate around the suite may have set, which no case's gate may inherit.
const INHERITED: [&str; 2] = ["GATE_STATE_DIR", "GATE_SLOT_HELD"];
/// Only guards against a stuck case; no case asserts how long anything took. Ten minutes, since a loaded machine
/// (several heavy gates building at once) has kept a healthy case past a minute, and a real hang still ends.
const HANG_GUARD: Duration = Duration::from_secs(600);
/// How long the output readers get to end once the program has exited; a reader whose pipe a grandchild still holds
/// would otherwise keep its case waiting the whole hang guard.
const READER_GRACE: Duration = Duration::from_secs(60);
/// How often a wait looks again.
const POLL: Duration = Duration::from_millis(10);

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

    /// Readies `command` for this case: the inherited gate variables removed, LOCALAPPDATA under the case folder
    /// ([`Case::local_app_data`]), the working directory the case folder,
    /// and `args` given with `--state-dir <root>` after a `run` or `stop-tree` subcommand.
    pub fn prepare(&self, command: &mut Command, args: &[&str]) {
        self.prepare_without_state_dir(command, &[]);
        let Some((first, rest)) = args.split_first() else {
            return;
        };
        command.arg(first);
        if matches!(*first, "run" | "stop-tree") {
            command.arg("--state-dir").arg(&self.state);
        }
        command.args(rest);
    }

    /// As [`Case::prepare`], with `args` given as they are.
    pub fn prepare_without_state_dir(&self, command: &mut Command, args: &[&str]) {
        for name in INHERITED.iter().chain(&GIT_REDIRECTS) {
            command.env_remove(name);
        }
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

/// Starts `command` with stdin null and its output gathered as it arrives.
pub fn start(mut command: Command) -> Running {
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().expect("the program starts");
    let (stdout, stdout_reader) = gather(child.stdout.take().expect("stdout is piped"));
    let (stderr, stderr_reader) = gather(child.stderr.take().expect("stderr is piped"));
    Running {
        child,
        stdout,
        stderr,
        readers: vec![stdout_reader, stderr_reader],
    }
}

/// Which of a program's output streams a caller stops reading at once.
#[derive(Clone, Copy)]
pub enum Closed {
    Stdout,
    Stderr,
}

/// Starts `command` with stdin null and both output streams piped, dropping this process's read end of `closed` at
/// once, so the program's next write to that stream fails; the other stream is gathered as [`start`] gathers it.
pub fn start_with_closed(closed: Closed, mut command: Command) -> Running {
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().expect("the program starts");
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
    Running {
        child,
        stdout,
        stderr,
        readers: vec![reader],
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
}

impl Running {
    /// Returns once stderr contains `needle`; fails with the stderr so far after the hang guard.
    pub fn wait_for_stderr(&self, needle: &str) {
        wait_for_text(&self.stderr, "stderr", needle);
    }

    /// Returns stderr so far once `check` holds for it; fails saying `what` did not happen, with the stderr so far,
    /// after the hang guard.
    pub fn wait_for_stderr_where(&self, what: &str, check: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + HANG_GUARD;
        loop {
            let text = text(&self.stderr);
            if check(&text) {
                return text;
            }
            assert!(Instant::now() < deadline, "{what} did not happen on stderr; stderr so far:\n{text}");
            thread::sleep(POLL);
        }
    }

    /// Returns stdout so far once it contains `needle`; fails with the stdout so far after the hang guard.
    pub fn wait_for_stdout(&self, needle: &str) -> String {
        wait_for_text(&self.stdout, "stdout", needle)
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
        let deadline = Instant::now() + READER_GRACE;
        while Instant::now() < deadline && !self.readers.iter().all(JoinHandle::is_finished) {
            thread::sleep(POLL);
        }
        for reader in self.readers.drain(..) {
            if reader.is_finished() {
                reader.join().expect("an output reader ends");
            }
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
            thread::sleep(POLL);
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
    /// Kills a program its case left running, as when the case failed before finishing it. A test declares its
    /// `Running` after its [`Case`], so the locals' reverse drop order ends the program before the case root goes.
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(None)) {
            return;
        }
        if let Err(error) = self.child.kill().and_then(|()| self.child.wait().map(drop)) {
            // A panic in drop would abort the run; the program is left for the machine to end.
            let _ = writeln!(io::stderr(), "could not kill a case's program {}: {error}", self.child.id());
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

/// Returns the text of `buffer`, the program's `stream`, once it contains `needle`; fails with the text so far after
/// the hang guard.
fn wait_for_text(buffer: &Buffer, stream: &str, needle: &str) -> String {
    let deadline = Instant::now() + HANG_GUARD;
    loop {
        let text = text(buffer);
        if text.contains(needle) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "{needle:?} did not appear on {stream}; {stream} so far:\n{text}"
        );
        thread::sleep(POLL);
    }
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
        thread::sleep(POLL);
    }
}

/// Returns once `check` holds; fails naming `what` after the hang guard.
pub fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + HANG_GUARD;
    while !check() {
        assert!(Instant::now() < deadline, "{what} did not happen within the hang guard");
        thread::sleep(POLL);
    }
}

/// Returns once `path` exists.
pub fn wait_for_file(path: &Path) {
    wait_until(&format!("{} appearing", path.display()), || path.exists());
}

/// Writes `json` as the counts file of `root`, through a file beside it moved over it, so a gate never reads it torn.
pub fn write_counts(root: &Path, json: &str) {
    let staged = root.join("slot-counts.json.staged");
    fs::write(&staged, json).expect("the counts can be staged");
    fs::rename(&staged, root.join("slot-counts.json")).expect("the counts can be moved into place");
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

/// The words of a light run logging to `log`, `extra` before `--` and `command` after it. Gates that run at once each
/// need a log of their own, since a gate refuses a log another gate is writing. The ceiling is far above the hang
/// guard, so a run a case holds is never ended by it. The server scan is confined to the case's folder, the gate's
/// working directory, so no case moves a build server of the machine it did not start itself. A case cannot pass a
/// `--test-servers-under` of its own beside these words: the gate refuses a flag given twice.
pub fn run_words<'a>(log: &'a str, extra: &[&'a str], command: &[&'a str]) -> Vec<&'a str> {
    let mut words = vec!["run", "--slot", "light", "--timeout", "3600", "--log", log, "--test-servers-under", "."];
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

/// The words of a critical run, as [`run_words`] gives those of a light one.
pub fn critical_words<'a>(log: &'a str, extra: &[&'a str], command: &[&'a str]) -> Vec<&'a str> {
    let mut words = run_words(log, extra, command);
    words[2] = "critical";
    words
}

/// Makes the case folder a git worktree of its own, so a gate there is at the root of a checkout named as the folder.
pub fn git_init(case: &Case) {
    git(case, &["init", "--quiet"]);
}

/// Runs git with `args` in the case folder, as a committer of the suite's own, with no hook and no signing; fails the
/// case when git fails.
pub fn git(case: &Case, args: &[&str]) {
    let mut command = Command::new("git");
    for name in GIT_REDIRECTS {
        command.env_remove(name);
    }
    let identity = ["-c", "user.name=gate suite", "-c", "user.email=suite@example.invalid"];
    let quiet = ["-c", "commit.gpgsign=false", "-c", "core.hooksPath=.no-hooks"];
    let status = command
        .args(identity)
        .args(quiet)
        .args(args)
        .current_dir(case.folder())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .expect("git starts");
    assert!(status.success(), "git {args:?} succeeds");
}

/// Writes `json` as the routing file the case's gates read, `<LOCALAPPDATA>\gate\remote.json`; its path.
pub fn write_remote_json(case: &Case, json: &str) -> PathBuf {
    let folder = case.local_app_data().join("gate");
    fs::create_dir_all(&folder).expect("the routing file's folder can be made");
    let path = folder.join("remote.json");
    fs::write(&path, json).expect("the routing file can be written");
    path
}

/// Copies fake-tool into the case folder as `name`, so a command can name it inside the case's worktree.
pub fn copy_tool(case: &Case, name: &str) {
    fs::copy(FAKE_TOOL, case.folder().join(name)).expect("fake-tool can be copied into the case folder");
}

/// Returns once every handle on `path` is closed: an open that shares nothing succeeds only then. A command a killed
/// gate left running holds its log open until it ends, so this waits until no process of that command has the log
/// open.
pub fn wait_until_closed(path: &Path) {
    wait_until(&format!("every handle on {} closing", path.display()), || {
        fs::OpenOptions::new().read(true).share_mode(0).open(path).is_ok()
    });
}

/// The payload of a `gate box run` of `command` for the work folder `key`, as the remote client sends it: every path
/// under the case folder's `remote` folder, and a job name unique to the case and the key, its 12-hex nonce ending in
/// the run's number `0000`.
pub fn box_payload(case: &Case, key: &str, command: &[&str]) -> Value {
    let remote = case.folder().join("remote");
    let path = |name: String| remote.join(name).to_str().expect("the case folder is Unicode").to_owned();
    json!({
        "Key": key,
        "Bare": path(format!("{key}.git")),
        "Work": path(key.to_owned()),
        "Lock": path(format!("{key}.lock")),
        "Index": path(format!("{key}.index")),
        "OutputFolder": path(format!("{key}.outputs")),
        "Job": format!(r"Local\gate-remote-{key}-{:04x}{:04x}0000", process::id() & 0xffff, case.number & 0xffff),
        "Command": command,
        "Subfolder": "",
        "Identity": {
            "GIT_AUTHOR_NAME": "gate suite",
            "GIT_AUTHOR_EMAIL": "suite@example.invalid",
            "GIT_COMMITTER_NAME": "gate suite",
            "GIT_COMMITTER_EMAIL": "suite@example.invalid",
        },
        "Outputs": [],
        "Nonce": "",
        "OutputsFull": false,
    })
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
