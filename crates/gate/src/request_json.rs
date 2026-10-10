//! The one JSON line `--test-print-request` prints: the request as the gate resolved it. A debug line for the
//! black-box suite, not a published format; the command's words are converted lossily for it.

use std::path::Path;

use gate_args::{RunRequest, StopRequest};
use serde_json::json;

use crate::resolve::ResolvedRoot;

pub(crate) fn run(request: &RunRequest, log: &Path, root: &ResolvedRoot) -> String {
    let command: Vec<String> = request.command.iter().map(|word| word.to_string_lossy().into_owned()).collect();
    let seams = &request.test_seams;
    json!({
        "mode": "run",
        "slot": request.slot.as_str(),
        "timeout": request.timeout,
        "log": log.to_string_lossy(),
        "stall": request.stall,
        "tail": request.tail,
        "noMarkers": request.no_markers,
        "local": request.local,
        "stateDir": root.path.to_string_lossy(),
        "defaultRoot": root.root.is_default(),
        "rootHash": root.root.hash(),
        "command": command,
        "testSeams": {
            "sampleMs": seams.sample_ms,
            "heartbeatMs": seams.heartbeat_ms,
            "pollMs": seams.poll_ms,
            "stallMs": seams.stall_ms,        },
    })
    .to_string()
}

pub(crate) fn stop_tree(request: &StopRequest, root: &ResolvedRoot) -> String {
    json!({
        "mode": "stop-tree",
        "pid": request.pid,
        "stateDir": root.path.to_string_lossy(),
        "defaultRoot": root.root.is_default(),
        "rootHash": root.root.hash(),
    })
    .to_string()
}
