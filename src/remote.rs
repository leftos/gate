//! The gate's `remote.json`, read for the remote pool's size: its `slots`, a whole number 1 to 16. The file's routing
//! keys are the gate's alone and are not read here.

use std::path::{Path, PathBuf};

use crate::counts::{format_count_value, property, read_counts_file};
use crate::model::{SlotCount, StateInputs};

/// The most slots the remote pool may have.
const MAX_SLOTS: u32 = 16;

/// The remote pool's size as `remote.json` gives it.
pub(crate) struct RemoteCount {
    /// The slots gates may take: `slots`, or 0 when the file is ignored.
    pub count: u32,
    /// Why the file was ignored, or `None`.
    pub problem: Option<String>,
}

/// `<local app data>\gate\remote.json`.
pub(crate) fn remote_settings_path(inputs: &StateInputs) -> PathBuf {
    inputs.local_app_data.join("gate").join("remote.json")
}

/// The remote pool's size, or `None` when `remote.json` does not exist. A file that cannot be read, is not a JSON
/// object, or whose `slots` (matched in any letter case) is missing or not a whole number 1 to 16 gives a count of 0
/// and a problem line naming the file.
pub(crate) fn read_remote_count(path: &Path) -> Option<RemoteCount> {
    let ignored = |reason: &str| RemoteCount {
        count: 0,
        problem: Some(format!("gate: ignored {}: {reason}; using no remote slots", path.display())),
    };
    let settings = match read_counts_file(path) {
        Ok(Some(settings)) => settings,
        Ok(None) => return None,
        Err(reason) => return Some(ignored(&reason)),
    };
    let Some(value) = property(&settings, "slots") else {
        return Some(ignored("it has no slots"));
    };
    match SlotCount::from_json(value).map(SlotCount::get).filter(|count| *count <= MAX_SLOTS) {
        Some(count) => Some(RemoteCount { count, problem: None }),
        None => Some(ignored(&format!(
            "slots must be a whole number 1 to {MAX_SLOTS}, got {}",
            format_count_value(value)
        ))),
    }
}
