# Local-run census, 2026-11-02

A run of the `local-run-census` skill (`.claude/skills/local-run-census/`), asked for by the owner over a one-day window. It covers every heavy command family the gate journal shows running on this PC, sorted into the three states of [routing-a-repo.md](../routing-a-repo.md#which-repos-and-which-commands): ready to route, blocked on work, or truly local.

## Window

The last day, 2026-11-01T17:50Z to 2026-11-02T17:50Z, read from 31 journal archives, `events.1.jsonl` and `events.jsonl`. 65,432 local heavy seconds (about 18 hours) in 77 families with local runs.

Most of it comes from three repos with no routing at all (career-driver, godot-pad-claim, godot-xaml's .NET commands) and one world-of-intrigue sweep driver.

Builder host names in the `--local` reasons below are replaced with `{the Windows builder}` and `{the Mac builder}`; the census does not yet do this itself (GATE-1060).

## Heavy families

| Repo | Family | Local | Routed | Unfinished | Covered | Local s total | Local median s | Routed median s | Local verdicts | notRouted | Fallbacks | State |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| world-of-intrigue | `uv run python .tmp/woi94/driver.py` | 69 | 0 | 0 | 0 | 21240.9 | 87.1 | - | passed 64, failed 4, timed-out 1 | - | - | - |
| career-driver | `dotnet build` | 586 | 0 | 0 | 0 | 7266.4 | 6.3 | - | passed 494, failed 91, stalled 1 | - | - | - |
| career-driver | `dotnet format` | 263 | 0 | 1 | 0 | 5083.6 | 14.4 | - | passed 259, failed 4 | - | - | - |
| world-of-intrigue | `dotnet run` | 17 | 0 | 2 | 0 | 4218.1 | 20.0 | - | passed 14, failed 3 | - | - | - |
| godot-xaml | `dotnet build` | 322 | 0 | 1 | 0 | 2900.4 | 6.8 | - | passed 251, failed 71 | - | - | - |
| yaat | `git commit` | 43 | 0 | 0 | 0 | 2595.1 | 39.7 | - | passed 39, failed 4 | --local 3 | - | - |
| claude-config | `uv run pytest` | 50 | 9 | 0 | 0 | 2454.1 | 11.0 | 30.7 | passed 30, failed 20 | --local 7 | - | - |
| world-of-intrigue | `blender` | 30 | 0 | 1 | 0 | 1688.5 | 51.4 | - | passed 30 | - | - | - |
| yaat | `dotnet build` | 8 | 137 | 0 | 0 | 1227.0 | 84.0 | 126.9 | passed 8 | --local 7 | busy 1 | - |
| rustling-tulip | `cargo build` | 2 | 1 | 0 | 0 | 1150.5 | 575.3 | 345.3 | passed 2 | --local 2 | - | - |
| godot-xaml | `dotnet test` | 230 | 0 | 0 | 0 | 1147.0 | 3.1 | - | passed 143, failed 87 | - | - | - |
| rustling-tulip | `cargo nextest` | 1 | 192 | 0 | 0 | 1046.8 | 1046.8 | 76.5 | passed 1 | - | busy 1 | - |
| yaat | `dotnet run` | 13 | 58 | 0 | 0 | 937.5 | 82.1 | 33.8 | failed 7, passed 6 | --local 12 | version 2 | - |
| rustling-tulip | `cargo run` | 2 | 17 | 0 | 0 | 909.7 | 454.8 | 30.1 | passed 2 | - | busy 2 | - |
| godot-pad-claim | `dotnet build` | 115 | 0 | 0 | 0 | 863.7 | 6.0 | - | passed 103, failed 12 | - | - | - |
| godot-pad-claim | `dotnet format` | 75 | 0 | 0 | 0 | 833.4 | 10.1 | - | passed 74, failed 1 | - | - | - |
| (no repo) | `pwsh .../harness.ps1` | 7 | 0 | 0 | 0 | 823.2 | 79.0 | - | passed 5, stalled 1, failed 1 | - | - | - |
| delve-the-dungeon | `pwsh tools/test-all.ps1` | 2 | 6 | 0 | 0 | 805.4 | 402.7 | 188.3 | failed 1, passed 1 | --local 2 | - | - |
| gate | `cargo build` | 16 | 0 | 0 | 0 | 799.5 | 46.7 | - | passed 14, failed 2 | --local 15 | - | - |
| career-driver | `blender 1` | 24 | 0 | 0 | 0 | 619.2 | 21.2 | - | passed 20, failed 4 | - | - | - |
| world-of-intrigue | `godot_console Dummy` | 2 | 0 | 0 | 0 | 576.7 | 288.4 | - | timed-out 1, passed 1 | - | - | - |
| world-of-intrigue | `git commit` | 18 | 0 | 0 | 0 | 550.5 | 18.8 | - | passed 16, failed 2 | - | - | - |
| world-of-intrigue | `godot_console` | 41 | 0 | 0 | 0 | 494.3 | 9.9 | - | passed 41 | - | - | - |
| yaat | `dotnet test` | 11 | 440 | 0 | 0 | 457.3 | 31.1 | 36.3 | passed 9, failed 2 | --local 10 | version 6, unreachable 5 | - |
| godot-mcp | `dotnet publish` | 42 | 0 | 0 | 0 | 411.7 | 8.2 | - | passed 42 | - | - | - |
| career-driver | `dotnet test` | 109 | 0 | 0 | 0 | 310.4 | 2.3 | - | passed 100, failed 9 | - | - | - |
| world-of-intrigue | `pwsh -Command` | 22 | 0 | 0 | 0 | 297.7 | 11.4 | - | passed 22 | - | - | - |
| gate | `cargo nextest` | 4 | 245 | 0 | 0 | 288.6 | 47.0 | 28.5 | passed 3, failed 1 | --local 4 | - | - |
| gate | `git commit` | 28 | 0 | 0 | 0 | 263.8 | 1.2 | - | passed 28 | - | - | - |
| yaat | `pwsh .../clone-watch.ps1` | 4 | 0 | 0 | 0 | 235.9 | 2.9 | - | failed 2, passed 2 | - | - | - |
| godot-xaml | `git push` | 21 | 0 | 0 | 0 | 228.9 | 9.7 | - | passed 21 | - | - | - |
| yaat | `pwsh .tmp/fmt-1266r4.ps1` | 3 | 0 | 0 | 0 | 218.7 | 71.8 | - | passed 3 | - | - | - |
| godot-xaml | `uv run python .tmp/r1mut/mutate.py` | 2 | 0 | 0 | 0 | 217.6 | 108.8 | - | log-failure 1, passed 1 | - | - | - |
| career-driver | `dotnet csharpier` | 145 | 0 | 2 | 0 | 212.5 | 0.9 | - | passed 135, failed 10 | - | - | - |
| yaat | `bash .tmp/probe1656/runall.sh` | 1 | 0 | 0 | 0 | 207.7 | 207.7 | - | passed 1 | --local 1 | - | - |
| godot-pad-claim | `prek run` | 1 | 0 | 0 | 0 | 182.8 | 182.8 | - | passed 1 | - | - | - |
| career-driver | `godot_console` | 16 | 0 | 0 | 0 | 146.9 | 7.5 | - | passed 16 | - | - | - |
| world-of-intrigue | `git push` | 4 | 0 | 0 | 0 | 142.7 | 36.5 | - | passed 4 | - | - | - |
| godot-xaml | `pwsh godot/export.ps1` | 3 | 0 | 0 | 0 | 135.7 | 36.0 | - | passed 2, failed 1 | - | - | - |
| gate | `cargo check` | 2 | 0 | 0 | 0 | 87.4 | 43.7 | - | failed 2 | - | - | - |
| claude-config | `pwsh tools/build-box/test-install-toolchains.ps1` | 1 | 0 | 0 | 0 | 85.3 | 85.3 | - | passed 1 | - | - | - |
| godot-mcp | `pwsh .../install.ps1` | 7 | 0 | 0 | 0 | 80.2 | 7.7 | - | passed 7 | - | - | - |
| yaat | `gpufallbackprobe` | 5 | 0 | 0 | 0 | 77.7 | 14.4 | - | passed 4, log-failure 1 | --local 5 | - | - |
| godot-pad-claim | `dotnet test` | 34 | 0 | 0 | 0 | 75.4 | 1.8 | - | passed 25, failed 9 | - | - | - |
| godot-pad-claim | `git push` | 4 | 0 | 0 | 0 | 72.6 | 19.1 | - | passed 4 | - | - | - |
| godot-xaml | `dotnet run` | 4 | 0 | 0 | 0 | 67.6 | 17.9 | - | passed 3, failed 1 | - | - | - |
| godot-xaml | `pwsh spikes/xscn-loader/editor-checks.ps1` | 1 | 0 | 0 | 0 | 63.1 | 63.1 | - | passed 1 | - | desktop 1 | - |
| yaat | `probe load` | 13 | 0 | 0 | 0 | 62.6 | 4.0 | - | passed 12, log-failure 1 | --local 13 | - | - |
| godot-xaml | `git commit` | 7 | 0 | 0 | 0 | 55.3 | 7.7 | - | passed 7 | - | - | - |
| godot-pad-claim | `dotnet csharpier` | 40 | 0 | 0 | 0 | 49.3 | 1.0 | - | passed 38, failed 2 | - | - | - |
| claude-config | `uv run python .tmp/e2e_thrice.py` | 2 | 0 | 0 | 0 | 43.9 | 21.9 | - | passed 2 | - | - | - |
| world-of-intrigue | `prek run` | 3 | 0 | 0 | 0 | 39.7 | 14.2 | - | passed 2, failed 1 | - | - | - |
| world-of-intrigue | `pwsh woi.ps1` | 1 | 0 | 0 | 0 | 38.2 | 38.2 | - | passed 1 | - | - | - |
| godot-pad-claim | `godot_console demo` | 6 | 0 | 0 | 0 | 37.9 | 6.0 | - | passed 6 | - | - | - |
| (gone) | `git push` | 1 | 0 | 0 | 0 | 37.0 | 37.0 | - | passed 1 | - | - | - |
| collie | `bun run` | 6 | 0 | 0 | 0 | 35.6 | 5.8 | - | passed 6 | - | - | - |
| yaat | `pwsh .tmp/measure-1663/scripts/run-gpu-test.ps1` | 2 | 0 | 0 | 0 | 35.3 | 17.6 | - | passed 2 | - | - | - |
| godot-mcp | `git commit` | 4 | 0 | 0 | 0 | 28.4 | 6.9 | - | passed 4 | - | - | - |
| delve-the-dungeon | `godot_console` | 3 | 88 | 1 | 0 | 27.9 | 8.9 | 43.1 | passed 1, log-failure 1, failed 1 | - | - | - |
| career-driver | `bash .tmp/mocks/drive-38-trees/src/iterate.sh` | 2 | 0 | 0 | 0 | 20.4 | 10.2 | - | failed 1, passed 1 | - | - | - |
| godot-mcp | `dotnet csharpier` | 9 | 33 | 0 | 0 | 18.7 | 1.8 | 5.7 | passed 9 | - | - | - |
| world-of-intrigue | `dotnet build` | 2 | 687 | 0 | 0 | 17.0 | 8.5 | 8.8 | passed 2 | --local 1 | version 1 | - |
| yaat | `probe1656 B01` | 1 | 0 | 0 | 0 | 15.3 | 15.3 | - | log-failure 1 | --local 1 | - | - |
| delve-the-dungeon | `dotnet msbuild` | 5 | 0 | 0 | 0 | 10.5 | 1.6 | - | passed 5 | - | - | - |
| yaat | `yaat.speechsandbox` | 1 | 0 | 0 | 0 | 9.8 | 9.8 | - | passed 1 | --local 1 | - | - |
| yaat | `gpufallbackprobe cpu` | 1 | 0 | 0 | 0 | 9.6 | 9.6 | - | passed 1 | --local 1 | - | - |
| (gone) | `dotnet build` | 1 | 0 | 0 | 0 | 7.1 | 7.1 | - | passed 1 | - | - | - |
| yaat | `pwsh .tmp/gpu-spike/run-nocuda.ps1` | 2 | 0 | 0 | 0 | 6.7 | 3.3 | - | failed 2 | --local 2 | - | - |
| (no repo) | `git` | 1 | 0 | 0 | 0 | 6.0 | 6.0 | - | passed 1 | - | - | - |
| collie | `bun` | 2 | 0 | 0 | 0 | 4.9 | 2.5 | - | passed 2 | - | - | - |
| career-driver | `blender` | 1 | 0 | 0 | 0 | 3.7 | 3.7 | - | passed 1 | - | - | - |
| gate | `git push` | 1 | 0 | 0 | 0 | 3.5 | 3.5 | - | passed 1 | - | - | - |
| yaat | `git cherry-pick` | 4 | 0 | 0 | 0 | 2.9 | 0.7 | - | passed 4 | - | - | - |
| godot-pad-claim | `dotnet msbuild` | 2 | 0 | 0 | 0 | 1.9 | 1.0 | - | passed 2 | - | - | - |
| career-driver | `dotnet msbuild` | 2 | 0 | 0 | 0 | 1.5 | 0.8 | - | passed 2 | - | - | - |
| collie | `bun install` | 4 | 0 | 0 | 0 | 1.3 | 0.3 | - | passed 4 | - | - | - |
| yaat | `dotnet tool` | 1 | 0 | 0 | 0 | 0.9 | 0.9 | - | passed 1 | - | - | - |
| delve-the-dungeon | `dotnet build` | 0 | 335 | 0 | 0 | 0 | - | 33.4 | - | - | - | - |
| delve-the-dungeon | `dotnet csharpier` | 0 | 134 | 0 | 0 | 0 | - | 9.8 | - | - | version 2, unreachable 1 | - |
| delve-the-dungeon | `dotnet format` | 0 | 97 | 0 | 0 | 0 | - | 41.9 | - | - | version 1 | - |
| delve-the-dungeon | `dotnet test` | 0 | 223 | 0 | 0 | 0 | - | 16.8 | - | - | - | - |
| gate | `cargo clippy` | 0 | 137 | 0 | 0 | 0 | - | 15.8 | - | - | - | - |
| godot-mcp | `dotnet build` | 0 | 89 | 0 | 0 | 0 | - | 13.3 | - | - | - | - |
| godot-mcp | `dotnet format` | 0 | 4 | 0 | 0 | 0 | - | 16.9 | - | - | - | - |
| godot-mcp | `dotnet test` | 0 | 15 | 0 | 0 | 0 | - | 127.0 | - | - | - | - |
| godot-mcp | `pwsh run.ps1` | 0 | 38 | 0 | 0 | 0 | - | 180.4 | - | - | - | - |
| rustling-tulip | `cargo check` | 0 | 1 | 0 | 0 | 0 | - | 77.2 | - | - | - | - |
| rustling-tulip | `cargo clippy` | 0 | 77 | 0 | 0 | 0 | - | 36.7 | - | - | - | - |
| rustling-tulip | `pwsh .../rt.ps1` | 0 | 8 | 0 | 0 | 0 | - | 283.9 | - | - | - | - |
| world-of-intrigue | `dotnet csharpier` | 0 | 202 | 0 | 0 | 0 | - | 7.2 | - | - | - | - |
| world-of-intrigue | `dotnet format` | 0 | 396 | 0 | 0 | 0 | - | 11.7 | - | - | - | - |
| world-of-intrigue | `dotnet test` | 0 | 162 | 0 | 0 | 0 | - | 10.6 | - | - | - | - |
| yaat | `dotnet csharpier` | 0 | 6 | 0 | 0 | 0 | - | 8.9 | - | - | - | - |
| yaat | `dotnet format` | 0 | 62 | 0 | 0 | 0 | - | 29.9 | - | - | - | - |
| yaat | `pwsh tools/test-all.ps1` | 0 | 97 | 0 | 0 | 0 | - | 410.4 | - | - | version 1 | - |
| yaat-server | `dotnet build` | 0 | 6 | 0 | 0 | 0 | - | 74.8 | - | - | - | - |

## `--local` runs

| Repo | Family | Reason | Runs |
|---|---|---|---|
| yaat | `probe load` | Windows-only native stderr capture proof | 13 |
| gate | `cargo build` | the installer builds the gate on this PC | 9 |
| gate | `cargo build` | the GATE-1039 spike runs in this PC's interactive session | 6 |
| yaat | `dotnet run` | throwaway probe compiled from gitignored .tmp via CustomAfterMicrosoftCommonTargets; a routed snapshot omits .tmp (docs/test-harness.md Throwaway probes) | 5 |
| yaat | `gpufallbackprobe` | GPU spike | 5 |
| claude-config | `uv run pytest` | linear needs 3.14; builder picks 3.13 (CC-555) | 4 |
| gate | `cargo nextest` | the GATE-819 red needs this PC's built fake-tool | 4 |
| claude-config | `uv run pytest` | builder picks CPython 3.13; linear needs 3.14 (argparse annotation TypeError at import) | 3 |
| yaat | `cloneprobe` | scratch probe outside the repo (YAAT-1676) | 3 |
| yaat | `dotnet build` | Windows-only native stderr capture proof | 3 |
| yaat | `dotnet build` | scratch probe outside the repo (YAAT-1676) | 3 |
| yaat | `dotnet run` | measure YAAT-1643 per NavData/CIFP combination on one machine; run C needs a seeded scratch app-data dir a routed snapshot does not carry | 3 |
| yaat | `dotnet test` | Windows-only installer and loader tests | 3 |
| yaat | `git commit` | routed pre-commit hook lacks restored obj (readonly ObservableProperty) | 3 |
| yaat | `dotnet run` | confirm the YAAT-1643 red test is red on both NavData cycles on one machine; the committed-data run needs a fresh scratch app-data dir | 2 |
| yaat | `dotnet test` | Windows-only installer tests | 2 |
| yaat | `pwsh .tmp/gpu-spike/run-nocuda.ps1` | GPU spike | 2 |
| claude-config | `uv run pytest` | linear needs 3.14; builder picks 3.13 (booked) | 1 |
| delve-the-dungeon | `pwsh tools/test-all.ps1` | DELVE-992: the routed test-all's Godot import crashes on the builder before it finishes (gate GATE-1026); local until fixed | 1 |
| delve-the-dungeon | `pwsh tools/test-all.ps1` | routed test-all failed at the Godot import on the builder (theme failed to load on the second pass; the branch does not change the theme) | 1 |
| gate | `gate run` | GATE-819 probe of the stall window | 1 |
| rustling-tulip | `cargo build` | debug run needs a local binary | 1 |
| rustling-tulip | `cargo build` | release smoke needs a local binary | 1 |
| world-of-intrigue | `dotnet build` | WOI-66: the client's Debug build for local windowed Godot measurement runs, per the brief | 1 |
| yaat | `bash .tmp/probe1656/runall.sh` | Windows-only native stderr capture proof | 1 |
| yaat | `dotnet build` | GPU spike needs a local binary | 1 |
| yaat | `dotnet build` | binary for the suspend rig (YAAT-1676) | 1 |
| yaat | `dotnet build` | rebuild the local bin/obj so the removed .tmp probe types and swapped GroundNavigator leave it (docs/test-harness.md Throwaway probes) | 1 |
| yaat | `dotnet build` | rebuild the local bin/obj so the removed .tmp probe types leave it (docs/test-harness.md Throwaway probes) | 1 |
| yaat | `dotnet csharpier` | routed csharpier format rewrote files on the builder only; edits must land in the local worktree | 1 |
| yaat | `dotnet restore` | binary for the suspend rig (YAAT-1676) | 1 |
| yaat | `dotnet run` | Piper voice is a local file | 1 |
| yaat | `dotnet run` | throwaway probe swapping GroundNavigator.cs for its pre-YAAT-1702 version from gitignored .tmp; a routed snapshot omits .tmp | 1 |
| yaat | `dotnet test` | Windows branches of SherpaNativeLoaderTests | 1 |
| yaat | `dotnet test` | Windows-only loader tests (mapped DLL copies) | 1 |
| yaat | `dotnet test` | decodes with the overlaid native on this PC | 1 |
| yaat | `dotnet test` | needs this PC's GPU | 1 |
| yaat | `dotnet test` | routed run landed on {the Mac builder}, which skips the Windows-only in-use and IO-failure removal tests this fix must prove | 1 |
| yaat | `git commit` | routed pre-commit hook on {the Windows builder} has no restored obj; dotnet format marked ObservableProperty fields readonly | 1 |
| yaat | `gpufallbackprobe cpu` | GPU spike | 1 |
| yaat | `probe crash` | Windows-only native stderr capture proof | 1 |
| yaat | `probe stderr` | Windows-only native stderr capture proof | 1 |
| yaat | `probe1656 B01` | Windows-only native stderr capture proof | 1 |
| yaat | `pwsh tools/Yaat.SpeechSandbox/New-SherpaNativeSandbox.ps1` | GPU spike: reads the local native cache | 1 |
| yaat | `yaat.speechsandbox` | GPU spike | 1 |

- Window: 2026-11-01T17:50:40+00:00 to 2026-11-02T17:50:40+00:00 (UTC).
- Files read: 31 archives, events.1.jsonl, events.jsonl; unreadable: none.
- Skipped lines: 0; duplicate lines left out: 2922.
- Runs with neither a queue nor a release line in the window: 2; heavy runs lent or covered, with no release of their own, left out: 0.
- The archive keeps only release lines: a release carries notRouted, reason, fellBack and parentPid, and one journalled before the gate wrote them keeps none; the live queue lines cover only the runs queued from 2026-11-02T06:42:23+00:00.
- nested gates linked by parentPid: 0; by containment: 0.
- commits with covered hook gates, share not split: 0.

## Sort

The window holds no unexplained pre-cut releases: every release in it carries its routing facts. Only families with local runs are sorted; families that only routed need nothing.

| Repo | Family | State | Evidence |
|---|---|---|---|
| career-driver | `dotnet build`, `dotnet format`, `dotnet test`, `dotnet csharpier` | ready to route | No `remote.json` entry at all. 1,103 local runs, 12,873 s. Builds are mostly `dotnet build <abs slnx> -c Release -warnaserror`; tests `dotnet test CareerDriver.slnx -c Release --no-build`. Short runs (build median 6.3 s), but world-of-intrigue's routed build median is 8.8 s against 8.5 s local, so routing costs little. |
| career-driver | `godot_console` (`--build-solutions` on `src/Career.Client`), `blender 1`, `blender` | not sorted | Asked of career-driver with GATE-1057: whether the client build feeds a local windowed run (as world-of-intrigue's does) and what Blender reads. |
| godot-pad-claim | `dotnet build`, `dotnet format`, `dotnet test`, `dotnet csharpier` | ready to route | No `remote.json` entry. 264 local runs, 1,822 s. |
| godot-pad-claim | `godot_console demo` | not sorted | Asked with GATE-1057, as above. |
| godot-xaml | `dotnet build`, `dotnet test` | ready to route | Its entry routes only `spikes?xscn-loader?editor-checks.ps1`. 552 local runs, 4,047 s. |
| godot-xaml | `pwsh godot/export.ps1`, `dotnet run`, `.tmp/r1mut/mutate.py` | not sorted / one-off | The export and run asked with GATE-1057; the mutation script is a one-off. |
| world-of-intrigue | `uv run python .tmp/woi94/driver.py` | one-off | 69 runs, 21,241 s. WOI-94's headless sim sweep, worktree removed (world-of-intrigue's answer). Later sweeps of its kind run `Intrigue.Sim` and route with it. |
| world-of-intrigue | `dotnet run` (`src/Intrigue.Sim`) | ready to route | 17 runs, 4,218 s. Writes stdout, stderr and, with `--out`, one JSONL file under `.tmp/` that must sync back. |
| world-of-intrigue | `blender` | truly local | Reads and writes the repo's private, large G: config folder (the 3D bake-off). Needs a `.gate/local.toml` pattern in world-of-intrigue. |
| world-of-intrigue | `godot_console` (`src/Intrigue.Client --build-solutions`), `godot_console Dummy` | truly local | The worktree warm step; the local `--import` and scratch runs use its output straight after (world-of-intrigue's rule). |
| world-of-intrigue | `pwsh -Command` (wrapping `dotnet build`) | ready to route, no change | An agent's own wrapping; `pwsh woi.ps1 build` hands the bare form, which routes. |
| yaat | `git commit` | ready to route (routes already), miscounted | 43 heavy commits, 2,595 s. yaat is in `commits`, and their pre-commit stage ran on the builder, but the commit's release line does not say so (GATE-1058). 3 more ran `--local` because the routed stage has no restored `obj` (GATE-1059). |
| yaat | `dotnet test`, `dotnet run`, `dotnet build`, probes, GPU spikes, `gpufallbackprobe`, `yaat.speechsandbox` | truly local, or routes already | The `--local` reasons are this PC's GPU, Windows-only native loader and stderr proofs, the local native cache, a Piper voice file and `.tmp` probes (the last booked in the 2026-11-01 census). Fallbacks: version 6, unreachable 5 on `dotnet test`. |
| claude-config | `uv run pytest` | blocked on work (claude-config's) | 50 local, 9 routed. The builder picks CPython 3.13 and the `linear` tool needs 3.14 (claude-config's CC-555). |
| rustling-tulip | `cargo nextest`, `cargo run` | routes already | 3 runs, 1,957 s, fell back on `busy`. |
| rustling-tulip | `cargo build` | truly local | `--local`: debug and release smoke runs need a local binary. |
| delve-the-dungeon | `pwsh tools/test-all.ps1` | blocked on work | `--local` 2: the routed run's Godot import crashes on the builder (GATE-1026). |
| gate | `cargo build`, `cargo nextest`, `cargo check` | truly local | The installer builds here; GATE-1039 and GATE-819 rigs need this PC's session or built stand-ins. |
| every repo | `git commit` with no nested gate, `git push`, `prek run`, `git cherry-pick`, `git` | truly local by rule | Heavy commits with no nested gate (gate 28, world-of-intrigue 18, godot-xaml 7, godot-mcp 4). Light commits with nested gates are not split (GATE-981). |
| godot-mcp | `dotnet publish`, `pwsh .../install.ps1` | truly local | Installs into this PC's folders. |
| collie | `bun run`, `bun`, `bun install` | not sorted | 12 runs, 42 s; under any routing floor. |
| (no repo) | `pwsh .../harness.ps1` | not sorted | 7 runs, 823 s, run from a folder in no repo. |

Fallbacks over the window: `version` 15 (mostly 2026-11-02T02Z, between a reinstall here and the push to the builders), `unreachable` 6, `busy` 4, `desktop` 1.

## Bookings

### Gate issues

- GATE-1057 (Route career-driver, godot-pad-claim and godot-xaml, and the world-of-intrigue commands its patterns miss), High, under GATE-192. world-of-intrigue's answers are on it as a comment.
- GATE-1059 (Restore yaat's obj before its routed commit stage runs dotnet format), High, bug.
- GATE-1058 (Journal a routed commit stage on the commit's release line and count it routed in the census), no priority.
- GATE-1060 (Keep builder host names out of the census report and the durable docs), no priority.

### Repo inbox notes

- world-of-intrigue: what `driver.py`, the `Intrigue.Sim` runs, Blender and the client build need. Answered the same hour; the answers are in the Sort above and on GATE-1057.

### Truly local, still to record

- world-of-intrigue's Blender bake-off runs and the client `--build-solutions` warm step: `.gate/local.toml` patterns in world-of-intrigue, part of GATE-1057's notes to that repo.
