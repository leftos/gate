//! The stand-in desktop helper the black-box suite's box cases run: the real helper, built beside `gate.exe` so a case
//! can put it where the box looks for the installed one, and started exactly as [`gate_desktop::run`] starts it. The
//! step it breaks comes from the helper's own seam, `GATE_DESKTOP_TEST_FAULT`; it is built only with the `test-seams`
//! feature.

#![windows_subsystem = "windows"]

use gate_desktop::HelperExit;

fn main() -> std::process::ExitCode {
    // First of all, so no DLL the helper loads by name afterwards comes from its own folder or the working folder.
    if gate_win32::set_default_dll_directories_system32().is_err() {
        return HelperExit::Internal.exit_code();
    }
    gate_desktop::run(std::env::args_os().skip(1)).exit_code()
}
