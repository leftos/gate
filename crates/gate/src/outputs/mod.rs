//! A routed run's outputs: the files under its output globs that the box packs after a passing run and the PC installs.
//! Both ends are this exe and share this module. [`glob`] matches a path against the globs and the exclusions, [`walk`]
//! lists the matching files below a folder, [`hash`] hashes a file on this PC, [`manifest`] holds the rule choosing what
//! the box packs, [`cache`] keeps the PC's hashes from one build of the PC's list to the next, and [`archive`] writes the
//! `.tar.zst` archive the files travel in and unpacks it on Windows. What both boxes must judge alike lives in
//! [`gate_inputs`]: the stream hash, the PC's manifest, the `.git*` and `.lfsconfig` names neither end brings back and
//! the plain-path rule the walk, the pack and the unpack share, and the archive's reader, its limits and its refusals;
//! [`names`] re-exports those names.

pub(crate) mod archive;
pub(crate) mod cache;
pub(crate) mod glob;
pub(crate) mod hash;
pub(crate) mod manifest;
pub(crate) mod names;
pub(crate) mod stale;
pub(crate) mod walk;
