//! The black-box suite's stand-in for an installed gate of another build, which a case copies to
//! `<LOCALAPPDATA>\gate\bin\gate.exe`. With the single word `--version` it prints `gate ` and the first line of the
//! file `version.txt` beside its own file, and nothing when there is no such file. With any other words it writes, beside
//! its own file, `argv.txt` (each word on its own line), `cwd.txt` (its working folder) and `marker.txt` (the value of
//! `GATE_HANDED_OVER`, or `unset`), prints `fake-gate ran` on stdout, and exits with the integer in the file `exit.txt`
//! beside it, 0 when there is none.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Exits through `std::process::exit`, so a case can name any 32-bit status in `exit.txt`.
fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let status = match run(&args) {
        Ok(status) => status,
        Err(message) => {
            // A stand-in that cannot write its own error has no other stream to say so on; its status still fails.
            let _ = writeln!(io::stderr().lock(), "fake-gate: {message}");
            2
        }
    };
    std::process::exit(status);
}

fn run(args: &[OsString]) -> Result<i32, String> {
    let folder = own_folder()?;
    let mut stdout = io::stdout().lock();
    if args.len() == 1 && args[0] == "--version" {
        if let Ok(text) = fs::read_to_string(folder.join("version.txt"))
            && let Some(first) = text.lines().next()
        {
            writeln!(stdout, "gate {first}").map_err(|error| format!("cannot write to stdout: {error}"))?;
        }
        return Ok(0);
    }
    let words = args.iter().fold(String::new(), |mut words, word| {
        words.push_str(&word.to_string_lossy());
        words.push('\n');
        words
    });
    write(&folder.join("argv.txt"), &words)?;
    let cwd = std::env::current_dir().map_err(|error| format!("cannot read the working folder: {error}"))?;
    write(&folder.join("cwd.txt"), &cwd.to_string_lossy())?;
    let marker = std::env::var_os("GATE_HANDED_OVER").map_or_else(|| "unset".to_owned(), |value| value.to_string_lossy().into_owned());
    write(&folder.join("marker.txt"), &marker)?;
    writeln!(stdout, "fake-gate ran").map_err(|error| format!("cannot write to stdout: {error}"))?;
    match fs::read_to_string(folder.join("exit.txt")) {
        Ok(text) => text.trim().parse().map_err(|error| format!("exit.txt holds no status: {error}")),
        Err(_) => Ok(0),
    }
}

/// The folder this program's own file is in.
fn own_folder() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|error| format!("cannot read its own path: {error}"))?;
    exe.parent().map(Path::to_path_buf).ok_or_else(|| "its own path has no folder".to_owned())
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    fs::write(path, text).map_err(|error| format!("cannot write {}: {error}", path.display()))
}
