//! The harness: a [`Case`] with its own state root, the programs under test, and order-only assertions.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
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
pub fn finish(mut command: Command) -> Outcome {
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().expect("the program starts");
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let stdout = thread::spawn(move || read_all(stdout));
    let stderr = thread::spawn(move || read_all(stderr));
    let deadline = Instant::now() + HANG_GUARD;
    let status = loop {
        if let Some(status) = child.try_wait().expect("the program's status can be read") {
            break Some(status);
        }
        if Instant::now() >= deadline {
            child.kill().expect("a stuck program can be killed");
            child.wait().expect("a killed program can be waited for");
            break None;
        }
        thread::sleep(Duration::from_millis(10));
    };
    Outcome {
        code: status.and_then(|status| status.code()),
        stdout: stdout.join().expect("the stdout reader ends"),
        stderr: stderr.join().expect("the stderr reader ends"),
    }
}

fn read_all(mut stream: impl Read) -> String {
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).expect("the program's output can be read");
    String::from_utf8_lossy(&bytes).into_owned()
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
