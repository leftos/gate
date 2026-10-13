//! `gate expect`: the expected time of a command run in the current folder, read from `durations.json` with no lock, for
//! each place with history of it. A reader only: it queues for nothing, takes no slot or mutex, writes no journal line
//! and neither writes nor rebuilds the store.

use std::collections::BTreeSet;
use std::path::Path;

use gate_args::{ExpectRequest, Slot, TestSeams};
use gate_state::{DurationStore, KeyInputs, RebuildReason, StoreRead, duration_key, read_store};
use jiff::Timestamp;

use crate::answer::FAILED_STATUS;
use crate::console::Console;
use crate::durations::{expected_line, folder_place, temp_root, unreadable_line};

/// What `gate expect` prints when no place has an estimate for the command.
const NO_HISTORY: &str = "no history";

/// Prints, on stdout, the expected-time line of `request`'s command run in the working directory for each place the
/// store under the state root `root` has an estimate for, or `no history`, and returns the exit status: 0, or 1 when the
/// working directory cannot be read, whatever the store holds. A store that cannot be read is no history, said on
/// stderr; a missing one, one of another key version or of a newer build is no history, said by nothing.
pub(crate) fn expect(request: &ExpectRequest, root: &Path, console: &mut Console) -> u8 {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd.to_string_lossy().into_owned(),
        Err(error) => {
            console.err(&format!("gate: cannot read the working directory: {error}"));
            return FAILED_STATUS;
        }
    };
    let Some(store) = current_store(root, console) else {
        console.out(NO_HISTORY);
        return 0;
    };
    let folder = folder_place(&cwd, &TestSeams::default());
    let temp_root = temp_root();
    let command = gate_args::command_text(&request.command);
    let here = KeyInputs {
        folder: &folder,
        remote_host: None,
        critical: matches!(request.slot, Slot::Critical),
        temp_root: temp_root.as_deref(),
        command: &command,
    };
    let lines = expected_lines(&store, &here, Timestamp::now());
    if lines.is_empty() {
        console.out(NO_HISTORY);
    }
    for line in &lines {
        console.out(line);
    }
    0
}

/// `root`'s store when it is current; `None` for one that is missing, of another key version or cannot be read, the
/// last said on stderr.
fn current_store(root: &Path, console: &mut Console) -> Option<DurationStore> {
    match read_store(root) {
        StoreRead::Current(store) => Some(store),
        StoreRead::NeedsRebuild(RebuildReason::Invalid(error)) | StoreRead::Unavailable(error) => {
            console.err(&unreadable_line(&error));
            None
        }
        StoreRead::NeedsRebuild(RebuildReason::Missing | RebuildReason::Stale) | StoreRead::Newer => None,
    }
}

/// The expected-time line of `here`'s command for each place `store` has an estimate of it at `now`, each place's exact
/// key first and its family key next: here, ending ` here`, then each builder in host order, ending ` on {host}`.
fn expected_lines(store: &DurationStore, here: &KeyInputs<'_>, now: Timestamp) -> Vec<String> {
    let local = store
        .estimate(&duration_key(here), now)
        .map(|estimate| format!("{} here", expected_line(&estimate)));
    let builders = builder_hosts(store).into_iter().filter_map(|host| {
        let key = duration_key(&KeyInputs {
            remote_host: Some(host),
            ..*here
        });
        store
            .estimate(&key, now)
            .map(|estimate| format!("{} on {host}", expected_line(&estimate)))
    });
    local.into_iter().chain(builders).collect()
}

/// The builders `store` holds keys of, each once, in host order: the host of each `remote:{host}:...` key.
fn builder_hosts(store: &DurationStore) -> BTreeSet<&str> {
    store
        .keys()
        .filter_map(|key| key.strip_prefix("remote:")?.split_once(':').map(|(host, _)| host))
        .collect()
}
