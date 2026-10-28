//! A command started detached: outside this process's job, on a hidden console of its own, in a process group of its own and in the
//! below-normal priority class, its stdout and stderr one file, and nothing kept of it but its pid.

use std::ffi::OsString;
use std::fs::File;
use std::iter;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsHandle, IntoRawHandle};
use std::path::Path;

use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::GENERIC_READ;
use windows_sys::Win32::System::Threading::{BELOW_NORMAL_PRIORITY_CLASS, CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};

use crate::create::{Creation, DesktopPolicy, Inherited, check_launch, create_inheriting_on};
use crate::launch::{nul, setup};
use crate::{EnvironmentBlock, LaunchError, own_handle};

const QUOTE: u16 = 0x22;
const BACKSLASH: u16 = 0x5C;
const SPACE: u16 = 0x20;

/// Starts `program` with `args` in `cwd` with the whole environment `env`, detached: outside this process's job
/// (`CREATE_BREAKAWAY_FROM_JOB`, which fails when that job refuses breakaway), on a hidden console of its own
/// (`CREATE_NO_WINDOW`: a command with no console at all, as `DETACHED_PROCESS` leaves it, gives every console program
/// it starts, git or ssh, a new visible window on the user's desktop, where these share the hidden one), in a process
/// group of its own (`CREATE_NEW_PROCESS_GROUP`, so a Ctrl+C aimed at this process's group never reaches it)
/// and in the below-normal priority class. Its stdin is `NUL` and its stdout and stderr both `output`; it inherits
/// those handles and no other, through copies open only while the spawn lock is held. The command line is `program`
/// then each of `args`, each written so the C runtime reads it back as one word. The command runs at once; its process
/// and thread handles are closed before this returns, so nothing of it is kept but its pid.
///
/// # Errors
///
/// [`LaunchError::CreateProcess`] when `CreateProcessW` fails (code 5, `ERROR_ACCESS_DENIED`, when this process's job
/// refuses breakaway); [`LaunchError::Setup`] when the launch is refused (a relative or batch-file program, or a NUL in
/// the program, a word or the working folder), a handle or the attribute list cannot be made, or the command cannot
/// be resumed, when it is ended first.
pub fn start_detached(program: &Path, args: &[OsString], env: &EnvironmentBlock, cwd: &Path, output: &File) -> Result<u32, LaunchError> {
    let words: Vec<Vec<u16>> = iter::once(program.as_os_str())
        .chain(args.iter().map(OsString::as_os_str))
        .map(|word| quoted(&word.encode_wide().collect::<Vec<u16>>()))
        .collect();
    let command_line = words.join(&SPACE);
    check_launch(&command_line, program, &[("working folder", cwd)])?;
    let input = nul(GENERIC_READ)?;
    let written = output_copy(output, "stdout")?;
    let errors = output_copy(output, "stderr")?;
    let creation = Creation {
        application: program,
        command_line: &command_line,
        directory: cwd,
        environment: env,
        token: None,
    };
    let inherited = Inherited {
        stdio: [&input, &written, &errors],
        others: &[],
    };
    let flags = CREATE_BREAKAWAY_FROM_JOB | CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | BELOW_NORMAL_PRIORITY_CLASS;
    let suspended = create_inheriting_on(&creation, flags, &inherited, DesktopPolicy::Absent, None)?;
    // The command holds its own copies now.
    drop((input, written, errors));
    let pid = suspended.pid();
    // Resuming consumes the thread handle's holder; dropping what it returns closes the process handle.
    drop(suspended.resume()?);
    Ok(pid)
}

/// A copy of `file`'s handle, never inheritable, that this process owns, for the command's `stream`.
fn output_copy(file: &File, stream: &str) -> Result<OwnedHandle, LaunchError> {
    let operation = format!("copy the {stream} handle for the command");
    let copy = file.as_handle().try_clone_to_owned().map_err(|error| setup(error, &operation))?;
    // SAFETY: `into_raw_handle` gives up the std guard's ownership, so the new guard alone owns the open copy.
    unsafe { own_handle(copy.into_raw_handle()) }.map_err(|error| setup(error, &operation))
}

/// `word` as the C runtime reads it back as one word: as it is when it is non-empty and holds no space, tab or `"`;
/// else wrapped in quotes, each `"` written `\"`, and the backslash run before a quote or at the end doubled.
fn quoted(word: &[u16]) -> Vec<u16> {
    let plain = !word.is_empty() && !word.iter().any(|&unit| unit == SPACE || unit == 0x09 || unit == QUOTE);
    if plain {
        return word.to_vec();
    }
    let mut out = Vec::with_capacity(word.len() + 2);
    out.push(QUOTE);
    let mut backslashes = 0;
    for &unit in word {
        if unit == BACKSLASH {
            backslashes += 1;
            continue;
        }
        if unit == QUOTE {
            out.extend(iter::repeat_n(BACKSLASH, backslashes * 2 + 1));
        } else {
            out.extend(iter::repeat_n(BACKSLASH, backslashes));
        }
        out.push(unit);
        backslashes = 0;
    }
    out.extend(iter::repeat_n(BACKSLASH, backslashes * 2));
    out.push(QUOTE);
    out
}
