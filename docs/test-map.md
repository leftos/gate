# Test map

Which tests a change reaches, so a step's proving loop, and in `test mode: unstable` an item's last gate (the `gate-nextup` profile, "Gates"), runs only those, with cargo-nextest filtersets. Reach is read from the code (which source handles which subcommand, seam flag and output line, and which suite modules drive them), not measured by coverage. A file not listed here, or a change that spans several rows' "central" files, runs the full suite.

Suite = `crates/gate/tests/suite/` (binary `suite`, needs `--all-features`; test ids are `<module>::<case>`, `boxside::outputs::<case>` for the sub-module). Unit tests of the `gate` bin are inline `mod tests` (ids like `run::tests::...`, binary `gate`).

## 0. How the suite reaches src (the shape that decides everything)

- Every case builds a `Case` (`support.rs`, own `--state-dir`), runs the built `gate.exe` and the `fake-tool` helper. `main.rs` -> `answer::answer` (`crates/gate/src/answer.rs:18`) dispatches: Run -> `run::run`; StopTree -> `stop_tree.rs`; Cutover -> `cutover.rs`; Push -> `push.rs`; Box -> `boxside::answer`; `--version/--help` in-place.
- Every `Run` goes `answer.rs` -> `resolve.rs` -> `run.rs` (`run.rs:63`), which uses `acquire, covered, environment, journal, log, machine, nesting, remote, route, slots, wait, watch` (`run.rs:22-36`). So run.rs, answer.rs, main.rs, resolve.rs, console.rs, slots.rs, wait.rs, journal.rs, log.rs, environment.rs, watch.rs are on the path of nearly every case.
- Seam flags are declared in `crates/gate-args/src/{run,request,boxcmd,values}.rs` and parsed to the per-feature behaviour below.

## 1. Table: gate crate source areas -> suite modules

"Central" = change runs the whole suite (`binary(suite)`) plus the gate unit tests.

| Area (crates/gate/src/) | Suite modules (evidence) | Central? |
|---|---|---|
| main.rs | all (entry; `answer::answer`, exit-status cast) | CENTRAL |
| answer.rs | all; dispatch per subcommand, usage errors (`cli`), `--test-print-request` branch (`request`, `compat`, `cutover`, `harness`, `launcher`) | CENTRAL |
| run.rs | all runs: launch, hold, stall, ceiling, cores, journal, slots, critical, nesting, routing (the local `--no-build` drop, its lines and kinds, the nothing-back record's write and clear), bridge, remote_client, interrupt, diagnostics, priority, build_servers, pause | CENTRAL |
| no_build.rs | `routing` (the drop reasons, the obj check, the record's write, clear and unreadable cases); the unit tests inside (record read, solution readers, `Directory.Build.*` skip, listing parse) | no: routing |
| resolve.rs | `request` (state root from `--state-dir`/`GATE_STATE_DIR`, log path), `compat`, `harness`, `launch` (log inside state root); used by every run | CENTRAL (resolve.rs:`root`/`log` on every run) |
| console.rs | every case reads stdout/stderr lines; `launch`, `hold`, `cli` | CENTRAL |
| log.rs | `launch` (log, `.err`, marker scan, tail, verdict line), `diagnostics` (block before kill, tail), `stall`/`ceiling` (kill lines in log), `hold` | no: launch, diagnostics, stall, ceiling, hold, cli, interrupt, boxside (shares log rules? no, box has own) |
| environment.rs | `launch` (command environment), `nesting` (`GATE_SLOT_HELD` marker set/inherited), `harness` (cleared gate env), `request` | no: launch, nesting, harness, request, slots |
| slots.rs | `slots` (claim, counts, hashed names, takeover GATE-30), `critical`, `nesting`, `pause`, `priority`, `routing` (remote pool), `journal`, `hold`, `interrupt` (ticket delete); `ThisGate` identity feeds every run | CENTRAL (ThisGate/take are on every run) |
| acquire.rs | `slots` (take, abandoned-take line), `critical`, `interrupt` (queued abandon), `routing` | no: slots, critical, interrupt, routing, nesting, pause |
| wait.rs | `slots` (wait lines, ticket order), `critical` (barrier, reserved light), `nesting` (lend of a light parent's slot), `pause` (pause marker writer), `hold`, `routing` (remote pool wait), `interrupt` | CENTRAL (every queued/taking run passes `Wait`) |
| machine.rs | `critical` (critical mutex, drain, take all, snapshot, release), `priority` (critical raises servers), `interrupt`(`--test-interrupt-queued` on critical via critical.rs) | no: critical, priority, nesting (critical refused), interrupt |
| covered.rs | `nesting` (`covered` journal line, covered marker), `journal` | no: nesting, journal |
| nesting/binding.rs | `nesting` (marker bound to the parent's job: `a_gate_under_cmd_and_pwsh_in_its_parents_command_is_covered`, unbound and unjudged lines) | no: nesting, critical, pause, slots, journal |
| nesting.rs | `nesting` (marker parse, liveness, covered/refused/own), `critical` (refusal under marker), `pause`, `slots` | no, but run.rs calls it on every run (`admitted`, run.rs:154): nesting, critical, pause, slots, journal |
| pause.rs | `pause` (marker rewritten each pass), `stall`/`ceiling` (paused time via `watch.rs` PauseReader), `nesting` | no: pause, nesting, stall, ceiling, cores |
| journal.rs | `journal` (v1 lines, verdict, quoted command, rollover), plus any case asserting a journal line: `slots`, `critical`, `nesting`, `cores`, `routing`, `interrupt`, `cutover`?, `stall` (load-adjusted s), `bridge` | CENTRAL-ish (writes every run; run `journal`, `cores`, `nesting`, `slots`, `critical`, `routing`, `stall`, `interrupt`, `launch`, `hold`) |
| watch.rs | `stall`, `ceiling` (`--test-sampler-fail`, backstop), `cores`, `diagnostics`, `build_servers`, `priority`, `pause`, `interrupt` (`--test-interrupt-after-samples`), `hold`, `launch` (every command is watched); unit tests inside (an escaped build client, the sample's `escaped cpu`, the departed members' children in the diagnostics) | CENTRAL (every launched command is watched) |
| servers.rs | `build_servers` (client in job, server outside), `priority` (`--test-server-image`, `--test-servers-under`), `ceiling`, `diagnostics`, `launch`/`cli`/`install` pass `--test-servers-under` only to confine the scan | no: build_servers, priority, diagnostics, ceiling, stall, pause (sampling) |
| priority.rs | `priority` (lower/raise servers, OtherGates guard), `critical` (raise to High), `build_servers` | no: priority, critical |
| cutover.rs | `cutover` (`gate cutover`, flag, counts copy, drain, Ctrl+C, resume), `launcher` (hand-over after flag) | no: cutover, launcher |
| push.rs | `push` (`gate push`, `--test-push-ssh`, `--test-push-bound-ms`, fake-ssh) | no: push |
| stop_tree.rs | `stop_tree` (`gate stop-tree`) | no: stop_tree |
| stopping.rs | `stop_tree` (the checked open `gate stop-tree` shares), `stall` (the kill's escaped processes) | no: stop_tree, stall |
| escaped.rs | `stall` (`a_stall_kill_ends_a_packaged_app_the_command_started`, machine-dependent; `a_stall_kill_ends_a_child_outside_the_job_whose_launcher_has_ended`, machine-alone `#[ignore]`, with `stall::a_stall_kill_ends_a_child_outside_the_job_whose_launcher_exited_at_once` (machine-alone `#[ignore]`: the launcher ends before the first sample, so only the job's memory of it reaches the escapee), `ceiling::a_ceiling_kill_ends_a_child_outside_the_job_whose_launcher_has_ended` and `interrupt::a_ctrl_c_ends_a_child_outside_the_job_whose_launcher_has_ended`; the escapee cases that start it with `fake-tool spawn-away` under `--test-job-breakaway`; the unit tests inside), `cores` (`a_command_whose_child_left_its_job_counts_that_child_s_cores`), `interrupt` and `ceiling` (the other kills run it too) | no: stall, cores, interrupt, ceiling |
| burn.rs | `burn` (`gate burn`: start line, status, parent watch), `cli` (usage and refusals) | no: burn, cli |
| request_json.rs | `request`, `compat`, `cutover` (`--test-print-request`), `launcher`, `harness`, `support` | no: request, compat, cutover, launcher, harness |
| route.rs | `routing` (decisions, `--test-git`, remote.json), `remote_client`, `bridge`, `request` | no: routing, remote_client, bridge |
| bin/fake-tool.rs | helper for nearly every module (`support.rs`); signature change -> whole suite | CENTRAL (helper) |
| bin/fake-ssh.rs | `remote_client` (`--test-remote-ssh`), `push` (`--test-push-ssh`), `routing`? (no) | no: remote_client, push |
| remote/mod.rs | `routing` (scripted client `--test-remote-client`, fallback reason `client`), `bridge`, `remote_client` | no: routing, bridge, remote_client |
| remote/bridge.rs | `bridge` (`--test-remote-script`, `--test-bridge`), `routing` (5 uses of `--test-remote-script`), `request` (print only) | no: bridge, routing |
| remote/{client,parity,retry,transport,watchdog,syncback,outputs,install,payload,snapshot}.rs | `remote_client` (`--test-remote-transport local:`, `--test-local-sdks`, `--test-dotnet-limit-ms`, `--test-remote-ssh`, `--test-remote-snapshot`, `--test-remote-watch-call`, `--test-remote-timeout-ms`, `--test-remote-fetch-cap-bytes`, `--test-remote-outputs-budget-bytes`...), `syncback_quarantine` (the sync-back's quarantine, move and sweep), `syncback_conversions` (the checkout-filter refusal, the `.lfsconfig` and spelled names, the weighted apply cap; a `test-seams` build feeds `check-attr` about two paths at a time); `transport.rs` ssh words + `Ssh::find` also `push`; `payload.rs` is what `boxside` payload tests read through `local:` builder | no: remote_client, syncback_quarantine, syncback_conversions (+ push for transport.rs, + boxside for payload.rs/outputs.rs) |
| remote/{git,launch}.rs | `remote_client` (`--test-git`), `routing` (`--test-git`, `--test-git-limit-ms` and `--test-git-drain-ms` in `GIT_HANG_GUARDS`, route facts), `syncback_quarantine` (`Call.set`, the sync-back's git environment), `syncback_conversions` (`OUTPUT_CAP` bounds the `check-attr` slices), `request`, `boxside` (`sdk_listing` reads the preflight's SDK list) | no: remote_client, routing, syncback_quarantine, syncback_conversions (+ boxside for git.rs) |
| boxside/mod.rs, payload.rs, preflight.rs, boot.rs, run.rs, stop.rs | `boxside` (29 flag/verb uses; the case runs `gate box boot/preflight/run/stop`; `--test-dotnet-limit-ms` bounds the preflight's SDK read, as in `boxside::preflight_a_dotnet_that_does_not_answer_is_an_unreadable_sdk_list`), `remote_client` (builder is `gate box` via `local:`) | no: boxside, remote_client |
| boxside/heartbeat.rs, forward.rs | `boxside` (`--test-heartbeat-ms`), `remote_client` | no: boxside, remote_client |
| boxside/tools.rs | `boxside` (dotnet tool restore and its elapsed line, `boxside::box_run_*restore*` cases) | no: boxside |
| boxside/outputs.rs | `boxside` sub-module `boxside::outputs` (manifest, pack, prune, `outputs` verb, `--test-outputs-budget-bytes`), `remote_client` | no: boxside, remote_client |
| outputs/ (glob, walk, hash, manifest, cache, archive, names) | `boxside::outputs` (pack), `remote_client` (manifest the client sends, the hash cache it saves and reuses, the fetched archive's install), `syncback_quarantine` and `syncback_conversions` (names, through the sync-back's forbidden paths); unit tests inside | no: boxside, remote_client, syncback_quarantine, syncback_conversions |

## 2. Table: other crates and files

| Area | Suite modules that reach it | Own tests | Central? |
|---|---|---|---|
| gate-args lib.rs / cli.rs / values.rs / problem.rs / usage.rs | every case parses argv: all suite (parse errors `cli`, usage `cli`) | `gate-args::args` (top, run, boxcmd), unit | CENTRAL |
| gate-args request.rs / run.rs (seam parsing) | `request`, plus every module whose flags it parses (`--bring-tracked`: `remote_client`) | args tests `run` | CENTRAL (a seam rename breaks callers) |
| gate-args compat.rs | `compat`, `launcher`, `request`, `cli` | args `compat` | no |
| gate-args boxcmd.rs | `boxside`, `boxside::outputs`, `remote_client` | args `boxcmd` | no |
| gate-args cmdline.rs / batch.rs / program.rs | `launch` (quoting, refused batch word, `.cmd`), `journal` (`command_text`), `boxside`, `remote_client` | args `quoting`, `batch`, `program` | no |
| gate-router (decide, rewrite, key, like, config) | `routing` (imports `DROPPED_NO_BUILD_LINE`, `NO_OUTPUTS_LINE`; `decide.rs`'s dotnet readers also drive the local drop and the record), `remote_client` (`repo_name`), `bridge`, `request` | `gate-router::router` (`decide`: the wrapped form, the drop verbs, the targets and settings) | no |
| gate-state v1.rs / root.rs / model.rs / lib.rs | everything (claim/ticket/counts/root hash/kernel names): CENTRAL. Suite itself reads `v1` in `critical, interrupt, journal, launch, nesting, pause, priority, slots`; `v1.rs`'s nothing-back record (path, write, strict read) is also read in `routing`, and its outputs cap record (`remote/outputs.rs`) in `remote_client` and the `remote::client` unit tests | `gate-state::state` (`v1`: the record's shape and strict read) | CENTRAL |
| gate-state journal_writer.rs / verdict.rs | `journal` + any journal assertion: `cores, nesting, routing, slots, critical, stall, launch, interrupt, bridge` | state `journal_write`, `verdict` | no, but wide (writes on every run) |
| gate-state archive.rs | `journal` (rollover and archive with concurrent appenders) | state `archive` | no |
| gate-state cutover.rs / drain.rs / counts.rs | `cutover`, `launcher` (gate/src/cutover.rs imports `copy_slot_counts_to_v1, powershell_gates, write_cutover_flag`); `counts.rs`'s `write_replacing` (the temp names) also writes the v1 files, so a change there adds `routing` | state `cutover`, `counts`; a unit test inside `counts.rs` for the temp names | no |
| gate-state tests/state/fixtures/ (captured files, `capture-*.ps1`) and tests/state/fixtures.rs | none (the fixtures are captured from `gate.exe` and the PowerShell gate by script and only read by the state tests) | state `fixtures` (incl. `remote_run_journal_from_gate_exe_is_read_into_runs_and_a_split_summary`) | no |
| gate-state claims.rs / remote.rs / state.rs / win32.rs / journal.rs (reader) | NOT imported by gate src (ops-tower-only, ARCHITECTURE.md Layers; except `normalize_command` from journal.rs, used by `slots.rs`:642 and journal.rs's command text -> `journal`, `slots`) | state tests | no (journal.rs: `journal`, `slots`) |
| gate-watchdog share.rs / watch.rs | `stall`, `ceiling`, `cores`, `pause`, `diagnostics`, `interrupt`, `build_servers`, `priority` | unit | no (wide) |
| gate-watchdog cores.rs | `cores`, `journal` (cores fields) | unit | no |
| gate-watchdog servers.rs | `build_servers`, `priority`, `diagnostics`, `ceiling` | unit | no |
| gate-watchdog diagnostics.rs | `diagnostics`, `stall`, `ceiling`, `build_servers` (`--test-diagnostics-budget-ms` cuts every block) | unit | no |
| gate-watchdog heartbeat.rs | `boxside`, `boxside::outputs`, `remote_client`, `critical`/`slots`/`pause` only via `--test-heartbeat-ms` flag (the wait heartbeat is gate/src/wait.rs, not this) | unit | no |
| gate-win32 job.rs / create.rs / launch.rs / lib.rs / mutex.rs | all (started command, job, slot mutexes): CENTRAL | `gate-win32` tests (launch, mutex, event, interrupt, `io` for `Job::written_bytes`...; unit tests of the completion port: `Job::remember` and its thread, `departed`, `forget`, among them `a_member_that_ended_before_any_look_keeps_its_exact_window_and_no_handle`, `a_member_found_gone_takes_no_one`, `a_member_put_in_the_job_after_its_start_can_have_started_a_process_only_since_the_job_was_created`, `an_exit_message_closes_the_handle_of_that_member_alone`, `the_members_a_memory_could_not_read_are_named_up_to_a_few_and_counted_after` and `dropping_a_remembering_job_ends_its_drain_thread_and_releases_what_it_shares`; in `crates/gate/src/escaped.rs`, `a_look_whose_snapshot_listed_nothing_forgets_no_member` and `a_child_whose_start_is_unknown_of_a_stranger_holding_a_remembered_member_s_pid_is_not_counted`) | CENTRAL |
| gate-win32 piped.rs / console.rs | `boxside` (`--test-box-console`, `--test-box-park-launch`), `bridge`, `remote_client` | win32 `launch` | no |
| gate-win32 priority.rs | `priority`, `critical`, `build_servers` | win32 `priority` | no |
| gate-win32 tree.rs | `stop_tree`, `stall` (escaped processes), `interrupt`, `boxside` (stop), `burn` (`WaitableProcess`), `nesting` (`OpenedJob::holds`) | win32 `membership` | no |
| gate-win32 interrupt.rs / event.rs | `interrupt`, `critical`, `cutover`, `routing`, `remote_client`, `boxside` (stop event), `slots` (wake) | win32 `interrupt`, `interrupt_again`, `event` | no |
| gate-win32 file.rs | `syncback_quarantine`, `syncback_conversions`, `remote_client` (`move_no_replace` moves a sync-back's fetched objects) | win32 `file` | no |
| gate-win32 device.rs | `routing` (subst drives in route facts) | - | no |
| gate-win32 times.rs / memory.rs | `stall`, `ceiling`, `cores`, `pause` (samples) | - | no |
| proc-table | `stop_tree`, `stall` (`parent_pid_of`) (imports `Win32ProcessTable`), `cutover` (`command_line_of`), `diagnostics`, `priority` (via servers/OtherGates), `stall`/`cores` (`ProcessTree::children` through the escaped walk) | proc-table unit | no |
| tools/gate.ps1 | `launcher` (runs the checked-in launcher), `cutover` (hand-over), `checkout` (`repo_file`) | - | no |
| tools/install.ps1 | `install`, `checkout` | - | no |
| tools/remote-bridge.ps1 | `bridge`, `routing` (via `--test-remote-script`), `install` (also installed) | - | no |
| suite support.rs / harness.rs / main.rs / fake tools | all suite (support.rs is `Case`): CENTRAL for suite; `harness` self-tests | - | CENTRAL |
| suite module file `X.rs` | its own module only (`test(/^X::/)`), plus `boxside/outputs.rs` -> `boxside::` | - | no |
| Cargo.toml (workspace), Cargo.lock, crates/gate/Cargo.toml, rust-toolchain, .cargo/, clippy config | whole workspace | - | CENTRAL |
| docs/, README | none | - | none (no tests) |

Note on `checkout.rs`: it scans the suite sources for paths derived from the checkout (`crates/gate/tests/suite/checkout.rs` header); any new/edited suite file is reached by `test(/^checkout::/)`. Add it to every suite-edit filter.

## 3. Profile-ready filtersets (nextest 0.9.148)

Let `S` = `binary(suite)`, `U` = `package(gate) and not binary(suite)` (gate bin unit tests; the `fake-*` bins have no tests of note). Inline unit tests of a src module `m` are ids starting `m::` (e.g. `test(/^run::/)`).

Always add `checkout` to any change that touches a suite file or adds a seam.

| Change | Filterset |
|---|---|
| CENTRAL (main.rs, answer.rs, run.rs, resolve.rs, console.rs, slots.rs, wait.rs, watch.rs, journal.rs, environment.rs, bin/fake-tool.rs, suite support.rs, workspace Cargo files, gate-args lib/cli/request/run/values/problem/usage, gate-state v1/root/model/lib, gate-win32 job/create/launch/lib/mutex) | no `-E`, or `-E 'package(gate) or rdeps(<crate>)'` (everything in the gate package; add the crate's own tests) |
| log.rs | `-E 'binary(suite) and test(/^(launch\|diagnostics\|stall\|ceiling\|hold\|cli\|interrupt)::/) or (U and test(/^log::/))'` |
| environment.rs (if not treated central) | `-E 'binary(suite) and test(/^(launch\|nesting\|harness\|request\|slots)::/) or (U and test(/^environment::/))'` |
| acquire.rs | `-E 'binary(suite) and test(/^(slots\|critical\|interrupt\|routing\|nesting\|pause)::/)'` |
| machine.rs | `-E 'binary(suite) and test(/^(critical\|priority\|nesting\|interrupt)::/) or (U and test(/^machine::/))'` |
| covered.rs | `-E 'binary(suite) and test(/^(nesting\|journal)::/) or (U and test(/^covered::/))'` |
| nesting.rs, nesting/binding.rs | `-E 'binary(suite) and test(/^(nesting\|critical\|pause\|slots\|journal)::/) or (U and test(/^nesting::/))'` |
| pause.rs | `-E 'binary(suite) and test(/^(pause\|nesting\|stall\|ceiling\|cores)::/) or (U and test(/^pause::/))'` |
| servers.rs | `-E 'binary(suite) and test(/^(build_servers\|priority\|diagnostics\|ceiling\|stall)::/) or (U and test(/^servers::/))'` |
| priority.rs | `-E 'binary(suite) and test(/^(priority\|critical\|build_servers)::/) or (U and test(/^priority::/))'` |
| cutover.rs | `-E 'binary(suite) and test(/^(cutover\|launcher)::/) or (U and test(/^cutover::/))'` |
| push.rs, bin/fake-ssh.rs (push side) | `-E 'binary(suite) and test(/^(push\|remote_client)::/)'` (fake-ssh also serves remote_client) |
| stop_tree.rs | `-E 'binary(suite) and test(/^stop_tree::/)'` |
| stopping.rs | `-E 'binary(suite) and test(/^(stop_tree\|stall)::/) or (U and test(/^stopping::/))'` |
| escaped.rs | `-E 'binary(suite) and test(/^(stall\|cores\|interrupt\|ceiling)::/) or (U and test(/^escaped::/))'` |
| burn.rs | `-E 'binary(suite) and test(/^(burn\|cli)::/) or (U and test(/^burn::/))'` |
| request_json.rs | `-E 'binary(suite) and test(/^(request\|compat\|cutover\|launcher\|harness)::/)'` |
| no_build.rs | `-E 'binary(suite) and test(/^routing::/) or (U and test(/^(no_build\|run)::/))'` |
| route.rs | `-E 'binary(suite) and test(/^(routing\|remote_client\|bridge\|request)::/) or (U and test(/^route::/))'` |
| remote/mod.rs | `-E 'binary(suite) and test(/^(routing\|bridge\|remote_client)::/) or (U and test(/^remote::/))'` |
| remote/bridge.rs, tools/remote-bridge.ps1 | `-E 'binary(suite) and test(/^(bridge\|routing\|install)::/) or (U and test(/^remote::bridge::/))'` |
| remote/{client,retry,watchdog,syncback,snapshot,outputs,install,payload,git,launch,transport}.rs | `-E 'binary(suite) and test(/^(remote_client\|routing\|syncback_quarantine\|syncback_conversions)::/) or (U and test(/^remote::/))'`; add `push` for transport.rs, `boxside` for payload.rs/outputs.rs/git.rs |
| boxside/{mod,payload,preflight,boot,run,stop,forward,heartbeat,tools}.rs | `-E 'binary(suite) and test(/^(boxside\|remote_client)::/) or (U and test(/^boxside::/))'` |
| boxside/outputs.rs, outputs/* | `-E 'binary(suite) and test(/^(boxside::outputs\|remote_client)::/) or (U and test(/^(boxside::outputs\|outputs)::/))'` |
| tools/gate.ps1 | `-E 'binary(suite) and test(/^(launcher\|cutover\|checkout)::/)'` |
| tools/install.ps1 | `-E 'binary(suite) and test(/^(install\|checkout)::/)'` |
| suite module `X.rs` only | `-E 'binary(suite) and test(/^(X\|checkout)::/)'` |
| gate-args compat.rs | `-E 'package(gate-args) or (binary(suite) and test(/^(compat\|launcher\|request\|cli)::/))'` |
| gate-args boxcmd.rs | `-E 'package(gate-args) or (binary(suite) and test(/^(boxside\|remote_client)::/))'` |
| gate-args cmdline/batch/program.rs | `-E 'package(gate-args) or (binary(suite) and test(/^(launch\|journal\|boxside\|remote_client)::/))'`; also `gate-state` `journal` tests mirror cmdline (ARCH Task Index "history key splits"): add `package(gate-state)` |
| gate-router (any) | `-E 'package(gate-router) or (binary(suite) and test(/^(routing\|remote_client\|bridge\|request)::/)) or (U and test(/^(route\|remote)::/))'` (do NOT use `rdeps(gate-router)`: it pulls the full gate package) |
| gate-state journal_writer/verdict | `-E 'package(gate-state) or (binary(suite) and test(/^(journal\|cores\|nesting\|slots\|critical\|routing\|stall\|launch\|interrupt\|bridge)::/))'` |
| gate-state archive | `-E 'package(gate-state) or (binary(suite) and test(/^journal::/))'` |
| gate-state cutover/drain/counts | `-E 'package(gate-state) or (binary(suite) and test(/^(cutover\|launcher)::/))'` |
| gate-state fixtures and fixtures.rs | `-E 'package(gate-state)'` |
| gate-state claims/remote/state/win32 (ops-tower-only readers) | `-E 'package(gate-state)'` (no gate suite module reads them; gate src does not import them) |
| gate-state journal.rs (reader, `normalize_command`) | `-E 'package(gate-state) or (binary(suite) and test(/^(journal\|slots)::/))'` |
| gate-state v1/root/model/lib | full: `-E 'rdeps(gate-state)'` (includes whole gate suite; accurate, the binary uses v1 on every run) |
| gate-watchdog share/watch | `-E 'package(gate-watchdog) or (binary(suite) and test(/^(stall\|ceiling\|cores\|pause\|diagnostics\|interrupt\|build_servers\|priority)::/))'` |
| gate-watchdog cores | `-E 'package(gate-watchdog) or (binary(suite) and test(/^(cores\|journal)::/))'` |
| gate-watchdog servers | `-E 'package(gate-watchdog) or (binary(suite) and test(/^(build_servers\|priority\|diagnostics\|ceiling)::/))'` |
| gate-watchdog diagnostics | `-E 'package(gate-watchdog) or (binary(suite) and test(/^(diagnostics\|stall\|ceiling)::/))'` |
| gate-watchdog heartbeat | `-E 'package(gate-watchdog) or (binary(suite) and test(/^(boxside\|remote_client)::/))'` |
| gate-win32 job/create/launch/lib/mutex | `-E 'rdeps(gate-win32)'` |
| gate-win32 piped/console | `-E 'package(gate-win32) or (binary(suite) and test(/^(boxside\|bridge\|remote_client)::/))'` |
| gate-win32 priority | `-E 'package(gate-win32) or (binary(suite) and test(/^(priority\|critical\|build_servers)::/))'` |
| gate-win32 tree | `-E 'package(gate-win32) or (binary(suite) and test(/^(stop_tree\|stall\|interrupt\|boxside\|burn\|nesting)::/))'` |
| gate-win32 interrupt/event | `-E 'package(gate-win32) or (binary(suite) and test(/^(interrupt\|critical\|cutover\|routing\|remote_client\|boxside\|slots)::/))'` |
| gate-win32 file | `-E 'package(gate-win32) or (binary(suite) and test(/^(syncback_quarantine\|syncback_conversions\|remote_client)::/))'` |
| gate-win32 device | `-E 'package(gate-win32) or (binary(suite) and test(/^routing::/))'` |
| gate-win32 times/memory | `-E 'package(gate-win32) or (binary(suite) and test(/^(stall\|ceiling\|cores\|pause)::/))'` |
| proc-table | `-E 'package(proc-table) or (binary(suite) and test(/^(stop_tree\|stall\|cutover\|diagnostics\|priority)::/))'` (`rdeps(proc-table)` would pull gate-state and all of gate) |
| docs only | no tests |

(In a real `-E`, the pipe inside a regex is a plain `|`; the backslashes above only keep the Markdown table intact. Nextest regexes use `/.../`. Precedence: `and` binds tighter than `or`, add parentheses as written.)

## 4. Rules for the profile

1. Any file not listed: run the full suite (`cargo nextest run --workspace --all-features --cargo-profile test-release`).
2. A change touching a seam flag name or its parse (gate-args `run.rs/request.rs/boxcmd.rs/values.rs`) runs the module(s) passing it (grep the flag in the suite) plus `request`; a renamed flag is central.
3. `rdeps(<crate>)` is accurate only for gate-state v1/root/model, gate-win32 core, and central files; for the rest use `package(<crate>)` plus the module list above, since `rdeps` on any crate that `gate` depends on pulls the whole `gate` package including `binary(suite)`.
4. Two plain-source hazards: `outputs/` has `#[expect(dead_code)]` outside tests (main.rs:20-28): a change that makes a non-test build warn will fail clippy, so the checkpoint run includes clippy `--all-features` and a build without features.
5. Checkpoint: the full workspace.
