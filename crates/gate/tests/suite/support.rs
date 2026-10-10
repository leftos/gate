//! The harness: a [`Case`] with its own state root, the programs under test, and order-only assertions.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use std::{fs, process, thread};

use serde_json::Value;

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

/// The variables a real gate around the suite may have set, which no case's gate may inherit.
const INHERITED: [&str; 2] = ["GATE_STATE_DIR", "GATE_SLOT_HELD"];
/// Only guards against a stuck case; no case asserts how long anything took.
const HANG_GUARD: Duration = Duration::from_secs(60);
/// How often a wait looks again.
const POLL: Duration = Duration::from_millis(10);

static NEXT_CASE: AtomicU64 = AtomicU64::new(0);

/// One case: its own state root under the target directory, removed when the case ends.
pub struct Case {
    root: PathBuf,
}

impl Case {
    pub fn new(name: &str) -> Case {
        let number = NEXT_CASE.fetch_add(1, Ordering::Relaxed);
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}-{number}", process::id()));
        if root.exists() {
            fs::remove_dir_all(&root).expect("a case root left by an earlier run can be removed");
        }
        fs::create_dir_all(&root).expect("the case root can be made");
        Case { root }
    }

    /// The case's state root, absolute as the gate reports it.
    pub fn root(&self) -> PathBuf {
        std::path::absolute(&self.root).expect("the case root has an absolute path")
    }

    /// Readies `command` for this case: the inherited gate variables removed, LOCALAPPDATA under the case root
    /// ([`Case::local_app_data`]), the working directory the case root,
    /// and `args` given with `--state-dir <root>` after a `run` or `stop-tree` subcommand.
    pub fn prepare(&self, command: &mut Command, args: &[&str]) {
        self.prepare_without_state_dir(command, &[]);
        let Some((first, rest)) = args.split_first() else {
            return;
        };
        command.arg(first);
        if matches!(*first, "run" | "stop-tree") {
            command.arg("--state-dir").arg(&self.root);
        }
        command.args(rest);
    }

    /// As [`Case::prepare`], with `args` given as they are.
    pub fn prepare_without_state_dir(&self, command: &mut Command, args: &[&str]) {
        for name in INHERITED {
            command.env_remove(name);
        }
        command.env("LOCALAPPDATA", self.local_app_data()).current_dir(&self.root).args(args);
    }

    /// The LOCALAPPDATA every case's gate sees, under the case root, so no case reads the host's default root. The
    /// folder is not created.
    pub fn local_app_data(&self) -> PathBuf {
        self.root().join("appdata")
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

    pub fn run(&self, args: &[&str]) -> Outcome {
        finish(self.gate(args))
    }

    /// Starts the gate with `args` as [`Case::gate`] readies it.
    pub fn start(&self, args: &[&str]) -> Running {
        start(self.gate(args))
    }

    /// The names of the entries the gate left in the root.
    pub fn root_entries(&self) -> Vec<String> {
        fs::read_dir(&self.root)
            .expect("the case root can be listed")
            .map(|entry| entry.expect("a root entry can be read").file_name().to_string_lossy().into_owned())
            .collect()
    }
}

impl Drop for Case {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.root) {
            // A root left under the target directory harms no later case, and a panic in drop would abort the run.
            let _ = writeln!(io::stderr(), "could not remove the case root {}: {error}", self.root.display());
        }
    }
}

/// What a program did: its status code (`None` when killed by the hang guard) and its output.
#[derive(Debug)]
pub struct Outcome {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
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
        let deadline = Instant::now() + HANG_GUARD;
        loop {
            let stderr = text(&self.stderr);
            if stderr.contains(needle) {
                return;
            }
            assert!(Instant::now() < deadline, "{needle:?} did not appear on stderr; stderr so far:\n{stderr}");
            thread::sleep(POLL);
        }
    }

    /// Whether the program has not exited yet.
    pub fn is_running(&mut self) -> bool {
        self.child.try_wait().expect("the program's status can be read").is_none()
    }

    /// Joins the readers that end within the hang guard of the program's exit. A reader whose pipe a grandchild still
    /// holds is left running; its buffer is taken as it stands.
    fn let_readers_end(&mut self) {
        let deadline = Instant::now() + HANG_GUARD;
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

fn text(buffer: &Buffer) -> String {
    String::from_utf8_lossy(&buffer.lock().expect("no reader panicked holding the buffer")).into_owned()
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

/// Asserts that `needles` occur in `haystack` in this order, each after the end of the one before.
pub fn assert_in_order(haystack: &str, needles: &[&str]) {
    let mut from = 0;
    for needle in needles {
        let found = haystack[from..].find(needle);
        assert!(found.is_some(), "{needle:?} does not follow byte {from} of:\n{haystack}");
        from += found.expect("checked above") + needle.len();
    }
}

/// `path` as the gate prints it in the request.
pub fn shown(path: &Path) -> String {
    path.to_string_lossy().into_owned()
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
