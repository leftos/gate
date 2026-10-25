//! A gate started under a script whose caller is gone refuses to start; one whose script still has its caller runs.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

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

/// Git's own bash, `usr\bin\bash.exe` under the Git root of the `git.exe` the suite's git cases find on PATH: the
/// folder above `cmd\git.exe`, `bin\git.exe` or `mingw64\bin\git.exe`.
fn git_bash() -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH is set");
    let git = std::env::split_paths(&path)
        .map(|folder| folder.join("git.exe"))
        .find(|git| git.is_file());
    let git = git.expect("git.exe is on PATH, as the suite's git cases need");
    let folder = git.parent().expect("git.exe lies in a folder");
    let named = |folder: &Path, name: &str| folder.file_name().is_some_and(|own| own.eq_ignore_ascii_case(name));
    let above = folder.parent();
    let root = if named(folder, "cmd") {
        above
    } else if named(folder, "bin") && above.is_some_and(|above| named(above, "mingw64")) {
        above.and_then(Path::parent)
    } else if named(folder, "bin") {
        above
    } else {
        None
    };
    assert!(
        root.is_some(),
        "{} lies in no cmd, bin or mingw64\\bin folder of a Git root",
        git.display()
    );
    let root = root.expect("checked above");
    let bash = root.join("usr").join("bin").join("bash.exe");
    assert!(bash.is_file(), "Git's bash {} is missing beside {}", bash.display(), git.display());
    bash
}

/// `path` with forward slashes, as a bash script names a Windows path.
fn posix(path: &Path) -> String {
    shown(path).replace('\\', "/")
}

/// How a `step.sh` reaches its gate.
#[derive(Clone, Copy)]
enum Reach {
    /// It runs the gate itself.
    Straight,
    /// It runs the gate from `pwsh -Command`, as the gate's launcher does, its paths given unconverted.
    Pwsh,
}

/// The case's `step.sh` for Git Bash and the files it waits on and writes.
struct BashStep {
    /// The script.
    script: PathBuf,
    /// The file whose coming lets a waiting script run its gate.
    go: PathBuf,
    /// The file a waiting script writes its own Windows pid to before it waits.
    ready: PathBuf,
    /// The gate's status, written whole once the gate has ended.
    status: PathBuf,
    /// The outer bash's stdout, which the script and its gate inherit.
    stdout: PathBuf,
    /// The outer bash's stderr, which the script and its gate inherit.
    stderr: PathBuf,
    /// The gate's log.
    log: PathBuf,
}

impl BashStep {
    /// Writes `case`'s `step.sh`: `/usr/bin` put on its PATH; when `waits`, its Windows pid written whole to the ready
    /// file and the go file waited for with `fake-tool wait`; then this suite's gate run on a trivial command with an
    /// absolute log under the case and the case's state root, reached as `reach` says, and `$?` written to a file moved
    /// over the status file, so the case never reads it torn.
    fn write(case: &Case, waits: bool, reach: Reach) -> BashStep {
        let folder = case.folder();
        let step = BashStep {
            script: folder.join("step.sh"),
            go: folder.join("go"),
            ready: folder.join("ready"),
            status: folder.join("status"),
            stdout: folder.join("bash.out"),
            stderr: folder.join("bash.err"),
            log: folder.join("step.log"),
        };
        let fake = posix(Path::new(FAKE_TOOL));
        let wait = if waits {
            let ready = posix(&folder.join("ready.tmp"));
            format!(
                "cat /proc/$$/winpid > '{ready}'\nmv '{ready}' '{whole}'\n'{fake}' wait '{go}'\n",
                whole = posix(&step.ready),
                go = posix(&step.go),
            )
        } else {
            String::new()
        };
        let run = match reach {
            Reach::Straight => format!(
                "'{gate}' run --state-dir '{root}' --slot light --timeout 3600 --log '{log}' --test-poll-ms 20 -- '{fake}' out hi\n",
                gate = posix(Path::new(GATE)),
                root = posix(&case.root()),
                log = posix(&step.log),
            ),
            Reach::Pwsh => format!(
                "MSYS_NO_PATHCONV=1 pwsh -NoProfile -Command \"& '{gate}' run --state-dir '{root}' --slot light --timeout 3600 \
                 --log '{log}' --test-poll-ms 20 -- '{fake}' out hi; exit \\$LASTEXITCODE\"\n",
                gate = GATE,
                root = shown(&case.root()),
                log = shown(&step.log),
                fake = FAKE_TOOL,
            ),
        };
        let written = posix(&folder.join("status.tmp"));
        let body = format!(
            "PATH=/usr/bin:$PATH\n\
             {wait}\
             {run}\
             echo $? > '{written}'\n\
             mv '{written}' '{status}'\n",
            status = posix(&step.status),
        );
        fs::write(&step.script, body).expect("the step script can be written");
        step
    }

    /// Starts Git's bash running the script as `bash -c "/usr/bin/bash step.sh; true"`, so the outer bash forks the
    /// script's bash rather than becoming it, its stdout and stderr sent to files.
    fn spawn(&self, case: &Case) -> Child {
        let mut bash = Command::new(git_bash());
        let line = format!("/usr/bin/bash '{}'; true", posix(&self.script));
        case.prepare_without_state_dir(&mut bash, &["-c", &line]);
        bash.stdin(Stdio::null())
            .stdout(fs::File::create(&self.stdout).expect("the outer bash's stdout file can be created"))
            .stderr(fs::File::create(&self.stderr).expect("the outer bash's stderr file can be created"));
        bash.spawn().expect("Git's bash starts")
    }

    /// The gate's status the script wrote, waited for under the hang guard, with the outer bash's stderr and the gate's
    /// log, when it wrote one.
    fn status(&self) -> (String, String) {
        wait_for_file(&self.status);
        let status = fs::read_to_string(&self.status).expect("the status file can be read");
        let stderr = fs::read_to_string(&self.stderr).expect("the outer bash's stderr can be read");
        let log = fs::read_to_string(&self.log).unwrap_or_default();
        (status.trim().to_owned(), format!("{stderr}{log}"))
    }
}

#[test]
fn a_gate_under_a_git_bash_script_whose_shell_lives_runs() {
    let case = Case::new("a_gate_under_a_git_bash_script_whose_shell_lives_runs");
    let step = BashStep::write(&case, false, Reach::Straight);
    let mut bash = step.spawn(&case);

    let (status, said) = step.status();
    let ended = bash.wait().expect("the outer bash can be waited for");

    assert_eq!(status, "0", "the gate runs its command under a live Git Bash script ({ended}): {said}");
    assert!(!said.contains("refused"), "the gate refuses nothing: {said}");
}

#[test]
fn a_gate_under_a_git_bash_script_through_pwsh_whose_shell_lives_runs() {
    let case = Case::new("a_gate_under_a_git_bash_script_through_pwsh_whose_shell_lives_runs");
    let step = BashStep::write(&case, false, Reach::Pwsh);
    let mut bash = step.spawn(&case);

    let (status, said) = step.status();
    let ended = bash.wait().expect("the outer bash can be waited for");

    assert_eq!(
        status, "0",
        "the gate runs its command from pwsh under a live Git Bash script ({ended}): {said}"
    );
    assert!(!said.contains("refused"), "the gate refuses nothing: {said}");
}

#[test]
fn a_gate_whose_git_bash_script_lost_its_shell_refuses_to_start() {
    let case = Case::new("a_gate_whose_git_bash_script_lost_its_shell_refuses_to_start");
    let step = BashStep::write(&case, true, Reach::Straight);
    let mut bash = step.spawn(&case);
    wait_for_file(&step.ready);
    let script_pid = fs::read_to_string(&step.ready).expect("the ready file can be read");
    let script_pid = script_pid.trim().to_owned();
    bash.kill().expect("the outer bash can be killed");
    bash.wait().expect("the killed outer bash can be waited for");
    fs::write(&step.go, "").expect("the go file can be written");

    let (status, said) = step.status();

    assert_eq!(status, "2", "the gate refuses under a script whose shell was killed: {said}");
    let refused = format!("gate: refused: the script that started this gate lost its caller (bash.exe {script_pid}, whose parent ");
    assert!(said.contains(&refused), "the refusal names the script's bash {script_pid}: {said}");
    assert!(
        said.contains(&format!("stop the sequence with gate.ps1 stop-tree {script_pid}")),
        "the refusal says to stop the script's bash {script_pid}: {said}"
    );
}
