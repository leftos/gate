# Per-run build servers (GATE-190)

Part A (GATE-196, the run id, the per-run settings and the run end) is done.

The plan for `feat/per-run-servers`: no gate run shares build servers with another, on this PC or the builder. The owner ruling and the orchestrator decisions (the per-run settings, the run id, ending the whole job at run end, what is removed, the brief order A, B, C1, C2) are in GATE-190's description and win over the open decisions below, which they settle. This file holds the measurements, the mechanisms with their sources, and the inventory of code that goes.

Investigation only; no tracked file changed in any repo. Sources: GATE-190 and GATE-187 (description and comments, read through Linear), the gated repos' docs and scripts, Microsoft Learn, the dotnet/msbuild, dotnet/sdk and dotnet/roslyn sources (quotes below, pins: msbuild c1c99ee0, sdk 680d5f1e, roslyn `main`), the local SDK 10.0.401 targets, a code inventory of the gate at `806a42d` (main), and 28 gated builds plus one probe on this PC.

## 1. Toolchains and how a server is kept inside the run

### What the gated repos run

| Repo | Gated commands (from their docs and scripts) |
|---|---|
| yaat | `dotnet build`, `dotnet test`, `dotnet run --project ...`, `pwsh tools/test-all.ps1` (AGENTS.md:26,60-68; CLAUDE.md:233,270; `.claude/skills/*`) |
| delve-the-dungeon | `dotnet build Delve.slnx`, `dotnet test`, `dotnet csharpier`, `Godot_console.exe` (`--import`, `--build-solutions`, exports, scratches), `pwsh tools/test-all.ps1`, which runs nested gated `dotnet build` Debug then Release (tools/test-all.ps1:156,164) |
| godot-mcp | everything through `run.ps1` (`dotnet build GodotMcp.slnx`, `dotnet test`, `dotnet publish`, `dotnet format`, `dotnet csharpier`, Godot headless runs, `uv run pytest`); its CsProbe itests pass `-p:UseSharedCompilation=false` and `MSBUILDDISABLENODEREUSE=1` themselves (tests/GodotMcp.TestSupport/CsProbeProject.cs:163,176) |
| ops-tower | `cargo fmt/clippy/deny/test`, `node` (oxfmt, oxlint) from prek.toml:40-95 |

Build servers in play: MSBuild worker nodes, the MSBuild server, the Roslyn compiler server (VBCSCompiler, which also hosts Razor as a source generator), and Godot builds, which run `dotnet` underneath. Cargo/rustc keep no server, and no sccache is configured here (`RUSTC_WRAPPER` unset, no `rustc-wrapper` in `~/.cargo/config*`, no `sccache` on PATH). Node tools keep none.

delve already sets `MSBUILDDISABLENODEREUSE=1` and `UseSharedCompilation=false` around Godot builds (dtd.ps1:229-238, tools/release/Export-Client.ps1:192-201, tools/worktree/New-Worktree.ps1:319-328), because "`Godot_console.exe` is a job-object wrapper that returns only when every process in its job has exited ... or the MSBuild worker nodes and the compiler server born inside the job hold the gate for their idle life, ten to fifteen minutes" (docs/DEVELOPMENT.md:301).

### Every server starts inside the launching job

None of the launchers asks for breakaway, so a server a job member starts is a member of that job.

- MSBuild `NodeLauncher.StartProcessWindows` (worker nodes, and the MSBuild server through `MSBuildClient`): flags are only `NORMAL_PRIORITY_CLASS | CREATE_NEW_CONSOLE`, `NORMAL_PRIORITY_CLASS`, `CREATE_NEW_CONSOLE` or `CREATE_NO_WINDOW`, plus `CREATE_UNICODE_ENVIRONMENT`. https://github.com/dotnet/msbuild/blob/c1c99ee05981494f34191590eb8ec2e47dba1958/src/Build/BackEnd/Components/Communications/NodeLauncher.cs
- Roslyn `BuildServerConnection`: `uint dwCreationFlags = NORMAL_PRIORITY_CLASS | CREATE_NO_WINDOW;` then `CREATE_UNICODE_ENVIRONMENT`, `bInheritHandles: false`. https://github.com/dotnet/roslyn/blob/main/src/Compilers/Shared/BuildServerConnection.cs
- The gate's job allows no breakaway (`docs/design.md:916`, `crates/gate/src/run.rs:1449-1453` uses `Job::create`; only the `--test-job-breakaway` seam allows it).
- Measured: in all nine per-run builds below, the per-run VBCSCompiler was in a job (`IsProcessInJob(h, NULL)` true) and still alive after the gate had exited. The gate's job is not kill-on-close and the gate does not end it on a clean exit, so a per-run server outlives its run by its idle keepalive today.

### Mechanisms

| Mechanism | How it is set from the environment | Effect | When the server ends | Source |
|---|---|---|---|---|
| MSBuild node reuse off | `MSBUILDDISABLENODEREUSE=1` (the gate sets it today, `crates/gate/src/environment.rs:68`) | Disables reuse across invocations: nodes serve one `dotnet`/`msbuild` invocation | Exits when that build ends; no job kill needed | XMake.cs `ProcessNodeReuseSwitch`: `if (Environment.GetEnvironmentVariable("MSBUILDDISABLENODEREUSE") == "1") ... enableNodeReuse = false;` (https://github.com/dotnet/msbuild/blob/c1c99ee05981494f34191590eb8ec2e47dba1958/src/MSBuild/XMake.cs). Learn: "`-nodeReuse:{value}` ... **False**. Nodes don't remain after the build completes." (https://learn.microsoft.com/visualstudio/msbuild/msbuild-command-line-reference#switches) |
| MSBuild node handshake salt | `MSBUILDNODEHANDSHAKESALT=<run id>` (the box sets it to the work-folder key today, `crates/gate/src/boxside/run.rs:496`) | Keeps reuse, private to whoever shares the salt: a per-run value gives reuse within the run only | Idles 15 min (`MSBUILDNODECONNECTIONTIMEOUT` overrides, in ms), else the job kill | Handshake.cs: `string handshakeSalt = Environment.GetEnvironmentVariable("MSBUILDNODEHANDSHAKESALT") ?? "";` ... `int salt = CommunicationsUtilities.GetHashCode($"{handshakeSalt}{toolsDirectory}{changeWaveSalt}");` (https://github.com/dotnet/msbuild/blob/c1c99ee05981494f34191590eb8ec2e47dba1958/src/Framework/BackEnd/Handshake.cs). CommunicationsUtilities.cs: `private const int DefaultNodeConnectionTimeout = 900 * 1000; // 15 minutes` and `GetValueAsInt32OrDefault("MSBUILDNODECONNECTIONTIMEOUT", DefaultNodeConnectionTimeout)` (https://github.com/dotnet/msbuild/blob/c1c99ee05981494f34191590eb8ec2e47dba1958/src/Framework/BackEnd/CommunicationsUtilities.cs) |
| MSBuild server | `DOTNET_CLI_USE_MSBUILD_SERVER=0` (the gate sets it, environment.rs:69); `MSBUILDUSESERVER=0` | Disables it | n/a | Learn: "Starting with the .NET 11 SDK, MSBuild Server is enabled by default ... In earlier .NET SDKs, MSBuild Server was off by default"; "after an idle time of 15 minutes, the server shuts down"; "MSBuild Server is a form of node reuse, so disabling node reuse also disables the server" (https://learn.microsoft.com/visualstudio/msbuild/msbuild-server). SDK: `if (string.IsNullOrEmpty(Env.GetEnvironmentVariable("MSBUILDUSESERVER"))) { EnvironmentVariable("MSBUILDUSESERVER", UseMSBuildServer ? "1" : "0"); }` (https://github.com/dotnet/sdk/blob/680d5f1e3c8cab8342a5e939389696deea0a9286/src/Cli/Microsoft.DotNet.Cli.Utils/MSBuildForwardingAppWithoutLogging.cs): an inherited `MSBUILDUSESERVER` wins over the gate's `DOTNET_CLI_USE_MSBUILD_SERVER=0`. Its pipe is `MSBuildServer-{handshake.ComputeHash()}` from the same `Handshake` base, so the salt should make it per-run too (inferred from the shared base class, not tested) |
| Roslyn compiler server, per-run pipe | `SharedCompilationId=<run id>` | Keeps the server and reuse within the run; no other run can connect | Idles 10 min (no environment knob), else the job kill | Local SDK `{dotnet root}\sdk\10.0.401\Roslyn\Microsoft.CSharp.Core.targets:160` `SharedCompilationId="$(SharedCompilationId)"`; ManagedCompiler.cs: `var pipeName = !string.IsNullOrEmpty(SharedCompilationId) ? SharedCompilationId : BuildServerConnection.GetPipeName(clientDirectory);` (https://github.com/dotnet/roslyn/blob/main/src/Compilers/Core/MSBuildTask/ManagedCompiler.cs); default pipe input `$"{userName}.{isAdmin}.{clientDirectory}"` (BuildServerConnection.cs); ServerDispatcher.cs: `internal static readonly TimeSpan DefaultServerKeepAlive = TimeSpan.FromMinutes(10);`, overridable only by the `keepalive` app setting or a client request. Measured here: the server started as `VBCSCompiler.exe "-pipename:gate190-probe-65672"` |
| Roslyn compiler server off | `UseSharedCompilation=false` | Disables it: each compile is a fresh `csc` | Nothing outlives the build | Local targets line 171 `UseSharedCompilation="$(UseSharedCompilation)"`; Microsoft.Managed.Core.targets defaults it to `true` when empty |
| Environment as properties | any of the two Roslyn names above | MSBuild reads environment variables as properties; a project's own definition wins | | Learn: "You can reference environment variables in project files the same way as build properties ... If the project contains a property definition that has the same name as an environment variable, the property in the project overrides the value of the environment variable." (https://learn.microsoft.com/visualstudio/msbuild/walkthrough-using-msbuild#environment-variables). No gated repo defines either property (a search of every gated repo); godot-mcp's `-p:UseSharedCompilation=false` is a global property and wins |
| Razor | none needed | Runs as a source generator inside the compiler server in current SDKs | as above | sdk `Sdk.Razor.CurrentVersion.targets`: `<UseRazorSourceGenerator Condition="'$(Language)' == 'C#' AND '$(UseRazorSourceGenerator)' == '' ">true</UseRazorSourceGenerator>` and the rzc `Compilation.targets` imported only when it is not `true` |
| `dotnet build-server shutdown` | n/a | Learn: "Shuts down build servers that are started from dotnet. By default, all servers are shut down." Options `--msbuild`, `--razor`, `--vbcscompiler` (https://learn.microsoft.com/dotnet/core/tools/dotnet-build-server) | It finds servers by their default names (sdk `BuildServerProvider.cs`), so a per-run pipe or a salted node is probably not found: inferred, not tested (I did not shut the shared server down, see 2) | |
| sccache (not in use) | `SCCACHE_SERVER_PORT`, `SCCACHE_IDLE_TIMEOUT` | Per-run port gives a per-run server | "terminate after (by default) 10 minutes of inactivity" (https://github.com/mozilla/sccache/blob/main/README.md) | |

Not verified: whether `vstest`/`testhost` processes outlive a `dotnet test` (none were seen after the runs here, but no test run was measured), and the Razor rzc launcher's flags (legacy path only).

### Priority of an in-job server

Roslyn creates VBCSCompiler with `NORMAL_PRIORITY_CLASS` (quote above). Measured under an ordinary heavy gate (BelowNormal command), the per-run server ran at **BelowNormal** (probe: `server pid 76640 priority BelowNormal`). Today's priority pass leaves an ordinary gate's in-job servers alone (`crates/gate/src/priority.rs:128-130`), so the class did not come from the gate's lowering; why the explicit Normal flag did not hold is not resolved (the SDK 10.0.401 compiler may differ from roslyn `main`). Under a critical gate the command is High and its children start at Normal (`docs/research/2026-10-09-powershell-gate-inventory.md:99`, K4; not measured in this repo), which is what today's in-job raise (`priority.rs:141`, suite `priority.rs:515 covered_gate_raises_its_own_jobs_server`) exists for.

## 2. Cold-start cost on this PC

**Setup.** yaat (`yaat.slnx`, 24 projects, SDK 10.0.401) in a scratch worktree (cut from `53118c35d`, removed after). Every build ran `pwsh -NoProfile -File tools/gate.ps1 -Log .tmp/m-<variant>-<case>-<n>.log -TimeoutSeconds 900 -Slot heavy -Local -- dotnet build yaat.slnx [--no-incremental]`. Figures are the journal's `release` records (`wallSeconds`, `adjustedSeconds`, `avgCores`), joined in run order.

Variants, interleaved per run (shared, per-run, no server) so machine-load drift hits all three:
- **shared**: today's settings (the gate's `MSBUILDDISABLENODEREUSE=1`, `DOTNET_CLI_USE_MSBUILD_SERVER=0`), the shared VBCSCompiler warm (a warm-up build ran first).
- **per-run**: plus `SharedCompilationId=gate190-<case>-<n>-<pid>`, so each build starts a cold server of its own; the script killed it after each run.
- **no server**: plus `UseSharedCompilation=false`.

Cases: **noop** (nothing changed), **touch** (one file in `src/Yaat.Sim` touched, so that project recompiles), **rebuild** (`--no-incremental`, every project compiled).

The shared compiler server was never shut down: the per-run variant gives a cold server by construction, and other sessions' gates were running throughout. The machine was heavily loaded by other sessions' heavy gates (cargo nextest and clippy runs): machine free cores ranged 2.5 to 8.4 of 16, so wall times are noisy and the load-adjusted figure (whole-run CPU share discounted, as the ceiling counts it) is the one to compare.

Load-adjusted seconds, three runs each (median, min-max):

| Case | shared (today) | per-run server | no server |
|---|---|---|---|
| noop | 6.6 (6.6-7.4) | 5.9 (5.8-6.8) | 5.5 (5.5-6.1) |
| touch, one project | 11.9 (9.7-15.8) | 15.4 (14.6-16.1) | 15.8 (15.6-18.9) |
| rebuild, 24 projects | 38.1 (34.1-43.2) | 52.5 (42.2-58.0) | 59.1 (58.4-64.6) |

Wall seconds (median): noop 21.7 / 14.6 / 13.9; touch 32.8 / 34.0 / 37.1; rebuild 108.5 / 129.3 / 155.3. CPU seconds (avgCores x wall, median): rebuild 325 / 634 / 755; touch 156 / 211 / 208. The shared variant's CPU counts the shared server only while a build client is in the job and includes other sessions' work on it, so its CPU figure is a rough floor, not a like-for-like.

Reading:
- A no-op build starts no compiler server (no per-run server existed after any per-run noop run), so it pays nothing.
- One cold server costs about 3.5 s load-adjusted on a one-project incremental build (+29%), the same as no server at all: a single compile cannot amortise the start.
- On a full rebuild the cold server costs about 14 s load-adjusted (+38%) and roughly doubles the CPU; no server at all costs about 21 s (+55%). Reuse within the run is worth about 7 s on a 24-project rebuild.
- Nested runs multiply it: delve's test-all runs two nested gated builds (Debug then Release, tools/test-all.ps1:156,164); with a server per nested gate each pays the cold start.

**The builder (not measured).** Measuring there would take: a work folder of a .NET repo on the builder (the box's `Work` folders under its output root), an ssh session with the builder's own `gate.exe`, and the same interleaved script run locally on the builder with `gate run --local`, comparing today's box environment (no `MSBUILDDISABLENODEREUSE`, `MSBUILDNODEHANDSHAKESALT=<work-folder key>`, shared compiler server) against `MSBUILDNODEHANDSHAKESALT=<run id>` (or node reuse off) plus `SharedCompilationId=<run id>`, three runs each of noop, touch and rebuild, with the builder idle of other routed runs. The builder loses more than this PC does: its MSBuild nodes are warm across runs in one work folder today (`docs/design.md:1489-1491`), so a per-run run pays a cold node start as well as a cold compiler.

## 3. What becomes dead in the gate

Full line-level inventory: [the appendix below](#appendix-dead-code-inventory) (read at `806a42d`). Summary with path:line:

**Pure crate `gate-watchdog`**
- `crates/gate-watchdog/src/servers.rs`: `is_build_server` (11-16), `is_compiler_server` (23-25), `is_compiler_image` (30-32), `is_build_client` (38-43), `PriorityClass` (48-61), `server_target` (69-78), `server_left` (85-90), `holds` (109-111); tests at 119, 142, 162, 178, 213, 236, 253. Survives: `is_wsl_command` (95-98) and `strip_exe` (101-106) with test 193, which move out of the file (caller `crates/gate/src/watch.rs:17,768`). Re-exports `lib.rs:20-22`.
- `crates/gate-watchdog/src/watch.rs`: `Sample.new_servers`/`shared_servers`/`build_client` (50-56); `Watch` fields 83-84, 94-95 (inits 136, 137, 145); `sample` 168-173, 191, 199-200; the server term of `progressed` (215-216); `final_cpu`'s server parameters (258-279; caller `crates/gate/src/watch.rs:381`); `Outside` fields 480-481; `own_gained`'s `client` argument and server term (488-495); `CpuMove.continued` (438-439, 463). Tests 1249, 1260, 1281, 1302, 1310, 1543 die; 825 and 843 use `server_tick` only as a progress source and are rewritten. `measure_cpu` (450-469), `share.rs` and `cores.rs` survive unchanged in shape.
- `crates/gate-watchdog/src/diagnostics.rs`: `ServerFacts` (50-62), `SampleFacts` server fields (95-102), `Report.servers` (121-122), `push_servers` (208-227) with the lines `gate:   build servers: none` and `gate:   build server <pid> <name>: cpu ...`, and the `new servers cpu`, `shared servers cpu`, `build client in job` parts of `sample_line` (240-261). Tests 392, 466, 481 (whole), 531, 553 change.

**Binary `gate`**
- `crates/gate/src/servers.rs`: the whole file (598 lines; 13 tests 318-587); `mod servers` at `main.rs:43`.
- `crates/gate/src/priority.rs`: the whole file (676 lines; 13 tests 453-666), the lines `gate: could not lower|raise ...` (160) and `gate: could not list the job of gate ...` (271), unless the in-job raise of a critical gate is kept (OPEN DECISIONS 5).
- `crates/gate/src/watch.rs`: fields 77, 80, 82, 106, 108, 110, type 125, `Watching.survey` 199-200; constructor 258-260, 272-274; `after_late_take` 343-344; `final_read` 380-381, 395-397; `take_sample` 432, 436; `prioritize` 462-467; `server_facts` 576-588 and its use at 529; `sample` 623-637, 646-657, 673-675; `outside_the_job` loses its spare predicate and client return (692-707); `sample_facts` 722-725; `server_cpu` 743-757; `absolute` 832-834; tests 941, 1001. `lowered` (796-813) survives.
- `crates/gate/src/escaped.rs`: `spared` (762-774; server half at 767, unreadable-`dotnet` half at 773, OPEN DECISIONS 6), `Reach.server_image` (165; used 267, 399-405), the spare parameter of `kill_look` (139), `Walk` (504-552), `Recorded::record` (455-465), `outside` (675), `children_since` (724); tests 1117, 1209, 1240, 1365, and the mechanical `spared(process, None, none)` closures (946-1584).
- `crates/gate/src/run.rs`: `Reach` built with `server_image` (1495-1499, 1747-1755); docs at 1862, 1875, 1895 ("but the build servers"); the survey hand-over to `Watching` (1526, 1552).
- `crates/gate/src/request_json.rs:28-29` (`serversUnder`, `serverImage`).
- Box: `crates/gate/src/boxside/heartbeat.rs` `JobAndServers`/`Counted` (85-95), the server read (103-127), `RunningSum`'s server map (133-166), tests 221, 237, 253, 269, 283, 290, 301, 308, 316 (275 keeps its job half; 336 survives); `crates/gate/src/boxside/run.rs:491-496` (the work-folder salt) and the doc at 233.

**`gate-args`**: `ValueOption::ServersUnder`/`ServerImage` (`src/run.rs:49,51`), parse `src/cli.rs:220-221`, set `src/run.rs:231,233`, `check_server_seams` (`src/run.rs:346-357`, call 313), `TestSeams.servers_under`/`server_image` (`src/request.rs:154-159`); tests `tests/args/seams.rs` 32-33, 54, 265, 304, 1217. `Problem::SeamNeeds` survives (also `src/run.rs:387`).

**Suite**
- `crates/gate/tests/suite/build_servers.rs`: the whole file (8 cases; machine-alone `a_quiet_build_waiting_on_a_busy_shared_compiler_is_not_stalled` 119, `a_server_started_after_the_command_counts_while_the_job_holds_a_client` 169, `a_dotnet_server_is_told_by_its_command_line` 201); its `tool` helper (15) moves if any priority case stays.
- `crates/gate/tests/suite/priority.rs`: 278, 300, 334, 378, 402 die; 436 and 473 lose their server halves; 515 survives only with the in-job raise.
- `--test-servers-under .` passed only to confine the scan: `support.rs:871-875` (`run_words`), `launch.rs:353,375,451`, `diagnostics.rs:18`, `ceiling.rs:20`, `install.rs:207`, `cli.rs:91`, `request.rs:52-53,112-115,126,132`, `crates/gate-state/tests/state/fixtures/capture-remote-run.ps1:143`.
- `diagnostics.rs:78` pins `gate:   build servers: none`; `launch.rs:180` (`the_build_server_environment_reaches_the_command`) changes with the environment; `boxside.rs:823,835` (the salt cases) change with the box environment.

**Docs**
- `docs/design.md`: 313, 716, 764, 767, 775, 791-811 (B1, B2, B3), 818, 827, 831, 847, 852, 855-856, 859, 864, 943-944, 1489-1491, 1514, 1518, 1520, 1759, 1792-1798, 1862, 1890 (not 666, which is the critical drain's "B1").
- `docs/ARCHITECTURE.md`: Task Index 47, 49, 51, 53; Layers 108, 110, 112, 127, 145, 192, 210, 216; footgun 234.
- `docs/GLOSSARY.md`: 21 Build client, 23 Build server, 45 Diagnostics block, 51 Escaped process, 124 Shared compiler server, 131 Stall.
- `docs/test-map.md`: 21, 30, 32, 37, 38, 39, 80, 82, 83, 87, 93, 116, 118, 121, 122, 155, 157, 162, 168.
- `README.md:28`; `.claude/skills/gate-nextup/SKILL.md:61-65` (the machine-alone list loses the three `build_servers::` cases and "held off by a build server the gate lowers").
- `docs/research/2026-10-09-powershell-gate-inventory.md` (B1-B3, BW9, K4) stays as the PowerShell gate's record.

**Not dead but must change**
- Local: nothing ends what the command leaves in its job after a clean exit (`crates/gate/src/run.rs:1440-1466` drops the job; `Job::create` is not kill-on-close, `crates/gate-win32/src/job.rs:233-241`), and `docs/design.md:716` rejects ending leftovers on a clean exit "since build servers and deliberate background work keep running".
- Box: the run's job is plain `Job::create` (`crates/gate/src/boxside/run.rs:802-803`), and the command's environment keeps nodes warm per work folder (496).
- No journal, claim or ticket field is server-specific; `avgCores`/`peakCores`/`freeCores` change value only because `own_gained` loses its server term.

## 4. Proposed change

### Where the settings go

1. **Local launch environment**, `crates/gate/src/environment.rs:64-72` (`overrides`), the single place the local gate sets build-server variables (reached from `crates/gate/src/run.rs:363,415`):
   - add `SharedCompilationId=gate-<pid>-<start_ms>` (the run id, from `ThisGate`, as `slot_marker` builds its marker at 88-94; under a non-default root add the root hash so a suite case never meets the machine's own runs);
   - add `MSBUILDUSESERVER=0` beside `DOTNET_CLI_USE_MSBUILD_SERVER=0`, since an inherited `MSBUILDUSESERVER` wins over the latter (SDK quote above) and .NET 11 turns the server on by default;
   - keep `MSBUILDDISABLENODEREUSE=1`, or replace it with `MSBUILDNODEHANDSHAKESALT=<run id>` (OPEN DECISIONS 3).
   `crates/gate-win32/src/launch.rs` needs no change: the block is generic (`EnvironmentBlock`), and the server inherits the command's environment.
2. **End the run's servers with the run**, `crates/gate/src/run.rs:1440-1466` (`command`) and `crates/gate-win32/src/job.rs`: either terminate the job once the command has exited (in `after_watch`, `Watched::Exited` arm, `run.rs:1581-1584`), or create the job kill-on-close (`Job::create_limited(Some(name), JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)`, already used for `create_kill_on_close`, job.rs:321-322), which also ends it when the gate itself is killed hard. Either one also ends non-server leftovers (OPEN DECISIONS 1). Without this step a per-run server idles 10 min in the orphaned job after every compiling run (measured: alive after the gate's exit in all nine per-run builds).
3. **Box**, `crates/gate/src/boxside/run.rs:491-496` (`command`): replace the work-folder salt with the same run-scoped settings (the payload's `Nonce`, or `Job`, as the run id), and end the job when the command's wait is over (`create_job` 802-803 kill-on-close, or `end_job` 988-992 after the wait). `crates/gate/src/boxside/heartbeat.rs` counts the job alone.
4. **Removal** of everything in section 3, after 1-3.

### Tests (one per behaviour; ordering-only, no wall-clock upper bound)

| Behaviour | Test (module) |
|---|---|
| The command gets a compiler-server id of its own run, replacing an inherited one | unit `environment::tests::the_command_gets_its_run_s_compiler_server_id_over_an_inherited_one` (crates/gate/src/environment.rs) |
| Two runs never share an id | suite `launch::two_runs_give_their_commands_different_compiler_server_ids` (fake-tool prints its environment) |
| The MSBuild server stays off over an inherited `MSBUILDUSESERVER=1` | suite `launch::the_build_server_environment_reaches_the_command` (launch.rs:180), extended |
| A process the command leaves in its job ends when the run ends | suite `launch::a_process_left_in_the_job_ends_with_the_run` (fake-tool `spawn-wait` leaves a child; the case waits on the child's handle and asserts it ended, the wait bounded only as a failure guard) |
| A hard-killed gate's job ends too (only if kill-on-close is chosen) | suite `stop_tree::a_gate_killed_hard_ends_its_command_s_job` |
| A busy process outside the job no longer keeps a quiet command alive | suite `stall::a_quiet_command_is_stalled_beside_a_busy_compiler_server_outside_its_job` (a fake-tool copy named `VBCSCompiler.exe`, started by the test, burns outside the job; the command is quiet; the kill line arrives), replacing `build_servers::a_quiet_build_waiting_on_a_busy_shared_compiler_is_not_stalled` and dropping its machine-alone mark |
| The kill's diagnostics carry no server lines | suite `diagnostics::a_kill_writes_its_diagnostics_to_the_log_before_the_kill_line` (diagnostics.rs:59), updated |
| The box gives the command a per-run id and ends its job with the run | suite `boxside::box_run_gives_the_command_its_run_s_compiler_server_id` and `boxside::box_run_ends_what_the_command_left_in_its_job`, replacing `boxside.rs:823,835` |
| The box heartbeat counts the job alone | unit `heartbeat::tests::heartbeat_sum_counts_the_job_alone` (crates/gate/src/boxside/heartbeat.rs), replacing the server sum tests |
| A critical gate raises its own members to High (only if kept) | suite `priority::a_critical_gate_raises_a_member_its_command_started_at_normal`, replacing `priority.rs:515` |
| The server seams are gone | `crates/gate-args/tests/args/seams.rs`: `--test-servers-under` and `--test-server-image` join the unknown-flag list (1217) |

A real-toolchain check (no suite case, since the suite runs no SDK): the brief's proving step runs one gated `dotnet build` of a small project with the installed build and confirms that no `VBCSCompiler` with the run's pipe name is alive once the gate has exited.

### Briefs (about 100 implementer tool calls each)

- **A. Per-run settings and the run's end (local)**: environment.rs, run.rs `command`/`after_watch`, gate-win32 job.rs if kill-on-close; the first five test rows; docs design.md 716 and 944. About 70 calls.
- **B. Box**: boxside/run.rs environment and job end, heartbeat.rs job-only CPU (this removes the box's use of `crates/gate/src/servers.rs`, so it must land before C); boxside and heartbeat tests. About 60 calls. Lands after GATE-187's base on `feat/remote-cutover` or before it; overlap with that branch is `boxside/run.rs` constants only.
- **C1. Watch removal (pure crate and reader)**: gate-watchdog `servers.rs` (keep the WSL classifier, moved), `watch.rs`, `diagnostics.rs`; gate `servers.rs` (delete), `watch.rs`, `request_json.rs`; gate-args seams; suite `build_servers.rs` (delete), the `--test-servers-under` users, `diagnostics.rs`, `request.rs`, the stall replacement case; the fixture capture script. About 100 calls.
- **C2. Priority and escaped sparing**: `priority.rs` (delete, or reduce to the member raise), `watch.rs` priority hooks and `after_late_take`, `escaped.rs` spare predicate, `run.rs` `Reach`; suite `priority.rs`. About 70 calls.
- **D. Docs sweep** (orchestrator or docs-writer, not the implementer): design.md, ARCHITECTURE.md (Task Index row below), GLOSSARY.md, test-map.md, README.md:28, gate-nextup SKILL.md:61-65. Then reinstall and `gate push` (the gate-nextup profile's "Reinstall"), and repo-inbox notes to delve-the-dungeon (docs/DEVELOPMENT.md:289 "while the compiler server stays shared") and godot-mcp.

Order: A, then B, then C1 and C2 (C1 deletes the reader B stops using). A alone already gives GATE-187 what it waits on for local runs; B gives it the builder.

## DEPENDS ON

- **GATE-15** (Durations part 3: ETAs in wait lines and an `expected` field in claims and tickets), being built on main: so far it changes only `crates/gate-state` (claims, model, state and their tests). Its planned `wait.rs`/`slots.rs`/`machine.rs` work does not touch the files here; the one shared reader is `priority.rs`'s `OtherGates`, which reads claims and tickets through `slots.rs` (`other_claims`, `critical_holding`, `listed_tickets`) and is deleted by C2, so C2 removes a GATE-15 conflict rather than adding one. No ordering needed.
- **GATE-180 / GATE-181 / GATE-182** (routing suite onto the Rust client; delete the remote.ps1 bridge; retire the remote-cutover flag) on `feat/remote-cutover`: that branch changes `crates/gate/src/boxside/run.rs` (two constants made `pub(super)`), adds `boxside/inflight.rs`, and changes `run.rs` (58 lines) and `docs/design.md` (140 lines) (`git diff --stat main...origin/feat/remote-cutover`). Brief B and brief A's `run.rs` edits merge with it; nothing in GATE-190 depends on the bridge, and the bridge's own job (`crates/gate/src/remote/bridge.rs:329`) runs pwsh, not builds.
- **GATE-187** (load-adjusted ceiling on the builder) is blocked on this: it needs brief B (the box's job holds only the run's own servers, so the box's stall check sees a hung command) and C1 (the box heartbeat and watch count no foreign servers). It lands on `feat/remote-cutover`, so B is best built on that branch or merged into it before GATE-187.
- **GATE-163** (a process starved during its kill can outlive the gate): ending the job at every run's end makes every run a kill site; a starved per-run server could outlive the gate the way GATE-163 describes. Kill-on-close does not change that (the terminate is still asynchronous). Brief A should wait (bounded) for the job's processes to end, or GATE-163's fix should cover the end-of-run terminate too.
- **GATE-143** (measure which shared daemons leave a gate's job, spare them): under the ruling no shared daemon is spared, and this investigation measured that VBCSCompiler started by a run stays in the run's job. Its sparing goal is superseded; what remains is the measurement of whether any daemon (mspdbsrv, a Framework `MSBuild.exe /nodemode` node) breaks away, which decides whether the escaped kill may end it. Suggest closing or rescoping GATE-143 with C2.
- **GATE-140** (end an escaped tree at the other kill sites): its plan text "sparing build servers" (design.md:852) goes; the rest stands.

## BRANCH

`feat/per-run-servers`. Evidence: about 25 source and test files across four crates (section 3), seven docs, two whole files deleted (`crates/gate/src/servers.rs`, `crates/gate/tests/suite/build_servers.rs`) and a third probably (`priority.rs`), a behaviour change every gated repo sees (the job ends with the run, a cold compiler per run), four implementer briefs, and a reinstall plus `gate push` to land it.

## OPEN DECISIONS

1. **What ends with the run.** Terminate the whole job when the command exits (servers and any deliberate background process the command left; `docs/design.md:716` rejected this for nesting "since build servers and deliberate background work keep running"), versus ending only processes classified as build servers (keeps a classifier on job members), versus a kill-on-close job (also ends the job when the gate is hard-killed or crashes). The ruling settles servers; it does not settle non-server leftovers.
2. **Nested gates.** A covered gate (delve's test-all runs two nested gated builds) either hands on its parent's run id and leaves its job's leftovers for the outermost gate to end (one cold compiler for the whole outer run), or takes its own id and ends its own job (one cold start per nested gate). Measured cost of a cold start on a 24-project rebuild: about 14 s load-adjusted.
3. **MSBuild worker nodes within a run.** Keep `MSBUILDDISABLENODEREUSE=1` locally (nodes per invocation, today's behaviour), or switch both ends to `MSBUILDNODEHANDSHAKESALT=<run id>` (reuse across the invocations of one run, ended by the job end). On the box, today's warm nodes go either way; which of the two the box uses is the same choice.
4. **The run id's value on the box**: the payload's `Nonce` or `Job` name (both per run), and whether a builder run uses the PC run's id.
5. **Priority.** Drop B2 entirely, or keep a member-based rule: a critical gate raises its job's members below High (today's `covered_gate_raises_its_own_jobs_server`), and an ordinary gate lowers members above its class. Measured: a per-run server under an ordinary gate ran BelowNormal; under a critical gate it is expected at Normal (unmeasured). Also: a critical gate no longer lowers an ungated build's shared servers (`docs/design.md:313`, `README.md:28`).
6. **Unreadable `dotnet` host sparing** (`crates/gate/src/escaped.rs:773`): drop it, so an escaped `dotnet` whose command line cannot be read is killed with the tree, or keep it.
7. **Godot builds.** With a per-run pipe, every Godot `--build-solutions`/export started under a gate spawns a server inside Godot's own job wrapper, which then holds `Godot_console.exe` for the 10 min keepalive unless the repo sets `UseSharedCompilation=false` (delve does; today a shared server often already exists, so the spawn is rarer). Whether the gate should do anything (it cannot set the property only for Godot) or the repos are told by inbox.

## DOC DRIFT

- `docs/ARCHITECTURE.md:127` says `OpenedJob::open_to_list` is what "the priority guard uses"; `crates/gate/src/nesting/binding.rs:59,72` uses it too.
- `docs/test-map.md:38` lists `launch`/`cli`/`install` as the cases passing `--test-servers-under`; it is also passed by `crates/gate/tests/suite/support.rs:875` (`run_words`, so every case using it), `ceiling.rs:20`, `diagnostics.rs:18`, `request.rs:112` and `crates/gate-state/tests/state/fixtures/capture-remote-run.ps1:143`.
- `docs/design.md:791` and `944` say `MSBUILDDISABLENODEREUSE=1` and `DOTNET_CLI_USE_MSBUILD_SERVER=0` keep the MSBuild server in the job; per the SDK source (`MSBuildForwardingAppWithoutLogging.cs`) an inherited `MSBUILDUSESERVER` overrides the latter (`crates/gate/src/environment.rs:68-69` sets only those two). Whether node reuse off also disables the server, as Learn says of `/nr:false`, was not tested for the environment variable.
- Outside this repo: delve-the-dungeon `docs/DEVELOPMENT.md:289` ("while the compiler server stays shared") becomes stale with brief A.

## TASK INDEX GAP

Answering this took ARCHITECTURE.md rows 47, 49, 51 and 53 plus about 30 further reads. Suggested row: "Change how the gate handles build servers (the per-run settings, the run's end, priority of the job's servers, the box's settings)" → design.md Watchdog and Command launching (Environment) and The box side → `crates/gate/src/environment.rs` → `crates/gate/src/run.rs` (`command`, `after_watch`) → `crates/gate-win32/src/job.rs` → `crates/gate/src/boxside/{run,heartbeat}.rs` → `crates/gate/src/{watch,escaped}.rs` → `crates/gate/tests/suite/{launch,stall,boxside,diagnostics}.rs`.

## Appendix: dead-code inventory

PROTOCOL: 1 overlay: none (no docs/agents/explore.md), read CLAUDE.md; 2 README: done (docs/README.md); 3 Task Index: done (rows at ARCHITECTURE.md:47, 49, 51, 53 plus layers 108-127, 192, 210, 216, 234-236); 4 ck: skipped (the prompt named the doc sections and the code is greppable; index not checked); 5 design docs: done (design.md 313, 760-880, 943-944, 1489-1520, 1790-1800, 1862, 1890; inventory K4/B1-B3/BW9); 6 source: done; 7 tree search: done for identifiers across crates/ (rg, not ast-grep).

Repo root: X:/dev/gate, HEAD 806a42dd. All line numbers from files read in this run. "Dies" = no reason to exist once every server is inside its own run's job. "Survives" names the reason.

======================================================================
0. What the code does today, in one paragraph (to read the rest against)
======================================================================
- Every sample (crates/gate/src/watch.rs:607-685) calls `Servers::read` (crates/gate/src/servers.rs:124). It splits the process table into: job members (`member`, servers.rs:152) and everything else (`server_of`, servers.rs:190). A non-member that is a build server is either "new" (created at/after the command, any parent) or "shared" (older, compiler server only; an older MSBuild node is classified but counted nowhere). A member that is a server image is pushed to `Found.servers` with `inside: true` (servers.rs:177-182). Members also set `Found.build_client`.
- The pure crate counts new/shared server CPU as progress and as the command's own CPU only while `build_client` is true (gate-watchdog/src/watch.rs:215, 488-495).
- After each sample `Prioritizer::run` (gate/src/priority.rs:117) lowers/raises every classified server (`inside` and outside) under a guard that reads tickets, claims and other gates' jobs (`OtherGates`, priority.rs:167-251).
- escaped.rs spares servers (and unreadable dotnet hosts) from the escaped-tree walk and kill (`spared`, escaped.rs:762-774).
- Box: heartbeat CPU = job accounting + every build server on the box (heartbeat.rs:97-127); command env gets MSBUILDNODEHANDSHAKESALT (boxside/run.rs:496).

Priority inheritance facts (item 4): gate-win32/src/launch.rs:144-146 documents that with `priority: None` a command gets NORMAL, "or this process's class when that is below normal or idle". run.rs:467-481 (`priority()`): critical => explicit `PriorityClass::High`; otherwise BelowNormal (or None when the gate is already BelowNormal/Idle). So under an ordinary gate every in-job child inherits BelowNormal. Under a critical gate the command is High but its children are created without a flag, so they come out Normal (inventory K4, docs/research/2026-10-09-powershell-gate-inventory.md:99: "Windows starts a High process's children at Normal"; the repo has no test that measures this child class; `crates/gate-win32/tests/launch.rs:287-290` only measures the explicit High flag).

======================================================================
1. crates/gate-watchdog/src/servers.rs (268 lines)
======================================================================
Callers found with rg across crates/ (excluding the file itself). Re-exported at gate-watchdog/src/lib.rs:20-22.

| item (line) | callers | verdict |
|---|---|---|
| `is_build_server` (11-16) | gate/src/servers.rs:10,65 (`is_server`), which is called by servers.rs:175,209 (reader) and escaped.rs:767 (`spared`) | DIES. Its only roles are the reader and the escaped sparing. See the open question below on the in-job raise. |
| `is_compiler_server` (23-25) | gate/src/servers.rs:215 (shared vs not) | DIES (the "shared" concept). |
| `is_compiler_image` (30-32) | gate/src/servers.rs:115 (`Servers::is_compiler_image`), used at 160 and 199 | DIES with the reader; also carried the `--test-server-image` seam. |
| `is_build_client` (38-43) | gate/src/servers.rs:173 (member flag), 251 (`holds_client` for escaped processes) | DIES: the client flag only gates whether server CPU counts. Nothing else reads it (no heartbeat, no diagnostics use except `build_client_in_job` display). |
| `PriorityClass` enum (48-61) | gate/src/priority.rs:13, 306-325 (`as_class`/`as_priority`) | DIES with `server_target`/`server_left`, unless a High raise of in-job processes keeps a pure rule (see item 4). The Win32 `Priority` (gate-win32/src/priority.rs) and the launch-flag `gate_win32::PriorityClass` (gate-win32/src/launch.rs:177) are different types and survive. |
| `server_target` (69-78) | gate/src/priority.rs:13,141 only | DIES, except its `high == true` arm if the critical raise of in-job servers is kept (raise anything below High to High). The `high == false` arm (lower from Normal/AboveNormal/High/RealTime) is only reachable for servers outside the job (priority.rs:128-130 returns early for `inside && !high`), so it dies. |
| `server_left` (85-90) | gate/src/priority.rs:13,145 only | DIES entirely: both arms are about other gates' jobs (`guarded_job`) and a critical gate holding the machine (`holds`). |
| `is_wsl_command` (95-98), `strip_exe` (101-106) | gate/src/watch.rs:17,768 (`config()`) | SURVIVES (WSL, not servers). It lives in servers.rs only for history; it must move out if the file goes. |
| `holds` (109-111) | internal helper of the above and `is_build_client` | goes with them; `is_wsl_command` does not use it. |

Unit tests in the file (all line numbers are the `#[test]` fn lines):
- 119 `build_servers_are_read_from_the_name_and_the_command_line` - dies
- 142 `a_test_server_image_is_a_compiler_server_only_when_named` - dies
- 162 `compiler_servers_exclude_an_old_msbuild_node` - dies
- 178 `build_clients_are_msbuild_or_dotnet_without_no_build` - dies
- 193 `wsl_commands_are_read_from_the_file_name` - SURVIVES (move with `is_wsl_command`)
- 213 `the_priority_target_of_a_server_is_read_from_its_class_and_the_gate` - dies (the `true` rows survive only if a High raise rule is kept)
- 236 `the_priority_target_of_a_server_whose_class_is_unknown_is_a_raise_only_under_a_high_gate` - dies (the `server_target(true, None)` assertion survives only with a kept raise rule; the `server_left` assertions die)
- 253 `the_priority_guard_leaves_a_listed_job_s_server_and_a_high_one_while_a_critical_gate_holds` - dies

escaped.rs sparing question (asked in the prompt): `spared` (escaped.rs:765) is applied only to processes OUTSIDE the job (it is the `spared` predicate of `Walk` 504-552, `outside` 675, `children_since` 724, `kill_look` 139, `Recorded::record` 455). A server a job member starts is created inside the job and is a member, not a descendant-outside, so it is never judged by `spared`. A server started by an earlier run or another session is not a descendant of any of this run's members (the walk goes by parent pid, `Walk::under` escaped.rs:540), so `spared` never has to protect it either; it was only ever "sparing" a server that the command's own member had started AND that was outside the job (breakaway or, per the walk, a recorded escapee's child). Therefore:
  - the build-server half of `spared` (escaped.rs:767 `servers::is_server`) is no longer needed for the ruled design;
  - the `unread` half (773: a `dotnet` host whose command line cannot be read is spared) exists only because that host "may be" a server; with no server outside the job it has no reason, but dropping it changes behaviour for an unreadable dotnet host that really is an escapee (it would then be killed). That is a design decision, flagged here, not a code fact.
  - One caveat to the ruling: `Job::create_breakaway` is used when `--test-job-breakaway` is set (run.rs:1449-1453); production `Job::create` does not allow breakaway (the inventory, line 175, says the job refuses breakaway on purpose). So an in-job-started VBCSCompiler stays a member in production.

======================================================================
2. crates/gate-watchdog/src/watch.rs
======================================================================
`Sample` fields (watch.rs:35-63):
- `new_servers: BTreeMap<u32,u64>` (50-51) - dies
- `shared_servers: BTreeMap<u32,u64>` (52-54) - dies
- `build_client: bool` (55-56) - dies
- `escaped` (57-60) and `vm_cpu` (61-62) survive.
`Watch` struct fields: `new_servers` (83), `shared_servers` (84), `build_client` (94-95) - die. Initialisers 136, 137, 145.
Logic that dies:
- `Watch::sample` lines 168-172 (the `Outside` builder keeps only `escaped`), 173 (`own_gained` with `sample.build_client`), 191 (`self.build_client = ...`), 199-200 (clone_from of the two server maps).
- `Watch::progressed` (209-217): line 215 `let servers = ... outside.new_servers.rose || outside.shared_servers.rose` and the `|| servers` term in 216; doc comment 204-208.
- `Watch::final_cpu` (258-279): the params `new_servers`, `shared_servers` (261-262), 269-270, 273 (`self.build_client`), 276-277; its caller gate/src/watch.rs:381 and the doc 253-257.
- `struct Outside` fields `new_servers`, `shared_servers` (480-481); only `escaped` survives (482). If only `escaped` remains, `Outside` itself can collapse to a `CpuMove`.
- `own_gained` (488-495): the `client` argument and the `servers` term die; it becomes `job + escaped.gained`.
- `CpuMove.continued` (438-439, set at 463) is read only through `outside.shared_servers.continued` (line 490). After the change nothing reads `continued`: rg over crates/ finds it set at 463, read only at 490, and asserted in test 1213's literals (1220 `continued: 50`, 1230 `continued: 0`). `gained`, `rose`, `appeared` are read for `escaped` (174-175, 214).
- `measure_cpu` (450-469): SURVIVES; it is generic over key (`K: Ord`) and measures `escaped` (key `(pid,start)`). Exported in lib.rs:24 and covered by test 1213.
- `progressed` itself survives (log, job CPU + escaped CPU floor, pid appeared, paused, VM).
Does `cores.rs`/`share.rs` take server CPU? No. `share.rs::free_cores(machine, before, after, own_gained, secs)` and `cores.rs` take `Ticks`/`own_gained` only (rg "server" in crates/gate-watchdog/src outside servers/watch/diagnostics returned only lib.rs:2,11,17,20,21). So they do not change; their input `own_gained` simply loses the server term. `tenths`/`Cores`/`hint` are untouched.
Test helpers and unit tests in watch.rs that cover only server behaviour:
- helper `server_tick` (674-681, sets `new_servers`, `shared_servers`, `build_client`), helper `server_share` (713-719); constructors `tick` (631-648, fields at 640-642) and `sample` (650-666, fields at 659-661) set the dead fields and just drop them.
- 1249 `a_build_server_that_only_appeared_is_not_progress` (-1257)
- 1260 `a_server_rising_counts_only_while_the_job_holds_a_build_client` (-1278)
- 1281 `a_new_server_rising_counts_only_while_the_job_holds_a_build_client` (-1299)
- 1302 `own_cpu_counts_the_servers_only_for_a_build_client` (-1307)
- 1310 `a_shared_server_counts_only_its_continuing_cpu_as_own` (-1328)
- 1543 `the_final_stretch_counts_the_build_servers_only_when_the_last_sample_held_a_client` (-1563)
Tests that use `server_tick` only as a convenient progress source and must be rewritten, not deleted: 825 `a_late_watcher_does_not_count_its_own_delay_as_quiet` (829, 835 use `server_tick`) and 843 `a_late_sample_with_no_progress_is_still_a_stall` (846-847). Test 1213 `measure_cpu_reads_the_moves_powershell_reads` survives (it tests the generic function; check it does not assert `continued` if that field goes).
Docs in the file to fix: 50-56, 83-84, 94, 164-165, 204-208, 253-257, 448, 476-477, 485-487.

======================================================================
3. crates/gate-watchdog/src/diagnostics.rs
======================================================================
- `ServerFacts` struct (50-62): dies. Re-export lib.rs:17.
- `SampleFacts` fields `new_servers_cpu` (95-96), `shared_servers_cpu` (97-98), `build_client_in_job` (101-102): die. `escaped_cpu` (99-100) survives (its doc says "the build servers left out": 99 doc line changes).
- `Report.servers: Vec<ServerFacts>` (121-122) dies; doc at 107-109 ("the build servers the last sample counted are listed all the same").
- `lines()` line 160 `push_servers(...)` and doc 151-152; `push_servers` fn (208-227) dies, including both literals `gate:   build servers: none` (211), `gate:   build server {} {}: cpu {} ({why})` (221) with "started after the command" (216) / "already running; counts while the job holds a build client" (218). `cpu_text` (272) stays if `process_line` uses it (not checked, `unreadable_parts_print_a_question_mark` test uses it for the server line at 549).
- `sample_line` (240-261): the parts `new servers cpu {} s, shared servers cpu {} s` (248, args 256-257) and `build client in job: {}` (248, arg 259) die; the line then ends `... escaped cpu {} s`.
- Tests in the file touching servers: 362-378 `sample()` helper (fields 372-375), 380-390 `empty()` (386 `servers: Vec::new()`), 392 `the_block_keeps_the_powershell_order_and_wording` (397-422: both ServerFacts rows and the two server lines at 419-420, and the sample line expectation at 422 must change), 466 `unread_outside_processes_alone_print_only_the_count` (474 expects `build servers: none`), 481 `a_cut_report_still_prints_its_server_lines` (dies wholly), 531 `unreadable_parts_print_a_question_mark` (541-549 server row dies, the rest survives), 553 `nothing_gathered_says_none` (556 expects the none line), and the `lines.len()` comments/asserts at 434, 451, 503, 565 that count "no servers" (the counts drop by one).
- gate/src/watch.rs consumers: 16 import `ServerFacts`, 529 `servers: self.server_facts(&tree)`, 576-588 `server_facts`, 711-727 `sample_facts` lines 722-723, 725.
- Suite: crates/gate/tests/suite/diagnostics.rs:78 pins `"gate:   build servers: none\n"`; 61-62 comment; build_servers.rs:290-331 pins the build-server line (see item 10).
- Format visibility: the block goes to the log only (design.md:853, 860). Whether ops-tower or other tools parse `build server` or `sample at ... new servers cpu` lines is unmeasured here (inventory line 187 documents the PowerShell format only).

======================================================================
4. crates/gate/src/servers.rs (reader, 598 lines) and crates/gate/src/priority.rs (676 lines)
======================================================================
servers.rs: the whole file dies, except possibly a small in-job classifier (open question below). Items (all lines):
- `Found` (22-33): `new_servers`, `shared_servers`, `build_client`, `servers` die.
- `Classified` (38-47): `inside` field (46) only has meaning for the guard; `pid/name/created` are what the priority pass needs.
- `counted()` (51-57): dies (feeds `Watcher.counted`, watch.rs:432, and the diagnostics server lines).
- `is_server()` (63-66): used by `spared` (escaped.rs:767) and the reader.
- `Scope` (71-79) including `under` (the `--test-servers-under` folder): dies.
- `Server` (84-91), `Servers` (96-102) with `command_lines` cache and `test_image`, `Servers::new` (106), `is_compiler_image` (114), `read` (124), `member` (152), `server_of` (190), `spares` (232), `holds_client` (244), `known_line` (256), `line` (267): die. The per-(pid,start) command-line cache exists for the classifier on processes outside the job.
- `under_folder`, `first_word`, `components` (275-295): die (test-seam folder confinement).
- Tests (all in-file): 318 `an_escaped_build_client_holds_the_servers_as_one_in_the_job_does`, 391 `build_servers_are_split_into_the_new_and_the_shared_by_their_start_time`, 422 `an_old_msbuild_node_is_left_out_while_a_new_one_counts`, 438 `a_server_whose_times_cannot_be_read_is_skipped_rather_than_failing_the_sample`, 458 `a_process_whose_start_cannot_be_read_is_skipped`, 474 `a_server_that_started_exactly_when_the_command_did_is_new`, 486 `a_command_line_is_read_once_per_pid_and_start_time_across_samples`, 502 `a_pid_that_reappears_with_a_later_start_reads_its_command_line_again`, 519 `a_command_line_no_live_process_holds_is_pruned`, 530 `a_build_client_is_msbuild_or_a_dotnet_host_without_no_build`, 546 `a_member_that_is_no_dotnet_host_has_no_command_line_read`, 563 `only_a_server_whose_program_lies_under_the_scoped_folder_counts`, 587 `a_job_member_is_a_client_wherever_its_program_lives`. All die.
- main.rs:43 `mod servers;`. ARCHITECTURE.md:210 describes it.
Other users of `Servers`/`Scope`: gate/src/boxside/heartbeat.rs:19,93,112,118-120 (item 7); escaped.rs:840 (test `use`) and 1262-1274 (test `a_dotnet_host_whose_line_a_later_sample_cannot_read_keeps_its_first_judgement` at 1240 uses `Servers::read` and `servers.spares`); watch.rs:77,258,648,933,971.

priority.rs:
- Header (1-6) describes B2.
- `LeftAlone` (23-28), `Guard` trait (31-34), `Pass` (38-48), `OtherGates` (167-236) incl. `holders` (198), `other_gates` (221), `impl Guard` (238-251), `jobs` (256-281), `job_of` (288-294): the guard is about OTHER gates' jobs, claims and tickets, so all die. Callees that survive because other code uses them: `slots::Survey::other_claims` (slots.rs:374; also machine.rs:312), `critical_holding` (slots.rs:474; also wait.rs:610), `listed_tickets` (slots.rs:563, also pause.rs:300 and slots.rs:308), `others` (slots.rs, used by wait.rs), `OpenedJob::open_to_list` (gate-win32/src/tree.rs:43; also nesting/binding.rs:59,72). `v1::{tickets_dir, slots_dir}` stay in gate-state. `Watching.survey` (watch.rs:199-200) and `OtherGates::new(... survey)` (watch.rs:273) plus run.rs:1552 passing `survey` exist only for the guard => dies (the field, the param, and the `&mut Survey` borrow in `Run::watch`'s `taker()` tuple: run.rs:1526 `let (taker, survey, held, covered) = self.taker(...)`; the survey is still used elsewhere by `taker`, so only the hand-over to `Watching` dies, not `Survey`).
- `Calls`/`System`/`SYSTEM` (52-83): exist as a test seam for the priority calls; die with `Prioritizer` unless a raise survives.
- `Prioritizer` (86-162): `new`, `set_high`, `run`, `move_one`, `could_not` and the `unchangeable` set. The `server.inside && !self.high` early return (128-130) already means an ordinary gate never touches in-job processes.
- `message()` (298-303), `as_class`/`as_priority` (305-327).
- Tests (all in-file, 452-675): 453 `the_priority_failure_line_is_said_once_for_a_server`, 469 `the_priority_of_a_pid_a_later_process_took_is_tried_again`, 484 `the_priority_of_a_high_gate_raises_and_of_an_ordinary_one_lowers`, 510 `the_priority_guard_leaves_a_listed_job_s_server_and_a_high_one_while_a_critical_gate_holds`, 537 `the_priority_of_an_ordinary_gate_leaves_its_own_job_alone_and_a_high_one_raises_it`, 556 `the_priority_of_a_server_that_has_gone_is_passed_over`, 566 `the_priority_guard_is_not_read_when_no_server_would_move`, 583 `the_priority_guard_is_read_once_a_pass_for_every_server_it_moves`, 602 `the_priority_of_a_server_whose_class_cannot_be_read_is_left_by_an_ordinary_gate`, 615 `the_priority_of_a_server_whose_class_cannot_be_read_is_raised_by_a_high_gate`, 626 `the_priority_job_listing_line_is_said_once_a_watch`, 641 `the_priority_job_listing_line_names_the_word_of_the_gate`, 666 `the_priority_guard_lists_every_process_of_the_jobs`.
- Output lines that die: `gate: could not lower|raise <name> <pid>: <message>` (priority.rs:160) and `gate: could not list the job of gate <id> (<message>); its build servers may be lowered|raised` (priority.rs:271); documented design.md:810-811.

Does a critical gate still need to raise servers inside its own job? Facts:
  - Ordinary gate: the whole lowering half dies. In-job servers are children of the BelowNormal command and inherit BelowNormal (launch.rs:144-146, run.rs:467-481), and `Prioritizer` already skips them (priority.rs:128-130).
  - Critical (High) gate: the command is created High (run.rs:468-469), its children come out Normal (inventory K4; not measured in this repo), so an in-job VBCSCompiler/MSBuild node would run at Normal while the command is High. The raise of the gate's own job's servers to High (priority.rs:128 for `high`, 141, `server_target(true, ..)`) is therefore the one piece with a reason to survive. It is test-pinned: suite priority.rs:515 `covered_gate_raises_its_own_jobs_server` and in-file 537.
  - What it needs if kept: the class read/set calls (`gate_win32::priority_of`, `set_priority`, which survive anyway for `lowered`), and a way to pick the in-job processes: either the classifier on job members (today `Servers::member` -> `is_server`, servers.rs:174-183) or a no-classifier rule (raise every job member below High, the mirror of `lowered`, watch.rs:796-813). Which one is a design choice; nothing in the code decides it.
  - Surviving neighbours that are NOT server code: `Watcher::after_late_take` (watch.rs:337-355) calls `self.priority.set_high(false)` and `self.others.set_high(false)` (343-344) [die/change] and then `lowered(...)` (352) which lowers job members at High to BelowNormal [survives]; `lowered` (796-813) and its test at ~1150 (watch.rs:1151-1164) survive.
  - A kill-on-late-take case: suite priority.rs:436 `a_gate_covered_by_a_critical_parent_that_ends_lowers_its_command_and_moves_servers_as_its_own_kind` has a surviving half (command High -> BelowNormal at the take: 451-457) and a dying half (the `InJob` server in another gate's job lowered, 458-466).
  - Doc claim to retire: design.md:313 and README.md:28 (a critical gate moves a `none` run's build servers).

======================================================================
5. crates/gate/src/watch.rs
======================================================================
Fields/imports that die: `servers: Servers` (77), `under: Option<PathBuf>` (80), `counted: Vec<(u32,bool)>` (82), `classified: Vec<Classified>` (106), `others: OtherGates<'a>` (108), `priority: Prioritizer<'a>` (110), type `ServerCpu` (125). `Watching.survey` (199-200). Imports at 16 (`ServerFacts`), 29 (`OtherGates, Prioritizer`), 31 (`Classified, Found, Scope, Servers, counted`).
Constructor: 258 (`Servers::new(request.test_seams.server_image.clone())`), 259 (`under` from `servers_under`), 260 (`counted`), 272-274 (`classified`, `others`, `priority`; `let high = runs_as == Slot::Critical` at 246 stays only if the High raise stays).
Methods:
- `after_late_take` 343-344 (see item 4).
- `end`/`final_read` (377-400): server part: `final_read` return type (395), 397 `server_cpu(&self.counted, ...)`, 380-381 `final_cpu` call.
- `take_sample` 430-436: `self.counted = counted(...)` (432) and `self.prioritize(console)` (436) die; 433 `counted_escaped` survives.
- `prioritize` (462-467) dies.
- `gather` 529 (`servers: self.server_facts(&tree)`), `server_facts` (576-588) die.
- `sample()` (607-685): 623-637 (Scope, `Servers::read`, destructuring of `Found`, `self.classified = servers`), 646-657 (`outside_the_job(... &self.servers ...)` returns `(cpu, client)`, `build_client = build_client || escaped_client`), 673-675 fields in `Sample`. The snapshot at 616, `Look` at 639-645, `escaped` sample CPU (646-652) survive; they need a variant of `outside_the_job` (692-707) whose spare predicate and client return go away (line 703 `recorded.record(look, |process| servers.spares(process, line))`, 705 `servers.holds_client`).
- `sample_facts` 722-725.
- `server_cpu` (743-757) dies.
- `absolute` (832-834) (`--test-servers-under` path made absolute) dies.
- Doc comments: 60-122 field docs, 332-336, 373-376, 392-395, 430-431, 447-449, 462-463, 576-577, 601-606, 688-691, 743-745, 830-831.
Unit tests: 941 `an_escaped_build_client_makes_the_sample_hold_one_on_a_single_read_of_its_line` (dies; the "one read of the line" claim exists for the classifier), 979 `a_sample_s_escaped_cpu_reaches_its_diagnostics_facts` (survives; drop fields 988-990), 1001 `an_end_read_skips_a_server_that_exited_or_cannot_be_read` (dies), imports at 929-933 (`server_cpu`, `outside_the_job`, `Servers`). `lowered` tests survive.

======================================================================
6. crates/gate/src/escaped.rs and crates/gate/src/run.rs
======================================================================
escaped.rs:
- `use crate::servers;` (17) and `spared` (762-774) - see item 1: server half dies; the `unread` half is a decision.
- `Reach.server_image` (165) and its doc 160-161; used at 267 (`find`), 399-405 (`started_since`). `Reach { job, root }` would remain.
- The `spared` predicate parameter threads through: `kill_look` (135-148, param 139), `Walk` (504-552: field `spared` at 504, `new` 515, use 552), `Recorded::record` (455-465, `spare` param), `outside` (672-690, param 675), `children_since` (720-740, param 724, used at 736). `Found.escaped` doc 172 ("the build servers and everything under them left out"), 276-277 ("A build server, shared with every other build ... is left running"), 451-452, 471 ("as the watch leaves out a build server it cannot read"). If both halves of `spared` go, the predicate parameter disappears from all of those.
- Tests that use server sparing: 1117 `a_build_server_under_a_remembered_member_that_has_ended_is_left_running_with_everything_under_it`, 1209 `a_build_server_outside_the_job_is_left_running_with_everything_under_it` (also exercises the unread dotnet host at 1223-1230), 1240 `a_dotnet_host_whose_line_a_later_sample_cannot_read_keeps_its_first_judgement` (uses `Servers::read`/`spares`, 1262-1285), 1365 `a_build_server_under_a_recorded_escapee_is_left_running_with_everything_under_it`. Many other escaped tests merely pass `|process| spared(process, None, none)` (946, 963, 990, 996, 1080, 1096, 1111, 1225, 1307, 1428, 1448, 1467, 1492, 1511, 1519, 1561, 1584) as a "nothing is spared" closure with helper `none` (1204); those need only a mechanical change. Helper docs at 987 and 1203 mention servers.
- No black-box suite case pins the sparing (rg of suite for `spared|server` in stall/ceiling/interrupt returned nothing relevant). `stop_tree.rs` has no server sparing at all (rg: nothing).

run.rs:
- Imports `escaped::{self, Found, Reach, Reached, Recorded}` (32).
- Reach construction: `watched()` 1495-1499 (`server_image: request.test_seams.server_image.as_deref()`), `reach()` 1747-1755 (1748 doc, 1752). `Run::reach` and `ender` stay with `Reach` minus `server_image`.
- Docs that say build servers are left running: 1862, 1875, 1895 (`stop`, `kill`, `interrupted`).
- `Watching { ... survey, ... }` at 1543-1555 (item 4).
- `priority()` (467-481), `Slot::Critical` => High: survives (it is the command's own class).
- `--test-servers-under`/`--test-server-image` are not otherwise plumbed in run.rs (the watch reads `request.test_seams` itself: watch.rs:258-259).
- Other users of the seams: crates/gate/src/request_json.rs:28-29 (`"serversUnder"`, `"serverImage"` in `--test-print-request`).

======================================================================
7. Box side
======================================================================
crates/gate/src/boxside/heartbeat.rs:
- Header 1-6: "plus every build server on the box ... The box keeps build servers warm, so a quiet `dotnet build` works in a compiler server or `MSBuild` node outside its job".
- `job_cpu` (45-51) doc "plus every build server's on the box outside it".
- `JobAndServers` (85-88) and `Counted { servers: Servers, sum: RunningSum }` (92-95); `impl CpuSource for JobAndServers` (97-128): lines 103-127 (member set, `processes` snapshot, `listed`, `Scope { since: 0, under: None }` at 112-116, `Servers::read` at 118-120, `counted.sum.add(job, found.new_servers, &listed)` at 126). After the change the CPU source is the job's accounting alone (the `name`, `accounting()` error text at 121-125 stay).
- `RunningSum` (133-166): its server map and `listed` logic die; `total/job` delta stays only if the "never falls" monotone sum is still wanted for `Pump` (gate-watchdog/src/heartbeat.rs `CpuSource`); job accounting is already cumulative.
- Imports 8, 16 (`Win32ProcessTable`), 19 (`Scope, Servers`).
- Tests: 206-218 helpers `servers`, `listed`, `read`; 221 `heartbeat_sum_keeps_every_server_over_a_failed_snapshot`, 237 `heartbeat_sum_keeps_a_listed_server_whose_times_could_not_be_read`, 253 `heartbeat_sum_drops_a_server_only_once_a_good_snapshot_no_longer_lists_it`, 269 `heartbeat_sum_first_read_counts_the_job_and_every_server_whole`, 275 `heartbeat_sum_adds_the_job_delta` (job half survives), 283 `heartbeat_sum_adds_a_rising_servers_delta`, 290 `heartbeat_sum_adds_a_new_server_whole`, 301 `heartbeat_sum_adds_a_server_whose_total_fell_whole`, 308 `heartbeat_sum_adds_nothing_for_a_vanished_server_and_all_of_one_back_again`, 316 `heartbeat_sum_never_falls`; 336 `heartbeat_write_waits_for_the_output_lock_and_writes_its_line_whole` survives.
- gate-watchdog/src/heartbeat.rs has no server logic (rg: no hit for server/VBCS/warm), so the beat rule and `Pump` are unchanged.
boxside/run.rs:
- 233 doc of `Beat::watch` ("the CPU of `job` and the box's build servers"); 236 `heartbeat::job_cpu(Arc::clone(job))` call stays.
- MSBUILDNODEHANDSHAKESALT: set at boxside/run.rs:496 `Variables::from_process().with("MSBUILDNODEHANDSHAKESALT", request.key)`; doc 491-494 ("so MSBuild's worker nodes stay warm within one work folder and are never reused across work folders"). `request.key` has other uses (boxside/run.rs:162, 1107, 1137, 1178), so only the `.with(...)` call at 496 goes.
- Tests: suite boxside.rs:823 `box_run_salts_msbuild_nodes_with_the_work_folder_key` and 835 `box_run_replaces_an_inherited_handshake_salt`; the helper `run_box_with_env` has exactly two occurrences in the suite (its definition and the use at boxside.rs:840), so it dies with the second test.
- `Variables::with` (environment.rs:32-34) is used by boxside/run.rs:496 and the unit test environment.rs:139-149 (`with_replaces_a_set_variable_whatever_its_letter_case_and_adds_an_unset_one`, which also hard-codes MSBUILDNODEHANDSHAKESALT as its example name); if the salt goes and no per-run variable replaces it on the box, `with` has no production caller.
- Box warm-server design text: design.md:1489-1491 (also the measurement about worker-node pools across work folders), 1514, 1518, 1520. inventory BW9/391 (PowerShell reference, historical).
- Anything else on the box: boxside/stop.rs:153 only counts job processes; `gate box stop` ends the job and nothing else. The box command's job is made by `create_job` (boxside/run.rs:802-803) with plain `Job::create(name)`, which is NOT kill-on-close (`create_kill_on_close` exists at gate-win32/src/job.rs:322 and is used for the tool restore, boxside/tools.rs:197). So today, when the box run ends, processes left in its job (nodes, compiler server) are not ended by the job closing; `gate box stop` terminates the job (stop.rs). Inventory 391 records the PowerShell wrapper's "no kill-on-close, so build servers stay warm". "Servers end when the run ends" on the box therefore needs more than removing the salt; that is a change, not a deletion.

======================================================================
8. crates/gate-args
======================================================================
- `ValueOption::ServersUnder` (src/run.rs:49), `ServerImage` (51); parse at cli.rs:220-221 (`"test-servers-under"`, `"test-server-image"`); set at run.rs:231 and 233.
- `TestSeams.servers_under` (request.rs:154-156), `server_image` (157-159, "Only with `servers_under`").
- Dependency check `check_server_seams` (run.rs:346-357, called from run.rs:313) producing `Problem::SeamNeeds { option: "--test-server-image", needs: "--test-servers-under" }` (problem.rs:90, display 127). `Problem::SeamNeeds` has a second user at run.rs:387, so the variant stays.
- Usage text: no hit; both seams are not in usage.rs (rg).
- Print-request: gate/src/request_json.rs:28 (`serversUnder`), 29 (`serverImage`); suite request.rs:52-53 (null defaults), 112-115 and 126, 132 (given values). In `--test-print-request` the order is documented design.md:1792, 1794, 1798 (`remoteSnapshot` "after `serverImage`").
- Tests: gate-args/tests/args/seams.rs: 32-33 and 54 in the all-seams case (fn around line 10; name not read), 265 `the_servers_under_seam_needs_a_value_once`, 304 `the_server_image_seam_needs_the_servers_under_seam_and_a_value_once`, 1217 (`"--test-server-image"` in the list of flags unknown without the feature; the list's fn name not read).
- Users that pass `--test-servers-under .` only to confine the scan: suite support.rs:873-875 (`run_words`, every case using it), launch.rs:353, 375, 451, diagnostics.rs:18, ceiling.rs:20, install.rs:207, cli.rs:91, capture-remote-run.ps1:143 (crates/gate-state/tests/state/fixtures/capture-remote-run.ps1). With no scan to confine, these flags and the doc comments at ceiling.rs:17, diagnostics.rs:15, install.rs:196, support.rs:871-873 go.

======================================================================
9. Environment settings
======================================================================
- crates/gate/src/environment.rs:61-72 `overrides()`: line 68 `MSBUILDDISABLENODEREUSE=1`, line 69 `DOTNET_CLI_USE_MSBUILD_SERVER=0`; header 1 and 61-63. This is the single place the local gate sets the build-server variables; a new per-run variable would be added to this vec (applied by `applied`, 76-84, case-insensitive replace). `Variables::for_command` (21-23) is the entry from run.rs.
- The box: crates/gate/src/boxside/run.rs:496 (`Variables::from_process().with(...)`) — the box does NOT set the two local variables (design.md:1489). `Variables::from_process` is environment.rs:26-28.
- crates/gate-win32/src/launch.rs: no environment setting for servers (rg found only an unrelated "server" in a comment at 376); `EnvironmentBlock` is generic.
- Suite pins: launch.rs:180-200 `the_build_server_environment_reaches_the_command` (185, 187).
- No Roslyn compiler-server variable is set anywhere in the repo (rg UseSharedCompilation: none).

======================================================================
10. Tests
======================================================================
crates/gate/tests/suite/build_servers.rs (332 lines; module declared at suite/main.rs:11):
- 119 `a_quiet_build_waiting_on_a_busy_shared_compiler_is_not_stalled` - `#[ignore]` machine-alone (118)
- 144 `a_quiet_command_is_stalled_once_the_shared_compiler_goes_quiet`
- 169 `a_server_started_after_the_command_counts_while_the_job_holds_a_client` - machine-alone (168)
- 201 `a_dotnet_server_is_told_by_its_command_line` - machine-alone (200)
- 221 `a_dotnet_host_without_a_server_on_its_command_line_is_no_server`
- 244 `a_build_with_no_build_is_not_a_client`
- 267 `an_old_msbuild_node_never_counts`
- 290 `a_stall_kill_lists_the_build_server_its_last_sample_counted`
Helpers: `tool` (15, `pub(crate)`, used by priority.rs:26), `Burner` (47-84), `seams`/`seams_with_stall` (24-34), `burn_words` (39), `wait_for_log` (89), `chatter_path`/`stop_chatter` (108-115). Whole file dies; if priority.rs cases remain, `tool` (copy_tool + shown) must move.
crates/gate/tests/suite/priority.rs (552 lines; `mod priority` suite): 278 `an_ordinary_gate_lowers_a_build_server_outside_its_job`, 300 `an_ordinary_gate_leaves_a_server_at_high_while_a_critical_ticket_holds`, 334 `an_ordinary_gate_leaves_a_server_in_a_holding_critical_gate_s_job`, 378 `a_critical_gate_raises_a_build_server_outside_other_gates_jobs`, 402 `high_gate_leaves_a_server_in_another_gates_job`, 436 `a_gate_covered_by_a_critical_parent_that_ends_lowers_its_command_and_moves_servers_as_its_own_kind` (half survives), 473 `a_light_gate_under_a_critical_parent_that_ends_runs_as_light_under_a_heavy_stand_in` (server asserts at 479-505 die; the stand-in assertions 496-511 survive), 515 `covered_gate_raises_its_own_jobs_server` (survives iff the in-job raise survives). Helpers dying with the dead cases: `SERVER` (41), `seams` (47), `NamedJob` (179-212), `InJob` (217-244), `Waiter` (249-275), `write_holding_ticket` (127), `write_claim` (145), `job_name` (173); `class_of`/`put_class`/`set_class` (85-115) survive if any class is read.
Other suite files: `stall.rs`, `cores.rs`, `ceiling.rs`, `interrupt.rs` have NO server-specific case (rg); they use the escaped tree. diagnostics.rs:59 `a_kill_writes_its_diagnostics_to_the_log_before_the_kill_line` pins `build servers: none` (78) and `ceiling_words` passes `--test-servers-under` (18). launch.rs:180 pins the env. boxside.rs:823, 835 pin the salt; boxside.rs:1135 `box_run_beats_while_a_busy_command_runs` uses `burn` in the job and its CPU comes from job accounting (survives; no server dependence found).
support.rs: only `run_words` (871-875) carries `--test-servers-under .`. No helper copies `gate-test-server.exe`; `copy_tool`/`tool` copy fake-tool under any name. `VBCSCompiler.exe`/`MSBuild.exe`/`dotnet.exe` copies are made in build_servers.rs only (dotnet.exe copies in routing.rs/remote_client.rs are the routed-run stubs, unrelated).
crates/gate/src/bin/fake-tool.rs: no mode is server-only. `burn` (122), `wait` (113), `spawn-wait` (132), `chatter` (128) are also used by boxside, critical, nesting, burn, stop_tree cases (counts from rg over suite). Doc line 202 mentions "a build server".
gate-watchdog / gate unit tests listed in items 1, 2, 3, 4, 5, 6, 7.
Machine-alone groups: no nextest config exists in the repo (`git ls-files | grep nextest|.config` is empty; no .config/nextest.toml). Machine-alone is by `#[ignore = "needs the machine alone..."]` only: build_servers.rs 118/168/200, stall.rs 68/105/133, ceiling.rs 45, interrupt.rs 45. The list is named in .claude/skills/gate-nextup/SKILL.md:61, 63, 65. The three build_servers entries leave that list; the stall/ceiling/interrupt escapee ones stay.
docs/test-map.md rows: 21 (run.rs), 27, 30 (machine.rs: `priority (critical raises servers)`), 32 (late.rs), 37 (watch.rs), 38 (servers.rs), 39 (priority.rs), 73, 80 (gate-watchdog share.rs/watch.rs), 82 (servers.rs), 83 (diagnostics.rs), 87 (gate-win32 priority.rs), 93 (proc-table), 116 (machine.rs filterset), 118 (late.rs filterset), 121 (servers.rs filterset), 122 (priority.rs filterset), 155, 157, 162, 168 (filtersets).

======================================================================
11. Docs
======================================================================
docs/design.md:
- 313 critical gate moves a `none` run's build servers (and README.md:28 same).
- 666 mention of "part B1" is the critical-drain part, NOT build server B1 (do not edit).
- 716 "Rejected: the parent ending its job's remaining processes ... since build servers and deliberate background work keep running" (rationale cites servers surviving the run).
- 764 sampler reads "the build servers".
- 767 stall: "no build-server progress"; 773 progress list (escaped tree; unchanged); 775 "stall clock also resets when a build server's CPU counts as progress".
- 778 free share: own cores (job + escaped; unchanged).
- 791-811 the "Build servers" bullet: B1 classification (793-799), B2 priority (801-811: ordinary gate lowers; High gate raises other gates' and own job's; unknown class; guard; failure lines 810-811), B3 env (791, 944).
- 818 "A build server ... is spared with everything under it, and so is a `dotnet` host whose command line cannot be read"; 827 same in the sample walk; 831 "An escaped build client ... counts as a build client in the job"; 847 "Server sparing is unchanged"; 852 GATE-140 planned item "sparing build servers (and an unreadable `dotnet` host)".
- 855-856, 859, 864 diagnostics server lines, sample line parts, the cut-block rule ("still lists the build servers"), "reads the build servers".
- 943 priority "The classes of the build servers outside the job are moved"; 944 environment (the two variables, "kept, P3, B3").
- 1489-1491 box warm servers + handshake salt + measurement; 1514 heartbeat CPU = job + every build server; 1518, 1520.
- 1759 test load note ("its command and the build servers it watches run at BelowNormal").
- 1792, 1794, 1796, 1798 the two seams; 1862 fake-tool copied under names the gate classifies; 1890 Cores hint `avg` includes server CPU while a client is held; 1910 spike item "other users' command lines (B1)".
docs/ARCHITECTURE.md: Task Index 47 (cites `gate-watchdog/src/servers.rs`, `gate/src/servers.rs`, `gate/src/priority.rs`, "the lowering and raising of their priority"), 49 ("`crates/gate/src/servers.rs` (the precedent)", `Sample`, `measure_cpu`, `own_gained`, `progressed`), 51/53 (heartbeat rows); Layers 108 ("including `measure_cpu` and the progress that build servers and the WSL VM add"), 110 (classifiers, `server_target`, `server_left`), 112 (diagnostics), 127 (`open_to_list` "the priority guard uses it"), 145 (gate-win32 `priority.rs`), 192 (`Reach` ... "the test seam's server image"), 210 (`servers.rs`, `priority.rs`, `OtherGates`), 216 (`heartbeat.rs`: "the job's and the build servers' CPU"), 234 (footgun: "build servers included"); 236 speaks of the escaped tree only and needs no server edit.
docs/GLOSSARY.md: 21 Build client, 23 Build server, 45 Diagnostics block ("the build servers"), 51 Escaped process ("sparing build servers"), 124 Shared compiler server, 131 Stall ("build-server work"). No "New server" entry exists.
docs/test-map.md: see item 10.
README.md: 28 only.
.claude/skills/gate-nextup/SKILL.md: 61-65 (machine-alone list, "held off by a build server the gate lowers to BelowNormal", and "When the diff touches ... the build-server classification or the priority code").
docs/research/2026-10-09-powershell-gate-inventory.md: K4 99, W2 138/145, B1-B3 159-167, 175, X3 187, X7 199, P3 207, BW5 391, BW9 408-412, Z2 466, 495, 563, 566 (historical record of the PowerShell gate; leave as history, it is the porting reference).
docs/design/*.md: no build-server text (rg hits are "warm cache").

======================================================================
12. Other things that exist because servers outlive runs
======================================================================
- `Job::remember`/`departed`/`forget` (job memory): not server-related (it is for escaped children of ended members). Survives.
- `gate stop-tree` (crates/gate/src/stop_tree.rs): no server sparing (rg: no hit). Other kill sites (box stop, remote client, bridge, push) end nothing outside the job and spare nothing; GATE-140 note is design.md:851-852 and ARCHITECTURE.md:234,236. The GATE-140 plan text "sparing build servers" (design.md:852) is dead; the rest of GATE-140 survives.
- Journal/claim/ticket formats: no server field (rg of crates/gate-state/src: none). The cores figures `avgCores`/`peakCores`/`freeCores` journalled and printed (design.md:1887-1890) change value only because `own_gained` loses the server term; no format change.
- Cores hint test: suite cores.rs has no server case.
- `gate-win32/src/priority.rs` doc (line 1: "what the build servers' rule moves a server between, and nothing else") and `gate-win32/tests/priority.rs:1`: the module itself survives because `lowered` (gate/src/watch.rs:796-813, used from 352) calls `priority_of`/`set_priority`; docs say otherwise afterwards.
- The `Watching.survey` hand-over and `OtherGates` are the only reason `Watcher` holds a `&mut Survey` (rg Survey in watch.rs: 32, 200, 234, 273 only).
- `Servers` also provides the command-line cache that `escaped` judgements reuse (servers.rs:232-263 `spares`/`known_line`): dies with the sparing.

======================================================================
13. Things that do NOT die but must change for "servers end when the run ends"
======================================================================
- Local: after the command's own exit nothing ends what is left in the job. `records_after` (run.rs:1845-1850) returns no record for `Watched::Exited`; `Job::create` is not kill-on-close (docs/ARCHITECTURE.md:88; inventory line 175: "The job does not kill its processes when the gate ends: the compiler server a build starts was measured inside the job and a normal exit must leave it running"); design.md:716 rejects ending the job's leftovers on a clean exit, citing build servers. That rejection and the inventory measurement are the doc/code basis of "servers survive the run" and change.
- Box: `create_job` uses plain `Job::create` (boxside/run.rs:802-803), see item 7.
- `Variables::for_command` env (environment.rs:64-72) and the box env (boxside/run.rs:496) are where per-run variables go.

======================================================================
DOC DRIFT
======================================================================
- docs/ARCHITECTURE.md:127 says `OpenedJob::open_to_list` is used by "the priority guard"; code also uses it in crates/gate/src/nesting/binding.rs:59,72. Doc incomplete. Owner: ARCHITECTURE.md (Layers, gate-win32).
- docs/test-map.md:38 (servers.rs row) lists `launch`/`cli`/`install` as the suites passing `--test-servers-under`; the flag is also in suite `support.rs:875` (`run_words`, so every case using it), `ceiling.rs:20`, `diagnostics.rs:18`, `request.rs:112` and fixtures `capture-remote-run.ps1:143`. Doc incomplete; owner test-map.md.
- .claude/skills/gate-nextup/SKILL.md:61 says "The machine-alone cases are" four names, then 63 adds four more and 65 says "The first four ..."; the ignore attributes in code number 8 (build_servers.rs:118,168,200; stall.rs:68,105,133; ceiling.rs:45; interrupt.rs:45), matching the 3+1+4 listed. Consistent; no drift found.
- docs/design.md:1489 says the local gate "sets it to 1 for every command" (MSBUILDDISABLENODEREUSE): matches environment.rs:68. No drift.
- Other claims checked against code and found true: design.md:795 (new = created at/after `since`: servers.rs:212 `>=`), 801-803 (priority.rs:128-130, 141-147), 818 (escaped.rs:773), 856 (diagnostics.rs:247-248), 1514/1518 (heartbeat.rs:112-126, `since: 0`).

TASK INDEX GAP
- The answer needed: Task Index row 47, row 49, row 51/53, plus design.md (Watchdog, Command launching, The box side, Black-box suite harness) and about 30 further reads/greps (all of servers.rs both crates, priority.rs, escaped.rs, heartbeat.rs, gate-args, suite). Suggested row: "Remove or change how the gate handles build servers (classification, sharing, priority, escaped-tree sparing, the box heartbeat's server CPU, the node-handshake salt, the server test seams)" -> design.md Watchdog (Build servers) and The box side -> `crates/gate-watchdog/src/{servers,watch,diagnostics}.rs` -> `crates/gate/src/{servers,priority,watch,escaped,run,environment,request_json}.rs` -> `crates/gate/src/boxside/{heartbeat,run}.rs` -> `crates/gate-args/src/{cli,run,request}.rs` -> `crates/gate-args/tests/args/seams.rs` -> `crates/gate/tests/suite/{build_servers,priority,diagnostics,launch,boxside,request}.rs` and the `--test-servers-under` users (support.rs, ceiling.rs, install.rs, cli.rs) -> test-map.md rows 38/39/82/83/121/122/157 -> gate-nextup SKILL.md:61-65.

MISSING ENTRY POINT
- none (docs/README.md, docs/ARCHITECTURE.md with a Task Index, and a glossary exist). Terms used in a project sense without a glossary entry: "New server" (design.md:795, watch.rs docs) and "Classified" server (priority pass) have no GLOSSARY entry; "B1/B2/B3" labels (design.md:791) are defined only by the inventory.
