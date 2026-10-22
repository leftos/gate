//! A routed run's outputs: the files under its output globs that the box packs after a passing run and the PC installs.
//! Both ends are this exe and share this module. [`walk`] lists the files below a folder that the globs match, [`hash`]
//! hashes a file on this PC, [`manifest`] builds the PC's manifest from a walk, [`cache`] keeps the PC's hashes from one
//! build of the PC's list to the next, and [`archive`] opens each file the pack takes and unpacks the `.tar.zst` archive
//! the files travel in on Windows. What both boxes must judge alike lives in [`gate_inputs`]: the stream hash, the
//! PC's manifest, the box's pre-command listing and the rule choosing what the box packs, the output globs and their
//! exclusions, what a walk finds, the `.git*` and `.lfsconfig` names neither end brings back and the plain-path rule
//! the walk, the pack and the unpack share, the archive's writer, and its reader, its limits and its refusals;
//! [`glob`], [`names`] and the other modules re-export those.

pub(crate) mod archive;
pub(crate) mod cache;
pub(crate) mod glob;
pub(crate) mod hash;
pub(crate) mod manifest;
pub(crate) mod names;
pub(crate) mod stale;
pub(crate) mod walk;
