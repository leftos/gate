//! The router's rules over strings: the routing file, the `-like` matcher, the path rewrite, the work-folder key and
//! the routing decision.

#![expect(clippy::expect_used, reason = "a fixture that does not parse must fail the test loudly")]

mod config;
mod decide;
mod key;
mod like;
mod rewrite;
mod support;
mod warm;
