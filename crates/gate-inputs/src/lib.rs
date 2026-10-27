//! What the Windows box and the Unix box must judge alike about the files a routed run's inputs and outputs carry:
//! [`names`] holds the path names neither end brings back and the plain-path rule, [`hash`] hashes a stream with
//! SHA-256 over a fixed buffer, [`glob`] matches a path against the output globs and the exclusions, [`walk`] holds what
//! a walk of the outputs finds, [`manifest`] holds the PC's list of its files, the box's check of the paths a sent
//! manifest names and the rule choosing which of the box's files to pack, [`failure`] holds the caps a failed run's
//! failure outputs are held to, [`pack`] writes the `.tar.zst` archive the files travel in, and [`archive`] reads it,
//! handing what it takes to each box's own writer. Nothing here touches the operating system, so it builds for every
//! box.
#![expect(
    clippy::missing_errors_doc,
    reason = "each reader's own doc names what it refuses and why, as the gate crate's docs always have"
)]

pub mod archive;
pub mod failure;
pub mod glob;
pub mod hash;
pub mod manifest;
pub mod names;
pub mod pack;
pub mod walk;
