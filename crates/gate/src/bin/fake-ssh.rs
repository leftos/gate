//! The black-box suite's stand-in for ssh, which `gate run --test-remote-ssh` starts with ssh's own words: the options,
//! `--`, the host and the line, whose last three words are `box <verb> <payload>`. It reads the steps file the variable
//! `GATE_FAKE_SSH_STEPS` names, one step a line, appends the verb it was called with to `<steps file>.log`, and acts on
//! the step of its call, counted by that log: the first call takes the first step.
//!
//! `ok` runs `<gate> box <verb> <payload>` with the gate the variable `GATE_FAKE_SSH_GATE` names, its standard streams
//! this process's, and exits with its status; `hold <path>` waits until the path exists, then does as `ok`;
//! `version <v>` does as `ok` with the gate's standard output read, its `gate: remote version` line said as
//! `gate: remote version <v>`; `fail-banner` writes ssh's `Connection timed out during banner exchange` on standard
//! error and exits 255; `fail-other` writes another first line there and exits 255; `out-then-drop` writes a line on
//! standard output and exits 255; `exit <n>` runs nothing and exits `<n>`, as cmd's 9009 for a program it cannot find.

#![expect(clippy::disallowed_methods, reason = "a test helper standing in for ssh, which holds no launch's handles")]

use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// The status ssh exits with when it fails itself.
const SSH_FAILED: i32 = 255;
/// How often `hold` looks for its path, and how long before it gives up: the suite's hang guard.
const HOLD_STEP: Duration = Duration::from_millis(20);
const HOLD_LIMIT: Duration = Duration::from_secs(600);
const VERSION: &str = "gate: remote version ";

fn main() {
    let status = match run() {
        Ok(status) => status,
        Err(message) => {
            // A helper that cannot write its own error has no other stream to say so on; its status still fails.
            let _ = writeln!(io::stderr().lock(), "fake-ssh: {message}");
            2
        }
    };
    std::process::exit(status);
}

fn run() -> Result<i32, String> {
    let line = std::env::args().next_back().ok_or("no line to run")?;
    let words: Vec<&str> = line.split(' ').collect();
    let [.., first, verb, payload] = words.as_slice() else {
        return Err(format!("the line holds no box call: {line}"));
    };
    if *first != "box" {
        return Err(format!("the line holds no box call: {line}"));
    }
    let steps = PathBuf::from(variable("GATE_FAKE_SSH_STEPS")?);
    let step = next_step(&steps, verb)?;
    let call = BoxCall { verb, payload };
    match step.split_once(' ') {
        None => bare_step(&step, &call),
        Some(("hold", path)) => {
            hold(Path::new(path))?;
            call.ok()
        }
        Some(("version", version)) => call.with_version(version),
        Some(("exit", status)) => status.parse().map_err(|_| format!("the step '{step}' names no status")),
        Some(_) => Err(format!("unknown step '{step}'")),
    }
}

fn bare_step(step: &str, call: &BoxCall<'_>) -> Result<i32, String> {
    match step {
        "ok" => call.ok(),
        "fail-banner" => said(&mut io::stderr().lock(), "Connection timed out during banner exchange"),
        "fail-other" => said(&mut io::stderr().lock(), "ssh: connect to host builder port 22: Connection refused"),
        "out-then-drop" => said(&mut io::stdout().lock(), "fake-ssh: a line before the connection dropped"),
        other => Err(format!("unknown step '{other}'")),
    }
}

/// Writes `line` on `stream` and gives ssh's own failure status.
fn said(stream: &mut impl Write, line: &str) -> Result<i32, String> {
    writeln!(stream, "{line}").map_err(|error| format!("cannot write: {error}"))?;
    Ok(SSH_FAILED)
}

fn variable(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|error| format!("{name}: {error}"))
}

/// The step of this call: the one after as many steps as the log holds calls, the call's `verb` appended to the log.
fn next_step(steps: &Path, verb: &str) -> Result<String, String> {
    let text = fs::read_to_string(steps).map_err(|error| format!("cannot read {}: {error}", steps.display()))?;
    let mut log = steps.as_os_str().to_owned();
    log.push(".log");
    let log = PathBuf::from(log);
    let made = fs::read_to_string(&log).map_or(0, |calls| calls.lines().count());
    OpenOptions::new()
        .append(true)
        .create(true)
        .open(&log)
        .and_then(|mut file| writeln!(file, "{verb}"))
        .map_err(|error| format!("cannot write {}: {error}", log.display()))?;
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .nth(made)
        .map(|step| step.trim().to_owned())
        .ok_or_else(|| format!("no step left for call {} ({verb})", made + 1))
}

fn hold(path: &Path) -> Result<(), String> {
    let deadline = Instant::now() + HOLD_LIMIT;
    while !path.exists() {
        if Instant::now() >= deadline {
            return Err(format!("{} did not appear", path.display()));
        }
        thread::sleep(HOLD_STEP);
    }
    Ok(())
}

/// The `gate box` call the line carries.
struct BoxCall<'a> {
    verb: &'a str,
    payload: &'a str,
}

impl BoxCall<'_> {
    fn command(&self) -> Result<Command, String> {
        let mut command = Command::new(variable("GATE_FAKE_SSH_GATE")?);
        command.args(["box", self.verb, self.payload]);
        Ok(command)
    }

    /// Runs the call with this process's streams; its status.
    fn ok(&self) -> Result<i32, String> {
        let status = self.command()?.status().map_err(|error| format!("cannot run the gate: {error}"))?;
        status.code().ok_or_else(|| "the gate ended with no status".to_owned())
    }

    /// Runs the call with its standard output read, its version line said with `version`; its status.
    fn with_version(&self, version: &str) -> Result<i32, String> {
        let mut child = self
            .command()?
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|error| format!("cannot run the gate: {error}"))?;
        let stdout = child.stdout.take().ok_or("the gate has no output pipe")?;
        let mut out = io::stdout().lock();
        for line in BufReader::new(stdout).lines() {
            let line = line.map_err(|error| format!("cannot read the gate: {error}"))?;
            let said = if line.starts_with(VERSION) {
                format!("{VERSION}{version}")
            } else {
                line
            };
            writeln!(out, "{said}").map_err(|error| format!("cannot write: {error}"))?;
        }
        let status = child.wait().map_err(|error| format!("cannot wait for the gate: {error}"))?;
        status.code().ok_or_else(|| "the gate ended with no status".to_owned())
    }
}
