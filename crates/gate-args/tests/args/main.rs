//! The gate's argv grammar: the subcommand CLI, the hidden `gate box`, the compatibility layer and the hidden test-seam
//! flags; and the command's words: their quoting for the C runtime and cmd.exe, the words a batch target refuses, and program resolution.

#![expect(clippy::expect_used, reason = "a test whose command line parses the wrong way must fail loudly")]

mod batch;
mod boxcmd;
mod compat;
mod cutover;
mod program;
mod push;
mod quoting;
mod run;
mod seams;
mod support;
mod top;
