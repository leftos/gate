//! The gate-state crate's behaviour against a fake process table, slot mutexes and working directories.

#![expect(clippy::expect_used, reason = "a test that cannot set up its files or find its pool must fail loudly")]

mod counts;
mod critical;
mod fixtures;
mod journal;
mod prefix;
mod queue;
mod root;
mod shape;
mod slots;
mod support;
mod waiting;
