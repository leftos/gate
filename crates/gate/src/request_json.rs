//! The one JSON line `--test-print-request` prints: the request as the gate resolved it. A debug line for the
//! black-box suite, not a published format; the command's words are converted lossily for it.

use std::path::Path;

use gate_args::{Ceiling, CutoverRequest, RunRequest, StopRequest};
use serde_json::json;

use crate::resolve::ResolvedRoot;

pub(crate) fn run(request: &RunRequest, log: &Path, root: &ResolvedRoot) -> String {
    let command: Vec<String> = request.command.iter().map(|word| word.to_string_lossy().into_owned()).collect();
    let seams = &request.test_seams;
    let test_seams = json!({
        "sampleMs": seams.sample_ms,
        "heartbeatMs": seams.heartbeat_ms,
        "pollMs": seams.poll_ms,
        "stallMs": seams.stall_ms,
        "stallFromOutput": seams.stall_from_output,
        "loadWindowMs": seams.load_window_ms,
        "interruptAfterSamples": seams.interrupt_after_samples,
        "interruptQueued": seams.interrupt_queued,
        "interruptBeforeKey": seams.interrupt_before_key,
        "interruptBeforeLater": seams.interrupt_before_later,
        "interruptAfterFallback": seams.interrupt_after_fallback,
        "samplerFail": seams.sampler_fail,
        "diagnosticsFail": seams.diagnostics_fail,
        "remoteClient": seams.remote_client.as_ref().map(|path| path.to_string_lossy()),
        "remoteWaitMs": seams.remote_wait_ms,
        "keyWaitMs": seams.key_wait_ms,
        "gitLimitMs": seams.git_limit_ms,
        "gitDrainMs": seams.git_drain_ms,
        "remoteSnapshot": seams.remote_snapshot,
        "remoteTransport": seams.remote_transport,
        "remoteSsh": seams.remote_ssh.as_ref().map(|path| path.to_string_lossy()),
        "remoteRetryPauseMs": seams.remote_retry_pause_ms,
        "remoteBoundMs": seams.remote_bound_ms,
        "remoteInterruptAt": seams.remote_interrupt_at,
        "remoteWatchCall": seams.remote_watch_call,
        "remoteTimeoutMs": seams.remote_timeout_ms,
        "remoteBackstopMs": seams.remote_backstop_ms,
        "remoteOutputsBudgetBytes": seams.remote_outputs_budget_bytes,
        "remoteInputsBudgetBytes": seams.remote_inputs_budget_bytes,
        "remoteLfsBudgetBytes": seams.remote_lfs_budget_bytes,
        "ownStartFails": seams.own_start_fails,
        "parentJobTrusted": seams.parent_job_trusted,
        "jobBreakaway": seams.job_breakaway,
    });
    let (timeout, timeout_auto) = match request.timeout {
        Ceiling::Given(seconds) => (seconds, false),
        Ceiling::Auto(floor) => (floor, true),
    };
    json!({
        "mode": "run",
        "slot": request.slot.as_str(),
        "timeout": timeout,
        "timeoutAuto": timeout_auto,
        "log": log.to_string_lossy(),
        "stall": request.stall,
        "tail": request.tail,
        "noMarkers": request.no_markers,
        "local": request.local,
        "reason": request.reason,
        "bringTracked": request.bring_tracked,
        "env": request.env,
        "cd": request.cd.as_deref().map(Path::to_string_lossy),
        "stateDir": root.path.to_string_lossy(),
        "defaultRoot": root.root.is_default(),
        "rootHash": root.root.hash(),
        "command": command,
        "testSeams": test_seams,
    })
    .to_string()
}

pub(crate) fn cutover(request: &CutoverRequest, local_app_data: &Path) -> String {
    let seams = &request.seams;
    json!({
        "mode": "cutover",
        "localAppData": local_app_data.to_string_lossy(),
        "testSeams": {
            "pollMs": seams.poll_ms,
            "heartbeatMs": seams.heartbeat_ms,
            "confirmMs": seams.confirm_ms,
            "interruptAfterList": seams.interrupt_after_list,
        },
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
