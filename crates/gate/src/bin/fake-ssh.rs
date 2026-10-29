//! The black-box suite's stand-in for ssh, which `gate run --test-remote-ssh` starts with ssh's own words: the options,
//! `--`, the host and the line, whose last three words are `box <verb> --stdin`, the call's request coming on its
//! standard input, which the box it runs reads as its own. git's own ssh starts it too, through
//! the `local-ssh:` transport, with the line `git-upload-pack '<path>'` or `git-receive-pack '<path>'`, whose verb is
//! the service's name. `gate push` starts it with a line that starts with `pwsh ` and holds `-EncodedCommand <bootstrap>`,
//! a push call, whose verb is `push`. It reads the steps file the variable `GATE_FAKE_SSH_STEPS` names, one step a line,
//! appends the verb it was called with to `<steps file>.log`, and the verb and ssh's options, the words before the
//! host with a last `--` left out, as one line to `<steps file>.words`, and the host and the line, a space between, as
//! one line to `<steps file>.lines`, and acts on the step of its call: the first call takes the first step, a file
//! without an `on {alias}` line counted by that log as a whole. An `on {alias}` line starts a block of steps that answer
//! only calls to that host, numbered by that host's own lines of `<steps file>.lines`; the steps before the first `on`
//! line answer the hosts with no block, numbered the same way; an alias has one block at most. A line that holds none of
//! these, as git's `-G` probe of which ssh it runs, fails with no step taken. Started with `--version` alone, as a push
//! case starts a copy of it standing in for a builder's old gate, it says `gate 0.0.0-old`, or `gate <v>` when the
//! variable `GATE_FAKE_SSH_VERSION` holds `<v>`, and exits 0;
//! started with `box --test-box-register ok register-desktop`, as a builder registers the desktop task with such a copy
//! installed, it says the verb's `gate-register: registered` line and exits 0.
//!
//! `gate push` starts it for a macOS builder with a line that starts with `/bin/sh -s -- `, the words after it the
//! script's: its verb is `mac-prepare` or `mac-build`, after the script's first word. Such a call runs Git for Windows'
//! `sh` (found beside a `git.exe` on `PATH`) with those words, fed the script it reads whole from its standard input and
//! keeps as `<steps file>.input`, with `/usr/bin/lockf`, `/usr/bin/stat` and `/bin/mv` in it made the paths of
//! stand-ins in the builder's `system` folder, which is on no `PATH`, so a call by a bare name reaches Git for Windows'
//! own GNU tool, which does not take macOS's words. The builder's root, `/` and the
//! variable `GATE_FAKE_SSH_UNIX_ROOT`'s value as a word of the line, is made `<builder>/root` in the builder's folder, the
//! folder named as the host in `GATE_FAKE_SSH_BUILDERS`, made when missing, its `root` left for the script to make; a git
//! service's path under that root is made the same folder's. The stand-ins are written afresh on every call: in
//! `system`, `lockf`, which locks nothing; `stat`, which answers macOS's `stat -f %l` with the link count and its
//! `stat -f %Lp` with `700`, or `750` on a `root-open` call, as Git for Windows shows no folder's own mode; and `mv`,
//! which takes macOS's `mv -fh` as `mv -fT`; in `tools`, `cc`; `cargo`, which logs `GATE_BUILD_ID=<id>` and its words as
//! a line of `<builder>/cargo.args` and the `RUSTC_WRAPPER`, `RUSTC_WORKSPACE_WRAPPER` and `RUSTFLAGS` it was given, each
//! in brackets and `unset` for one it was not, and its working folder, as a line of `<builder>/cargo.env`, writes
//! `<target dir>/release/gate-box`, a script saying `gate 0.1.0+<id>`, and names it on a JSON artifact line; and
//! `login-shell`, the call's `SHELL`, which runs
//! its command with `tools` first on `PATH`. `lock-busy` runs a macOS call as `ok` does with its `lockf` answering 75,
//! busy; `cargo-fail` runs it with its `cargo` failing with 101; `cargo-bound` with its `cargo` exiting 142, the status
//! the script's bound says for a build it killed; `broken-src-git` runs it as `ok` does with the builder's
//! `gate-push/src.git/HEAD` removed first, as a push cut short after its prepare call leaves it; `no-perl` runs it with the script's check for
//! `/usr/bin/perl` looking for a path that does not exist; `no-cargo` runs it with no `cargo` stand-in and a login `PATH`
//! of `tools`, `/usr/bin` and the first folder holding a `git.exe` along `PATH` alone.
//!
//! `ok` runs `<gate> box <verb> --stdin` with the gate the variable `GATE_FAKE_SSH_GATE` names, a `run`, `lfs`,
//! `inputs`, `drain`, `outputs` or `channel-check` call given the words of the variable `GATE_FAKE_SSH_BOX_SEAMS`, when set, before its verb as box
//! seams, and a `preflight` call those of `GATE_FAKE_SSH_PREFLIGHT_SEAMS` the same way, a `preflight` or `drain` call
//! then `--test-box-root-walk-stop <folder>` when the variable `GATE_FAKE_SSH_ROOT_WALK_STOP` names the folder, one path
//! never split; a keyed prune's line, `<exe> box prune --root <root> --keys <key>...`, which has no `--stdin`, runs
//! `<gate> box` with that walk stop, `prune` and the words after it, split at each space;
//! or `git upload-pack <path>` or `git receive-pack <path>`, or a push call's words with this PC's `pwsh`, its `LOCALAPPDATA`
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
//! there and exits 255; `fail-host` writes ssh's `connect to host <host>` refusal there, naming the host it was given,
//! and exits 255; `out-then-drop` writes a line on standard output and exits 255; `exit <n>` runs nothing and
//! exits `<n>`, as cmd's 9009 for a program it cannot find; `say-exit <n> <line>` runs nothing, writes the line on
//! standard error and exits `<n>`, as a call that fails with a line of its own does; `flood <n>` runs nothing, writes `<n>` bytes of `x`
//! and no line end on standard error, which git passes through as its own, and exits 0; `lines <count> <length>` writes
//! that many numbered lines of that many bytes, by turns on standard output and standard error, then does as `ok`;
//! `say-hold <word> <path>` writes
//! the word as a line on standard output, then does as `hold <path>`; `progress-ok <n>` writes at least `<n>` bytes of
//! short progress updates, each ended by a carriage return, and a line end on standard error, then does as `ok`;
//! `ok-then-out <line>` does as `ok`, then writes the line on standard output, after all the call wrote;
//! `out-exit <n> <line>` runs nothing, writes the line on standard output and exits `<n>`, as a box that says its own
//! failure line and ends with that status does;
//! `truncate <n>` does as `ok` with the call's standard output read, only its first `<n>` bytes passed on raw and the
//! rest read and dropped, then writes ssh's `Connection timed out during banner exchange` on standard error and exits
//! 255, so only the bytes it sent tell the drop from one before the banner; `garbage <n>` runs nothing, writes `<n>`
//! bytes of `x`, no outputs archive, raw on standard output and exits 0; `bytes-exit <n> <status>` runs nothing, writes
//! `<n>` bytes of `x` raw on standard output and exits `<status>`; `flood-ok <n>` writes `<n>` bytes of `x` and
//! no line end on standard output, then does as `ok`, whose output goes on that one line; `bytes-hold <n> <path>` writes `<n>` bytes of
//! `x` raw on standard output, then does as `hold <path>`; `away` runs the call as `ok` does, but outside this
//! process's job (`CREATE_BREAKAWAY_FROM_JOB`, which the job must allow), its three standard streams pipes this process
//! relays, so a gate killed with its call's job leaves the call running with its streams closed, as sshd closes a
//! box's streams once the connection drops.

#![expect(clippy::disallowed_methods, reason = "a test helper standing in for ssh, which holds no launch's handles")]

use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::windows::process::CommandExt as _;
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
/// The variable whose value this program says to `--version` in place of [`OLD_VERSION`].
const VERSION_VARIABLE: &str = "GATE_FAKE_SSH_VERSION";
/// The words a builder registers the desktop task with through the push's seam.
const REGISTER_WORDS: [&str; 4] = ["box", "--test-box-register", "ok", "register-desktop"];
/// What starts a macOS builder's push line: `/bin/sh` reading its script from stdin.
const MAC_LINE: &str = "/bin/sh -s -- ";
/// The verbs of a macOS builder's two script calls.
const MAC_PREPARE: &str = "mac-prepare";
const MAC_BUILD: &str = "mac-build";
/// The marks the `no-perl` and `no-cargo` steps give a macOS call, among its variables.
const NO_PERL: &str = "GATE_FAKE_NO_PERL";
const NO_CARGO: &str = "GATE_FAKE_NO_CARGO";
/// The variable naming the mode bits the stand-in `stat` answers macOS's `stat -f %Lp` with; `700` when unset, as Git
/// for Windows lists every folder `drwxr-xr-x` whatever its mode.
const ROOT_MODE: &str = "GATE_FAKE_ROOT_MODE";
/// The variable naming the root the routing file gives the macOS builders.
const UNIX_ROOT_VARIABLE: &str = "GATE_FAKE_SSH_UNIX_ROOT";
/// The stand-in `lockf`: busy when `GATE_FAKE_LOCKF` is `busy`, else it takes the words the script gives and locks
/// nothing.
const LOCKF: &str = r#"#!/bin/sh
if [ "${GATE_FAKE_LOCKF-}" = busy ]; then exit 75; fi
[ "$*" = '-s -t 0 9' ] || { echo "stand-in lockf: unexpected words: $*" >&2; exit 64; }
exit 0
"#;
/// The stand-in `cc`.
const CC: &str = "#!/bin/sh\nexit 0\n";
/// The stand-in `cargo`, `@ARGS@` the file it logs its calls to and `@ENV@` the one it logs their wrappers and flags to.
const CARGO: &str = r#"#!/bin/sh
printf '%s\n' "GATE_BUILD_ID=${GATE_BUILD_ID-} $*" >>'@ARGS@'
printf 'RUSTC_WRAPPER=[%s] RUSTC_WORKSPACE_WRAPPER=[%s] RUSTFLAGS=[%s] PWD=[%s]\n' "${RUSTC_WRAPPER-unset}" \
  "${RUSTC_WORKSPACE_WRAPPER-unset}" "${RUSTFLAGS-unset}" "$(pwd)" >>'@ENV@'
if [ "${GATE_FAKE_CARGO-}" = fail ]; then echo 'error: could not compile gate-box-unix' >&2; exit 101; fi
if [ "${GATE_FAKE_CARGO-}" = bound ]; then exit 142; fi
target=''
while [ "$#" -gt 0 ]; do if [ "$1" = --target-dir ]; then target=$2; fi; shift; done
[ -n "$target" ] || { echo 'stand-in cargo: no --target-dir' >&2; exit 2; }
mkdir -p "$target/release" || exit 2
exe=$target/release/gate-box
printf '#!/bin/sh\necho "gate 0.1.0+%s"\n' "$GATE_BUILD_ID" >"$exe" || exit 2
printf '{"reason":"compiler-artifact","target":{"kind":["bin"],"name":"gate-box"},"executable":"%s","fresh":false}\n' "$exe"
printf '{"reason":"build-finished","success":true}\n'
"#;
/// The stand-in `stat`: macOS's `stat -f %l <file>`, the link count, as GNU `stat -c %h`; and `stat -f %Lp <path>`,
/// the mode bits, as [`ROOT_MODE`] names them, else `700`.
const STAT: &str = r#"#!/bin/sh
if [ "$1" = -f ] && [ "$2" = %l ]; then shift 2; exec /usr/bin/stat -c %h "$@"; fi
if [ "$1" = -f ] && [ "$2" = %Lp ]; then echo "${GATE_FAKE_ROOT_MODE:-700}"; exit 0; fi
exec /usr/bin/stat "$@"
"#;
/// The stand-in `mv`: macOS's `mv -fh`, which never moves into a folder a link names, as GNU `mv -fT`.
const MV: &str = r#"#!/bin/sh
if [ "$1" = -fh ]; then shift; exec /usr/bin/mv -fT "$@"; fi
exec /usr/bin/mv "$@"
"#;
/// The stand-in login shell, `@TOOLS@` the folder it puts first on `PATH` and `@PATH@` what follows it.
const LOGIN_SHELL: &str = r#"#!/bin/sh
PATH='@TOOLS@':@PATH@
export PATH
if [ "$1" = -l ]; then shift; fi
if [ "$1" = -c ]; then shift; fi
exec /bin/sh -c "$1"
"#;
/// `CREATE_BREAKAWAY_FROM_JOB`: the call `away` starts is in none of this process's jobs below the one that allows it.
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

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
        let version = std::env::var(VERSION_VARIABLE).unwrap_or_else(|_| OLD_VERSION.to_owned());
        writeln!(io::stdout().lock(), "gate {version}").map_err(|error| format!("cannot write: {error}"))?;
        return Ok(0);
    }
    if words == REGISTER_WORDS {
        writeln!(io::stdout().lock(), r"gate-register: registered \gate\desktop-helper for the stand-in")
            .map_err(|error| format!("cannot write: {error}"))?;
        return Ok(0);
    }
    let (host, line) = match &words[..] {
        [] => return Err("no line to run".to_owned()),
        [line] => ("", line.as_str()),
        [.., host, line] => (host.as_str(), line.as_str()),
    };
    let steps = PathBuf::from(variable("GATE_FAKE_SSH_STEPS")?);
    let call = BoxCall::of(line, host, &steps)?;
    appended(&beside(&steps, ".lines"), &format!("{host} {line}"))?;
    let before_host = words.get(..words.len().saturating_sub(2)).unwrap_or_default();
    let options = match before_host.split_last() {
        Some((last, options)) if last == "--" => options,
        _ => before_host,
    };
    let step = next_step(&steps, host, call.verb, options)?;
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
        Some(("out-exit", rest)) => out_exit(&step, rest),
        Some(("truncate", bytes)) => call.truncated(byte_count(&step, bytes)?),
        Some(("flood-ok", bytes)) => {
            filler(byte_count(&step, bytes)?)?;
            call.ok()
        }
        Some(("garbage", bytes)) => {
            filler(byte_count(&step, bytes)?)?;
            Ok(0)
        }
        Some(("bytes-exit", rest)) => {
            let (bytes, status) = rest.split_once(' ').ok_or_else(|| format!("the step '{step}' names no status"))?;
            filler(byte_count(&step, bytes)?)?;
            status.parse().map_err(|_| format!("the step '{step}' names no status"))
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

/// `out-exit <n> <line>`, given as `rest` after the step's name: writes the line on standard output and gives `<n>`.
fn out_exit(step: &str, rest: &str) -> Result<i32, String> {
    let (status, line) = rest.split_once(' ').ok_or_else(|| format!("the step '{step}' names no line"))?;
    let status = status.parse().map_err(|_| format!("the step '{step}' names no status"))?;
    writeln!(io::stdout().lock(), "{line}").map_err(|error| format!("cannot write: {error}"))?;
    Ok(status)
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
        "away" => call.away(),
        "no-boot" => call.rewritten(BOOT, None),
        "lock-busy" => call.mac_call(&[("GATE_FAKE_LOCKF", "busy")]),
        "cargo-fail" => call.mac_call(&[("GATE_FAKE_CARGO", "fail")]),
        "cargo-bound" => call.mac_call(&[("GATE_FAKE_CARGO", "bound")]),
        "broken-src-git" => call.broken_src_git().and_then(|()| call.mac_call(&[])),
        "no-perl" => call.mac_call(&[(NO_PERL, "1")]),
        "no-cargo" => call.mac_call(&[(NO_CARGO, "1")]),
        "root-open" => call.mac_call(&[(ROOT_MODE, "750")]),
        "drop" => Ok(SSH_FAILED),
        "fail-banner" => said(&mut io::stderr().lock(), PRE_BANNER),
        "fail-other" => said(&mut io::stderr().lock(), "ssh: connect to host builder port 22: Connection refused"),
        "fail-host" => said(
            &mut io::stderr().lock(),
            &format!("ssh: connect to host {} port 22: Connection refused", call.host),
        ),
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
/// for a `run`, `lfs`, `inputs` or `drain` call and `GATE_FAKE_SSH_PREFLIGHT_SEAMS` for a preflight, none when it is unset.
fn seams_of(name: &str) -> Result<String, String> {
    match std::env::var(name) {
        Err(std::env::VarError::NotPresent) => Ok(String::new()),
        read => read.map_err(|error| format!("{name}: {error}")),
    }
}

/// The step of this call: a step of `host`'s own `on {alias}` block, numbered by the calls made to that host, else of
/// the steps before the first `on` line, numbered the same way; a file with no `on` line at all numbers all its steps by
/// the log as a whole. The call's `verb` is appended to the log, and the verb and the call's ssh `options` as one line to
/// `<steps file>.words`.
fn next_step(steps: &Path, host: &str, verb: &str, options: &[String]) -> Result<String, String> {
    let text = fs::read_to_string(steps).map_err(|error| format!("cannot read {}: {error}", steps.display()))?;
    let log = beside(steps, ".log");
    let made = fs::read_to_string(&log).map_or(0, |calls| calls.lines().count());
    appended(&log, verb)?;
    let said = std::iter::once(verb)
        .chain(options.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    appended(&beside(steps, ".words"), &said)?;
    let blocks = Blocks::of(&text)?;
    let index = if blocks.any() {
        // The current call is one of the host's lines, `run` having appended it before the step is chosen.
        calls_to(steps, host).saturating_sub(1)
    } else {
        made
    };
    blocks
        .step(host, index)
        .map(str::to_owned)
        .ok_or_else(|| format!("no step left for call {} ({verb})", index + 1))
}

/// The calls made so far to `host`, counting this one, which `run` appends to `<steps file>.lines` before the step is
/// chosen.
fn calls_to(steps: &Path, host: &str) -> usize {
    let prefix = format!("{host} ");
    fs::read_to_string(beside(steps, ".lines")).map_or(0, |lines| lines.lines().filter(|line| line.starts_with(&prefix)).count())
}

/// The steps file's `on {alias}` blocks: the steps before the first, and each alias's own.
struct Blocks<'a> {
    shared: Vec<&'a str>,
    named: Vec<(&'a str, Vec<&'a str>)>,
}

impl<'a> Blocks<'a> {
    /// The blocks of the steps file `text`, one step a line and blank lines skipped; else the line naming the first `on`
    /// line that cannot be one.
    fn of(text: &'a str) -> Result<Blocks<'a>, String> {
        let mut blocks = Blocks {
            shared: Vec::new(),
            named: Vec::new(),
        };
        for (index, line) in text.lines().enumerate().filter(|(_, line)| !line.trim().is_empty()) {
            let step = line.trim();
            let Some(alias) = step.strip_prefix("on ") else {
                match blocks.named.last_mut() {
                    Some((_, steps)) => steps.push(step),
                    None => blocks.shared.push(step),
                }
                continue;
            };
            if alias.is_empty() {
                return Err(format!("line {}: on takes a builder's alias", index + 1));
            }
            if alias.contains(' ') {
                return Err(format!("line {}: on takes one builder's alias, got '{alias}'", index + 1));
            }
            if blocks.named.iter().any(|(named, _)| *named == alias) {
                return Err(format!("line {}: {alias} already has its own steps", index + 1));
            }
            blocks.named.push((alias, Vec::new()));
        }
        Ok(blocks)
    }

    /// Whether the file holds any `on` line.
    fn any(&self) -> bool {
        !self.named.is_empty()
    }

    /// The step numbered `index` of a call to `host`: its own block's, else the steps before the first `on` line.
    fn step(&self, host: &str, index: usize) -> Option<&'a str> {
        let steps = match self.named.iter().find(|(alias, _)| *alias == host) {
            Some((_, steps)) => steps,
            None => &self.shared,
        };
        steps.get(index).copied()
    }
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
fn fed_call(command: Command, steps: &Path, after: Option<&Path>) -> Result<i32, String> {
    let input = kept_input(steps)?;
    if let Some(path) = after {
        hold(path)?;
    }
    fed_with(command, &input)
}

/// This process's standard input, read whole and kept beside the steps file `steps` as `<steps file>.input`.
fn kept_input(steps: &Path) -> Result<Vec<u8>, String> {
    let mut input = Vec::new();
    io::stdin()
        .lock()
        .read_to_end(&mut input)
        .map_err(|error| format!("cannot read the call's input: {error}"))?;
    let kept = beside(steps, ".input");
    fs::write(&kept, &input).map_err(|error| format!("cannot write {}: {error}", kept.display()))?;
    Ok(input)
}

/// Runs `command` fed `input`; its status.
fn fed_with(mut command: Command, input: &[u8]) -> Result<i32, String> {
    let mut child = command.stdin(Stdio::piped()).spawn().map_err(|error| cannot_run(&command, &error))?;
    let mut stdin = child.stdin.take().ok_or("the call has no input pipe")?;
    stdin.write_all(input).map_err(|error| format!("cannot feed the call: {error}"))?;
    drop(stdin);
    let status = child.wait().map_err(|error| format!("cannot wait for the call: {error}"))?;
    status.code().ok_or_else(|| "the call ended with no status".to_owned())
}

/// Puts `child`, just started outside this process's jobs, in a new job that lets a process in it start one outside it,
/// as a box run's command is started: breaking away left it in the first job above that refuses it. The job does not
/// end its processes when its handle closes, so the call outlives this process; its handle is left open for this
/// process's life.
fn allow_breakaway(child: &std::process::Child) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle as _;

    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };

    // SAFETY: no security attributes and no name are given; a null handle is a failure.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(format!("cannot create the call's job: {}", io::Error::last_os_error()));
    }
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_BREAKAWAY_OK;
    let size = u32::try_from(size_of_val(&limits)).map_err(|error| format!("the job's limits do not fit a u32: {error}"))?;
    // SAFETY: the job is open and `limits` is a whole extended limit structure of `size` bytes, read only.
    let set = unsafe { SetInformationJobObject(job, JobObjectExtendedLimitInformation, (&raw const limits).cast(), size) };
    if set == 0 {
        return Err(format!("cannot let the call's job allow breakaway: {}", io::Error::last_os_error()));
    }
    // SAFETY: the job is open, and the child's handle is open while `child` lives.
    let assigned = unsafe { AssignProcessToJobObject(job, child.as_raw_handle()) };
    if assigned == 0 {
        return Err(format!("cannot put the call in its job: {}", io::Error::last_os_error()));
    }
    Ok(())
}

/// Passes what `from` gives on to `to` until it ends, each chunk written whole and flushed as it comes.
fn relayed(from: &mut impl Read, to: &mut impl Write) -> Result<(), String> {
    let mut buffer = [0; 8192];
    loop {
        let read = from.read(&mut buffer).map_err(|error| format!("cannot read the call: {error}"))?;
        let Some(chunk) = buffer.get(..read).filter(|chunk| !chunk.is_empty()) else {
            return Ok(());
        };
        to.write_all(chunk)
            .and_then(|()| to.flush())
            .map_err(|error| format!("cannot pass the call's output on: {error}"))?;
    }
}

/// The root the routing file gives the macOS builders: `/` and the value of `GATE_FAKE_SSH_UNIX_ROOT`, which leaves the
/// leading `/` out, since Git for Windows' `sh`, which git starts its ssh through, rewrites a value starting with `/` as
/// a path of its own.
fn unix_root() -> Result<String, String> {
    variable(UNIX_ROOT_VARIABLE).map(|root| format!("/{root}"))
}

/// `path` as Git for Windows' `sh` names it: `/`, the drive letter in lower case, then the rest with `/` separators.
fn msys(path: &Path) -> String {
    let text = path.display().to_string().replace('\\', "/");
    match text.as_bytes() {
        [drive, b':', ..] => format!("/{}{}", char::from(*drive).to_ascii_lowercase(), text.get(2..).unwrap_or_default()),
        _ => text,
    }
}

/// Git for Windows' `sh.exe`: `usr\bin\sh.exe` in the folder of the first `git.exe` along `PATH`, or one or two folders
/// above it.
fn git_sh() -> Result<PathBuf, String> {
    git_folders()?
        .into_iter()
        .flat_map(|folder| folder.ancestors().take(3).map(|above| above.join(r"usr\bin\sh.exe")).collect::<Vec<_>>())
        .find(|sh| sh.is_file())
        .ok_or_else(|| "no Git for Windows sh.exe beside a git.exe on PATH".to_owned())
}

/// The folders along `PATH` that hold a `git.exe`, in `PATH`'s order.
fn git_folders() -> Result<Vec<PathBuf>, String> {
    let path = std::env::var_os("PATH").ok_or("PATH is not set")?;
    Ok(std::env::split_paths(&path).filter(|folder| folder.join("git.exe").is_file()).collect())
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

/// What a keyed prune's line holds after the box's exe: the verb, whose words after it are its root and keys, with no
/// `--stdin`.
const PRUNE_LINE: &str = " box prune ";
/// The verb of a keyed prune call.
const PRUNE: &str = "prune";

/// The call the line carries: `gate box`'s verb and the word after it, `--stdin`, a git service's name and the
/// repository's path, [`PRUNE`] and the prune's words after it, or [`PUSH`] and the push call's whole line, as `rest`;
/// the host it was made to, and the steps file it acts on.
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
        if let Some(rest) = line.strip_prefix(MAC_LINE) {
            let verb = match rest.split(' ').next() {
                Some("prepare") => MAC_PREPARE,
                Some("build") => MAC_BUILD,
                _ => return Err(format!("the macOS line names no script step: {line}")),
            };
            return Ok(BoxCall { verb, rest, host, steps });
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
        if let Some((_, rest)) = line.split_once(PRUNE_LINE) {
            return Ok(BoxCall {
                verb: PRUNE,
                rest,
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
            command.arg(service).arg(self.unix_mapped(self.rest)?);
            return Ok(command);
        }
        if self.is_mac() {
            return Err(format!("the {} call runs only through a step that feeds it its script", self.verb));
        }
        let mut command = Command::new(variable("GATE_FAKE_SSH_GATE")?);
        command.arg("box");
        let seams = match self.verb {
            "run" | "lfs" | "inputs" | "drain" | "outputs" | "channel-check" => Some("GATE_FAKE_SSH_BOX_SEAMS"),
            "preflight" => Some("GATE_FAKE_SSH_PREFLIGHT_SEAMS"),
            _ => None,
        };
        if let Some(name) = seams {
            command.args(seams_of(name)?.split_whitespace());
        }
        if matches!(self.verb, "preflight" | "drain" | PRUNE)
            && let Some(stop) = std::env::var_os("GATE_FAKE_SSH_ROOT_WALK_STOP")
        {
            command.arg("--test-box-root-walk-stop").arg(stop);
        }
        if self.verb == PRUNE {
            command.arg(PRUNE).args(self.rest.split(' '));
        } else {
            command.args([self.verb, self.rest]);
        }
        Ok(command)
    }

    /// This PC's `pwsh` with the push call's words after `pwsh`, its `LOCALAPPDATA` the host's folder in the folder
    /// `GATE_FAKE_SSH_BUILDERS` names, made when missing.
    fn push_command(&self) -> Result<Command, String> {
        let folder = self.builder_folder()?;
        let mut command = Command::new("pwsh");
        command.args(self.rest.split(' ').skip(1)).env("LOCALAPPDATA", folder);
        Ok(command)
    }

    /// Runs the call with this process's streams; its status. A push call is fed its standard input as [`fed_push`]
    /// feeds it.
    fn ok(&self) -> Result<i32, String> {
        if self.is_mac() {
            return self.mac_call(&[]);
        }
        let mut command = self.command()?;
        if self.verb == PUSH {
            return fed_call(command, self.steps, None);
        }
        let status = command.status().map_err(|error| cannot_run(&command, &error))?;
        status.code().ok_or_else(|| "the gate ended with no status".to_owned())
    }

    /// Whether the call runs a macOS builder's script.
    fn is_mac(&self) -> bool {
        self.verb == MAC_PREPARE || self.verb == MAC_BUILD
    }

    /// The builder's folder, the host's in the folder `GATE_FAKE_SSH_BUILDERS` names, made when missing.
    fn builder_folder(&self) -> Result<PathBuf, String> {
        if self.host.is_empty() {
            return Err("the call names no host".to_owned());
        }
        let folder = PathBuf::from(variable("GATE_FAKE_SSH_BUILDERS")?).join(self.host);
        fs::create_dir_all(&folder).map_err(|error| format!("cannot make {}: {error}", folder.display()))?;
        Ok(folder)
    }

    /// `path` with the macOS builders' root ([`unix_root`]) made the builder's `root` folder; as it is when it is not
    /// under that root or the variable is unset.
    fn unix_mapped(&self, path: &str) -> Result<String, String> {
        let Ok(root) = unix_root() else {
            return Ok(path.to_owned());
        };
        match path.strip_prefix(root.as_str()).filter(|rest| rest.starts_with('/')) {
            Some(rest) => Ok(format!("{}{}", self.builder_folder()?.join("root").display(), rest.replace('/', "\\"))),
            None => Ok(path.to_owned()),
        }
    }

    /// Removes the builder's `gate-push/src.git/HEAD`, which leaves `src.git` no repository.
    fn broken_src_git(&self) -> Result<(), String> {
        let head = self.builder_folder()?.join("root").join("gate-push").join("src.git").join("HEAD");
        fs::remove_file(&head).map_err(|error| format!("cannot remove {}: {error}", head.display()))
    }

    /// Runs a macOS builder's script call in Git for Windows' `sh` as the file's header says, `variables` added to its
    /// environment; its status.
    fn mac_call(&self, variables: &[(&str, &str)]) -> Result<i32, String> {
        if !self.is_mac() {
            return Err(format!("the step runs only a macOS builder's call, not {}", self.verb));
        }
        let set = |name: &str| variables.iter().any(|(set, _)| *set == name);
        let builder = self.builder_folder()?;
        let root = builder.join("root");
        let tools = builder.join("tools");
        let system = builder.join("system");
        for folder in [&tools, &system] {
            fs::create_dir_all(folder).map_err(|error| format!("cannot make {}: {error}", folder.display()))?;
        }
        let system_tools = [("/usr/bin/lockf", "lockf", LOCKF), ("/usr/bin/stat", "stat", STAT), ("/bin/mv", "mv", MV)];
        for (_, name, text) in system_tools {
            let path = system.join(name);
            fs::write(&path, text).map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        }
        let tools_word = msys(&tools);
        let cargo = CARGO
            .replace("@ARGS@", &msys(&builder.join("cargo.args")))
            .replace("@ENV@", &msys(&builder.join("cargo.env")));
        let after_tools = if set(NO_CARGO) {
            let git = git_folders()?.first().map(|folder| msys(folder)).ok_or("no git.exe on PATH")?;
            format!("/usr/bin:'{git}'")
        } else {
            "$PATH".to_owned()
        };
        let login = LOGIN_SHELL.replace("@TOOLS@", &tools_word).replace("@PATH@", &after_tools);
        let mut stand_ins = vec![("cc", CC), ("login-shell", login.as_str())];
        let cargo_path = tools.join("cargo");
        if set(NO_CARGO) {
            fs::remove_file(&cargo_path)
                .or_else(|error| if error.kind() == io::ErrorKind::NotFound { Ok(()) } else { Err(error) })
                .map_err(|error| format!("cannot remove {}: {error}", cargo_path.display()))?;
        } else {
            stand_ins.push(("cargo", cargo.as_str()));
        }
        for (name, text) in stand_ins {
            let path = tools.join(name);
            fs::write(&path, text).map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        }
        let sh = git_sh()?;
        let unix_root = unix_root()?;
        let root_word = msys(&root);
        let words = self
            .rest
            .split(' ')
            .map(|word| if word == unix_root { root_word.clone() } else { word.to_owned() });
        let mut command = Command::new(&sh);
        command.args(["-s", "--"]).args(words);
        let path = std::env::var_os("PATH").unwrap_or_default();
        let folders = sh.parent().map(Path::to_path_buf).into_iter().chain(std::env::split_paths(&path));
        let path = std::env::join_paths(folders).map_err(|error| format!("cannot make sh's PATH: {error}"))?;
        command
            .env("PATH", path)
            .env("SHELL", msys(&tools.join("login-shell")))
            .envs(variables.iter().copied());
        let input = kept_input(self.steps)?;
        let mut script = String::from_utf8(input).map_err(|error| format!("the script is not UTF-8: {error}"))?;
        for (path, name, _) in system_tools {
            script = script.replace(path, &msys(&system.join(name)));
        }
        if set(NO_PERL) {
            script = script.replace("[ -x /usr/bin/perl ]", "[ -x /usr/bin/no-such-perl ]");
        }
        fed_with(command, script.as_bytes())
    }

    /// Runs the call outside this process's job, its standard input fed from this process's and its standard output and
    /// error passed on raw to this process's as they come, each chunk flushed; its status once it and its output have
    /// ended. The feeding thread is not waited for: it ends with this process.
    fn away(&self) -> Result<i32, String> {
        let mut command = self.command()?;
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_BREAKAWAY_FROM_JOB)
            .spawn()
            .map_err(|error| cannot_run(&command, &error))?;
        // Before the call is fed its request, so before it can start a process of its own outside its job.
        let in_job = allow_breakaway(&child);
        if let Err(message) = in_job {
            // A call left outside the case's control is ended; the case fails on the line said.
            let _ = child.kill();
            return Err(message);
        }
        let mut stdin = child.stdin.take().ok_or("the call has no input pipe")?;
        let mut stdout = child.stdout.take().ok_or("the call has no output pipe")?;
        let mut stderr = child.stderr.take().ok_or("the call has no error pipe")?;
        // Dropping the call's input pipe once this process's input ends tells the call its input ended.
        thread::spawn(move || drop(io::copy(&mut io::stdin().lock(), &mut stdin)));
        let errors = thread::spawn(move || relayed(&mut stderr, &mut io::stderr()));
        relayed(&mut stdout, &mut io::stdout())?;
        errors.join().map_err(|_| "the error relay panicked".to_owned())??;
        let status = child.wait().map_err(|error| format!("cannot wait for the call: {error}"))?;
        status.code().ok_or_else(|| "the call ended with no status".to_owned())
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
