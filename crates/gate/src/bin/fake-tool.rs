//! The black-box suite's helper program. It runs the steps on its command line in order:
//! `out <text>` and `err <text>` print a line on stdout or stderr; `sleep <ms>` sleeps; `exit <n>` exits with status n;
//! `argv` prints every word after it, each on its own line, and ends, so it is always the last step.
//! With no `exit` step it exits 0. For example `fake-tool out hi err oops exit 3`.

use std::ffi::OsString;
use std::io::{self, Write};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match run(&args) {
        Ok(status) => ExitCode::from(status),
        Err(message) => {
            // A helper that cannot write its own error has no other stream to say so on; its status still fails.
            let _ = writeln!(io::stderr().lock(), "fake-tool: {message}");
            ExitCode::from(2)
        }
    }
}

fn run(args: &[OsString]) -> Result<u8, String> {
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
            "err" => {
                stdout.flush().map_err(|error| write_failed(&error))?;
                writeln!(io::stderr().lock(), "{}", operand(&mut steps, "err")?).map_err(|error| write_failed(&error))?;
            }
            "sleep" => {
                let text = operand(&mut steps, "sleep")?;
                let ms: u64 = text.parse().map_err(|_| format!("sleep takes milliseconds, got '{text}'"))?;
                thread::sleep(Duration::from_millis(ms));
            }
            "exit" => {
                let text = operand(&mut steps, "exit")?;
                stdout.flush().map_err(|error| write_failed(&error))?;
                return text.parse().map_err(|_| format!("exit takes a status from 0 to 255, got '{text}'"));
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

fn write_failed(error: &io::Error) -> String {
    format!("cannot write: {error}")
}
