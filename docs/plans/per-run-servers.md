# Per-run build servers (GATE-190)

Each gate run has build servers of its own, on this PC and the builder; the mechanism is in [design.md, Watchdog](../design.md#watchdog) and [Command launching](../design.md#command-launching). With node reuse on, MSBuild nodes idle about 15 minutes inside the command's job, so anything waiting on its whole tree would hang; the command gets `MSBUILDDISABLENODEREUSE=1` beside the per-run `SharedCompilationId` and `MSBUILDNODEHANDSHAKESALT`.

This file holds what GATE-140, GATE-143, GATE-163, GATE-222 and GATE-225 cite: the measurements, the mechanisms with their sources, and the facts about where a server lives. Sources: the gated repos' docs and scripts, Microsoft Learn, the dotnet/msbuild, dotnet/sdk and dotnet/roslyn sources (quotes below, pins: msbuild c1c99ee0, sdk 680d5f1e, roslyn `main`), the local SDK 10.0.401 targets, and 28 gated builds plus one probe on this PC.

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
- The gate's job allows no breakaway (`Job::create`; only the `--test-job-breakaway` seam allows it).
- Measured, with a gate that did not yet end the job at the run's end: in all nine per-run builds below, the per-run VBCSCompiler was in a job (`IsProcessInJob(h, NULL)` true) and still alive after the gate had exited, so it outlived its run by its idle keepalive.

### Mechanisms

| Mechanism | How it is set from the environment | Effect | When the server ends | Source |
|---|---|---|---|---|
| MSBuild node reuse off | `MSBUILDDISABLENODEREUSE=1` (the gate sets it) | Disables reuse across invocations: nodes serve one `dotnet`/`msbuild` invocation | Exits when that build ends; no job kill needed | XMake.cs `ProcessNodeReuseSwitch`: `if (Environment.GetEnvironmentVariable("MSBUILDDISABLENODEREUSE") == "1") ... enableNodeReuse = false;` (https://github.com/dotnet/msbuild/blob/c1c99ee05981494f34191590eb8ec2e47dba1958/src/MSBuild/XMake.cs). Learn: "`-nodeReuse:{value}` ... **False**. Nodes don't remain after the build completes." (https://learn.microsoft.com/visualstudio/msbuild/msbuild-command-line-reference#switches) |
| MSBuild node handshake salt | `MSBUILDNODEHANDSHAKESALT=<run id>` | Keeps reuse, private to whoever shares the salt: a per-run value gives reuse within the run only | Idles 15 min (`MSBUILDNODECONNECTIONTIMEOUT` overrides, in ms), else the job kill | Handshake.cs: `string handshakeSalt = Environment.GetEnvironmentVariable("MSBUILDNODEHANDSHAKESALT") ?? "";` ... `int salt = CommunicationsUtilities.GetHashCode($"{handshakeSalt}{toolsDirectory}{changeWaveSalt}");` (https://github.com/dotnet/msbuild/blob/c1c99ee05981494f34191590eb8ec2e47dba1958/src/Framework/BackEnd/Handshake.cs). CommunicationsUtilities.cs: `private const int DefaultNodeConnectionTimeout = 900 * 1000; // 15 minutes` and `GetValueAsInt32OrDefault("MSBUILDNODECONNECTIONTIMEOUT", DefaultNodeConnectionTimeout)` (https://github.com/dotnet/msbuild/blob/c1c99ee05981494f34191590eb8ec2e47dba1958/src/Framework/BackEnd/CommunicationsUtilities.cs) |
| MSBuild server | `DOTNET_CLI_USE_MSBUILD_SERVER=0` (the gate sets it); `MSBUILDUSESERVER=0` | Disables it | n/a | Learn: "Starting with the .NET 11 SDK, MSBuild Server is enabled by default ... In earlier .NET SDKs, MSBuild Server was off by default"; "after an idle time of 15 minutes, the server shuts down"; "MSBuild Server is a form of node reuse, so disabling node reuse also disables the server" (https://learn.microsoft.com/visualstudio/msbuild/msbuild-server). SDK: `if (string.IsNullOrEmpty(Env.GetEnvironmentVariable("MSBUILDUSESERVER"))) { EnvironmentVariable("MSBUILDUSESERVER", UseMSBuildServer ? "1" : "0"); }` (https://github.com/dotnet/sdk/blob/680d5f1e3c8cab8342a5e939389696deea0a9286/src/Cli/Microsoft.DotNet.Cli.Utils/MSBuildForwardingAppWithoutLogging.cs): an inherited `MSBUILDUSESERVER` wins over the gate's `DOTNET_CLI_USE_MSBUILD_SERVER=0`. Its pipe is `MSBuildServer-{handshake.ComputeHash()}` from the same `Handshake` base, so the salt should make it per-run too (inferred from the shared base class, not tested) |
| Roslyn compiler server, per-run pipe | `SharedCompilationId=<run id>` | Keeps the server and reuse within the run; no other run can connect | Idles 10 min (no environment knob), else the job kill | Local SDK `{dotnet root}\sdk\10.0.401\Roslyn\Microsoft.CSharp.Core.targets:160` `SharedCompilationId="$(SharedCompilationId)"`; ManagedCompiler.cs: `var pipeName = !string.IsNullOrEmpty(SharedCompilationId) ? SharedCompilationId : BuildServerConnection.GetPipeName(clientDirectory);` (https://github.com/dotnet/roslyn/blob/main/src/Compilers/Core/MSBuildTask/ManagedCompiler.cs); default pipe input `$"{userName}.{isAdmin}.{clientDirectory}"` (BuildServerConnection.cs); ServerDispatcher.cs: `internal static readonly TimeSpan DefaultServerKeepAlive = TimeSpan.FromMinutes(10);`, overridable only by the `keepalive` app setting or a client request. Measured here: the server started as `VBCSCompiler.exe "-pipename:gate190-probe-65672"` |
| Roslyn compiler server off | `UseSharedCompilation=false` | Disables it: each compile is a fresh `csc` | Nothing outlives the build | Local targets line 171 `UseSharedCompilation="$(UseSharedCompilation)"`; Microsoft.Managed.Core.targets defaults it to `true` when empty |
| Environment as properties | any of the two Roslyn names above | MSBuild reads environment variables as properties; a project's own definition wins | | Learn: "You can reference environment variables in project files the same way as build properties ... If the project contains a property definition that has the same name as an environment variable, the property in the project overrides the value of the environment variable." (https://learn.microsoft.com/visualstudio/msbuild/walkthrough-using-msbuild#environment-variables). No gated repo defines either property (a search of every gated repo); godot-mcp's `-p:UseSharedCompilation=false` is a global property and wins |
| Razor | none needed | Runs as a source generator inside the compiler server in current SDKs | as above | sdk `Sdk.Razor.CurrentVersion.targets`: `<UseRazorSourceGenerator Condition="'$(Language)' == 'C#' AND '$(UseRazorSourceGenerator)' == '' ">true</UseRazorSourceGenerator>` and the rzc `Compilation.targets` imported only when it is not `true` |
| `dotnet build-server shutdown` | n/a | Learn: "Shuts down build servers that are started from dotnet. By default, all servers are shut down." Options `--msbuild`, `--razor`, `--vbcscompiler` (https://learn.microsoft.com/dotnet/core/tools/dotnet-build-server) | It finds servers by their default names (sdk `BuildServerProvider.cs`), so a per-run pipe or a salted node is probably not found: inferred, not tested (I did not shut the shared server down, see 2) | |
| sccache (not in use) | `SCCACHE_SERVER_PORT`, `SCCACHE_IDLE_TIMEOUT` | Per-run port gives a per-run server | "terminate after (by default) 10 minutes of inactivity" (https://github.com/mozilla/sccache/blob/main/README.md) | |

Not verified: whether `vstest`/`testhost` processes outlive a `dotnet test` (none were seen after the runs here, but no test run was measured), the Razor rzc launcher's flags (legacy path only), and whether any daemon (mspdbsrv, a Framework `MSBuild.exe /nodemode` node) breaks away from the job (GATE-143).

### Priority of an in-job server

Roslyn creates VBCSCompiler with `NORMAL_PRIORITY_CLASS` (quote above). Measured under an ordinary heavy gate (BelowNormal command), the per-run server ran at **BelowNormal** (probe: `server pid 76640 priority BelowNormal`).

The gate's priority pass then left an ordinary gate's in-job servers alone, so the class did not come from the gate's lowering; why the explicit Normal flag did not hold is not resolved (the SDK 10.0.401 compiler may differ from roslyn `main`). Under a critical gate the command is High and its children start at Normal (`docs/research/2026-10-09-powershell-gate-inventory.md:99`, K4; not measured in this repo).

## 2. Cold-start cost on this PC

**Setup.** yaat (`yaat.slnx`, 24 projects, SDK 10.0.401) in a scratch worktree (cut from `53118c35d`, removed after). Every build ran `pwsh -NoProfile -File tools/gate.ps1 -Log .tmp/m-<variant>-<case>-<n>.log -TimeoutSeconds 900 -Slot heavy -Local -- dotnet build yaat.slnx [--no-incremental]`. Figures are the journal's `release` records (`wallSeconds`, `adjustedSeconds`, `avgCores`), joined in run order.

Variants, interleaved per run (shared, per-run, no server) so machine-load drift hits all three:
- **shared**: the shared-server settings (the gate's `MSBUILDDISABLENODEREUSE=1`, `DOTNET_CLI_USE_MSBUILD_SERVER=0`), the shared VBCSCompiler warm (a warm-up build ran first).
- **per-run**: plus `SharedCompilationId=gate190-<case>-<n>-<pid>`, so each build starts a cold server of its own; the script killed it after each run.
- **no server**: plus `UseSharedCompilation=false`.

Cases: **noop** (nothing changed), **touch** (one file in `src/Yaat.Sim` touched, so that project recompiles), **rebuild** (`--no-incremental`, every project compiled).

The shared compiler server was never shut down: the per-run variant gives a cold server by construction, and other sessions' gates were running throughout. The machine was heavily loaded by other sessions' heavy gates (cargo nextest and clippy runs): machine free cores ranged 2.5 to 8.4 of 16, so wall times are noisy and the load-adjusted figure (whole-run CPU share discounted, as the ceiling counts it) is the one to compare.

Load-adjusted seconds, three runs each (median, min-max):

| Case | shared | per-run server | no server |
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

**The builder (not measured).**

Measuring there would take a work folder of a .NET repo on the builder (the box's `Work` folders under its output root), an ssh session with the builder's own `gate.exe`, and the same interleaved script run locally on the builder with `gate run --local`.

That run compares a work-folder salt (no `MSBUILDDISABLENODEREUSE`, `MSBUILDNODEHANDSHAKESALT=<work-folder key>`, shared compiler server) against `MSBUILDNODEHANDSHAKESALT=<run id>` (or node reuse off) plus `SharedCompilationId=<run id>`, three runs each of noop, touch and rebuild, with the builder idle of other routed runs.

The builder loses more than this PC does: under a work-folder salt its MSBuild nodes are warm across runs in one work folder, so a per-run run pays a cold node start as well as a cold compiler.
