//! The desktop helper: started by the Task Scheduler in the logged-on session with the box's `Arg0`, it starts one
//! command on that session's desktop for the box and hands it over. It has no console and prints nothing; its exit
//! code says how it ended ([`gate_desktop::HelperExit`]).

#![windows_subsystem = "windows"]

fn main() -> std::process::ExitCode {
    gate_desktop::run(std::env::args_os().skip(1)).exit_code()
}
