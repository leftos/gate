//! What the Windows box and the Unix box must judge alike about the files a routed run's inputs and outputs carry:
//! [`names`] holds the path names neither end brings back and the plain-path rule, [`hash`] hashes a stream with
//! SHA-256 over a fixed buffer, and [`manifest`] holds the PC's list of its files and the box's check of the paths a
//! sent manifest names. Nothing here touches the operating system, so it builds for every box.
#![expect(
    clippy::missing_errors_doc,
    reason = "each reader's own doc names what it refuses and why, as the gate crate's docs always have"
)]

pub mod hash;
pub mod manifest;
pub mod names;
