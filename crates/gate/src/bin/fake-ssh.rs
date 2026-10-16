//! The black-box suite's stand-in for ssh, which `gate run --test-remote-ssh` starts with ssh's own words: the options,
//! `--`, the host and the line, whose last three words are `box <verb> --stdin`, the call's request coming on its
//! standard input, which the box it runs reads as its own. git's own ssh starts it too, through
//! the `local-ssh:` transport, with the line `git-upload-pack '<path>'` or `git-receive-pack '<path>'`, whose verb is
//! the service's name. `gate push` starts it with a line that starts with `pwsh ` and holds `-EncodedCommand <bootstrap>`,
//! a push call, whose verb is `push`. It reads the steps file the variable `GATE_FAKE_SSH_STEPS` names, one step a line,
//! appends the verb it was called with to `<steps file>.log`, and the verb and ssh's options, the words before the
//! host with a last `--` left out, as one line to `<steps file>.words`, and acts on the step of its call, counted by that log: the
//! first call takes the first step. A line that holds none of these, as git's `-G` probe of which ssh it runs, fails
//! with no step taken. Started with `--version` alone, as a push case starts a copy of it standing in for a builder's
//! old gate, it says `gate 0.0.0-old` and exits 0.
//!
//! `ok` runs `<gate> box <verb> --stdin` with the gate the variable `GATE_FAKE_SSH_GATE` names, a `run` call given
//! the words of the variable `GATE_FAKE_SSH_BOX_SEAMS`, when set, before its verb as box seams, and a `preflight` call
//! those of `GATE_FAKE_SSH_PREFLIGHT_SEAMS` the same way, or `git
//! upload-pack <path>` or `git receive-pack <path>`, or a push call's words with this PC's `pwsh`, its `LOCALAPPDATA`
//! the folder named as the host, the word before the line, in the folder the variable `GATE_FAKE_SSH_BUILDERS` names,
//! made when missing, fed this process's standard input, which it reads whole first and keeps as `<steps file>.input`;
//! its standard streams are otherwise this process's, and it exits with its status;
//! `hold <path>` waits until the path exists, then does as `ok`; `fed-hold <path>` reads its standard input whole and
//! keeps it as `<steps file>.input`, then waits until the path exists, then runs the call as `ok` does, fed what it
//! read, so the call's input has all been sent before the call does anything; `version <v>` does as `ok` with the gate's standard
//! output read, its `gate: remote version` line said as `gate: remote version <v>`; `boot <time>` does the same with
//! its `gate: remote boot` line, said as `gate: remote boot <time>`, and `no-boot` with that line dropped;
//! `drop` exits 255 with nothing written, as a connection lost after the banner; `fail-banner` writes ssh's
//! `Connection timed out during banner exchange` on standard error and exits 255; `fail-other` writes another first line
//! there and exits 255; `out-then-drop` writes a line on standard output and exits 255; `exit <n>` runs nothing and
//! exits `<n>`, as cmd's 9009 for a program it cannot find; `say-exit <n> <line>` runs nothing, writes the line on
//! standard error and exits `<n>`, as a call that fails with a line of its own does; `flood <n>` runs nothing, writes `<n>` bytes of `x`
//! and no line end on standard error, which git passes through as its own, and exits 0; `lines <count> <length>` writes
//! that many numbered lines of that many bytes, by turns on standard output and standard error, then does as `ok`;
//! `say-hold <word> <path>` writes
//! the word as a line on standard output, then does as `hold <path>`; `progress-ok <n>` writes at least `<n>` bytes of
//! short progress updates, each ended by a carriage return, and a line end on standard error, then does as `ok`;
//! `ok-then-out <line>` does as `ok`, then writes the line on standard output, after all the call wrote;
//! `truncate <n>` does as `ok` with the call's standard output read, only its first `<n>` bytes passed on raw and the
//! rest read and dropped, then writes ssh's `Connection timed out during banner exchange` on standard error and exits
//! 255, so only the bytes it sent tell the drop from one before the banner; `garbage <n>` runs nothing, writes `<n>`
//! bytes of `x`, no outputs archive, raw on standard output and exits 0; `flood-ok <n>` writes `<n>` bytes of `x` and
//! no line end on standard output, then does as `ok`, whose output goes on that one line; `bytes-hold <n> <path>` writes `<n>` bytes of
//! `x` raw on standard output, then does as `hold <path>`.

#![expect(clippy::disallowed_methods, reason = "a test helper standing in for ssh, which holds no launch's handles")]

use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// The status ssh exits with when it fails itself.
const SSH_FAILED: i32 = 255;
/// ssh's line for a connection that ended before the banner, which the client retries on.
const PRE_BANNER: &str = "Connection timed out during banner exchange";
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
    let steps = PathBuf::from(variable("GATE_FAKE_SSH_STEPS")?);
    let call = BoxCall::of(line, host, &steps)?;
    let before_host = words.get(..words.len().saturating_sub(2)).unwrap_or_default();
    let options = match before_host.split_last() {
        Some((last, options)) if last == "--" => options,
        _ => before_host,
    };
    let step = next_step(&steps, call.verb, options)?;
    match step.split_once(' ') {
        None => bare_step(&step, &call),
        Some(("hold", path)) => {
            hold(Path::new(path))?;
            call.ok()
        }
        Some(("fed-hold", path)) => fed_call(call.command()?, call.steps, Some(Path::new(path))),
        Some(("version", version)) => call.rewritten(VERSION, Some(version)),
        Some(("boot", time)) => call.rewritten(BOOT, Some(time)),
        Some(("exit", status)) => status.parse().map_err(|_| format!("the step '{step}' names no status")),
        Some(("say-exit", rest)) => say_exit(&step, rest),
        Some(("flood", bytes)) => flood(&step, bytes),
        Some(("lines", rest)) => {
            lines(&step, rest)?;
            call.ok()
        }
        Some(("progress-ok", bytes)) => {
            progress(&step, bytes)?;
            call.ok()
        }
        Some(("ok-then-out", line)) => {
            let status = call.ok()?;
            writeln!(io::stdout().lock(), "{line}").map_err(|error| format!("cannot write: {error}"))?;
            Ok(status)
        }
        Some(("truncate", bytes)) => call.truncated(byte_count(&step, bytes)?),
        Some(("flood-ok", bytes)) => {
            filler(byte_count(&step, bytes)?)?;
            call.ok()
        }
        Some(("garbage", bytes)) => {
            filler(byte_count(&step, bytes)?)?;
            Ok(0)
        }
        Some(("bytes-hold", rest)) => {
            let (bytes, path) = rest.split_once(' ').ok_or_else(|| format!("the step '{step}' names no path"))?;
            filler(byte_count(&step, bytes)?)?;
            hold(Path::new(path))?;
            call.ok()
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

/// `lines <count> <length>`, given as `rest` after the step's name: writes that many lines, each that many bytes before
/// its line end, `line <n> ` and then `x` up to the length, the even-numbered ones on standard output and the odd ones
/// on standard error, so each stream carries half of them.
fn lines(step: &str, rest: &str) -> Result<(), String> {
    let (count, length) = rest.split_once(' ').ok_or_else(|| format!("the step '{step}' names no line length"))?;
    let (count, length) = (byte_count(step, count)?, byte_count(step, length)?);
    let (mut stdout, mut stderr) = (io::stdout().lock(), io::stderr().lock());
    for number in 0..count {
        let mut line = format!("line {number} ");
        line.extend(std::iter::repeat_n('x', length.saturating_sub(line.len())));
        line.push('\n');
        let stream: &mut dyn Write = if number % 2 == 0 { &mut stdout } else { &mut stderr };
        stream.write_all(line.as_bytes()).map_err(|error| format!("cannot write: {error}"))?;
    }
    stdout
        .flush()
        .and_then(|()| stderr.flush())
        .map_err(|error| format!("cannot write: {error}"))
}

/// The byte count `bytes` the step `step` names after its name.
fn byte_count(step: &str, bytes: &str) -> Result<usize, String> {
    bytes.parse().map_err(|_| format!("the step '{step}' names no byte count"))
}

/// Writes `count` bytes of `x` raw on standard output, and flushes it.
fn filler(count: usize) -> Result<(), String> {
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(&vec![b'x'; count])
        .and_then(|()| stdout.flush())
        .map_err(|error| format!("cannot write: {error}"))
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
        "fail-banner" => said(&mut io::stderr().lock(), PRE_BANNER),
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

/// The box seams a call's gate is given before its verb: the words of the variable `name`, `GATE_FAKE_SSH_BOX_SEAMS`
/// for a `run` call and `GATE_FAKE_SSH_PREFLIGHT_SEAMS` for a preflight, none when it is unset.
fn seams_of(name: &str) -> Result<String, String> {
    match std::env::var(name) {
        Err(std::env::VarError::NotPresent) => Ok(String::new()),
        read => read.map_err(|error| format!("{name}: {error}")),
    }
}

/// The step of this call: the one after as many steps as the log holds calls, the call's `verb` appended to the log,
/// and the verb and the call's ssh `options` appended as one line to `<steps file>.words`.
fn next_step(steps: &Path, verb: &str, options: &[String]) -> Result<String, String> {
    let text = fs::read_to_string(steps).map_err(|error| format!("cannot read {}: {error}", steps.display()))?;
    let log = beside(steps, ".log");
    let made = fs::read_to_string(&log).map_or(0, |calls| calls.lines().count());
    appended(&log, verb)?;
    let said = std::iter::once(verb)
        .chain(options.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    appended(&beside(steps, ".words"), &said)?;
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .nth(made)
        .map(|step| step.trim().to_owned())
        .ok_or_else(|| format!("no step left for call {} ({verb})", made + 1))
}

/// The path of `steps` with `suffix` after its name.
fn beside(steps: &Path, suffix: &str) -> PathBuf {
    let mut path = steps.as_os_str().to_owned();
    path.push(suffix);
    PathBuf::from(path)
}

/// Appends `line` and a line end to the file at `path`, made when missing.
fn appended(path: &Path, line: &str) -> Result<(), String> {
    OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .and_then(|mut file| writeln!(file, "{line}"))
        .map_err(|error| format!("cannot write {}: {error}", path.display()))
}

/// Runs the call `command` fed this process's standard input, read whole first and kept beside the steps file `steps`
/// as `<steps file>.input`, so a case can tell what the call sent, once the path `after` exists when one is given; its
/// status.
fn fed_call(mut command: Command, steps: &Path, after: Option<&Path>) -> Result<i32, String> {
    let mut input = Vec::new();
    io::stdin()
        .lock()
        .read_to_end(&mut input)
        .map_err(|error| format!("cannot read the call's input: {error}"))?;
    let kept = beside(steps, ".input");
    fs::write(&kept, &input).map_err(|error| format!("cannot write {}: {error}", kept.display()))?;
    if let Some(path) = after {
        hold(path)?;
    }
    let mut child = command.stdin(Stdio::piped()).spawn().map_err(|error| cannot_run(&command, &error))?;
    let mut stdin = child.stdin.take().ok_or("the call has no input pipe")?;
    stdin.write_all(&input).map_err(|error| format!("cannot feed the call: {error}"))?;
    drop(stdin);
    let status = child.wait().map_err(|error| format!("cannot wait for the call: {error}"))?;
    status.code().ok_or_else(|| "the call ended with no status".to_owned())
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

/// The call the line carries: `gate box`'s verb and the word after it, `--stdin`, a git service's name and the
/// repository's path, or [`PUSH`] and the push call's whole line, as `rest`; the host it was made to, and the steps
/// file it acts on.
struct BoxCall<'a> {
    verb: &'a str,
    rest: &'a str,
    host: &'a str,
    steps: &'a Path,
}

impl<'a> BoxCall<'a> {
    /// The call `line`, made to `host` and acting on the steps file `steps`, carries: a git service and its path,
    /// unquoted, a push call, or the last three words, `box <verb> --stdin`; a box call whose last word is not
    /// `--stdin` is refused, so a client that stops sending its request on standard input fails here by name.
    fn of(line: &'a str, host: &'a str, steps: &'a Path) -> Result<BoxCall<'a>, String> {
        if let Some((verb, path)) = line.split_once(' ').filter(|(verb, _)| GIT_SERVICES.contains(verb)) {
            return Ok(BoxCall {
                verb,
                rest: path.trim_matches('\''),
                host,
                steps,
            });
        }
        let words: Vec<&'a str> = line.split(' ').collect();
        let script = words.iter().skip_while(|word| **word != "-EncodedCommand").nth(1);
        if words.first() == Some(&"pwsh") && script.is_some_and(|script| !script.is_empty()) {
            return Ok(BoxCall {
                verb: PUSH,
                rest: line,
                host,
                steps,
            });
        }
        match words[..] {
            [.., "box", verb, rest @ "--stdin"] => Ok(BoxCall { verb, rest, host, steps }),
            [.., "box", _, _] | [.., "box", _] => Err(format!("box call without --stdin: {line}")),
            _ => Err(format!("the line holds no box call: {line}")),
        }
    }

    fn command(&self) -> Result<Command, String> {
        if self.verb == PUSH {
            return self.push_command();
        }
        if let Some(service) = self.verb.strip_prefix("git-") {
            let mut command = Command::new("git");
            command.args([service, self.rest]);
            return Ok(command);
        }
        let mut command = Command::new(variable("GATE_FAKE_SSH_GATE")?);
        command.arg("box");
        let seams = match self.verb {
            "run" => Some("GATE_FAKE_SSH_BOX_SEAMS"),
            "preflight" => Some("GATE_FAKE_SSH_PREFLIGHT_SEAMS"),
            _ => None,
        };
        if let Some(name) = seams {
            command.args(seams_of(name)?.split_whitespace());
        }
        command.args([self.verb, self.rest]);
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
        command.args(self.rest.split(' ').skip(1)).env("LOCALAPPDATA", folder);
        Ok(command)
    }

    /// Runs the call with this process's streams; its status. A push call is fed its standard input as [`fed_push`]
    /// feeds it.
    fn ok(&self) -> Result<i32, String> {
        let mut command = self.command()?;
        if self.verb == PUSH {
            return fed_call(command, self.steps, None);
        }
        let status = command.status().map_err(|error| cannot_run(&command, &error))?;
        status.code().ok_or_else(|| "the gate ended with no status".to_owned())
    }

    /// Runs the call with its standard output read, only its first `count` bytes passed on raw and the rest read and
    /// dropped, so the call is never stopped by a full pipe, then writes ssh's pre-banner line on standard error; ssh's
    /// own failure status, whatever the call's.
    fn truncated(&self, count: usize) -> Result<i32, String> {
        let mut command = self.command()?;
        let mut child = command.stdout(Stdio::piped()).spawn().map_err(|error| cannot_run(&command, &error))?;
        let mut stdout = child.stdout.take().ok_or("the gate has no output pipe")?;
        let limit = u64::try_from(count).map_err(|error| format!("cannot count {count} bytes: {error}"))?;
        let mut out = io::stdout().lock();
        io::copy(&mut (&mut stdout).take(limit), &mut out)
            .and_then(|_| out.flush())
            .map_err(|error| format!("cannot pass the gate's output on: {error}"))?;
        io::copy(&mut stdout, &mut io::sink()).map_err(|error| format!("cannot read the gate: {error}"))?;
        child.wait().map_err(|error| format!("cannot wait for the gate: {error}"))?;
        said(&mut io::stderr().lock(), PRE_BANNER)
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
