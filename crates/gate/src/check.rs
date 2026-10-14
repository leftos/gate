//! `gate check`: what this machine lacks for the repo it is run in, and how to install it. [`local`] reads the repo's
//! `.gate/toolchain.toml`, `global.json` and `.gate/siblings.toml` from the worktree on disk, probes the manifest's
//! tools, lists this PC's .NET SDKs and looks for each sibling; [`report`] words what was found as `check: here: `
//! lines, an `install: here: ` hint per missing or too-old item and a summary, and gives the exit status: 0 when
//! nothing is missing, 1 when something is or the check could not run. Nothing is installed.

mod local;
mod report;

pub(crate) use local::check;
