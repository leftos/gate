//! The desktop helper: started by the Task Scheduler in the logged-on session with the box's `Arg0`, it starts one
//! command on that session's desktop for the box and hands it over. It has no console and prints nothing; its exit
//! code says how it ended ([`gate_desktop::HelperExit`]).

#![windows_subsystem = "windows"]

use gate_desktop::HelperExit;

fn main() -> std::process::ExitCode {
    // First of all, so no DLL the helper loads by name afterwards comes from its own folder or the working folder.
    if gate_win32::set_default_dll_directories_system32().is_err() {
        return HelperExit::Internal.exit_code();
    }
    gate_desktop::run(std::env::args_os().skip(1)).exit_code()
}
