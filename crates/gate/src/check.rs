//! `gate check`: what this machine, or a builder `--builder` names, lacks for the repo it is run in, and how to install
//! it. [`local`] reads the repo's `.gate/toolchain.toml`, `global.json`, `.gate/siblings.toml` and `.gate/warmup.toml`
//! from the worktree on disk, probes the manifest's tools, lists this PC's .NET SDKs, looks for each sibling, and reads
//! `remote.json`'s warm-up consent and the work folder's warm-up record; [`builder`] reads the same files and asks the
//! builder's `gate box preflight` what it has, its work folder's warm-up among it, as a routed run's preflight asks;
//! [`report`] words what was found as `check: <where>: ` lines, an `install: <where>: ` hint per missing or too-old
//! item and a summary, and gives the exit status: 0 when nothing is missing, 1 when something is or the check could
//! not run, 2 for a builder `remote.json` does not list. Nothing is installed.

use gate_args::CheckRequest;

use crate::check::report::{Line, Said};
use crate::console::Console;

mod builder;
mod local;
mod report;

/// Checks this PC, or the builder the request names, and returns the exit status.
pub(crate) fn check(request: &CheckRequest, console: &mut Console) -> u32 {
    match &request.builder {
        None => local::check(request, console),
        Some(host) => builder::check(request, host, console),
    }
}

/// Says each of `said`'s lines on its stream through `console`; its exit status.
fn say(said: &Said, console: &mut Console) -> u32 {
    for line in &said.lines {
        match line {
            Line::Out(text) => console.out(text),
            Line::Err(text) => console.err(text),
        }
    }
    said.status
}
