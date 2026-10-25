//! A gate started under a script whose caller is gone refuses to start; one whose script still has its caller runs.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::support::{Case, FAKE_TOOL, GATE, said_pid, shown, start, wait_for_file};

/// The case's `step.cmd` and the files it waits on and writes.
struct Step {
    /// The script: it waits for the go file, then runs the gate, writing the gate's status and stderr beside it.
    script: PathBuf,
    /// The file whose coming lets the script run its gate.
    go: PathBuf,
    /// The gate's stderr.
    stderr: PathBuf,
    /// The gate's status, written whole once the gate has ended.
    status: PathBuf,
}

impl Step {
    /// Writes `case`'s `step.cmd`: it waits for the go file with `fake-tool wait`, runs this suite's gate on a trivial
    /// command with an absolute log under the case and the case's state root, and writes `%ERRORLEVEL%` to a file moved
    /// over the status file, so the case never reads it torn.
    fn write(case: &Case) -> Step {
        let folder = case.folder();
        let step = Step {
            script: folder.join("step.cmd"),
            go: folder.join("go"),
            stderr: folder.join("gate.err"),
            status: folder.join("status"),
        };
        let written = folder.join("status.tmp");
        let body = format!(
            "@echo off\r\n\
             \"{fake}\" wait \"{go}\"\r\n\
             \"{gate}\" run --state-dir \"{root}\" --slot light --timeout 3600 --log \"{log}\" --test-poll-ms 20 -- \"{fake}\" out hi \
             > \"{out}\" 2> \"{err}\"\r\n\
             >\"{written}\" echo %ERRORLEVEL%\r\n\
             move /y \"{written}\" \"{status}\" > nul\r\n",
            fake = FAKE_TOOL,
            gate = GATE,
            go = shown(&step.go),
            root = shown(&case.root()),
            log = shown(&folder.join("step.log")),
            out = shown(&folder.join("gate.out")),
            err = shown(&step.stderr),
            written = shown(&written),
            status = shown(&step.status),
        );
        fs::write(&step.script, body).expect("the step script can be written");
        step
    }

    /// Lets the script run its gate, waits for the gate's status under the hang guard, and returns it with the gate's
    /// stderr.
    fn go_and_wait(&self) -> (String, String) {
        fs::write(&self.go, "").expect("the go file can be written");
        wait_for_file(&self.status);
        let status = fs::read_to_string(&self.status).expect("the status file can be read");
        let stderr = fs::read_to_string(&self.stderr).expect("the gate's stderr can be read");
        (status.trim().to_owned(), stderr)
    }
}

#[test]
fn a_gate_whose_script_lost_its_caller_refuses_to_start() {
    let case = Case::new("a_gate_whose_script_lost_its_caller_refuses_to_start");
    let step = Step::write(&case);
    // The helper's output goes to a file, not a pipe: the script inherits the helper's handles, so a pipe would stay
    // open, and a reader waiting on it, until the script ends.
    let said = case.folder().join("helper.out");
    let output = fs::File::create(&said).expect("the helper's output file can be created");
    let mut helper = Command::new(FAKE_TOOL);
    case.prepare_without_state_dir(&mut helper, &["orphan-run", "cmd.exe", "/c", &shown(&step.script)]);
    helper
        .stdin(Stdio::null())
        .stdout(output.try_clone().expect("the helper's output file can be shared"))
        .stderr(output);
    let mut helper = helper.spawn().expect("the helper starts");
    let helper_pid = helper.id();
    let started = helper.wait().expect("the helper can be waited for");
    let started_said = fs::read_to_string(&said).expect("the helper's output can be read");
    assert!(started.success(), "the helper starts the script and exits: {started}, {started_said}");
    let cmd_pid = said_pid(&started_said, "spawned ");

    let (status, stderr) = step.go_and_wait();

    assert_eq!(status, "2", "the gate refuses with 2: {stderr}");
    assert_eq!(
        stderr.trim_end(),
        format!(
            "gate: refused: the script that started this gate lost its caller (cmd.exe {cmd_pid}, whose parent {helper_pid} is gone); \
             stop the sequence with gate.ps1 stop-tree {cmd_pid}"
        )
    );
}

#[test]
fn a_gate_whose_script_has_its_caller_runs() {
    let case = Case::new("a_gate_whose_script_has_its_caller_runs");
    let step = Step::write(&case);
    let mut script = Command::new("cmd.exe");
    case.prepare_without_state_dir(&mut script, &["/c", &shown(&step.script)]);
    let running = start(script);

    let (status, stderr) = step.go_and_wait();
    let ran = running.finish();

    assert_eq!(status, "0", "the gate runs its command: {stderr}");
    assert!(!stderr.contains("refused"), "the gate refuses nothing: {stderr}");
    assert_eq!(ran.code, Some(0), "{ran:?}");
}
