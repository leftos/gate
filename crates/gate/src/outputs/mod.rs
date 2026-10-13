//! A routed run's outputs: the files under its output globs that the box packs after a passing run and the PC installs.
//! Both ends are this exe and share this module. [`glob`] matches a path against the globs and the exclusions, [`walk`]
//! lists the matching files below a folder, [`hash`] hashes a stream with SHA-256 over a fixed buffer, [`manifest`]
//! holds the PC's list of its matching files and the rule choosing what the box packs, [`cache`] keeps the PC's hashes
//! from one build of that list to the next, [`archive`] writes and reads the `.tar.zst` archive the files travel in,
//! and [`names`] holds the `.git*` and `.lfsconfig` names neither end brings back and the plain-path rule the walk, the
//! pack and the unpack share.

pub(crate) mod archive;
pub(crate) mod cache;
pub(crate) mod glob;
pub(crate) mod hash;
pub(crate) mod manifest;
pub(crate) mod names;
pub(crate) mod walk;
