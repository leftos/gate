//! The black-box suite's helper program. It runs the steps on its command line in order:
//! `out <text>` and `err <text>` print a line on stdout or stderr; `sleep <ms>` sleeps; `wait <path>` sleeps until the
//! path exists, and exits 1 when it has not appeared after 120 s; `burn <stop-path>` prints `burning` on stdout and
//! spins on the CPU until the file at `<stop-path>` exists, checking about every 10 ms, then goes on to its next step;
//! `chatter <path>` prints `chatter <n>`, flushed, about every 50 ms until the file at `<path>` exists, then goes on,
//! so a case can hold its command's log growing, and the watch's stall clock with it, until it is ready for the command
//! to go quiet;
//! `spawn-wait <path>` starts `fake-tool wait <path>`
//! with this process's stdin, stdout and stderr, prints `spawned <its pid>` and goes on without waiting for it;
//! `spawn-orphan <path>` runs `fake-tool spawn-wait <path>` with this process's handles and waits for it to end, so the
//! helper that one starts is left with no parent and only the job this process runs in still reaches it; `exit <n>`
//! exits with status n, any 32-bit code, negative ones as Windows reads them back;
//! `env <name>` prints the variable's value, or `fake-tool: <name> is not set`, on its own line; `bytes <hex>` writes the
//! bytes the hex digits spell to stdout as they are, with no line feed added; `lines <n> <text>` prints n lines on
//! stdout, the i-th `<text><i>` counting from 1; `pid` prints `pid <its own pid>` on stdout;
//! `argv` prints every word after it, each on its own line, and ends, so it is always the last step.
//! With no `exit` step it exits 0. For example `fake-tool out hi err oops exit 3`.

#![expect(
    clippy::disallowed_methods,
    reason = "a test helper that starts copies of itself and holds no launch's handles"
)]

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

/// How often `wait` looks for its path.
const WAIT_STEP: Duration = Duration::from_millis(20);
/// How long `wait` looks for its path before it gives up: the suite's hang guard, so a loaded machine never ends a
/// helper its case still means to let go.
const WAIT_LIMIT: Duration = Duration::from_secs(600);
/// How long `burn` spins between two looks for its stop file.
const BURN_STEP: Duration = Duration::from_millis(10);
/// How long `chatter` waits between two of its lines.
const CHATTER_STEP: Duration = Duration::from_millis(50);

/// Exits through `std::process::exit`, because `ExitCode` carries 8 bits and the suite needs codes above 255 and
/// negative ones.
fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let status = match run(&args) {
        Ok(status) => status,
        Err(message) => {
            // A helper that cannot write its own error has no other stream to say so on; its status still fails.
            let _ = writeln!(io::stderr().lock(), "fake-tool: {message}");
            2
        }
    };
    std::process::exit(status);
}

fn run(args: &[OsString]) -> Result<i32, String> {
    let mut stdout = io::stdout().lock();
    let mut steps = args.iter();
    while let Some(step) = steps.next() {
        match step.to_string_lossy().as_ref() {
            "argv" => {
                for word in steps.by_ref() {
                    writeln!(stdout, "{}", word.to_string_lossy()).map_err(|error| write_failed(&error))?;
                }
                break;
            }
            "out" => writeln!(stdout, "{}", operand(&mut steps, "out")?).map_err(|error| write_failed(&error))?,
            "env" => writeln!(stdout, "{}", variable(&operand(&mut steps, "env")?)).map_err(|error| write_failed(&error))?,
            "bytes" => stdout
                .write_all(&decode_hex(&operand(&mut steps, "bytes")?)?)
                .map_err(|error| write_failed(&error))?,
            "err" => {
                stdout.flush().map_err(|error| write_failed(&error))?;
                writeln!(io::stderr().lock(), "{}", operand(&mut steps, "err")?).map_err(|error| write_failed(&error))?;
            }
            "lines" => {
                let text = operand(&mut steps, "lines")?;
                let count: u32 = text.parse().map_err(|_| format!("lines takes a count, got '{text}'"))?;
                let line = operand(&mut steps, "lines")?;
                for number in 1..=count {
                    writeln!(stdout, "{line}{number}").map_err(|error| write_failed(&error))?;
                }
            }
            "sleep" => {
                let text = operand(&mut steps, "sleep")?;
                let ms: u64 = text.parse().map_err(|_| format!("sleep takes milliseconds, got '{text}'"))?;
                thread::sleep(Duration::from_millis(ms));
            }
            "wait" => {
                let path = operand(&mut steps, "wait")?;
                if !wait_for(&path)? {
                    stdout.flush().map_err(|error| write_failed(&error))?;
                    writeln!(io::stderr().lock(), "fake-tool: {path} did not appear within 120 s").map_err(|error| write_failed(&error))?;
                    return Ok(1);
                }
            }
            "burn" => {
                let path = operand(&mut steps, "burn")?;
                writeln!(stdout, "burning").map_err(|error| write_failed(&error))?;
                stdout.flush().map_err(|error| write_failed(&error))?;
                burn_until(&path)?;
            }
            "chatter" => {
                let path = operand(&mut steps, "chatter")?;
                chatter_until(&path, &mut stdout)?;
            }
            "spawn-wait" => {
                let pid = spawn_wait(&operand(&mut steps, "spawn-wait")?)?;
                writeln!(stdout, "spawned {pid}").map_err(|error| write_failed(&error))?;
            }
            "spawn-orphan" => {
                stdout.flush().map_err(|error| write_failed(&error))?;
                spawn_orphan(&operand(&mut steps, "spawn-orphan")?)?;
            }
            "exit" => {
                let text = operand(&mut steps, "exit")?;
                stdout.flush().map_err(|error| write_failed(&error))?;
                return text.parse().map_err(|_| format!("exit takes a 32-bit status, got '{text}'"));
            }
            "pid" => {
                writeln!(stdout, "pid {}", std::process::id()).map_err(|error| write_failed(&error))?;
                stdout.flush().map_err(|error| write_failed(&error))?;
            }
            other => return Err(format!("unknown step '{other}'")),
        }
    }
    stdout.flush().map_err(|error| write_failed(&error))?;
    Ok(0)
}

fn operand<'a>(steps: &mut impl Iterator<Item = &'a OsString>, step: &str) -> Result<String, String> {
    steps
        .next()
        .map(|word| word.to_string_lossy().into_owned())
        .ok_or_else(|| format!("{step} needs a value"))
}

/// Sleeps until `path` exists, or gives up after [`WAIT_LIMIT`], so a helper no case lets go (one orphaned by a case
/// that failed, which the case's own kill does not reach) never holds a case folder for ever. Whether the path came.
fn wait_for(path: &str) -> Result<bool, String> {
    let deadline = Instant::now() + WAIT_LIMIT;
    loop {
        match Path::new(path).try_exists() {
            Ok(true) => return Ok(true),
            Ok(false) if Instant::now() >= deadline => return Ok(false),
            Ok(false) => thread::sleep(WAIT_STEP),
            Err(error) => return Err(format!("cannot check {path}: {error}")),
        }
    }
}

/// Spins on the CPU until the file at `path` exists, looking about every [`BURN_STEP`], so a case can stand a copy of
/// this program in for a build server that is busy until its stop file appears. The arithmetic between two looks is
/// kept by [`std::hint::black_box`], so an optimised build does not drop the burn.
fn burn_until(path: &str) -> Result<(), String> {
    let mut state: u64 = 1;
    loop {
        let until = Instant::now() + BURN_STEP;
        while Instant::now() < until {
            state = std::hint::black_box(state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407));
        }
        match Path::new(path).try_exists() {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) => return Err(format!("cannot check {path}: {error}")),
        }
    }
}

/// Prints a numbered line about every [`CHATTER_STEP`], flushing each, until the file at `path` exists: the log a case's
/// command keeps growing, so the watch it runs under sees progress and its stall clock cannot run.
fn chatter_until(path: &str, stdout: &mut impl Write) -> Result<(), String> {
    let mut line: u64 = 0;
    loop {
        line += 1;
        writeln!(stdout, "chatter {line}").map_err(|error| write_failed(&error))?;
        stdout.flush().map_err(|error| write_failed(&error))?;
        thread::sleep(CHATTER_STEP);
        match Path::new(path).try_exists() {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) => return Err(format!("cannot check {path}: {error}")),
        }
    }
}

/// Starts `fake-tool wait <path>` with this process's handles, and does not wait for it; its pid.
fn spawn_wait(path: &str) -> Result<u32, String> {
    let me = std::env::current_exe().map_err(|error| format!("cannot find this program: {error}"))?;
    Command::new(me)
        .arg("wait")
        .arg(path)
        .spawn()
        .map(|helper| helper.id())
        .map_err(|error| format!("cannot start the waiting helper: {error}"))
}

/// Runs `fake-tool spawn-wait <path>` with this process's handles and waits for it to end, which it does as soon as
/// its helper is started, so that helper outlives its parent.
fn spawn_orphan(path: &str) -> Result<(), String> {
    let me = std::env::current_exe().map_err(|error| format!("cannot find this program: {error}"))?;
    let status = Command::new(me)
        .arg("spawn-wait")
        .arg(path)
        .status()
        .map_err(|error| format!("cannot start the orphaning helper: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("the orphaning helper ended with {status}"))
    }
}

/// The value of the variable `name`, or the line saying it is not set.
fn variable(name: &str) -> String {
    std::env::var_os(name).map_or_else(|| format!("fake-tool: {name} is not set"), |value| value.to_string_lossy().into_owned())
}

/// The bytes two hex digits each spell.
fn decode_hex(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) {
        return Err(format!("bytes takes pairs of hex digits, got '{text}'"));
    }
    (0..text.len())
        .step_by(2)
        .map(|at| {
            text.get(at..at + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or_else(|| format!("bytes takes pairs of hex digits, got '{text}'"))
        })
        .collect()
}

fn write_failed(error: &io::Error) -> String {
    format!("cannot write: {error}")
}
