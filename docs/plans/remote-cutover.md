# Remote cutover (GATE-46)

The plan for `feat/remote-cutover`: the Rust remote client replaces the remote.ps1 bridge. Owner and agent rulings are in GATE-46's description; this file holds the split and the runbook. Part 1 (R1–R7) selects the Rust client behind a flag and drains; the orchestrator flips once part 1 is green. Part 2 (R8–R10) deletes the bridge.

## Part 1: select the Rust client behind the flag, and drain

### R1. Stamp the build's commit into `gate --version` and the box preflight; fall back on a stale builder copy

- `gate --version` prints `gate 0.1.0+{12 hex}` (`-dirty` for a dirty tree, `+dev` when no id was given); the preflight's `gate: remote version` line carries the same text, and the client compares the whole text, so a builder with the same package version but another build falls back `version`. `tools/install.ps1` sets `GATE_BUILD_ID` from `git rev-parse --short=12 HEAD`; the id is read with `option_env!("GATE_BUILD_ID")` (a hand-built `+dev` compares equal only to another `+dev`). An unreadable id falls back `version`, never routes.
- Files: `crates/gate/src/answer.rs`, `boxside/preflight.rs`, `remote/client.rs` (`OURS`, through one shared `build_version()`), `tools/install.ps1`; suite `cli.rs`, `boxside.rs`.
- Tests: `cli::version_names_the_package_version_and_the_build`, `remote_client::a_builder_gate_of_the_same_version_without_this_build_falls_back_version`, `install::the_installed_gate_names_the_commit_it_was_built_from`; guard `push::a_push_reports_the_build_it_sends`.
- First: search claude-config and ops-tower for a parser of `gate --version`.
- Docs: design.md CLI `--version`, `gate push`, Routing's preflight bullet, the box side, Install; GLOSSARY "build id".

### R2. Build the production Rust client over ssh and select it when the state root holds `remote-cutover.flag`

- Remove the test-seams gating from the client modules, `FAILED`/`FAILED_RUN`, and the `Transport`/`Ssh` items in `transport.rs`; add `RustClient::ssh(request)` with production limits from constants (test-seams overrides only in a test-seams build). `chosen` returns a switch client whose `ready()` checks `{state root}\remote-cutover.flag` when the routed attempt starts: flag present, the Rust client over `Ssh::find`; absent, the bridge. Drop the `expect(dead_code)` this makes stale.
- Files: `remote/{mod,client,transport,git}.rs`, `gate-state/src/v1.rs` (`remote_cutover_flag_path`), suite `routing.rs`.
- Tests: `routing::the_remote_cutover_flag_routes_through_ssh_to_the_builders_gate` (fake-ssh as `ssh.exe` on PATH, step `exit 9009`, a preflight call then fallback `version`), unit `the_ssh_client_has_the_builder_layout_root`, guard `routing::without_the_flag_a_routed_run_keeps_the_bridge`. No-features clippy stays clean.
- Docs: design.md Routing ("dormant"), the State root Files table, the box side; ARCHITECTURE rows for the remote client; GLOSSARY "remote-cutover flag".

### R3. Add `gate box inflight`, which lists the builder's work folders a run holds

- Hidden verb `gate box inflight {payload}` (payload carries `Root`): for each `{Root}\work\*.lock`, an open with read access sharing everything; a sharing violation means held. Output `gate: remote in flight {n}`, then one `gate: remote in flight {key}` per held lock; a missing root is 0; exit 0. The PowerShell wrapper and `gate box run` hold the same file, so bridge and Rust runs are both seen.
- Files: `gate-args/src/boxcmd.rs`, `gate/src/boxside/{mod,inflight,payload}.rs`, `remote/payload.rs`, suite `boxside.rs`.
- Tests: `boxside::inflight_names_a_work_folder_whose_lock_is_held`, `boxside::inflight_counts_none_for_free_locks_or_a_missing_root`, `boxside::inflight_leaves_a_free_lock_takeable`, `args::box_takes_the_inflight_verb`.
- Docs: design.md `gate box` usage and The box side; ARCHITECTURE; test-map boxside rows.

### R4. Read live remote claims and tickets for the drain, and define the drain marker

- gate-state: a reader returning the live `gate-remote-slot-N` claims and live `remote:true` tickets of a v1 root (pid, since, command; the dashboard's liveness rule), plus the PowerShell root's remote claims. The marker `{state root}\remote-cutover.draining`, temp-then-rename, `{"v":1,"pid":…,"start":…,"since":…}`; a reader returns the drainer alive, dead or unreadable.
- Files: `gate-state/src/{drain,v1,lib}.rs`, `tests/state/drain.rs`, fixtures.
- Tests: `drain::a_live_v1_remote_claim_is_listed`, `drain::a_waiting_remote_ticket_is_listed`, `drain::a_dead_holders_remote_claim_is_not`, `v1::the_drain_marker_names_a_dead_drainer`.
- No ops-tower request: formats unchanged. Docs: ARCHITECTURE (drain reads PowerShell gates only), design.md State root Files table.

### R5. Add `gate remote-cutover`: write the flag, drain this PC's and every builder's routed runs, clear the marker

- New subcommand honouring `--state-dir`/`GATE_STATE_DIR` (so it can be rehearsed in a scratch root): write the marker then the flag (`gate: remote-cutover: flag written; routed runs now wait for the drain`); poll the PC every 2 s with `gate cutover`'s 5 s confirmation window, naming entries as it does; once clear, call `gate box inflight` on each builder in `remote.json`, saying `gate: remote-cutover: {host}: {n} run(s) in flight` every 10 s until all are 0, then check the PC once more. An unreachable builder or one without the verb is not drained and is named (`{host} cannot say what runs there (status {n}); run gate push`). Remove the marker: `gate: remote-cutover: done; routed runs use the Rust client`, then `…to go back to the bridge, delete {flag path}`. Ctrl+C leaves flag and marker and exits 0xC000013A; a rerun resumes.
- Files: `gate-args/src/{cli,request,usage}.rs`, `gate/src/{answer,remote_cutover}.rs`, suite `remote_cutover.rs`; seams `--test-poll-ms`, `--test-confirm-ms`, `--test-remote-ssh`.
- Tests: `remote_cutover::waits_on_a_live_routed_claim_and_names_it`, `…::waits_while_a_builder_reports_a_run_in_flight`, `…::a_builder_without_inflight_holds_the_drain_and_says_so`, `…::an_interrupt_leaves_the_flag_and_marker_and_a_rerun_resumes`, `…::done_removes_the_marker_and_keeps_the_flag`. Lower-bound waits only.
- Depends on R3, R4. Docs: design.md CLI and Cutover ("The remote cutover"); ARCHITECTURE Task Index row; test-map; GLOSSARY "drained (remote)".

### R6. A routed run that starts during the drain waits for it, then starts on the Rust client

- In the switch client's `ready()`: flag and live marker, say once `gate: waiting for the remote cutover's drain (pid {pid}) before routing`, hold no key lock, ticket or slot, poll every 1 s, then the Rust client once the marker is gone; flag gone while waiting, the bridge (rollback); marker whose drainer is dead or unreadable, `gate: the remote cutover's drain is not running; run gate remote-cutover again; running here` and fall back `client`; Ctrl+C, `gate: interrupted before the command started`, 0xC000013A. `ready()` gains the console and the interrupt.
- A marker that stands but whose opening is refused (a pending delete) keeps the run waiting; refusals that last 5 polls are said once, `gate: waiting for the remote cutover; cannot read {marker path} ({reason})`. The Ctrl+C journals `abandon` after `queue`, not a `release`.
- Files: `remote/{mod,bridge,switch}.rs`, `run.rs`, suite `routing.rs`, `bridge.rs` (`start_in_own_console` shared).
- Tests: `routing::a_routed_run_during_the_drain_waits_then_routes_on_the_rust_client`, `…::a_routed_run_whose_drainer_has_ended_runs_here_with_reason_client`, `…::deleting_the_flag_during_the_wait_returns_the_run_to_the_bridge`, `…::a_ctrl_c_during_the_drain_wait_ends_before_the_command`.
- Depends on R2, R4.

### R7. Say what the remote stop waits for after a Ctrl+C

- When a Ctrl+C leads to `Attempt::stop`, say `gate: stopping the command on {host} (up to {bound} s)` once (stderr and log) before the bounded call; a second Ctrl+C adds nothing; stall and ceiling stops say no such line. Extend `--test-remote-interrupt-at` with a second point, `stop`.
- Files: `remote/client.rs`, `gate-args` values/run, suite `remote_client.rs`.
- Tests: `remote_client::a_ctrl_c_says_once_what_the_stop_waits_for`, `remote_client::a_second_ctrl_c_during_the_stop_adds_nothing` (lower bound only).
- Depends on R2. Docs: design.md Routing "The stop" and "Ctrl+C".

## Part 2: delete the bridge

### R8. Move the routing suite's default-client cases onto the Rust client

- Cases that rely on the bridge as the default client move to the flag plus fake-ssh, or the scripted client; `with_no_test_client_a_routed_run_falls_back_with_reason_client` becomes `routing::with_no_ssh_a_routed_run_falls_back_with_reason_client`. Files: suite `routing.rs`, `support.rs`. Depends on R2.

### R9. Delete the remote.ps1 bridge, its shim, its install copy and its test seams

- `chosen` always builds the Rust client. Delete `remote/bridge.rs`, `tools/remote-bridge.ps1`, the install copy (install removes a leftover shim), the `--test-remote-script`/`--test-bridge-pwsh` seams and their JSON keys, suite `bridge.rs` and the related cases; `Interrupt::presses` only if nothing else uses it. Test: `install::an_install_removes_a_leftover_bridge_shim`. Depends on R8.
- Docs: design.md "Routing and the remote.ps1 bridge" (keep the anchor), the Bridge subsection and its mentions; ARCHITECTURE; test-map; GLOSSARY bridge entries; `docs/design/gate-remote-slot.md`.

### R10. Retire the remote-cutover flag and `gate remote-cutover`

- Remove the flag and marker reads, the drain wait, the subcommand and their paths; keep `gate box inflight` and the drain reader. Tests: `cli::remote_cutover_is_an_unknown_subcommand`, guard `routing::a_flag_left_in_the_state_root_changes_nothing`. Depends on R9.

## Runbook (orchestrator, after R7 is green)

0. The branch has main merged (GATE-146 part B included); full suite and both clippy runs pass.
1. Install on the PC from the branch head; `gate --version` shows `0.1.0+{head}`. No flag yet, routing unchanged. Rollback: reinstall main's commit.
2. `gate push`: every builder says installed `0.1.0+{head}` or already installed. Rollback: reinstall the previous commit and push.
3. Rehearse: `gate remote-cutover --state-dir {scratch root}` with no routed work running; it asks every builder through `inflight` and says `done`. Rollback: delete the scratch root.
4. Verified routed run through the scratch flag: `gate run --state-dir {scratch root} … -- dotnet build` on a solution in a repo `remote.json` lists; check no fallback, a remote take/release in the scratch journal, the sync-back applied, outputs installed, no nothing-back record. Once, a long routed command with Ctrl+C: the stop line appears once. Rollback: delete the scratch root.
5. Drain and flip: `gate remote-cutover` on the default root until `done`. Rollback at any point: delete the flag and `.draining` in the state root.
6. Confirm on the next ordinary routed runs: no `client` or `version` fallbacks, no nothing-back record after a build that brought its outputs back.
7. Watch one working session: fallback reasons, busy counts, stop lines.
8. After R9 merges to main: install from main and push; `remote-bridge.ps1` is gone from the install. Rollback: reinstall the part-1 commit (the flag still selects the Rust client; deleting it returns to the bridge).
9. After R10: reinstall, push, delete the leftover flag. The builders' old PowerShell wrappers and claude-config's remote scripts are GATE-11's.

GATE-109 (one ssh connection) follows the cutover.
