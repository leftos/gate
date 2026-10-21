//! `gate-box`: the box side of a routed run on a Unix builder, speaking the wire the PC's remote client speaks to a
//! Windows box's `gate box`.

use std::process::ExitCode;

#[cfg(unix)]
fn main() -> ExitCode {
    gate_box_unix::cli::main(std::env::args_os().skip(1).collect())
}

#[cfg(not(unix))]
fn main() -> ExitCode {
    let mut console = gate_box_unix::console::Console::stdio();
    console.err("gate-box: runs only on a Unix builder");
    ExitCode::from(1)
}
