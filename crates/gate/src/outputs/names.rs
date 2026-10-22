//! The path names a run never brings back and the plain-path rule, held by [`gate_inputs::names`] so the Windows box
//! and the Unix box judge them alike; re-exported here for the walk, the pack, the unpack and the sync-back.

pub(crate) use gate_inputs::names::{
    PATH_DEPTH_LIMIT, is_git_s, is_lfs_config, is_plain_part, is_plain_path, is_spelled_git_s, is_spelled_lfs_config,
};

#[cfg(test)]
pub(crate) use gate_inputs::names::is_device;
