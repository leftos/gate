//! The gate-state crate's behaviour against a fake process table, slot mutexes and working directories.

#![expect(clippy::expect_used, reason = "a test that cannot set up its files or find its pool must fail loudly")]

mod archive;
mod counts;
mod critical;
mod cutover;
mod fixtures;
mod journal;
mod journal_write;
mod prefix;
mod queue;
mod root;
mod shape;
mod slots;
mod support;
mod v1;
mod verdict;
mod waiting;
