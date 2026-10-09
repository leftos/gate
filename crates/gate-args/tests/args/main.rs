//! The gate's argv grammar: the subcommand CLI, the compatibility layer and the hidden test-seam flags.

#![expect(clippy::expect_used, reason = "a test whose command line parses the wrong way must fail loudly")]

mod compat;
mod run;
mod seams;
mod support;
mod top;
