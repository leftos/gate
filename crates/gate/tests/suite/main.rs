//! The black-box suite: drives the built `gate` through its command line and reads only what it writes.

#![expect(clippy::expect_used, reason = "a case that cannot start its program or read its output must fail loudly")]
#![expect(
    clippy::disallowed_methods,
    reason = "a case starts the built gate and its helpers directly, holding no launch's handles"
)]

mod boxside;
mod build_servers;
mod ceiling;
mod checkout;
mod cli;
mod compat;
mod diagnostics;
mod harness;
mod hold;
mod install;
mod interrupt;
mod journal;
mod launch;
mod request;
mod routing;
mod slots;
mod stall;
mod stop_tree;
mod support;
