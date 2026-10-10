//! The black-box suite: drives the built `gate` through its command line and reads only what it writes.

#![expect(clippy::expect_used, reason = "a case that cannot start its program or read its output must fail loudly")]

mod checkout;
mod cli;
mod compat;
mod harness;
mod journal;
mod request;
mod support;
