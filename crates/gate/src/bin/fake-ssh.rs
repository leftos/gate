//! The black-box suite's stand-in for ssh, which `gate run --test-remote-ssh` starts with ssh's own words: the options,
//! `--`, the host and the line, whose last three words are `box <verb> <payload>`. git's own ssh starts it too, through
//! the `local-ssh:` transport, with the line `git-upload-pack '<path>'` or `git-receive-pack '<path>'`, whose verb is
//! the service's name. `gate push` starts it with a line that starts with `pwsh ` and holds `-EncodedCommand <script>`,
//! a push call, whose verb is `push`. It reads the steps file the variable `GATE_FAKE_SSH_STEPS` names, one step a line,
//! appends the verb it was called with to `<steps file>.log`, and acts on the step of its call, counted by that log: the
//! first call takes the first step. A line that holds none of these, as git's `-G` probe of which ssh it runs, fails
//! with no step taken. Started with `--version` alone, as a push case starts a copy of it standing in for a builder's
//! old gate, it says `gate 0.0.0-old` and exits 0.
//!
//! `ok` runs `<gate> box <verb> <payload>` with the gate the variable `GATE_FAKE_SSH_GATE` names, or `git
//! upload-pack <path>` or `git receive-pack <path>`, or a push call's words with this PC's `pwsh`, its `LOCALAPPDATA`
//! the folder named as the host, the word before the line, in the folder the variable `GATE_FAKE_SSH_BUILDERS` names,
//! made when missing; its standard streams are this process's, and it exits with its status;
//! `hold <path>` waits until the path exists, then does as `ok`; `version <v>` does as `ok` with the gate's standard
//! output read, its `gate: remote version` line said as `gate: remote version <v>`; `boot <time>` does the same with
//! its `gate: remote boot` line, said as `gate: remote boot <time>`, and `no-boot` with that line dropped;
//! `drop` exits 255 with nothing written, as a connection lost after the banner; `fail-banner` writes ssh's
//! `Connection timed out during banner exchange` on standard error and exits 255; `fail-other` writes another first line
//! there and exits 255; `out-then-drop` writes a line on standard output and exits 255; `exit <n>` runs nothing and
//! exits `<n>`, as cmd's 9009 for a program it cannot find; `say-exit <n> <line>` runs nothing, writes the line on
//! standard error and exits `<n>`, as the box says its own refusal; `flood <n>` runs nothing, writes `<n>` bytes of `x`
//! and no line end on standard error, which git passes through as its own, and exits 0; `say-hold <word> <path>` writes
//! the word as a line on standard output, then does as `hold <path>`; `progress-ok <n>` writes at least `<n>` bytes of
//! short progress updates, each ended by a carriage return, and a line end on standard error, then does as `ok`;
//! `ok-then-out <line>` does as `ok`, then writes the line on standard output, after all the call wrote.

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
const BOOT: &str = "gate: remote boot ";
/// The git services git's own ssh runs, each with the repository's path in single quotes after its name.
const GIT_SERVICES: [&str; 2] = ["git-upload-pack", "git-receive-pack"];
/// The verb of a push call, which `gate push` makes.
const PUSH: &str = "push";
/// The version this program says to `--version`, standing in for a builder's old gate.
const OLD_VERSION: &str = "0.0.0-old";

/// Why `command` could not be started, naming its program.
fn cannot_run(command: &Command, error: &io::Error) -> String {
    format!("cannot run {}: {error}", command.get_program().to_string_lossy())
}

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
    let words: Vec<String> = std::env::args().skip(1).collect();
    if words == ["--version"] {
        writeln!(io::stdout().lock(), "gate {OLD_VERSION}").map_err(|error| format!("cannot write: {error}"))?;
        return Ok(0);
    }
    let (host, line) = match &words[..] {
        [] => return Err("no line to run".to_owned()),
        [line] => ("", line.as_str()),
        [.., host, line] => (host.as_str(), line.as_str()),
    };
    let call = BoxCall::of(line, host)?;
    let steps = PathBuf::from(variable("GATE_FAKE_SSH_STEPS")?);
    let step = next_step(&steps, call.verb)?;
    match step.split_once(' ') {
        None => bare_step(&step, &call),
        Some(("hold", path)) => {
            hold(Path::new(path))?;
            call.ok()
        }
        Some(("version", version)) => call.rewritten(VERSION, Some(version)),
        Some(("boot", time)) => call.rewritten(BOOT, Some(time)),
        Some(("exit", status)) => status.parse().map_err(|_| format!("the step '{step}' names no status")),
        Some(("say-exit", rest)) => say_exit(&step, rest),
        Some(("flood", bytes)) => flood(&step, bytes),
        Some(("progress-ok", bytes)) => {
            progress(&step, bytes)?;
            call.ok()
        }
        Some(("ok-then-out", line)) => {
            let status = call.ok()?;
            writeln!(io::stdout().lock(), "{line}").map_err(|error| format!("cannot write: {error}"))?;
            Ok(status)
        }
        Some(("say-hold", rest)) => {
            let (word, path) = rest.split_once(' ').ok_or_else(|| format!("the step '{step}' names no path"))?;
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "{word}")
                .and_then(|()| stdout.flush())
                .map_err(|error| format!("cannot write: {error}"))?;
            hold(Path::new(path))?;
            call.ok()
        }
        Some(_) => Err(format!("unknown step '{step}'")),
    }
}

/// `flood <n>`, given as `bytes` after the step's name: writes that many bytes of `x` and no line end on standard error,
/// and gives 0.
fn flood(step: &str, bytes: &str) -> Result<i32, String> {
    let count: usize = bytes.parse().map_err(|_| format!("the step '{step}' names no byte count"))?;
    let mut stderr = io::stderr().lock();
    stderr
        .write_all(&vec![b'x'; count])
        .and_then(|()| stderr.flush())
        .map_err(|error| format!("cannot write: {error}"))?;
    Ok(0)
}

/// `progress-ok <n>`, given as `bytes` after the step's name: writes short progress updates, each ended by a carriage
/// return, until they hold at least that many bytes, then a line end, all on standard error.
fn progress(step: &str, bytes: &str) -> Result<(), String> {
    let count: usize = bytes.parse().map_err(|_| format!("the step '{step}' names no byte count"))?;
    let mut updates = String::new();
    let mut number = 0;
    while updates.len() < count {
        updates = updates + "Writing objects: " + &(number % 100).to_string() + "%\r";
        number += 1;
    }
    updates.push_str("Writing objects: 100%, done.\n");
    let mut stderr = io::stderr().lock();
    stderr
        .write_all(updates.as_bytes())
        .and_then(|()| stderr.flush())
        .map_err(|error| format!("cannot write: {error}"))
}

/// `say-exit <n> <line>`, given as `rest` after the step's name: writes the line on standard error and gives `<n>`.
fn say_exit(step: &str, rest: &str) -> Result<i32, String> {
    let (status, line) = rest.split_once(' ').ok_or_else(|| format!("the step '{step}' names no line"))?;
    let status = status.parse().map_err(|_| format!("the step '{step}' names no status"))?;
    writeln!(io::stderr().lock(), "{line}").map_err(|error| format!("cannot write: {error}"))?;
    Ok(status)
}

fn bare_step(step: &str, call: &BoxCall<'_>) -> Result<i32, String> {
    match step {
        "ok" => call.ok(),
        "no-boot" => call.rewritten(BOOT, None),
        "drop" => Ok(SSH_FAILED),
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

/// The call the line carries: `gate box`'s verb and payload, a git service's name and the repository's path, or
/// [`PUSH`] and the push call's whole line; and the host it was made to.
struct BoxCall<'a> {
    verb: &'a str,
    payload: &'a str,
    host: &'a str,
}

impl<'a> BoxCall<'a> {
    /// The call `line`, made to `host`, carries: a git service and its path, unquoted, a push call, or the last three
    /// words, `box <verb> <payload>`.
    fn of(line: &'a str, host: &'a str) -> Result<BoxCall<'a>, String> {
        if let Some((verb, path)) = line.split_once(' ').filter(|(verb, _)| GIT_SERVICES.contains(verb)) {
            return Ok(BoxCall {
                verb,
                payload: path.trim_matches('\''),
                host,
            });
        }
        let words: Vec<&'a str> = line.split(' ').collect();
        let script = words.iter().skip_while(|word| **word != "-EncodedCommand").nth(1);
        if words.first() == Some(&"pwsh") && script.is_some_and(|script| !script.is_empty()) {
            return Ok(BoxCall {
                verb: PUSH,
                payload: line,
                host,
            });
        }
        match words[..] {
            [.., "box", verb, payload] => Ok(BoxCall { verb, payload, host }),
            _ => Err(format!("the line holds no box call: {line}")),
        }
    }

    fn command(&self) -> Result<Command, String> {
        if self.verb == PUSH {
            return self.push_command();
        }
        if let Some(service) = self.verb.strip_prefix("git-") {
            let mut command = Command::new("git");
            command.args([service, self.payload]);
            return Ok(command);
        }
        let mut command = Command::new(variable("GATE_FAKE_SSH_GATE")?);
        command.args(["box", self.verb, self.payload]);
        Ok(command)
    }

    /// This PC's `pwsh` with the push call's words after `pwsh`, its `LOCALAPPDATA` the host's folder in the folder
    /// `GATE_FAKE_SSH_BUILDERS` names, made when missing.
    fn push_command(&self) -> Result<Command, String> {
        if self.host.is_empty() {
            return Err("the push call names no host".to_owned());
        }
        let folder = PathBuf::from(variable("GATE_FAKE_SSH_BUILDERS")?).join(self.host);
        fs::create_dir_all(&folder).map_err(|error| format!("cannot make {}: {error}", folder.display()))?;
        let mut command = Command::new("pwsh");
        command.args(self.payload.split(' ').skip(1)).env("LOCALAPPDATA", folder);
        Ok(command)
    }

    /// Runs the call with this process's streams; its status.
    fn ok(&self) -> Result<i32, String> {
        let mut command = self.command()?;
        let status = command.status().map_err(|error| cannot_run(&command, &error))?;
        status.code().ok_or_else(|| "the gate ended with no status".to_owned())
    }

    /// Runs the call with its standard output read, each line that starts with `prefix` said as `prefix` and `value`,
    /// or dropped when there is no value; with a value and no such line, `prefix` and `value` are said once at the end,
    /// so a step does not depend on this machine's own reading of what the line says. Its status.
    fn rewritten(&self, prefix: &str, value: Option<&str>) -> Result<i32, String> {
        let mut command = self.command()?;
        let mut child = command.stdout(Stdio::piped()).spawn().map_err(|error| cannot_run(&command, &error))?;
        let stdout = child.stdout.take().ok_or("the gate has no output pipe")?;
        let mut out = io::stdout().lock();
        let mut seen = false;
        for line in BufReader::new(stdout).lines() {
            let line = line.map_err(|error| format!("cannot read the gate: {error}"))?;
            let said = match value {
                _ if !line.starts_with(prefix) => line,
                Some(value) => {
                    seen = true;
                    format!("{prefix}{value}")
                }
                None => continue,
            };
            writeln!(out, "{said}").map_err(|error| format!("cannot write: {error}"))?;
        }
        if let Some(value) = value.filter(|_| !seen) {
            writeln!(out, "{prefix}{value}").map_err(|error| format!("cannot write: {error}"))?;
        }
        let status = child.wait().map_err(|error| format!("cannot wait for the gate: {error}"))?;
        status.code().ok_or_else(|| "the gate ended with no status".to_owned())
    }
}
