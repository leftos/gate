//! The black-box suite: drives the built `gate` through its command line and reads only what it writes.

#![expect(clippy::expect_used, reason = "a case that cannot start its program or read its output must fail loudly")]
#![expect(
    clippy::disallowed_methods,
    reason = "a case starts the built gate and its helpers directly, holding no launch's handles"
)]

mod boxside;
mod burn;
mod ceiling;
mod checkout;
mod cli;
mod compat;
mod cores;
mod critical;
mod cutover;
mod diagnostics;
mod durations;
mod harness;
mod hold;
mod install;
mod interrupt;
mod journal;
mod launch;
mod launcher;
mod nesting;
mod pause;
mod priority;
mod push;
mod remote_client;
mod request;
mod routing;
mod slots;
mod stall;
mod stop_tree;
mod support;
mod syncback_conversions;
mod syncback_quarantine;
