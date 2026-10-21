# GATE-592 step 4: the portable .NET repos timed by hand on the Mac mini builder

Measured 2026-10-21 (Mac local time 12:00 to 12:45). Each repo was cloned on the Mac from a bundle of this PC's `main` HEAD: yaat e82b78f5b, yaat-server 9e2a1cb3, delve-the-dungeon 3f65072f, opening-hand 5f306452, in-the-sky 40c5d8c. None of the five uses Git LFS (`git lfs ls-files` is empty, and no `.gitattributes` has an lfs filter), so the bundle was enough. yaat and yaat-server sit side by side under `~/timing-592/`.

## How the numbers were taken

- **Mac** is the wall time of the command alone, run in a login zsh in the repo root (harness: `~/timing-592/time.sh`; one log per run in `~/timing-592/logs/<label>.log`).
  - **cold** is a fresh tree (`git clean -xfd`) with the MSBuild and VBCSCompiler servers killed and a warm NuGet cache. That is the same state as the Windows builder's cold run in a new work folder.
  - **warm** is the same command run again straight after, which is a no-op build. Windows "warm" runs follow real edits, so for builds I also timed **touched**: `touch` one core source file, then build. That recompiles that one project; its dependents skip CoreCompile because the reference assembly is unchanged (checked with `-v:n` on opening-hand: 1 project compiled, 13 skipped). This is what a body-only edit costs.
- **Windows** is the median `wallSeconds` of routed `release` lines with verdict `passed`, from `%LOCALAPPDATA%\gate\v1\events*.jsonl` plus `archive\*.gz`, covering 2026-10-11T19:08Z to 2026-10-21T18:51Z. All 7,433 routed releases for these repos went to the Windows builder.
  - Commands were matched after normalizing worktree paths (`<repo>.wt\<tree>\`), `-p: X` spacing and the Godot binary path, with a one-off script that was not kept.
  - A routed run's `wallSeconds` includes the push and sync-back. Where the journal has `phases.command` (recent runs only), its median is given as **cmd** too: that is the builder's time for the command alone.
- **ratio** = Windows wall median ÷ Mac cold (the Mac's slowest case). Against Mac warm or touched, the ratio for builds is 30-100×.
- **Load:** see "Load, reruns and what is clean" below. The tables give the clean set; the set taken under the macOS indexers is in brackets.

## opening-hand: all pass

| command | Mac cold | Mac warm | touched | Windows median (n) | ratio | result |
|---|---|---|---|---|---|---|
| `dotnet build src/Duel.Client/Duel.Client.csproj -c Debug -warnaserror` | 4.8 [5.7] | 0.6 [0.6] | 0.7 [0.8] | 42.5 (675) | 8.9× | pass |
| `dotnet build Duel.slnx -c Release -warnaserror` | 5.3 [5.4] | 0.8 [0.8] | 0.8 [0.8] | 29.4 (463) | 5.5× | pass |
| `dotnet test Duel.slnx -c Release --no-build` (1,373 tests) | 3.9 [3.4] | 3.5 [3.2] | | 42.9 (51) | 11× | pass |
| `dotnet csharpier check .` | 1.0 [0.9] | 0.9 [0.9] | | 21.7 (68) | 22× | pass after `dotnet tool restore` (see failures) |
| `dotnet format style Duel.slnx --verify-no-changes --severity info` | 8.5 [9.4] | 8.5 [8.6] | | 44.5 (76) | 5.2× | pass |
| `dotnet format analyzers Duel.slnx --verify-no-changes --severity info` | 11.1 [11.2] | 14.2 [13.2] | | 54.3 (80) | 4.9× | pass |
| `Godot --headless --path src/Duel.Client --build-solutions --quit` (Mac binary `$GATE_GODOT_ROOT/4.7.2/Godot.app/Contents/MacOS/Godot`; Windows `Godot_console.exe`) | 3.0 [3.3] | 1.9 [2.5] | | 64.6 (33) | 22× | pass |

There is no `phases.command` for opening-hand in the journal window. For scale, the same commands run locally on this PC have medians of 13.7 s (client Debug build), 22.1 s (Release build) and 28.9 s (format style).

**Verdict.** opening-hand gains the most for the least work. Every gated command passes on the Mac, and every one runs in 1-14 s against 22-65 s routed to the Windows builder. Even against this PC running locally, the Mac is 3-5× faster on builds.

Part of the Windows figure is routing overhead (push and sync), which a Mac route also pays, so the real gain is the Windows builder's command time minus the Mac's. To route there, it needs:
- the router's per-repo or per-command builder choice and a macOS box side (GATE-591 §B/§C);
- a translation of the Godot word (`Godot_console.exe` or `F:\Godot\Godot_console.exe` to the Mac binary, which `GATE_GODOT_ROOT` already locates);
- a `dotnet tool restore` before csharpier in a fresh work folder.

## delve-the-dungeon: all pass except one golden float test

| command | Mac cold | Mac warm | touched | Windows median (n) | cmd (n) | ratio | result |
|---|---|---|---|---|---|---|---|
| `dotnet build Delve.slnx -c Release -warnaserror` | 8.5 [7.2] | 0.8 [0.8] | 0.9 [1.0] | 57.2 (720) | 44.7 (63) | 6.7× | pass |
| `dotnet build Delve.slnx -c Debug -warnaserror` | 9.1 [7.2] | 0.8 [0.8] | 1.0 [1.0] | 81.9 (540) | 81.1 (63) | 9.0× | pass |
| `dotnet test Delve.slnx -c Release` (4,873 tests) | 12.2 [11.5] | 12.1 [11.5] | | 48.0 (64) | 94.7 (2) | 3.9× | **fail, 1 test** |
| `dotnet test --project tests/Delve.Client.Tests -c Release` | 7.0 [6.7] | 6.9 [6.9] | | 48.1 (404) | 36.5 (45) | 6.9× | **fail, same test** |
| `dotnet build src/Delve.Client -warnaserror` | 0.6* [0.7] | 0.6 [0.7] | | 70.5 (211) | 65.3 (21) | n/a* | pass |
| `dotnet csharpier check .` | 2.2 [2.1] | 2.2 [2.2] | | 26.3 (576) | 13.3 (61) | 12× | pass |
| `dotnet format Delve.slnx --verify-no-changes --severity info` | 29.9 [28.9] | 35.6 [30.9] | | 99.9 (542) | 96.2 (55) | 3.3× | pass |
| `dotnet format style Delve.slnx --severity info` | 18.0 [18.9] | 17.9 [16.6] | | 69.0 (141) | 51.8 (33) | 3.8× | pass (no file changed) |
| `Godot --headless --path src/Delve.Client --build-solutions --quit` (Mac binary) | 3.1 [3.5] | 2.4 [2.4] | | 88.6 (468) | 67.5 (88) | 29× | pass |

\* The Debug solution build before it had already built Delve.Client, so this is not a cold figure. The Godot `--build-solutions` run also follows those builds; on Windows it usually does too.

**Failure:** `Delve.Client.Tests.StoneOutlineTests.TheWholeStoneOfEveryPieceIsPinned`, with 4,871 passed, 1 failed and 1 skipped:

```
Assert.Equal() Failure: Values are not within tolerance 0.050000000000000003
Expected: 305371655.85913342
Actual:   305371655.9740606
  at Delve.Client.Tests.StoneOutlineTests.TheWholeStoneOfEveryPieceIsPinned() in tests/Delve.Client.Tests/StoneOutlineTests.cs:621
```

My read: this is **a test pinned to x64 float results**, not Windows-only code. The structure hash on line 620 matches, so the geometry is the same; only the float sums of the outline points differ, by 0.11 on 3×10⁸. That fits arm64 RyuJIT fusing multiply-adds (or a different float math path) in the `Vector2` outline code. Ubuntu CI is x64, so it passes there.

**Verdict.** delve gains strongly. Builds take 8-9 s cold and about 1 s for a body edit, against the Windows builder's 45-81 s command time; format runs in 30 s against 96 s; Godot `--build-solutions` takes 3 s against 68 s. To route there, it needs:
- what opening-hand needs (builder choice, a macOS box, the Godot word, tool restore);
- the `StoneOutlineTests` golden values made architecture-tolerant (a wider tolerance or per-architecture constants on lines 621-623), or `dotnet test` kept on Windows until then;
- the `"C:\Program Files\dotnet\dotnet.exe" …` spellings (routing map §A) stay Windows-only by their first word.

The `test-all.ps1` smokes stay Windows-only (routing map §A); I did not time them.

## in-the-sky: all pass, too little Windows history to compare

| command | Mac cold | Mac warm | touched | Windows median (n) | ratio | result |
|---|---|---|---|---|---|---|
| `dotnet build InTheSky.slnx -c Release -warnaserror` | 1.5 [2.3] | 0.8 [0.8] | 0.9 [0.9] | none (0) | | pass |
| `dotnet test InTheSky.slnx -c Release --no-build` | 2.0 [1.9] | 1.8 [2.1] | | none (0) | | pass |
| `dotnet test --project tests/Sky.Engine.Tests -c Release` | 1.8 [2.4] | 1.8 [1.9] | | 38.1 (1) | 21× | pass |
| `dotnet csharpier check .` | 0.4 [0.4] | 0.4 [0.4] | | none (0) | | pass |
| `dotnet format style InTheSky.slnx --verify-no-changes --severity info` | 4.6 [4.8] | 4.6 [4.8] | | none (0) | | pass |
| `dotnet format analyzers InTheSky.slnx --verify-no-changes --severity info` | 5.5 [5.4] | 5.8 [6.7] | | none (0) | | pass |

The journal window holds one routed passed run of these commands and no local runs: the repo was hardly worked on between 10-11 and 10-21. Its launcher does call gate.exe.

**Verdict.** in-the-sky passes everything and every command takes 6 s or less, so it would route to the Mac as it stands, Sky.SimConnect included (plain `net10.0`). With its low load, the absolute gain is small. To route there, it needs what opening-hand needs minus Godot. I did not time `tools/test-all.ps1` itself: it is a composite of these same commands under nested gates.

## yaat: passes once Rosetta 2 is installed and TMPDIR is short

These timings were taken after the owner installed Rosetta 2. Cold is a fresh tree with the build servers killed.

| command | Mac cold | Mac warm | Windows median (n) | cmd (n) | ratio | result |
|---|---|---|---|---|---|---|
| `dotnet build -p:TreatWarningsAsErrors=true` | 36.1 | 8.1 | 130.0 (217) | 105.7 (52) | 3.6× | pass |
| `dotnet build -c Release -p:TreatWarningsAsErrors=true` | 26.2† | 2.9 | 149.8 (25) | 129.6 (10) | 5.7× | pass |
| `pwsh tools/test-all.ps1` (both repos: build, then both suites) | 142.8 | 121.3 | 747.6 (135) | 726.8 (18) | 5.2× | **fail**: 218 yaat UI tests (socket path) |
| same, with `TMPDIR=/tmp/` (diagnostic) | 141.0 | 122.0 | | | 5.3× | **pass**, both repos |
| `dotnet test tests/Yaat.Sim.Tests -c Release -- --filter-class '*.CallsignTests'` | 1.4‡ | 1.4 | 27.7 (20) | none | 20× | pass |
| `dotnet test tests/Yaat.Client.Tests -c Release` | 29.7 | 31.3 | 90.0 (7) | 106.5 (1) | 3.0× | pass |
| `dotnet test tests/Yaat.Client.UI.Tests -c Release` | 60.7 | 69.1 | 280.6 (15) | 285.4 (10) | 4.6× | **fail**: 218 of 3,166 |
| same, with `TMPDIR=/tmp/` (diagnostic) | 84.2 | | | | 3.3× | **fail**: 1 test, a `Nightly` one |
| `dotnet format style yaat.slnx --verify-no-changes --severity info` | 33.5 | 33.2 | 148.0 (2) | none | 4.4× | pass |

† after the Debug build in the same tree (restore done, Release outputs new). ‡ after `test-all` had built the Release test projects.

An earlier attempt, killed by the PC gate's stall watchdog (see "Mac setup"), had a Debug cold build of 44.1 s and a Release build of 27.4 s.

**Failures:**

1. **Before Rosetta, every yaat and yaat-server command failed at the build.** Yaat.Sim compiles `Proto/nav_data.proto` through Grpc.Tools 2.71.0. Grpc.Tools 2.71.0 ships only an x86_64 protoc for macOS (`tools/` has `macosx_x64` and no macOS arm64 folder; `file` says `Mach-O 64-bit executable x86_64` for both `protoc` and `grpc_csharp_plugin`). Without Rosetta 2, exec fails with EBADARCH:

   ```
   ~/.nuget/packages/grpc.tools/2.71.0/build/_protobuf/Google.Protobuf.Tools.targets(291,5): error MSB6003: The specified task executable
   "~/.nuget/packages/grpc.tools/2.71.0/tools/macosx_x64/protoc" could not be run. System.ComponentModel.Win32Exception (86):
   An error occurred trying to start process '…/macosx_x64/protoc' … [yaat/src/Yaat.Sim/Yaat.Sim.csproj]
   ```

   Before Rosetta, `dotnet format style yaat.slnx` exited 2 with IDE0008 ("Use explicit type instead of 'var'") on Yaat.Sim.Tests files. My read: the same cause, because the workspace could not generate the proto types; it passes with Rosetta. **This is why the builder needs Rosetta 2** (or a native arm64 protoc passed as `Protobuf_ProtocFullPath`; none is on the box).
2. **217 of the 218 UI test failures** share one cause:

   ```
   System.ArgumentOutOfRangeException : The path '/var/folders/ry/<…>/T/CoreFxPipe_yaat-automation-test-e3ced49b98b1424ca6fe8ec3837a33d6'
   is of an invalid length for use with domain sockets on this platform.
     at System.Net.Sockets.UnixDomainSocketEndPoint..ctor(String path, String boundFileName)
     at System.IO.Pipes.NamedPipeClientStream.TryConnect(Int32 _)
   ```

   The 218th, `AutomationHostTests.Host_IsNotStarted_WhenAutomationModeOff`, expects a `TimeoutException` and gets the same `ArgumentOutOfRangeException`.

   My read: **a test that assumes Windows-length pipe names**. .NET named pipes on Unix are Unix domain sockets under `$TMPDIR`. macOS's per-user `TMPDIR` is 49 characters, so `CoreFxPipe_yaat-automation-test-<32 hex>` overruns the 104-byte `sun_path` limit. Linux CI's `/tmp/` is short, so it passes there. With `TMPDIR=/tmp/`, all of these pass.
3. **The one failure left with `TMPDIR=/tmp/`:** `AutomationHostTests.Dispose_NoClientCanConnectAfterwards_Repeated`, `Assert.Equal() Failure: Expected: 0, Actual: 4` at `AutomationHostTests.cs:363`. It is `[Trait("Category", "Nightly")]`, so `test-all`'s default scope leaves it out.

   My read: the pipe server's dispose semantics differ on Unix sockets; clients still connect after `Dispose`. That could be a real Unix-only defect in the automation host, not just a test assumption; I didn't investigate further.

Load: all yaat timings ran after Spotlight was turned off and the owner's GUI session had ended. Other accounts' CPU peaked at 0.76-0.94 of a core (`audiomxd`). They were not preceded by the "load under 2 for a minute" wait: the load average was about 9, decaying from my own earlier runs and the Rosetta install.

**Verdict.** yaat is the biggest routed class (`test-all` at 747.6 s, n=135), and it gains about 5× on the Mac: `test-all` takes 122-141 s when it passes, builds 26-36 s cold, format 33 s against 148 s. To route there, it needs:
- Rosetta 2 on the builder, which is now installed and should go into `install-macos.sh`; or a native protoc wired in;
- a short `TMPDIR` for routed runs on the Mac (the box setting `TMPDIR=/tmp/` or a short per-run folder), or yaat shortening its automation pipe names;
- the box's sibling-pair layout, so yaat-server sits beside yaat (GATE-591 §A);
- the `Nightly` dispose test kept off the Mac, or the host fixed, before `-Full` runs route there;
- about 33 GB of disk per yaat pair work folder once Debug and Release are both built (measured: yaat 32 GB, yaat-server 1.1 GB).

## yaat-server: passes with Rosetta

| command | Mac cold | Mac warm | Windows median (n) | cmd (n) | ratio | result |
|---|---|---|---|---|---|---|
| `dotnet build -p:TreatWarningsAsErrors=true` | 4.3§ | 2.2 | 141.7 (10) | 72.3 (6) | 33× | pass |
| `dotnet build -c Release -p:TreatWarningsAsErrors=true` | 0.7§ | 0.7 | 35.9 (7) | 14.2 (7) | n/a§ | pass |
| `dotnet test tests/Yaat.Server.Tests -c Release` (4,894 tests: 4,889 passed, 5 skipped) | 137.8 | 139.8 | 705.8 (8) | 480.7 (2) | 5.1× | pass |
| `dotnet format style yaat-server.slnx --verify-no-changes --severity info` | 19.8 | 20.9 | none (0); local PC 46.8 (2) | | | pass |

§ Not cold. yaat's Debug build had already built the sibling Yaat.Sim, and `test-all` had built yaat-server in Release, so these runs follow them.

The skips include three `SessionPersistenceTests` marked "Only Windows refuses to rename a folder that holds an open file". The repo already guards those.

Before Rosetta, every yaat-server command failed with the protoc EBADARCH error above, through its reference to `../yaat/src/Yaat.Sim`.

**Verdict.** The server suite takes 138 s on the Mac against 706 s routed (481 s command time) and passes as written once protoc runs. yaat-server routes wherever yaat does, since it needs the yaat sibling beside it and its protoc. Nothing in its own code blocked the Mac.

## Load, reruns and what is clean

- **The orchestrator's transfer test (11:55:30-11:58:30 Mac time):** no timed run overlapped it. The first timed run started at 12:00:52 (log birth times on the Mac), so nothing was rerun for it.
- **Indexers:** until about 12:10, the admin account's GUI session's first-login indexers took 2-4 cores: `mediaanalysisd` at 170-235% and `spotlightknowledged.updater` at about 265%. The first sets for opening-hand, delve and in-the-sky, and the first touched builds, ran under them (other-account peak 129-466%).
  - After Spotlight was turned off and the GUI session ended, I waited for the 1-minute load to stay under 2 for 75 s (5 reads 15 s apart, last read 1.51). Then I reran all of those commands from fresh trees. These are the **clean** figures in the tables; the bracketed ones are the indexer-loaded set.
  - The two sets differ by about a second at most, so on 15 cores the indexers barely moved these short runs.
- **yaat pair:** timed with no indexers and other accounts under one core, but not after the load-under-2 wait (see the yaat section).
- **Discarded:** the first yaat-server warm test run started the moment Rosetta landed, mixed in with stale state, and I killed it. The first yaat-pair run with Rosetta was killed by the PC gate's stall watchdog during `test-all`.

## Mac setup: what looked wrong

1. **Rosetta 2 was missing.** That blocked yaat and yaat-server entirely (Grpc.Tools' x86_64-only macOS protoc). The owner has now installed it; `install-macos.sh` should install it too, or check for it.
2. **The default `TMPDIR` is too long for .NET named pipes** (`/var/folders/ry/<…>/T/`, 49 characters; the Unix socket path limit is 104 bytes). A routed run should get a short `TMPDIR` from the box.
3. **Repo-local dotnet tools are not restored in a fresh clone.** `dotnet csharpier check .` failed first with `Run "dotnet tool restore" to make the "csharpier" command available.` I ran `dotnet tool restore` (1.1 s) in each repo before timing. A Mac box needs that step in its warmup or before a routed csharpier run. I didn't check whether the Windows box does it.
4. **Steady background CPU on an idle headless box:** `audiomxd` at about 75% and `configd` at 38-40% the whole time, so roughly 1.1 cores with nothing running. Worth a look: `configd` churn may come from network or Tailscale reconfiguration, and `audiomxd` has no reason to be busy on a server.
5. **First-login indexers:** Spotlight (now off machine-wide with `mdutil -a -i off`) and `mediaanalysisd` in the GUI session took 2-4 cores for the first two hours. A builder should keep them off, or the GUI account logged out.
6. **The login PATH was fine:** dotnet 10.0.401 and 8.0.425 with wasm-tools, pwsh 7.6.6, git-lfs, node, uv, bun, prek, and Godot through `GATE_GODOT_ROOT`. git is `/usr/bin/git` (Xcode CLT). The limits were fine too (`ulimit -n` and `launchctl maxfiles` 524288). `rg` is not installed; none of these repos needs it. NuGet's cache started nearly empty, and opening-hand's first restore fetched from nuget.org with no trouble.
7. **The PC gate cannot see the Mac's CPU through a hand-run ssh.** The first yaat run was killed with `gate: STALLED: no output and no CPU for 121 s`. The stall rule (design.md:1477) counts output and the CPU of the job's processes on the PC (plus the WSL VM for `wsl` commands, W6). A `--slot none` ssh whose remote command writes only to a remote log looks idle even while the Mac runs 15 cores flat out.
   - Real routed runs are watched on the builder's side, so this hits only hand-run ssh wrappers. I used `--stall 3600` for the rest.
   - If such wrappers are meant to be gated, they need a heartbeat, or an ssh rule like W6's.

## Left on the Mac

`~/timing-592/` came to 36 GB: yaat 32 GB (build output), bundles 2.2 GB, yaat-server 1.1 GB, delve 832 MB, opening-hand 270 MB, in-the-sky 124 MB, logs 4 MB. It was deleted after the measurement. Nothing outside it was changed, apart from what `dotnet tool restore` and NuGet restore put in `~/.nuget/packages`.
