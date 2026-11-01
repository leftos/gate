# Local-run census, 2026-11-01

A run of the `local-run-census` skill (`.claude/skills/local-run-census/`), asked for by the owner. It covers every heavy command family the gate journal shows running on this PC over the last 7 days, sorted into the three states of [routing-a-repo.md](../routing-a-repo.md#which-repos-and-which-commands): ready to route, blocked on work, or truly local.

## Window

The last 7 days, 2026-10-25T05:41Z to 2026-11-01T05:41Z, read from 27 journal archives, `events.1.jsonl` and `events.jsonl`. 262,676 local heavy seconds (about 73 hours) in 190 families. 36 families make 90% of it.

By repo, in local seconds: yaat 67,094, rustling-tulip 58,262, godot-mcp 45,591, towercab-3d 30,446, claude-config 24,815, gate 14,244, delve-the-dungeon 5,695, in-the-sky 3,667, opening-hand 2,700, craft-tester 2,535, yamv 2,022, in-the-sky-ops 1,491.

**The week overstates today's gap.** Releases journalled before 2026-10-29T16:52Z carry no `notRouted`, and `fellBack` starts at 07:24Z the same day. In the .NET and Godot repos alone, 906 local runs (54,066 s) match a current `repos` pattern and have no reason on their release; after the cut, nearly every such command routes or carries a reason.

Several large families also moved during the window:

- godot-mcp's lane gates went inside one routed `run.ps1 itest` at godot-mcp ee1edb1d (10-30): 35,711 s.
- towercab-3d `cargo test` got its pattern on 10-27: 8,267 s.
- rustling-tulip `rt.ps1 test` got its absolute-path patterns on 10-31: 6,086 s.

A census over a window wholly after 2026-10-29 is the honest baseline.

## Heavy families

| Repo | Family | Local | Routed | Unfinished | Covered | Local s total | Local median s | Routed median s | Local verdicts | notRouted | Fallbacks | State |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| godot-mcp | `pwsh .../hidden-desktop.ps1` | 196 | 210 | 0 | 0 | 32464.2 | 119.0 | 203.2 | passed 191, failed 5 | - | desktop 23, busy 4 | - |
| yaat | `git commit (ungated hooks)` | 180 | 0 | 0 | 0 | 29548.1 | 144.6 | - | - | - | - | blocked on work: ungated hook commands in the repo's git hooks |
| rustling-tulip | `pwsh .../rt.ps1` | 85 | 9 | 0 | 0 | 27429.4 | 253.4 | 147.2 | passed 69, failed 14, stalled 2 | - | - | - |
| claude-config | `pwsh tools/build-box/image/Invoke-ImageDrill.ps1` | 9 | 0 | 0 | 0 | 18905.8 | 1465.7 | - | failed 8, passed 1 | --local 9 | - | - |
| yaat | `dotnet run` | 300 | 265 | 1 | 0 | 12143.7 | 22.2 | 25.8 | passed 201, failed 97, timed-out 1, stalled 1 | --local 5, declared local 1 | busy 2 | - |
| towercab-3d | `pnpm vitest` | 358 | 0 | 0 | 0 | 10361.2 | 19.1 | - | passed 205, failed 153 | - | - | - |
| gate | `cargo nextest` | 52 | 1305 | 0 | 0 | 9876.1 | 80.4 | 59.5 | passed 40, failed 12 | --local 7 | - | - |
| towercab-3d | `cargo test` | 86 | 2 | 0 | 0 | 9588.1 | 85.3 | 103.1 | passed 47, failed 38, stalled 1 | - | - | - |
| rustling-tulip | `cargo build` | 38 | 0 | 1 | 0 | 8176.6 | 179.9 | - | passed 27, failed 11 | --local 1 | - | - |
| rustling-tulip | `pwsh -Command` | 27 | 0 | 0 | 0 | 7513.6 | 266.3 | - | passed 15, failed 12 | - | - | - |
| rustling-tulip | `cargo run` | 123 | 0 | 0 | 0 | 7274.4 | 12.3 | - | passed 121, failed 2 | - | - | - |
| yaat | `git commit` | 68 | 0 | 0 | 0 | 7263.2 | 58.4 | - | passed 63, failed 4, timed-out 1 | - | - | - |
| yaat | `dotnet build` | 106 | 754 | 0 | 0 | 5462.5 | 45.2 | 108.4 | passed 90, failed 16 | --local 32, gitignored input 2 | - | - |
| yaat | `pwsh tools/test-all.ps1` | 8 | 275 | 2 | 0 | 4453.8 | 547.2 | 276.2 | failed 7, stalled 1 | - | - | - |
| claude-config | `uv run pytest` | 86 | 0 | 0 | 0 | 4160.6 | 13.6 | - | passed 56, failed 30 | - | - | - |
| godot-mcp | `pwsh .../run.ps1` | 26 | 44 | 0 | 0 | 3246.7 | 102.4 | 168.0 | passed 25, failed 1 | - | desktop 5 | - |
| yaat | `dotnet test` | 58 | 1760 | 2 | 0 | 2876.5 | 32.0 | 26.8 | failed 29, passed 28, timed-out 1 | --local 5 | - | - |
| gate | `cargo build` | 54 | 0 | 0 | 0 | 2668.0 | 37.5 | - | passed 53, failed 1 | --local 7 | - | - |
| opening-hand | `git commit` | 41 | 0 | 0 | 0 | 2645.1 | 55.4 | - | passed 40, failed 1 | - | - | - |
| towercab-3d | `uv run python scripts/shipping/data-generation/extract-airport-buildings.py` | 2 | 0 | 0 | 0 | 2581.9 | 1291.0 | - | passed 2 | - | - | - |
| towercab-3d | `pnpm run` | 24 | 95 | 0 | 0 | 2541.3 | 17.3 | 32.4 | passed 19, failed 5 | - | - | - |
| rustling-tulip | `cargo nextest` | 22 | 1369 | 0 | 0 | 2479.7 | 10.6 | 70.4 | passed 18, failed 4 | --local 20, another worktree 1 | - | - |
| godot-mcp | `dotnet build` | 146 | 961 | 0 | 0 | 2314.7 | 8.0 | 25.2 | passed 143, failed 3 | - | busy 2 | - |
| godot-mcp | `dotnet test` | 64 | 137 | 0 | 0 | 2175.0 | 25.9 | 132.8 | failed 32, passed 32 | faster here 1 | - | - |
| in-the-sky | `git commit` | 39 | 0 | 0 | 0 | 2109.0 | 44.2 | - | passed 38, failed 1 | - | - | - |
| godot-mcp | `git commit` | 58 | 0 | 0 | 0 | 2051.9 | 30.9 | - | passed 52, failed 6 | - | - | - |
| towercab-3d | `git commit` | 45 | 0 | 0 | 0 | 2039.5 | 13.2 | - | passed 41, failed 4 | - | - | - |
| rustling-tulip | `pwsh ./rt.ps1` | 4 | 2 | 0 | 0 | 1889.2 | 462.4 | 53.4 | passed 3, failed 1 | - | - | - |
| claude-config | `pwsh tools/collie-patched-update/test-collie-patched-update.ps1` | 5 | 0 | 0 | 0 | 1532.0 | 319.1 | - | passed 3, failed 2 | - | - | - |
| in-the-sky | `dotnet test` | 115 | 184 | 0 | 0 | 1437.0 | 8.9 | 7.8 | failed 63, passed 52 | faster here 4 | - | - |
| craft-tester | `pnpm web` | 217 | 0 | 0 | 0 | 1427.9 | 2.9 | - | passed 177, failed 40 | - | - | - |
| delve-the-dungeon | `dotnet format` | 17 | 915 | 0 | 0 | 1289.5 | 65.7 | 37.1 | passed 17 | - | busy 1 | - |
| delve-the-dungeon | `dotnet test` | 33 | 1765 | 0 | 0 | 1278.5 | 29.9 | 26.1 | passed 27, failed 6 | --local 18 | - | - |
| rustling-tulip | `pwsh .tmp/rt551/stress.ps1` | 1 | 0 | 0 | 0 | 1249.2 | 1249.2 | - | failed 1 | - | - | - |
| godot-mcp | `prek run` | 20 | 0 | 0 | 0 | 1198.3 | 57.7 | - | passed 17, failed 3 | - | - | - |
| rustling-tulip | `git push` | 30 | 0 | 0 | 0 | 1181.9 | 34.0 | - | passed 28, failed 2 | - | - | - |
| yaat | `pwsh tools/server-image/build.ps1` | 6 | 2 | 0 | 0 | 1174.6 | 45.7 | 226.5 | failed 3, passed 3 | - | - | - |
| yaat | `git cherry-pick` | 9 | 0 | 0 | 0 | 1090.8 | 26.2 | - | passed 6, failed 3 | - | - | - |
| towercab-3d | `cargo clippy` | 21 | 15 | 0 | 0 | 1064.9 | 18.8 | 43.6 | passed 17, failed 4 | - | - | - |
| yamv | `git push` | 41 | 0 | 0 | 0 | 978.8 | 21.5 | - | passed 41 | - | - | - |
| godot-mcp | `pwsh run.ps1` | 10 | 33 | 0 | 0 | 962.8 | 82.4 | 118.9 | passed 10 | --local 2 | desktop 8 | - |
| delve-the-dungeon | `dotnet build` | 14 | 2443 | 1 | 0 | 962.0 | 69.5 | 38.9 | passed 12, failed 2 | --local 6 | version 1 | - |
| delve-the-dungeon | `git` | 6 | 0 | 0 | 0 | 717.7 | 114.5 | - | passed 6 | - | - | - |
| yaat | `git` | 3 | 0 | 0 | 0 | 697.0 | 4.7 | - | passed 3 | - | - | - |
| towercab-3d | `node scripts/shipping/build/vnas.js` | 10 | 0 | 0 | 0 | 688.1 | 15.3 | - | passed 9, failed 1 | - | - | - |
| yaat | `bash .tmp/nemo-export.sh` | 1 | 0 | 0 | 0 | 687.2 | 687.2 | - | passed 1 | - | - | - |
| world-of-intrigue | `7z a` | 1 | 0 | 0 | 0 | 675.1 | 675.1 | - | passed 1 | - | - | - |
| (no repo) | `bash -c` | 7 | 0 | 0 | 0 | 657.0 | 82.9 | - | passed 7 | --local 7 | - | - |
| herdr-gpui | `cargo clippy` | 1 | 23 | 0 | 0 | 627.4 | 627.4 | 140.4 | passed 1 | - | - | - |
| gate | `cargo clippy` | 12 | 820 | 0 | 0 | 621.9 | 24.0 | 29.9 | passed 8, failed 4 | - | - | - |
| towercab-3d | `uv run python .../build_media.py` | 2 | 0 | 0 | 0 | 618.8 | 309.4 | - | passed 2 | - | - | - |
| yaat | `yaat.speechsandbox` | 9 | 0 | 0 | 0 | 618.5 | 61.1 | - | passed 9 | --local 9 | - | - |
| in-the-sky-ops | `dotnet format` | 58 | 269 | 0 | 0 | 598.0 | 8.5 | 9.5 | passed 58 | - | - | - |
| gate | `git commit` | 167 | 0 | 0 | 0 | 593.0 | 0.9 | - | passed 166, failed 1 | - | - | - |
| in-the-sky-ops | `dotnet build` | 53 | 538 | 0 | 0 | 567.3 | 7.9 | 7.1 | passed 43, failed 10 | --local 3 | - | - |
| yamv | `git commit` | 29 | 0 | 0 | 0 | 524.4 | 14.5 | - | passed 29 | - | - | - |
| sherpa-onnx | `cmake build-cuda` | 1 | 0 | 0 | 0 | 487.7 | 487.7 | - | passed 1 | --local 1 | - | - |
| rustling-tulip | `cargo check` | 3 | 34 | 0 | 0 | 440.7 | 91.3 | 20.2 | passed 3 | - | - | - |
| sherpa-onnx | `cmake build-cpu` | 1 | 0 | 0 | 0 | 432.3 | 432.3 | - | failed 1 | --local 1 | - | - |
| delve-the-dungeon | `git commit (ungated hooks)` | 16 | 0 | 0 | 0 | 391.6 | 17.2 | - | - | - | - | blocked on work: ungated hook commands in the repo's git hooks |
| godot-mcp | `pwsh .../suspend-rig.ps1` | 7 | 0 | 0 | 0 | 379.4 | 48.2 | - | failed 4, passed 3 | - | - | - |
| gate | `cargo check` | 9 | 0 | 0 | 0 | 349.9 | 13.8 | - | failed 8, passed 1 | - | - | - |
| rustling-tulip | `pwsh .../e2e-sealed-local.ps1` | 1 | 0 | 0 | 0 | 335.3 | 335.3 | - | passed 1 | --local 1 | - | - |
| godot-mcp | `dotnet publish` | 39 | 0 | 0 | 0 | 327.6 | 5.7 | - | passed 39 | - | - | - |
| delve-the-dungeon | `bash docs/plans/canvases/fill-reveal/shoot.sh` | 2 | 0 | 0 | 0 | 313.4 | 156.7 | - | passed 2 | - | - | - |
| craft-tester | `uv run pytest` | 12 | 0 | 0 | 0 | 300.1 | 19.9 | - | passed 11, failed 1 | - | - | - |
| rustling-tulip | `cargo clippy` | 1 | 596 | 0 | 0 | 292.0 | 292.0 | 28.7 | passed 1 | another worktree 1 | - | - |
| godot-mcp | `uv run pytest` | 8 | 22 | 0 | 0 | 288.2 | 34.5 | 26.6 | failed 5, passed 3 | - | - | - |
| yamv | `pwsh -Command` | 15 | 0 | 0 | 0 | 287.5 | 11.4 | - | failed 10, passed 5 | --local 4 | - | - |
| ops-tower | `pwsh -Command` | 2 | 0 | 0 | 0 | 242.2 | 121.1 | - | failed 2 | - | - | - |
| delve-the-dungeon | `dotnet run` | 18 | 0 | 0 | 0 | 241.3 | 9.7 | - | passed 17, failed 1 | - | - | - |
| yaat | `pwsh -Command` | 5 | 0 | 0 | 0 | 235.5 | 40.3 | - | failed 5 | - | - | - |
| yamv | `dotnet test` | 9 | 100 | 0 | 0 | 231.4 | 22.8 | 39.6 | passed 7, failed 2 | faster here 8, --local 1 | - | - |
| sherpa-onnx | `cmake` | 2 | 0 | 0 | 0 | 208.4 | 104.2 | - | passed 2 | --local 2 | - | - |
| towercab-3d | `pnpm install` | 10 | 0 | 0 | 0 | 208.1 | 18.3 | - | passed 10 | - | - | - |
| yaat | `dotnet format` | 6 | 194 | 0 | 0 | 187.9 | 15.6 | 31.1 | passed 6 | - | - | - |
| towercab-3d | `cargo doc` | 9 | 0 | 0 | 0 | 182.9 | 11.1 | - | passed 9 | - | - | - |
| craft-tester | `uv run craft-gen` | 22 | 0 | 0 | 0 | 179.5 | 7.9 | - | passed 20, failed 2 | - | - | - |
| towercab-3d | `bash .tmp/mut/run.sh` | 1 | 0 | 0 | 0 | 176.8 | 176.8 | - | passed 1 | - | - | - |
| towercab-3d | `cargo build` | 1 | 0 | 0 | 0 | 173.0 | 173.0 | - | passed 1 | - | - | - |
| in-the-sky-ops | `godot_console` | 13 | 0 | 0 | 0 | 170.3 | 7.6 | - | passed 13 | - | - | - |
| delve-the-dungeon | `godot_console` | 11 | 631 | 0 | 0 | 138.8 | 5.5 | 47.5 | passed 11 | - | - | - |
| yaat | `pwsh tools/release-local/pack-win-linux.ps1` | 1 | 6 | 0 | 0 | 137.7 | 137.7 | 69.0 | passed 1 | - | busy 1 | - |
| claude-config | `wsl Ubuntu` | 3 | 0 | 0 | 0 | 134.7 | 4.8 | - | failed 1, passed 1, stalled 1 | - | - | - |
| craft-tester | `pwsh .tmp/f1.ps1` | 1 | 0 | 0 | 0 | 132.3 | 132.3 | - | passed 1 | - | - | - |
| yaat-server | `dotnet test` | 2 | 2 | 0 | 0 | 114.1 | 57.1 | 326.0 | passed 2 | - | - | - |
| yaat | `dotnet publish` | 2 | 0 | 0 | 0 | 106.9 | 53.4 | - | passed 2 | - | - | - |
| delve-the-dungeon | `uv run tools/sound/sync_compare.py` | 3 | 0 | 0 | 0 | 102.2 | 45.3 | - | passed 2, failed 1 | - | - | - |
| delve-the-dungeon | `docker build` | 4 | 0 | 0 | 0 | 101.6 | 25.4 | - | passed 4 | - | - | - |
| yaat | `pwsh .../cuda-closure.ps1` | 3 | 0 | 0 | 0 | 95.4 | 34.3 | - | passed 3 | --local 3 | - | - |
| towercab-3d-vnas | `cargo clippy` | 1 | 0 | 0 | 0 | 95.2 | 95.2 | - | passed 1 | - | - | - |
| godot-mcp | `dotnet csharpier` | 43 | 281 | 0 | 0 | 94.8 | 1.8 | 18.6 | passed 43 | - | busy 1 | - |
| gate | `pwsh -Command` | 2 | 0 | 0 | 0 | 93.8 | 46.9 | - | passed 2 | - | - | - |
| craft-tester | `pwsh .tmp/s1.ps1` | 1 | 0 | 0 | 0 | 93.1 | 93.1 | - | passed 1 | - | - | - |
| towercab-3d | `git push` | 10 | 0 | 0 | 0 | 91.5 | 8.4 | - | passed 9, failed 1 | - | - | - |
| herdr-gpui | `cargo test` | 1 | 22 | 0 | 0 | 85.9 | 85.9 | 257.7 | passed 1 | - | - | - |
| ops-tower | `cargo build` | 1 | 0 | 0 | 0 | 85.8 | 85.8 | - | passed 1 | - | - | - |
| towercab-3d-vnas | `cargo test` | 1 | 0 | 0 | 0 | 80.2 | 80.2 | - | passed 1 | - | - | - |
| towercab-3d | `git` | 2 | 0 | 0 | 0 | 78.8 | 39.4 | - | passed 2 | - | - | - |
| claude-config | `pwsh tools/build-box/test-install-toolchains.ps1` | 2 | 0 | 0 | 0 | 74.4 | 37.2 | - | failed 1, passed 1 | - | - | - |
| yaat | `bash .tmp/r1b-nodes.sh` | 1 | 0 | 0 | 0 | 65.9 | 65.9 | - | passed 1 | - | - | - |
| in-the-sky-ops | `git commit` | 5 | 0 | 0 | 0 | 64.3 | 13.4 | - | passed 5 | - | - | - |
| craft-tester | `pwsh .../feat-final.ps1` | 1 | 0 | 0 | 0 | 63.2 | 63.2 | - | passed 1 | - | - | - |
| delve-the-dungeon | `dotnet csharpier` | 30 | 1183 | 0 | 0 | 62.5 | 1.2 | 6.9 | passed 30 | another worktree 22 | - | - |
| godot-mcp | `git push` | 4 | 0 | 0 | 0 | 58.6 | 12.8 | - | passed 4 | - | - | - |
| yaat-server | `git commit (ungated hooks)` | 3 | 0 | 0 | 0 | 57.5 | 2.5 | - | - | - | - | blocked on work: ungated hook commands in the repo's git hooks |
| towercab-3d-vnas | `git push` | 1 | 0 | 0 | 0 | 55.9 | 55.9 | - | passed 1 | - | - | - |
| in-the-sky | `blender` | 4 | 0 | 0 | 0 | 55.6 | 13.8 | - | passed 4 | - | - | - |
| opening-hand | `dotnet test` | 3 | 56 | 0 | 0 | 55.3 | 15.1 | 10.4 | failed 2, passed 1 | - | - | - |
| in-the-sky-ops | `dotnet test` | 17 | 83 | 0 | 0 | 55.2 | 2.6 | 7.7 | passed 16, failed 1 | - | - | - |
| craft-tester | `pwsh -Command` | 5 | 0 | 0 | 0 | 53.0 | 5.9 | - | passed 3, failed 2 | - | - | - |
| craft-tester | `pwsh .tmp/last.ps1` | 1 | 0 | 0 | 0 | 48.3 | 48.3 | - | passed 1 | - | - | - |
| collie | `bun run` | 18 | 0 | 0 | 0 | 46.3 | 2.0 | - | passed 18 | - | - | - |
| craft-tester | `pwsh .tmp/f2.ps1` | 1 | 0 | 0 | 0 | 44.0 | 44.0 | - | passed 1 | - | - | - |
| craft-tester | `bash .tmp/fix-final.sh` | 1 | 0 | 0 | 0 | 42.7 | 42.7 | - | passed 1 | - | - | - |
| gate | `cargo test` | 2 | 0 | 0 | 0 | 41.3 | 20.7 | - | failed 2 | - | - | - |
| delve-the-dungeon | `dotnet msbuild` | 35 | 0 | 0 | 0 | 40.2 | 0.6 | - | passed 35 | - | - | - |
| craft-tester | `bash .tmp/step2.sh` | 2 | 0 | 0 | 0 | 39.0 | 19.5 | - | failed 1, passed 1 | - | - | - |
| craft-tester | `bash .tmp/final.sh` | 1 | 0 | 0 | 0 | 37.4 | 37.4 | - | passed 1 | - | - | - |
| delve-the-dungeon | `git commit` | 4 | 0 | 0 | 0 | 37.3 | 9.0 | - | passed 4 | - | - | - |
| craft-tester | `prek run` | 3 | 0 | 0 | 0 | 35.3 | 10.9 | - | passed 3 | - | - | - |
| in-the-sky | `bash .../pr68-build-test.sh` | 1 | 0 | 0 | 0 | 35.0 | 35.0 | - | passed 1 | - | - | - |
| ops-tower | `git commit` | 3 | 0 | 0 | 0 | 34.9 | 9.7 | - | passed 2, failed 1 | - | - | - |
| yaat | `yaat.client.ui.tests *ClientMenuHostGroundTests*` | 1 | 0 | 0 | 0 | 31.3 | 31.3 | - | failed 1 | - | - | - |
| yaat | `cargo nextest` | 2 | 0 | 0 | 0 | 30.6 | 15.3 | - | failed 2 | - | - | - |
| towercab-3d | `pwsh .../suspend-rig.ps1` | 1 | 0 | 0 | 0 | 29.9 | 29.9 | - | failed 1 | - | - | - |
| yaat | `dotnet` | 6 | 0 | 0 | 0 | 29.7 | 5.2 | - | passed 6 | - | - | - |
| yaat | `yaat.sim.tests *ProbeH_*` | 1 | 0 | 0 | 0 | 28.9 | 28.9 | - | failed 1 | - | - | - |
| (gone) | `cargo test` | 1 | 0 | 0 | 0 | 26.3 | 26.3 | - | failed 1 | - | - | - |
| yaat | `pwsh .tmp/rig/run-round3.ps1` | 3 | 0 | 0 | 0 | 25.9 | 7.2 | - | passed 3 | - | - | - |
| in-the-sky-ops | `dotnet csharpier` | 32 | 213 | 0 | 0 | 24.9 | 0.7 | 4.3 | passed 30, failed 2 | - | - | - |
| towercab-3d-vnas | `cargo doc` | 1 | 0 | 0 | 0 | 23.6 | 23.6 | - | passed 1 | - | - | - |
| godot-mcp | `dotnet run` | 2 | 0 | 0 | 0 | 22.4 | 11.2 | - | passed 2 | - | - | - |
| sherpa-onnx | `pwsh .tmp/smoke.ps1` | 5 | 0 | 0 | 0 | 21.8 | 4.2 | - | passed 4, log-failure 1 | --local 5 | - | - |
| yaat | `pwsh .tmp/rig/run-round2.ps1` | 1 | 0 | 0 | 0 | 21.5 | 21.5 | - | passed 1 | - | - | - |
| in-the-sky | `bash -c` | 1 | 0 | 0 | 0 | 20.2 | 20.2 | - | passed 1 | - | - | - |
| towercab-3d-vnas | `git commit` | 1 | 0 | 0 | 0 | 19.2 | 19.2 | - | passed 1 | - | - | - |
| towercab-3d | `bash .tmp/tc3d401/order.sh` | 1 | 0 | 0 | 0 | 18.6 | 18.6 | - | passed 1 | - | - | - |
| delve-the-dungeon | `dotnet publish` | 1 | 0 | 0 | 0 | 18.5 | 18.5 | - | passed 1 | - | - | - |
| yaat | `yaat.sim.tests` | 5 | 0 | 0 | 0 | 16.1 | 4.0 | - | failed 3, unknown 1, passed 1 | --local 3 | - | - |
| craft-tester | `pwsh .tmp/s4.ps1` | 1 | 0 | 0 | 0 | 14.1 | 14.1 | - | passed 1 | - | - | - |
| yaat-server | `git commit` | 2 | 0 | 0 | 0 | 11.6 | 5.8 | - | passed 2 | - | - | - |
| yaat | `pwsh .tmp/rig/run-native.ps1` | 1 | 0 | 0 | 0 | 11.3 | 11.3 | - | passed 1 | - | - | - |
| collie | `bun` | 6 | 0 | 0 | 0 | 11.1 | 1.7 | - | passed 6 | - | - | - |
| craft-tester | `bash .tmp/gen-step1.sh` | 2 | 0 | 0 | 0 | 9.7 | 4.8 | - | passed 2 | - | - | - |
| yaat | `uv run python .../mcp_probe.py` | 7 | 0 | 0 | 0 | 9.2 | 1.5 | - | passed 7 | - | - | - |
| yaat | `pwsh .tmp/rig/run-round4.ps1` | 1 | 0 | 0 | 0 | 8.9 | 8.9 | - | passed 1 | - | - | - |
| in-the-sky-ops | `prek run` | 3 | 0 | 0 | 0 | 8.7 | 1.8 | - | passed 3 | - | - | - |
| craft-tester | `bash -c` | 1 | 0 | 0 | 0 | 8.2 | 8.2 | - | failed 1 | - | - | - |
| yaat | `git rebase` | 1 | 0 | 0 | 0 | 8.1 | 8.1 | - | passed 1 | - | - | - |
| yaat | `yaat.client.tests *HoldShortMenu*` | 1 | 0 | 0 | 0 | 7.4 | 7.4 | - | failed 1 | - | - | - |
| yaat-server | `dotnet build` | 1 | 19 | 0 | 0 | 7.4 | 7.4 | 59.0 | passed 1 | - | - | - |
| in-the-sky | `ffmpeg` | 1 | 0 | 0 | 0 | 7.1 | 7.1 | - | passed 1 | - | - | - |
| claude-config | `bash .../lint.sh` | 1 | 0 | 0 | 0 | 7.0 | 7.0 | - | passed 1 | - | - | - |
| godot-mcp | `pwsh .../install.ps1` | 4 | 0 | 0 | 0 | 6.3 | 1.6 | - | passed 4 | - | - | - |
| yaat | `dec` | 2 | 0 | 0 | 0 | 6.1 | 3.0 | - | passed 2 | - | - | - |
| craft-tester | `git push` | 1 | 0 | 0 | 0 | 5.0 | 5.0 | - | passed 1 | - | - | - |
| yaat | `yaat.server.tests` | 1 | 0 | 0 | 0 | 4.8 | 4.8 | - | failed 1 | - | - | - |
| in-the-sky | `ffmpeg 24` | 1 | 0 | 0 | 0 | 3.2 | 3.2 | - | passed 1 | - | - | - |
| yaat | `yaat.sim.tests *SecondTaxiGivenOffThe28RPavement*` | 1 | 0 | 0 | 0 | 3.2 | 3.2 | - | failed 1 | - | - | - |
| towercab-3d | `cargo fmt` | 2 | 0 | 0 | 0 | 3.0 | 1.5 | - | passed 2 | - | - | - |
| yaat | `dotnet csharpier` | 1 | 25 | 0 | 0 | 2.8 | 2.8 | 7.8 | passed 1 | - | - | - |
| in-the-sky-ops | `dotnet run` | 1 | 0 | 0 | 0 | 2.7 | 2.7 | - | passed 1 | - | - | - |
| collie | `bun install` | 12 | 0 | 0 | 0 | 2.5 | 0.2 | - | passed 12 | - | - | - |
| yaat-server | `dotnet restore` | 1 | 0 | 0 | 0 | 2.0 | 2.0 | - | passed 1 | - | - | - |
| yaat | `dotnet msbuild` | 1 | 0 | 0 | 0 | 1.7 | 1.7 | - | passed 1 | - | - | - |
| craft-tester | `uv run ruff` | 4 | 0 | 0 | 0 | 1.5 | 0.2 | - | passed 4 | - | - | - |
| craft-tester | `uv run ty` | 2 | 0 | 0 | 0 | 1.1 | 0.6 | - | passed 2 | - | - | - |
| yaat | `dotnet restore` | 1 | 0 | 0 | 0 | 1.1 | 1.1 | - | passed 1 | - | - | - |
| yaat | `pwsh .tmp/rig/run-watch.ps1` | 1 | 0 | 0 | 0 | 0.7 | 0.7 | - | passed 1 | - | - | - |
| yaat-server | `git cherry-pick` | 1 | 0 | 0 | 0 | 0.7 | 0.7 | - | passed 1 | - | - | - |
| claude-config | `pwsh tools/build-box/gl-probe.ps1` | 1 | 8 | 0 | 0 | 0.5 | 0.5 | 12.1 | failed 1 | - | - | - |
| delve-the-dungeon | `pwsh tools/test-all.ps1` | 0 | 5 | 0 | 0 | 0 | - | 162.5 | - | - | - | - |
| gate | `bash tools/builder/test-install-macos.sh` | 0 | 3 | 0 | 0 | 0 | - | 17.8 | - | - | - | - |
| gate | `git add` | 1 | 0 | 0 | 0 | 0.0 | 0.0 | - | passed 1 | - | - | - |
| godot-mcp | `dotnet format` | 0 | 47 | 0 | 0 | 0 | - | 44.7 | - | - | - | - |
| herdr-gpui | `just ci` | 0 | 7 | 0 | 0 | 0 | - | 191.6 | - | - | - | - |
| herdr-gpui | `wsl Ubuntu` | 0 | 14 | 0 | 0 | 0 | - | 188.8 | - | - | - | - |
| in-the-sky | `dotnet build` | 0 | 749 | 0 | 0 | 0 | - | 25.7 | - | - | - | - |
| in-the-sky | `dotnet csharpier` | 0 | 221 | 0 | 0 | 0 | - | 3.8 | - | - | - | - |
| in-the-sky | `dotnet format` | 0 | 424 | 0 | 0 | 0 | - | 9.2 | - | - | - | - |
| opening-hand | `dotnet build` | 0 | 417 | 0 | 0 | 0 | - | 24.8 | - | - | - | - |
| opening-hand | `dotnet csharpier` | 0 | 80 | 0 | 0 | 0 | - | 5.0 | - | - | - | - |
| opening-hand | `dotnet format` | 0 | 158 | 0 | 0 | 0 | - | 15.0 | - | - | - | - |
| opening-hand | `godot_console` | 0 | 16 | 0 | 0 | 0 | - | 73.1 | - | - | - | - |
| ops-tower | `pwsh tools/browser-test.ps1` | 0 | 1 | 0 | 0 | 0 | - | 154.1 | - | - | - | - |
| race-anywhere | `uv run pytest` | 0 | 1 | 0 | 0 | 0 | - | 449.0 | - | - | - | - |
| vatsim_control_recs | `uv run pytest` | 0 | 5 | 0 | 0 | 0 | - | 139.0 | - | - | - | - |
| yaat | `pwsh tools/release-local/pack-macos.ps1` | 0 | 7 | 0 | 0 | 0 | - | 275.5 | - | - | - | - |
| yamv | `dotnet build` | 0 | 209 | 0 | 0 | 0 | - | 13.1 | - | - | - | - |

## `--local` runs

| Repo | Family | Reason | Runs |
|---|---|---|---|
| delve-the-dungeon | `dotnet test` | the strikes test writes the generated file into the checkout it runs from | 26 |
| rustling-tulip | `cargo nextest` | fault-injected env var must reach the local test process | 11 |
| (no repo) | `bash -c` | YAAT-1564 fp32 NeMo export needs the local venv and model cache | 7 |
| claude-config | `pwsh tools/build-box/image/Invoke-ImageDrill.ps1` | QEMU drill needs this PC's hypervisor | 7 |
| gate | `cargo build` | the installer builds the gate on this PC | 7 |
| yaat | `dotnet csharpier` | formatter must rewrite the worktree files in place | 7 |
| sherpa-onnx | `pwsh .tmp/smoke.ps1` | native MSVC/CUDA build whose binaries must be on this machine | 5 |
| yaat | `dotnet build` | replay test reads the bug bundle from gitignored .tmp, which a routed snapshot omits | 5 |
| delve-the-dungeon | `dotnet build` | GATE-958 local hook-build measurement | 4 |
| rustling-tulip | `cargo nextest` | launches a real process through this PC's WMI | 4 |
| yamv | `pwsh -Command` | routed run: csharpier not available on builder after dotnet tool restore | 4 |
| gate | `cargo nextest` | GATE-844: the desktop inner-gate case needs an interactive session | 3 |
| rustling-tulip | `cargo nextest` | suspend-rig repro needs the local process | 3 |
| yaat | `dotnet` | probe reads local binaries and scratchpad files | 3 |
| yaat | `dotnet build` | sandbox binary for the private-corpus smoke | 3 |
| yaat | `dotnet build` | throwaway probe compiled from gitignored .tmp via CustomAfterMicrosoftCommonTargets; a routed snapshot omits .tmp | 3 |
| yaat | `dotnet build` | timing run needs a local Release binary | 3 |
| yaat | `pwsh .../cuda-closure.ps1` | CUDA closure probe needs the local GPU and sandbox | 3 |
| yaat | `yaat.sim.tests` | replay test reads the bug bundle from gitignored .tmp, which a routed snapshot omits | 3 |
| yaat | `yaat.speechsandbox` | reads the private corpus on G: | 3 |
| claude-config | `pwsh tools/build-box/image/Invoke-ImageDrill.ps1` | QEMU drill needs this PC's WSL and KVM | 2 |
| delve-the-dungeon | `dotnet build` | scratch harness compiled from gitignored .tmp via CustomAfterMicrosoftCommonTargets; the routed build cannot see it | 2 |
| godot-mcp | `pwsh run.ps1` | GATE-844 trial: rerun locally only what failed routed | 2 |
| in-the-sky-ops | `dotnet build` | AIR-27 scale run | 2 |
| rustling-tulip | `pwsh .../suspend-rig.ps1` | suspend-rig repro needs the local process | 2 |
| sherpa-onnx | `cmake` | native MSVC/CUDA build whose binaries must be on this machine | 2 |
| yaat | `dotnet build` | probe cleanup: rebuild the local bin/obj the --local probe build left | 2 |
| yaat | `dotnet build` | timing runs need a local Release binary | 2 |
| yaat | `dotnet run` | red test source lives in gitignored .tmp, which a routed snapshot omits | 2 |
| yaat | `dotnet run` | the local tree holds the only build of the untracked probe; routed runs drop its .tmp outputs | 2 |
| yaat | `dotnet test` | routed run cannot return the probe's untracked .tmp outputs (tick JSONs, census TSV) | 2 |
| yaat | `dotnet test` | throwaway probe compiled from gitignored .tmp via CustomAfterMicrosoftCommonTargets; a routed snapshot omits .tmp | 2 |
| delve-the-dungeon | `dotnet test` | runs the scratch harness that only the local build carries | 1 |
| delve-the-dungeon | `pwsh .../loop.ps1` | repeats the scratch red test from the local build | 1 |
| delve-the-dungeon | `pwsh .../suspend-rig.ps1` | suspend rig over the local scratch build | 1 |
| gate | `cargo nextest` | GATE-844: does the desktop inner-gate case pass in an interactive session (it fails 0xC0000142 in the builder's session 0) | 1 |
| gate | `cargo nextest` | the rig needs this PC's gate.exe | 1 |
| gate | `cargo nextest` | the rig needs this PC's gate.exe and suite binary | 1 |
| gate | `cargo nextest` | the rig needs this PC's suite binary | 1 |
| gate | `powershell -Command` | GATE-959: parse check of the builder installer | 1 |
| gate | `prek run` | GATE-959: repo hygiene hooks on the builder installer | 1 |
| gate | `pwsh -Command` | GATE-959: parse check of the builder installer | 1 |
| godot-mcp | `dotnet build` | routed build leaves the probe dll under the ignored .godot on the build box | 1 |
| in-the-sky-ops | `dotnet build` | on-demand scale timing run | 1 |
| rustling-tulip | `cargo build` | the probe launches the local binary | 1 |
| rustling-tulip | `cargo fmt` | rustfmt writes the worktree's files in place | 1 |
| rustling-tulip | `cargo nextest` | launches the local daemon through WMI | 1 |
| rustling-tulip | `cargo nextest` | the WMI probe must measure this PC's desktop logon; routed to the builder it wrote nothing | 1 |
| rustling-tulip | `cargo run` | the shot needs this PC's desktop window | 1 |
| rustling-tulip | `pwsh .../e2e-sealed-local.ps1` | GATE-736 trial: rerun locally the one e2e spec that failed routed | 1 |
| rustling-tulip | `pwsh .../matrix.ps1` | suspend-rig repro needs the local process | 1 |
| sherpa-onnx | `cmake build-cpu` | native MSVC/CUDA build whose binaries must be on this machine | 1 |
| sherpa-onnx | `cmake build-cuda` | native MSVC/CUDA build whose binaries must be on this machine | 1 |
| sherpa-onnx | `ctest build-cpu` | native MSVC/CUDA build whose binaries must be on this machine | 1 |
| towercab-3d | `pnpm vitest` | scratch writes .tmp file read locally | 1 |
| towercab-3d | `pnpm vitest` | scratch writes a .tmp file read locally | 1 |
| yaat | `dotnet build` | YAAT-1564 whisper CPU debug: build scratch probe referencing Yaat.Client (needs local LM-Kit natives and the G: corpus) | 1 |
| yaat | `dotnet build` | YAAT-1564 whisper CPU debug: rebuild scratch probe (needs local LM-Kit natives and the G: corpus) | 1 |
| yaat | `dotnet build` | YAAT-1599 parity: sandbox Release build for the native overlay | 1 |
| yaat | `dotnet build` | local binary to re-prove the stock-native refusal | 1 |
| yaat | `dotnet build` | probe needs local binaries | 1 |
| yaat | `dotnet build` | rebuild the local test bin so the removed probe class leaves bin/obj | 1 |
| yaat | `dotnet build` | sandbox Release build for timing runs on this PC's native overlay | 1 |
| yaat | `dotnet build` | sandbox binary for YAAT-1571 two-pass timing | 1 |
| yaat | `dotnet build` | sandbox binary for decoder recall measure r1 | 1 |
| yaat | `dotnet build` | sandbox binary for decoder recall measure r2 | 1 |
| yaat | `dotnet build` | sandbox binary for decoder recall measure r3 | 1 |
| yaat | `dotnet build` | sandbox binary for the YAAT-1570 weight sweep | 1 |
| yaat | `dotnet build` | sandbox binary for the native overlay timing runs | 1 |
| yaat | `dotnet build` | verify the overlaid native in the local bin; a routed build leaves no local bin | 1 |
| yaat | `dotnet run` | probe source lives in gitignored .tmp, which a routed snapshot omits | 1 |
| yaat | `dotnet test` | routed run cannot return the census TSVs written under .tmp | 1 |
| yaat | `pwsh .tmp/spike-sherpa-cuda-run.ps1` | native GPU probe needs this PC's RTX 4090 | 1 |
| yaat | `robocopy` | YAAT-1564 whisper CPU debug: copy sandbox build without GPU backends (file copy, needs the local build) | 1 |
| yaat | `uv run tools/publish_stt_downloads.py` | uploads local files to R2 from this PC | 1 |
| yaat | `yaat.speechsandbox` | YAAT-1571 full run argon-resolve on the private corpus | 1 |
| yaat | `yaat.speechsandbox` | YAAT-1599 parity 804a1d0c29-cpu-beam on the private corpus | 1 |
| yaat | `yaat.speechsandbox` | YAAT-1599 parity 804a1d0c29-cpu-greedy on the private corpus | 1 |
| yaat | `yaat.speechsandbox` | YAAT-1599 parity 804a1d0c29-gpu-beam on the private corpus | 1 |
| yaat | `yaat.speechsandbox` | YAAT-1599 parity edad9a7fe1-cpu-beam on the private corpus | 1 |
| yaat | `yaat.speechsandbox` | YAAT-1599 parity edad9a7fe1-cpu-greedy on the private corpus | 1 |
| yaat | `yaat.speechsandbox` | YAAT-1599 parity edad9a7fe1-gpu-beam on the private corpus | 1 |
| yaat | `yaat.speechsandbox` | decoder recall measure r0 (cloud, cached) | 1 |
| yaat | `yaat.speechsandbox` | decoder recall measure r1 (cloud, cached) | 1 |
| yaat | `yaat.speechsandbox` | decoder recall measure r2 (cloud, cached) | 1 |
| yaat | `yaat.speechsandbox` | decoder recall measure r3 (cloud, cached) | 1 |
| yaat | `yaat.speechsandbox` | stock native must refuse beam | 1 |
| yaat | `yaat.speechsandbox` | stock native must refuse beam after the setup-abort refactor | 1 |
| yamv | `dotnet test` | 8.3 short names are off on the builder volume, so the short-name test only skips there | 1 |

- Window: 2026-10-25T05:41:03+00:00 to 2026-11-01T05:41:03+00:00 (UTC).
- Files read: 27 archives, events.1.jsonl, events.jsonl; unreadable: none.
- Skipped lines: 0; duplicate lines left out: 3312.
- Runs with neither a queue nor a release line in the window: 17; heavy runs lent or covered, with no release of their own, left out: 0.
- The archive keeps only release lines: a release carries notRouted, reason, fellBack and parentPid, and one journalled before the gate wrote them keeps none; the live queue lines cover only the runs queued from 2026-10-31T09:00:24+00:00.
- nested gates linked by parentPid: 37; by containment: 162.
- commits with covered hook gates, share not split: 0.

## Sort

Two read-only passes sorted the families: one for the .NET and Godot repos (yaat, yaat-server, godot-mcp, delve-the-dungeon, yamv, in-the-sky, in-the-sky-ops, opening-hand), one for the rest. Each read the actual commands of the families making the first 90% of its repos' local seconds, and gave the long tail a lighter check.

### .NET and Godot repos

| Repo | Family | Local runs | Local s | Why local (counts) | State | What moves it | Issue or change | s/week recoverable |
|---|---|---|---|---|---|---|---|---|
| godot-mcp | `pwsh .../hidden-desktop.ps1` | 196 | 32,464 | pre-cut no reason 161, fallback desktop 23, busy 4, post-cut no reason 8. 134 of the 169 unexplained overlapped a routed desktop run (another lane held the builder's one desktop); 35 ran alone (logs gone) | ready to route (routes already) | Nothing more for the family. These were lane gates of an ungated `run.ps1 itest`, 2-3 at once. Since godot-mcp ee1edb1d (10-30T07:59Z) the lanes run inside the routed `run.ps1 itest -InGate`, and no lane gate has run on the PC since. What remains is the desktop contention in the `pwsh run.ps1` row | none (godot-mcp ee1edb1d) | 32,464 already moved; 0 left |
| yaat | `git commit (ungated hooks)` | 180 | 29,548 | uncovered share of 180 heavy commits whose nested pre-commit `dotnet build -p:TreatWarningsAsErrors=true` routed (187 nested builds, all routed) | blocked on work | The ungated pre-commit hooks: `bash tools/hooks/dotnet-format-wrapper.sh style info` (`dotnet format style --severity info --include`, then a `--verify-no-changes` pass for IDE0059/IDE0060), `bash tools/hooks/csharpier-wrapper.sh` (`dotnet csharpier format`), ruff `--fix`/format, `whitespace-fix-autostage.sh`, `node tools/build-cheatsheet.mjs --check`. All but the last fix and `git add`, so routing-a-repo.md keeps them local as hook gates | GATE-634 | 29,548. Since yaat ac7b80558 (10-29T22:04Z, build to pre-push) this time shows in the `git commit` row |
| yaat | `dotnet run` | 300 | 12,144 | pre-cut no reason 191 (6,276 s); no pattern 99 (5,488 s); --local 5; busy 2; local.toml 2; declared local 1 | split | `tests/Yaat.Sim.Tests` (113 runs) and the other `tests/*` projects: routes already, with 0 unexplained local runs post-cut. No pattern: `tools/Yaat.SpeechSandbox` 43 runs / 2,538 s, truly local (private corpus on G:, this PC's native overlay and GPU, .tmp audio). `tools/Yaat.PrecomputeCache` 6 / 968 s: the 2 compute runs (826 s) are blocked (they write tracked `src/Yaat.Sim/Data/PrecomputeCache`), and the 4 `--check` runs are local by time (GATE-910's trial: 34 s routed against 20 s local). `tools/Yaat.GuideCapture` with no `--out` 2 / 1,001 s: blocked (writes tracked `docs/user-guide/img`, `docs/releases/img`; untrialled). `-c Release --no-build --project tools/Yaat.LayoutInspector` 19 / 90 s: truly local by time (4.7 s against yaat's routed `dotnet run` median 25.8 s). `dotnet run --no-build --project tests/Yaat.Sim.Tests` 5 / 40 s: ready, add `dotnet run --no-build --project tests/*`. `.tmp` and scratch projects: one-offs | new issue: trial yaat's PrecomputeCache compute and GuideCapture docs runs routed with outputs `src/Yaat.Sim/Data/PrecomputeCache/**`, `docs/user-guide/img/**`, `docs/releases/img/**`; pattern `dotnet run --no-build --project tests/*` | about 1,870 (826 + 1,001 + 40) |
| yaat | `git commit` | 68 | 7,263 | heavy commits with no nested gate | blocked on work. The census rule calls this truly local, but its time is the ungated formatter hooks of the row above, and since 10-29T22:04Z every yaat commit is of this kind | same hooks as `git commit (ungated hooks)` | GATE-634 | 7,263 |
| yaat | `dotnet build` | 106 | 5,462 | pre-cut no reason 72 (3,562 s); --local 32 (1,806 s); gitignored input 2 | pre-cut: routes already (285 routed post-cut). `--local`: 21 runs / 1,305 s of SpeechSandbox sandbox and timing builds; 8 / 367 s .tmp probes; 3 cleanup rebuilds | Sandbox and timing builds: a caller check. yaat's `outputs` `**/bin/**` bring a routed Release bin back, so `--local` is needed only where the native overlay must survive in the local bin. .tmp probes: blocked, since a `CustomAfterMicrosoftCommonTargets=.tmp/...` probe's sources (`ProbeSource=.tmp/...cs`, bug bundles) are not carried as inputs | caller check in yaat (SpeechSandbox build); new issue: carry the gitignored `.tmp` files a probe's `.targets` names | 367; up to 1,672 if the caller check holds |
| yaat | `pwsh tools/test-all.ps1` | 8 | 4,454 | pre-cut no reason 7 (3,819 s, all failed); no pattern 1 (`pwsh -NoProfile tools/test-all.ps1` without `-File`, 635 s) | ready to route | add `pwsh -NoProfile tools/test-all.ps1` and `pwsh -NoProfile tools/test-all.ps1 *`. The rest routes already (113 routed post-cut, 0 local) | remote.json, yaat only | 635 |
| godot-mcp | `pwsh .../run.ps1` (`itest-csharp`) | 26 | 3,247 | pre-cut no reason 21 (20 overlapped a routed desktop run), fallback desktop 5 | ready to route (routes already) | inside the routed itest since ee1edb1d | none | 3,247 already moved |
| yaat | `dotnet test` | 58 | 2,877 | pre-cut no reason 53; --local 5 (.tmp probe outputs and sources) | routes already (666 routed post-cut); the --local runs are blocked | the .tmp probe issue above | new issue (.tmp probe) | 172 |
| opening-hand | `git commit` | 41 | 2,645 | heavy commits, no nested gate (the build is pre-push) | blocked on work | ungated hooks: `pwsh tools/hooks/dotnet-format-wrapper.ps1 style` and `csharpier-wrapper.ps1` (both fix and `git add`), `Test-DocDrift.ps1`, `agent-mail-guard.ps1`, builtins | GATE-634 | 2,645 |
| godot-mcp | `dotnet build` | 146 | 2,315 | pre-cut no reason 144 (130 `itest-build` of IntegrationTests), busy 2 | routes already | the lanes' build now runs inside the routed itest | none | 2,315 already moved |
| godot-mcp | `dotnet test` | 64 | 2,175 | pre-cut no reason 62, faster here 1, no pattern 1; all 64 are `--filter-class` unit runs | truly local by time | None of godot-mcp's filtered `dotnet test` runs routed in the window (0 of 64). Post-cut the gate says `faster here`. Local median 25.5 s; the routed unfiltered `dotnet test` median is 132.8 s, and there is no routed filtered run to compare. 32 failed, in red-test loops (`*ValueWriterTests` 4 of 6, `*Stall*` 3 of 4) | gate's own judgement, no booking | 0 |
| in-the-sky | `git commit` | 39 | 2,109 | heavy commits, no nested gate | blocked on work | ungated hooks: dotnet-format-wrapper.ps1 style and csharpier-wrapper.ps1 (fixers), Invoke-ScriptAnalyzerHook.ps1, Test-LineLength.ps1, gitleaks, `uv run ... provenance check`, Test-DocDrift.ps1, Invoke-AgentMailGuard.ps1 | GATE-634 | 2,109 |
| godot-mcp | `git commit` | 58 | 2,052 | heavy commits, none holding a gate (the 29 commits that held run.ps1 format-check's csharpier gate were light, which the census does not split) | blocked on work | Ungated hooks: `uv run tools/linelength.py`, `gdformat --check`, `gdlint`, `uv run --with gdtoolkit tools/gdcomplexity.py`, `uvx ruff` format/check, agent-mail-guard.ps1. `run.ps1 format-check` is gated and stays local silently, since godot-mcp is not in `hooks`. These are check-only, so godot-mcp could gate them itself | GATE-634, or a godot-mcp change (gate the check-only hooks, add godot-mcp to `hooks`) | 2,052 |
| in-the-sky | `dotnet test` | 115 | 1,437 | pre-cut no reason 111, faster here 4; 113 of the 115 are `--filter-class` | truly local by time | 0 of 113 filtered runs routed. Local median 8.9 s, under the routing floor of about 13 s. 63 failed, in loops (`*LavEscortTests` 18 of 26 failed), where a fast local fail loop beats routing. See surprises: in-the-sky's routed `dotnet test` median is 7.8 s | gate's judgement | 0 |
| delve-the-dungeon | `dotnet format` | 17 | 1,290 | pre-cut no reason 16, busy 1 | routes already (488 routed post-cut, 0 local) | none | none | 1,290 already moved |
| delve-the-dungeon | `dotnet test` | 33 | 1,279 | --local 18 (662 s: EffectTableTests strikes rewrite); pre-cut no reason 15 (617 s, 2 of them a .tmp probe project) | blocked on work | The strikes rewrite writes tracked `src/Delve.Client/Data/effect-strikes.json` when `DELVE_WRITE_STRIKES=1`. It needs an outputs entry `src/Delve.Client/Data/effect-strikes.json` and one trial. Delve's own failure message (`tests/Delve.Client.Tests/EffectTableTests.Strikes.cs:20-23`) tells callers to use `-Local`; it should pass `-Env DELVE_WRITE_STRIKES=1` instead | new issue: route delve's strikes rewrite with its generated file as an output; caller change in delve | 662 |
| godot-mcp | `prek run` | 20 | 1,198 | git/prek local by rule (17 `prek run --all-files`) | truly local | stays local silently | none | 0 |
| yaat | `pwsh tools/server-image/build.ps1` | 6 | 1,175 | matched today, but all 6 ran 10-30T18:03-18:34Z, before the pattern was added | routes already (the 2 runs after it routed) | none | none | 1,175 already moved |
| yaat | `git cherry-pick` (+ `git` 3 / 697 s: `git -c core.editor=true cherry-pick/rebase --continue`) | 9 | 1,091 | git local by rule; the `--continue` runs (7, 1,760 s) run the commit hooks | truly local by rule; its time is the ungated formatter hooks | as for yaat `git commit` | GATE-634 | 1,788 |
| godot-mcp | `pwsh run.ps1` (`itest -InGate` / `-Filter`) | 10 | 963 | fallback desktop 8 (734 s), --local 2 (GATE-844 trial reruns) | blocked on work | The builder has one desktop. An itest that finds another itest holding it falls back after the 60 s wait and runs here. This hit 8 of 43 itests since ee1edb1d | new issue: queue a desktop-route run behind the builder's busy desktop instead of falling back after 60 s | about 2,700 at the post-ee1edb1d rate (734 s in 1.9 days) |
| delve-the-dungeon | `dotnet build` | 14 | 962 | --local 5 (3 GATE-958 measurement runs, 2 .tmp harness); pre-cut no reason 8; version 1 | routes already. The measurement runs are truly local (they time this PC) | .tmp harness: the probe issue | none | 30 |
| delve-the-dungeon | `git` (`git -C <worktree> commit`) | 6 | 718 | commit runs the census names `git`; heavy, no nested gate | blocked on work | Ungated hooks: csharpier-wrapper.ps1 (fixer), ruff check/format --check, Test-DocDrift.ps1, Invoke-ScriptAnalyzerHook.ps1, line-length, agent-mail-guard.ps1 | GATE-634 | 718 |
| yaat | `bash .tmp/nemo-export.sh` | 1 | 687 | no pattern, one-off .tmp script (needs the local venv and model cache) | truly local, one-off | none | none | 0 |
| yaat | `yaat.speechsandbox` | 9 | 618 | --local 9 (private corpus on G:, cloud-cached recall runs, stock-native checks) | truly local | No pattern matches it, so no `.gate/local.toml` line is needed; routing-a-repo.md's table, with SpeechSandbox's `dotnet run` | table row | 0 |
| in-the-sky-ops | `dotnet format` / `dotnet build` / `dotnet test` / `dotnet csharpier` | 58 / 53 / 17 / 32 | 598 / 567 / 55 / 25 | 157 runs of 10-31T05-14Z, before in-the-sky-ops was in `remote.json`; --local 3 (AIR-27 scale and timing runs) | routes already. The timing runs are truly local | none | none | 1,245 already moved |
| yamv | `git commit` | 29 | 524 | heavy, no nested gate (the build is pre-push) | blocked on work | ungated hooks: dotnet-format-wrapper.ps1 style, csharpier-wrapper.ps1 (fixers) | GATE-634 | 524 |
| yamv | `git push` | 41 | 979 | git local by rule. The pre-push `tools/gate.ps1 run ... -- dotnet build Yamv.slnx -c Release` inside it routes (209 routed yamv builds), so the PC mostly waits | truly local | none | none | 0 |
| delve-the-dungeon | `git commit (ungated hooks)` / `git commit` | 16 / 4 | 392 / 37 | uncovered share of heavy commits with a routed nested build; plain commits | blocked on work | delve's ungated hooks, as in its `git` row | GATE-634 | 429 |
| godot-mcp | `pwsh .../suspend-rig.ps1` | 7 | 379 | no pattern | truly local (a rig of this PC's own processes) | none | routing-a-repo.md table row | 0 |
| godot-mcp | `dotnet publish` | 39 | 328 | no pattern; median 5.7 s | truly local by time (godot-mcp's routed `dotnet build` median 25.2 s) | none | none | 0 |
| delve-the-dungeon | `bash docs/plans/canvases/fill-reveal/shoot.sh`, `uv run tools/sound/sync_compare.py` | 2 / 3 | 313 / 102 | no pattern; mock and recipe renders over `.tmp` captures | one-offs, sorted as their commands (a render, .tmp inputs) | none unless rerun | none | 0 |
| godot-mcp | `uv run pytest` | 8 | 288 | pre-cut no reason 8 | routes already (18 routed post-cut) | none | none | 288 already moved |
| yamv | `pwsh -Command` | 15 | 288 | no pattern 11, --local 4 | ready to route as separate gates | 6 runs / 210 s are `pwsh -Command "dotnet build ...; dotnet test ..."`: gate each step on its own, and both match. 8 runs / about 70 s are `dotnet csharpier format` (a fixer, so local) | caller change in yamv | 210 |
| yaat | `pwsh -Command` | 5 | 236 | no pattern, all failed | ready to route as separate gates | 4 runs are `dotnet build ...; dotnet test ...` (156 s). 1 is `$env:YAAT_ROUTING_CENSUS_REGENERATE='1'; dotnet test ...`, which should be `--env YAAT_ROUTING_CENSUS_REGENERATE=1 -- dotnet test ...` once its regenerated file has an outputs entry | caller change in yaat | 156 |
| delve-the-dungeon | `dotnet run` | 18 | 241 | no pattern (`tools/Delve.Bundle -- info .tmp/...zip` 15, .tmp probe 3) | truly local by time / .tmp input (13 s median) | none | none | 0 |
| yamv | `dotnet test` | 9 | 231 | faster here 8; --local 1 (8.3 short names are off on the builder volume, so that test only skips there) | truly local by time; the short-name test is a small builder-setup blocker | none | none | 0 |
| yaat | `dotnet format` | 6 | 188 | pre-cut no reason | routes already | none | none | 188 already moved |
| in-the-sky-ops | `godot_console` | 13 | 170 | no pattern (`Godot_console --headless --path src/Ops.Client --build-solutions --quit`), median 7.6 s | blocked on work (small) | a pattern like opening-hand's, plus `inputs` for `src/Ops.Client/.godot/**`; in-the-sky-ops' routed `dotnet build` median is 7.1 s, so it is not provably faster here | remote.json after a trial | 170 |
| delve-the-dungeon | `godot_console` | 11 | 139 | 10 `--import` runs started from `src/Delve.Client`, where the rewrite makes `--path` `.` and the pattern misses; 1 pre-cut | ready to route | add `*Godot_console* --headless --path . --import*` | GATE-936 | at most 139 (imports take 5.5 s here, so they may be faster here) |
| yaat | `pack-win-linux.ps1` (busy 1), yaat-server `dotnet test` (pre-cut 2), opening-hand `dotnet test` (pre-cut 3), yaat `dotnet csharpier` (pre-cut 1), yaat-server `dotnet build` (pre-cut 1) | 8 | 317 | fallback or pre-cut | routes already | none | none | 317 already moved |
| godot-mcp | `dotnet csharpier` | 43 | 95 | 31 hook gates of run.ps1 format-check (godot-mcp not in `hooks`), pre-cut 11, busy 1 | truly local by time (1.8 s here against 18.6 s routed) | none | none | 0 |
| delve-the-dungeon | `dotnet csharpier` | 30 | 63 | another worktree 22, pre-cut 8 | ready to route | the caller passes `--cd <tree>` | caller change in delve | 28 |
| yaat-server | `git commit (ungated hooks)` / `git commit` | 3 / 2 | 58 / 12 | uncovered share; plain commits | blocked on work | ungated hooks: bash dotnet-format-wrapper.sh style info, csharpier-wrapper.sh (fixers), ruff --fix, `.githooks/pre-commit` (submodule auto-stage) | GATE-634 | 70 |
| in-the-sky-ops | `git commit` | 5 | 64 | heavy, no nested gate | blocked on work | dotnet-format-wrapper.ps1, csharpier-wrapper.ps1 (fixers), script analyzer, line length, gitleaks | GATE-634 | 64 |
| (tail) | yaat `dotnet publish` (2 / 107 s, to `.tmp/...`), delve `docker build` (4 / 102 s), yaat `cuda-closure.ps1` (--local GPU, 3 / 95 s), in-the-sky `blender` (4 / 56 s, .tmp render), delve `dotnet msbuild` (35 / 40 s, 0.6 s each), yaat `.tmp/rig` scripts, exe probes, `mcp_probe.py`, `cargo nextest`, `dotnet`, `dec` (about 230 s together), godot-mcp `git push`, `dotnet run`, `install.ps1`, in-the-sky `bash` and `ffmpeg`, in-the-sky-ops `prek run` and `dotnet run`, yaat-server `dotnet restore` and `git cherry-pick`, yaat `git rebase` | each under 40 | each 110 s or less (not summed) | no pattern, git/prek, or --local | truly local or one-off (.tmp scratch, this PC's GPU, a short command); docker is not booked unless it recurs | none | none | 0 |

#### Surprises

- **The cut hides the reasons.** Releases before 2026-10-29T16:52Z carry no `notRouted`; `fellBack` starts at 07:24Z. 54,066 of these repos' local seconds are matched runs with no reason. After the cut, nearly all of those commands route or carry one. So the week's local totals overstate today's gap. A census over a window wholly after the cut is the honest baseline.
- **The census never splits light commits.** It splits only heavy commits. These repos made 730 light commits: about 27,000 s of wall time, about 11,200 s of it outside any nested gate. delve-the-dungeon alone made 414 light commits, holding 151 nested routed builds, with 6,097 s uncovered. None of that appears in a `git commit (ungated hooks)` row.
- **`git -C <tree> commit` is named family `git`.** These are commit runs, but the census names them `git`, not `git commit` (delve, 6 runs, 718 s); the same goes for `git -c core.editor=true cherry-pick/rebase --continue` in yaat. `census.family_of` skips neither `-C <path>` nor `-c k=v`.
- **yaat's hook time changed rows.** Since ac7b80558 moved yaat's build hook to pre-push, its commits have no nested gate. The same formatter-hook time now lands in `git commit`, which the census rule sorts truly local.
- **`faster here` appears on heavy-slot runs.** yamv, in-the-sky and godot-mcp `dotnet test --filter-class` runs carry it on heavy-slot `queue` lines, for example yamv on 10-31T15:04Z on a heavy slot. But routing-a-repo.md and `crates/gate-router/src/decide.rs:1517` compute the learned local p50 only for `Slot::Light`. Filtered `dotnet test` runs never routed in the window: godot-mcp 0 of 64, in-the-sky 0 of 113. in-the-sky's routed unfiltered `dotnet test` median (7.8 s) is below its local filtered median (8.9 s).
- **No current pattern matches something it should not.** GuideCapture's narrowed patterns, the LayoutInspector `--ticks` local.toml line and godot-mcp's desktop consent all behave as written.
- **Gaps routing should already catch:** delve's import from the project folder (GATE-936), and yaat's `pwsh -NoProfile tools/test-all.ps1` without `-File`.

### Rust, tooling and other repos

How I read the evidence:

- **No reasons before 2026-10-29T09:18Z.** Archived `release` lines carry `notRouted` only from then on. For runs released earlier, I judged the cause from each command against the `remote.json` backups of that day: the `remote.json.bak-*` stamps show when each pattern landed.
- **Local with no reason means no pattern matched.** A run that has a `queue` line, no `notRouted` and no `remote` matched no pattern. That case is silent (GATE-403).
- Dates when patterns landed:
  - rustling-tulip `pwsh -NoProfile -File ./rt.ps1 test *`: between 2026-10-27T23:55Z and 2026-10-28T20:40Z.
  - The absolute-path form, rewritten as `pwsh -NoProfile -File rt.ps1 test[ *]`: 2026-10-31T00:00Z.
  - `rt.ps1 native-e2e`: 2026-11-01T05:16Z (GATE-736). Its desktop consent came at 05:38Z.
  - towercab-3d `cargo test[ *]`: 2026-10-27, about 21:30Z.

| Repo | Family | Local runs | Local s | Why local (counts) | State | What moves it | Issue or change | s/week recoverable |
|---|---|---|---|---|---|---|---|---|
| rustling-tulip | `pwsh .../rt.ps1` (native-e2e, own tree) | 46 | 13,987 | No pattern for `native-e2e` until 2026-11-01T05:16Z. The 1,147 s run queued at 04:57Z, before the pattern. | blocked on work | The pattern landed. The specs need the builder's desktop. | GATE-736 (routed), GATE-974 (desktop route) | 13,987 |
| rustling-tulip | `pwsh .../rt.ps1` (cross-worktree) | 17 | 6,588 | The caller's gate ran from the main checkout and named `rt.ps1` in another worktree. The word is not rewritten, so no pattern matches and the run stays local silently. Breakdown: 8 `native-e2e` (2,473 s), 8 `test -Validate` (4,106 s, 3 of them after the abs-path patterns landed), 1 sealed (9 s). | ready to route (caller change) | Run `rt.ps1` bare from any shell (it gates itself with `--cd <its tree>`), or gate it with `--cd <worktree>`. Never wrap it in a gate started in the main checkout. | rustling-tulip: say so in CLAUDE.md or the nextup profile. Gate side: new issue (see 3). | 4,106 now, 2,473 after GATE-974 |
| rustling-tulip | `pwsh .../rt.ps1` (`test -Validate`, own tree) | 16 | 6,086 | Absolute `-File <tree>\rt.ps1` is rewritten to `rt.ps1`. Only `./rt.ps1` patterns existed before 2026-10-31T00:00Z. | ready to route | Routes already. The 9 routed runs all come after the change. | none | 6,086 |
| rustling-tulip | `pwsh .../rt.ps1` (`native-e2e -SealedSecretOnly`) | 6 | 768 | No pattern matches. `.gate/local.toml` keeps it in any case. | truly local | The sealed-secret spec runs on this PC's own secret store. | `.gate/local.toml` already lists it | 0 |
| rustling-tulip | `cargo build` (`--release -p daemon -p tracer -p rustling-tulip-native`, from `rt.ps1 build -Release`, `installer`, hotfix builds) | 16 | 4,643 | No `cargo build` pattern for rustling-tulip. | ready to route (needs outputs) | Add `cargo build --manifest-path Cargo.toml --release -p daemon -p tracer -p rustling-tulip-native` and `cargo build --manifest-path Cargo.toml -p daemon -p tracer -p rustling-tulip-native --release`, plus an `outputs` entry for the release exes (`target/release/*.exe`). Today `outputs.rustling-tulip` is `[]`. The PC launches or packages these exes. | remote.json | 4,643 |
| rustling-tulip | `cargo build` (`--all-targets` compile builds) | 17 | 2,374 | No `cargo build` pattern. 8 of the 17 failed. | ready to route | Add `cargo build --workspace --all-targets*` and `cargo build -p * --all-targets*`. | remote.json | 2,374 |
| rustling-tulip | `cargo build` (other: e2e-loopback daemon, `--bin rustling-tulipd`, `-p daemon -p tracer`, a test-release native build) | 6 | 1,160 | No pattern. 1 run was `--local`: "the probe launches the local binary". | blocked on work | These feed a local launch, a probe or an e2e run. They move with what they feed: the e2e ones inside the routed `native-e2e` re-entry. | GATE-974 | about 270 (e2e builds) |
| rustling-tulip | `pwsh -Command` (e2e spec loops: `cargo build ... --features daemon/e2e-loopback-peers; cargo nextest run -p rustling-tulip-native --run-ignored only --test e2e_*`) | 21 | 5,824 | No pattern can match a `pwsh -Command` wrapper. 10 of the 21 failed, an agent fix loop of about 280 s per run, so routing pays. | blocked on work (caller change) | rt.ps1 change: give `native-e2e` a spec and filter argument (for example `-Test e2e_ssh -Filter <expr>`). Then route `pwsh -NoProfile -File rt.ps1 native-e2e *` with the same desktop consent. `.gate/local.toml` still keeps `-SealedSecretOnly`. | rustling-tulip change + remote.json, after GATE-974 | 5,824 |
| rustling-tulip | `pwsh -Command` (clippy and nextest chains) | 4 | 1,457 | No pattern matches the wrapper. | ready to route (caller change) | Run each step as its own gate: `cargo clippy *` and `cargo nextest run *` already route. | rustling-tulip agents' practice | 1,457 |
| rustling-tulip | `pwsh -Command` (other) | 2 | 233 | One run sets `$env:GATE_BOX_JOB = '1'` by hand around a sealed-secret spec. One is a broken one-liner (1 s). | truly local | One-off. The sealed spec is local. | none | 0 |
| rustling-tulip | `cargo run` (`--example shot`, from `rt.ps1 native-shot`) | 123 | 7,274 | No pattern. The window is cloaked but needs a desktop session, and the PNG must come back. 1 run was `--local`: "the shot needs this PC's desktop window". Timing: 84 runs under 30 s took 1,066 s (median 12.3 s), below the routing floor; 39 rebuild runs of 30 s or more took 6,208 s. | blocked on work | Desktop route for the shot, `outputs` for `.tmp/shots/*.png`, and a pattern for `cargo run --manifest-path Cargo.toml -p rustling-tulip-native --example shot *`. A GPUI window on the builder's desktop needs a trial. | New issue: "Route rustling-tulip's native-shot on the builder's desktop, with .tmp/shots outputs". The 2026-10-27 census commented it onto GATE-736, which GATE-974 now closes out for e2e. | about 5,700 (the rebuild runs, less 13 s each) |
| rustling-tulip | `cargo nextest` | 22 | 2,480 | 11 `--local` "fault-injected env var must reach the local test process" (586 s). 3 suspend-rig repros (558 s). 5 WMI launch tests (296 s). 1 WMI probe "must measure this PC's desktop logon" (200 s). 1 `another worktree` (751 s). 1 `cargo nextest list` with no pattern (87 s). | mixed | `--env NAME` sends the variable, so the fault-injection runs can route: ready, caller change. Run the other-worktree run with `--cd`. The 5 WMI launch tests need a `.gate/desktop.toml` pattern plus consent, as herdr-gpui's `credential_manager` has. Rig and logon probe: truly local. | rustling-tulip practice (`--env`, `--cd`); desktop.toml change for the WMI tests | 586 + 751 now; 296 with desktop |
| rustling-tulip | `pwsh ./rt.ps1` | 4 | 1,889 | 2 `test -Validate` before `test *` landed (1,293 s). 1 `native-e2e` before its pattern (162 s). 1 `installer` with no pattern (435 s). | test: ready (routes already); e2e: GATE-974; installer: blocked on work | The installer needs NSIS 3, which no builder installs (`tools/builder/install-toolchains.ps1` has no NSIS step), and `outputs` for `target/installer/*` plus the release exes. | New issue: "Install NSIS 3 on the builders for rustling-tulip's rt.ps1 installer" | 1,455 + 435 |
| rustling-tulip | `pwsh .tmp/rt551/stress.ps1` | 1 | 1,249 | No pattern. A `.tmp` one-off stress loop over nextest. | ready to route (one-off) | Sorted like the nextest it wraps. No booking. | none | 0 |
| rustling-tulip | `git push` | 30 | 1,182 | `git` stays local. Inside the push, the pre-push hook runs `cargo clippy --workspace --all-targets --all-features -- -D warnings` ungated: its children show clippy-driver and `cargo check --workspace --all-targets`. `cargo fmt --check` and `check-agent-names.ps1` also run there, since `.pre-commit-config.yaml` sets no `default_stages`. | truly local (push); hook: caller change | Make the clippy hook's entry `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/prek-clippy.log --timeout 1800 --slot heavy -- cargo clippy ...`. A pre-push gate routes by `cargo clippy *` with no `hooks` consent. | rustling-tulip `.pre-commit-config.yaml` | up to 1,182 (the clippy share) |
| rustling-tulip | `cargo check` | 3 | 441 | 2026-10-26/27, before `cargo check[ *]` landed. | ready to route | Routes already: 34 routed. | none | 441 |
| rustling-tulip | `pwsh .../e2e-sealed-local.ps1` | 1 | 335 | `--local`, GATE-736 trial: rerun of the sealed spec. | truly local | The sealed secret, as above. | none | 0 |
| rustling-tulip | `cargo clippy` | 1 | 292 | `another worktree`. | ready to route (caller change) | `--cd <worktree>`. | practice | 292 |
| claude-config | `pwsh tools/build-box/image/Invoke-ImageDrill.ps1` | 9 | 18,906 | 9 `--local`, "QEMU drill needs this PC's hypervisor / WSL and KVM". 8 failed, about 2,100 s each: a drill iteration loop. | truly local | It stages the private image archive from this PC's Drive, writes that archive's manifest, and uses this PC's ssh identity and the box's private keys. None of that may be sent. | claude-config `.gate/local.toml`: `pwsh -NoProfile -File tools/build-box/image/Invoke-ImageDrill.ps1*`, so callers drop `--local` | 0 |
| claude-config | `uv run pytest` (`--directory tools/linear`) | 29 | 3,088 | No pattern: `repos.claude` lists only the GL probe. Median 53.2 s. | ready to route | Add `uv run --directory tools/linear pytest*` to `repos.claude`. `.claude-wt` worktrees are named `claude` by `repo_name`, which trims the dot. Trial it, since some tests may read this PC's state. | remote.json | 3,088 |
| claude-config | `uv run pytest` (other tools and skills) | 57 | 1,073 | No pattern. Median 9.3 s. | truly local by time | Below the about 13 s routing floor. claude-config has no routed pytest yet; the only routed figure is the GL probe's 12.1 s median. | none | 0 |
| claude-config | `pwsh tools/collie-patched-update/test-collie-patched-update.ps1` | 5 | 1,532 | No pattern. Median 319 s. The cases build throwaway git repos under `.tmp/`. | ready to route | Add `pwsh -NoProfile -File tools/collie-patched-update/test-collie-patched-update.ps1*` to `repos.claude`. | remote.json | 1,532 |
| claude-config | `wsl Ubuntu` | 3 | 135 | Runs in this PC's WSL distro, in the runner's home `ci-local` venv. | truly local | This PC's WSL state. | routing-a-repo table (already covered) | 0 |
| claude-config | `pwsh tools/build-box/test-install-toolchains.ps1` | 2 | 74 | No pattern. Median 37 s. | ready to route (low value) | `pwsh -NoProfile -File tools/build-box/test-install-toolchains.ps1*`. Its elevated cases skip on the builder, as they do here unelevated. | remote.json | about 50 |
| claude-config | `bash .../lint.sh`, `pwsh tools/build-box/gl-probe.ps1` | 1 + 1 | 7 + 0.5 | lint: no pattern, 7 s. gl-probe: 1 failed local run against 8 routed. | truly local by time / routes already | none | none | 0 |
| towercab-3d | `pnpm vitest` (full suite: `pnpm vitest run`, `--reporter=default`) | 72 | 4,275 | No vitest pattern. Median 40.6 s; 8 not passed. | ready to route | Add `pnpm vitest run` and `pnpm vitest run --reporter=*`. A trial is needed: vitest has never routed. | remote.json | about 3,050 (4,275 less about 17 s each) |
| towercab-3d | `pnpm vitest` (filtered files) | 286 | 6,086 | No pattern. Median 14.0 s; 145 of 286 not passed. The failures cluster in single-file fix loops (`contactHold` 16 of 18, `restingHeight` 21 of 33). 2 `--local` "scratch writes .tmp file read locally". | truly local by time | Median 14.0 s against the about 17 s routing adds to the repo's `pnpm run typecheck` (routed 32.4 s, local 15.3 s). | none | 0 |
| towercab-3d | `cargo test` (app folder) | 60 | 8,267 | All 2026-10-26/27, before `cargo test[ *]` landed (GATE-835). No local run since. | ready to route | Routes already. | none | 8,267 |
| towercab-3d | `cargo test`, `cargo clippy`, `cargo doc` (in the pair worktree's `towercab-3d-vnas` folder) | 26 + 12 + 9 | 1,321 + 590 + 183 | The census names these towercab-3d by the `.wt` segment. The router, by its checkout, names the folder's repo `towercab-3d-vnas`, which `repos` does not list. All 15 routed towercab-3d clippy runs came from the app folder; none came from this folder. | ready to route | See towercab-3d-vnas below. `cargo doc` (median about 20 s) stays local by time. | remote.json | 1,911 |
| towercab-3d | `cargo clippy` (app folder) | 9 | 475 | 2026-10-26/27. The pattern existed, so these are reasons not kept: likely short reruns beside 15 routed runs. | ready to route | Routes already. | none | 475 |
| towercab-3d | `uv run python scripts/shipping/data-generation/extract-airport-buildings.py` | 2 | 2,582 | No pattern. Data generation (osmium) that writes repo assets. | blocked on work (one-off) | Would need `outputs` for the generated assets and the OSM input. 2 runs; the previous census saw the same pair. | none unless it recurs | 0 |
| towercab-3d | `pnpm run` (`check`) | 8 | 2,309 | No pattern. Median 272 s. `check.js` runs the vNAS crate's checks from the sibling `../towercab-3d-vnas`. | blocked on work | A towercab-3d plus towercab-3d-vnas sibling pair (`siblings` in remote.json, `.gate/siblings.toml`), with towercab-3d's `inputs` (converter exe, `dist/**`) carried in a pair run. | GATE-332 (inputs in a sibling pair), then pattern `pnpm run check` | 2,309 |
| towercab-3d | `pnpm run` (`vite:build`, `typecheck`) | 16 | 232 | vite:build has no pattern (median 10.2 s). typecheck: 1 run, which routes already. | truly local by time / routes already | none | none | 0 |
| towercab-3d | `git commit` | 45 | 2,040 | No gate nested in the commit. The time is ungated hooks. Children: `cargo check --all-targets` in 9 commits (1,101 s), `tsc -p tsconfig.web.json` / `pnpm run typecheck` in 28 (310 s), `tools/agent-mail-guard.ps1` in 19 (145 s), biome. The cargo check and tsc ran at pre-commit on older branches. main's `.pre-commit-config.yaml` now runs typecheck and clippy at pre-push. | truly local (git); hooks: caller change | Bring the branches onto main's hook config. Gate the typecheck and clippy entries through `tools/gate.ps1`: `pnpm run typecheck` and `cargo clippy *` both route, and pre-push needs no `hooks` consent. | towercab-3d hooks | up to about 1,400 |
| towercab-3d | `node scripts/shipping/build/vnas.js` | 10 | 688 | No pattern. Median 15.3 s. It also patches in the private crate. | truly local by time | Against the about 25 s routing adds to towercab-3d `cargo clippy` (routed 43.6 s, local 18.8 s). | none | 0 |
| towercab-3d | `uv run python .../build_media.py`, `bash .tmp/mut/run.sh`, `bash .tmp/tc3d401/order.sh`, `pwsh .../suspend-rig.ps1` | 2 + 1 + 1 + 1 | 619 + 177 + 19 + 30 | `.tmp` one-offs: a mock-media recipe over local captures, a mutation run, an order probe, and a suspend rig. | truly local (one-offs and rig) | none | none | 0 |
| towercab-3d | `pnpm install` | 10 | 208 | Installs into this checkout's `node_modules`. | truly local | none | none | 0 |
| towercab-3d | `cargo build` | 1 | 173 | The Tauri app build the PC launches. | blocked on work (outputs) | 1 run. No booking. | none | 0 |
| towercab-3d | `git push`, `git`, `cargo fmt` | 10 + 2 + 2 | 92 + 79 + 3 | git stays local. Pre-push runs `pnpm run typecheck` and `cargo clippy` through `bash -c` ungated. | truly local | Gate the pre-push entries (as for git commit above). | towercab-3d hooks | about 60 |
| towercab-3d-vnas | `cargo clippy`, `cargo test`, `cargo doc` (main checkout) | 1 + 1 + 1 | 95 + 80 + 24 | Repo not in `repos`. | ready to route | Add `repos.towercab-3d-vnas`: `cargo test`, `cargo test *`, `cargo clippy`, `cargo clippy *`. Its Cargo.toml has no path dependency outside the crate. | remote.json | 175 (+1,911 above) |
| towercab-3d-vnas | `git push`, `git commit` | 1 + 1 | 56 + 19 | git. The pre-push runs `cargo check --all-targets` ungated (52 s). | truly local | Gate the pre-push check. | towercab-3d-vnas hooks | about 50 |
| gate | `cargo nextest` (cross-worktree) | 14 | 5,808 | 2026-10-28, from the main checkout with `--manifest-path` in `gate.wt\gate-890`. The router keeps that local as another worktree. 3 runs took about 1,800 s each. | ready to route (caller change) | Run with `--cd gate.wt\<tree>`. | gate agents' practice | 5,808 |
| gate | `cargo nextest` (`--no-run` rig builds) | 24 | 2,551 | 3 `--local` "the rig needs this PC's gate.exe / suite binary". 21 have no reason kept, all from before 10-29. gate-nextup prescribes `-Local` with `--no-run` for rigs. | truly local | The suite binary a rig on this PC runs. | gate `.gate/local.toml`: `cargo nextest run *--no-run*`, so callers drop `-Local` | 0 |
| gate | `cargo nextest` (desktop inner-gate case) | 4 | 230 | `--local`, GATE-844: "needs an interactive session". It fails 0xC0000142 in session 0. | blocked on work (config) | gate `.gate/desktop.toml` pattern for `cargo nextest run -p gate * -E test(=boxside::desktop::*)`, plus consent. | gate repo change (GATE-844 is not in the open list) | 230 |
| gate | `cargo nextest` (filtered, worktree) | 10 | 1,288 | Pattern matches. No reason kept (before 10-29): likely flake repros. | ready to route | Routes already: 1,305 routed. | none | 0 (unknown) |
| gate | `cargo build` | 54 | 2,668 | 50 `--release -p gate -p gate-desktop`: the installer (7 `--local` "the installer builds the gate on this PC"). 4 others with no pattern (291 s, median 77 s). | installer: truly local; others: ready (low value) | Installer: an install into this PC's folders. Others: `cargo build --workspace *` and `cargo build -p * --profile test-release*`, with `.gate/local.toml` keeping `cargo build --release -p gate -p gate-desktop*` once a `cargo build` pattern exists. | none / remote.json | 291 |
| gate | `cargo clippy` | 12 | 622 | 9 cross-worktree (`gate.wt\gate-890` from main, 456 s). 1 linux target (14 s). 2 with no reason kept (152 s). | ready to route (caller change) | `--cd`. Linux-target runs need the target on the builder. | practice; GATE-505 | 456 |
| gate | `cargo check` | 9 | 350 | No `cargo check` pattern for gate. 5 `--target x86_64-unknown-linux-gnu` (103 s). Median 13.8 s. | truly local by time (borderline) | Gate's routed `cargo clippy` adds about 6 s (29.9 s routed, 24.0 s local), so `cargo check *` could route if wanted. The linux target is GATE-505. | none | 0 |
| gate | `git commit`, `git add`, `pwsh -Command`, `cargo test` | 167 + 1 + 2 + 2 | 593 + 0 + 94 + 41 | git (the hooks are builtin hygiene only). A fmt-and-clippy `-Command` chain. `cargo test` of a `.tmp\890-rig` copy. | truly local / caller change (chain) / one-off | Split the chain into its own gates. | none | 94 |
| craft-tester | `pnpm web` | 217 | 1,428 | No `repos.craft-tester` at all. Median 2.9 s; `pnpm -C web test` about 15 s. | truly local by time | Below or at the about 13 s floor. craft-tester has no routed runs to measure. | none | 0 |
| craft-tester | `uv run pytest` | 12 | 300 | No repo entry. Median 19.9 s. | ready to route (low value) | Add `repos.craft-tester`: `uv run pytest*` and `uv run --directory generator pytest*`, then a trial. | remote.json | about 150 |
| craft-tester | `uv run craft-gen`, `uv run ruff`, `uv run ty`, `prek run`, `git push`, `pwsh -Command`, `bash -c`, 10 `.tmp` scripts | 22 + 4 + 2 + 3 + 1 + 5 + 1 + 12 | 180 + 1.5 + 1.1 + 35 + 5 + 53 + 8 + 528 | Short checks (median 0.2-11 s), git/prek, and `.tmp` one-offs. | truly local by time / one-offs | none | none | 0 |
| sherpa-onnx | `cmake build-cuda`, `cmake build-cpu`, `cmake` | 1 + 1 + 2 | 488 + 432 + 208 | `--local`: "native MSVC/CUDA build whose binaries must be on this machine". | blocked on work (one-off) | No CUDA toolkit on the builders, and `outputs` for `build-*`. A one-off spike in an `_ext` checkout. | none unless it recurs | 0 |
| sherpa-onnx | `pwsh .tmp/smoke.ps1` | 5 | 22 | `--local`: it runs the CUDA binaries on this PC's GPU. | truly local | none | none | 0 |
| herdr-gpui | `cargo clippy` | 1 | 627 | Pattern matches. 2026-10-26, no reason kept. | ready to route | Routes already: 23 routed. | none | 627 |
| herdr-gpui | `cargo test` (`-p herdr-credentials -p herdr-github --all-features credential_manager`) | 1 | 86 | `cargo test *` matches, but the desktop pattern names only `-p herdr-gpui`. A routed run would land in session 0. | blocked on work (config) | Add this command to herdr-gpui's `.gate/desktop.toml` and to `desktop.herdr-gpui`. | herdr-gpui (`_ext`) change + remote.json | 86 |
| world-of-intrigue | `7z a` | 1 | 675 | No pattern. It archives this PC's Unreal install folders, outside the worktree, into `.tmp`. 7-Zip is not on the builders. | truly local (one-off) | This PC's folders. | none | 0 |
| (no repo) | `bash -c` | 7 | 657 | `--local`: "YAAT-1564 fp32 NeMo export needs the local venv and model cache". It runs in an app-data cache folder, in no repo. | truly local | No repo to route. | none | 0 |
| ops-tower | `pwsh -Command` (`Set-Location tests/browser; pnpm exec playwright test specs/...`) | 2 | 242 | No pattern for the wrapper. Both runs failed. | ready to route (caller change) | Run the specs through `pwsh -NoProfile -File tools/browser-test.ps1 <specs>`, which routes. | ops-tower practice | 242 |
| ops-tower | `cargo build --release --example fixture_server` | 1 | 86 | `cargo build --release -p ops-tower*` does not match the example. The PC runs the fixture server. | blocked on work (outputs) | 1 run. No booking. | none | 0 |
| ops-tower | `git commit` | 3 | 35 | git. The hooks are gated `--slot light`, except `agent-mail-guard.ps1` and `glossary.ps1 -Check`. | truly local | none | none | 0 |
| collie | `bun run`, `bun`, `bun install` | 18 + 6 + 12 | 46 + 11 + 2.5 | A daily job in the `_ext` collie-debug checkout. It runs `bun` by absolute path to this PC's install. Median 0.2-2 s. | truly local by time | none | none | 0 |
| (gone) | `cargo test` | 1 | 26 | A `.tmp` mutation copy of towercab-3d. | truly local (one-off) | none | none | 0 |

#### Surprises

- **A command naming another worktree's script matches no pattern and stays local without a word.** `pwsh -NoProfile -File rustling-tulip.wt\<tree>\rt.ps1 ...`, gated from the main checkout, is not rewritten, because it is outside that run's worktree, so no `rt.ps1` pattern matches. The router's `another worktree` / `--cd` line (`outside_reason` in `crates/gate-router/src/decide.rs`) fires only for a command that already matched. That cost 17 rt.ps1 runs (6,588 s), 3 of them after the abs-path patterns landed.
  - The router could rewrite a path in another worktree of the same listed repo against that tree. It would then match the patterns and say the `--cd` line.
  - New issue: "Say the --cd line for an unmatched command whose word names a file in another worktree of a listed repo". Related: GATE-403, GATE-664, GATE-905.
- **The census and the router disagree on the repo of a paired towercab-3d worktree.** `census.py` names `towercab-3d.wt\<tree>\towercab-3d-vnas` as towercab-3d, by its `.wt` segment. The router names it `towercab-3d-vnas` from git, and `repos` does not list that name. So 47 runs (2,094 s) of the private crate sit in towercab-3d's `cargo test`, `cargo clippy` and `cargo doc` rows, and look as if they should route.
  - Those folders are gone, so I could not ask git directly. The evidence: in the same hours, all 15 routed towercab-3d `cargo clippy` runs came from the app folder, and all 12 vnas-folder runs stayed local.
  - A `census.py` fix: when a `.wt` segment's tree holds a nested repo, prefer git's answer for that folder.
- **GATE-404's premise no longer holds.** The GL probe routes from the `.claude` main checkout under the key `claude`: `crates/gate-router/src/key.rs` `repo_name` trims the leading dot, so a `.claude-wt` worktree is `claude` too. claude-config's pytest is blocked only by its missing patterns. GATE-404 can be closed or re-scoped.
- **rustling-tulip's `.gate/local.toml` never fires today.** The `repos` pattern `pwsh -NoProfile -File rt.ps1 native-e2e` is exact, so the `-SealedSecretOnly` form matches no pattern. The local.toml check runs only after a pattern match. The sealed runs are journalled with no `notRouted` rather than `declared local`. That is harmless now. It becomes load-bearing once `native-e2e *` is added (action 4), and it is placed correctly for that.
- **rustling-tulip's `cargo nextest run *` matches probe tests that must not leave the PC, so callers carry `--local`.** The WMI and desktop-logon probes and the suspend-rig repros are examples. Likewise gate's `cargo nextest run *` matches the `--no-run` rig builds. In each case a `.gate/local.toml` or `.gate/desktop.toml` line states the ruling in place of a flag.
- **One rustling-tulip `pwsh -Command` sets `$env:GATE_BOX_JOB = '1'` by hand.** That marker is how `rt.ps1`'s `Test-UnderGate` knows it is in a routed box job. Faking it on the PC skips rt.ps1's own gating. Worth a note to rustling-tulip.
- **herdr-gpui's `cargo test *` matches `cargo test --locked -p herdr-credentials -p herdr-github --all-features credential_manager`.** That test needs the desktop, but the desktop pattern names only `-p herdr-gpui`, so a routed run would go to session 0.

## Bookings

### Gate issues

- GATE-975 (High, bug): Say why a command naming another worktree's file stays local, and route it with `--cd`. 13,886 s.
- GATE-982 (High): Queue a desktop run behind the builder's busy desktop, and run session-only commands without the desktop lock. About 2,700 s, plus GATE-974's contention.
- GATE-976 (High): Route rustling-tulip's native-shot on the builder's desktop, with `.tmp/shots` outputs. About 5,700 s.
- GATE-977 (High): Install NSIS 3 on the builders so rustling-tulip's `rt.ps1 installer` routes. 435 s.
- GATE-978 (High): Send the gitignored `.tmp` files a routed dotnet probe names as inputs. About 900 s.
- GATE-979 (High): Route yaat's PrecomputeCache and GuideCapture runs and delve's strikes rewrite with their tracked outputs. About 2,500 s.
- GATE-980 (High, bug): Find why heavy filtered `dotnet test` runs say `faster here` and never route.
- GATE-981 (no priority): Name nested repos, `git -C` commits and light commits correctly in the local-run census.
- GATE-634: raised to High, blocked by GATE-958. Route a commit's whole pre-commit stage. About 47,200 s, plus about 11,200 s from light commits.
- GATE-974 (High, Blocked on rustling-tulip): rustling-tulip's whole `rt.ps1 native-e2e` over the desktop route. 13,987 s. Related to GATE-982.
- Comments:
  - on GATE-404: its premise no longer holds, since `.claude` routes as `claude`;
  - on GATE-936: delve's import from the project folder.

### remote.json and trials

These go under GATE-192, trialled in this session; results are in GATE-192's comment.

- yaat: `pwsh -NoProfile tools/test-all.ps1[ *]`, `dotnet run --no-build --project tests/*`.
- rustling-tulip: `cargo build` for `--all-targets` and the release builds, with outputs for `target/release`.
- claude-config (key `claude`): the linear tool's pytest and the collie-patched-update tests.
- towercab-3d: `pnpm vitest run[ --reporter=*]`.
- towercab-3d-vnas: a new entry, `cargo test[ *]` and `cargo clippy[ *]`.
- craft-tester: a new entry, its pytest forms.

### Repo inbox notes

- rustling-tulip:
  - never gate another worktree's `rt.ps1` from the main checkout;
  - give `native-e2e` a spec and filter argument;
  - split `pwsh -Command` chains;
  - gate the pre-push clippy hook;
  - use `--env` for fault-injection tests;
  - never fake `GATE_BOX_JOB`.
- towercab-3d: bring branches onto main's hook config, and gate the pre-push typecheck and clippy.
- towercab-3d-vnas: gate the pre-push `cargo check`.
- claude-config: a `.gate/local.toml` for `Invoke-ImageDrill.ps1`.
- ops-tower: run Playwright specs through `tools/browser-test.ps1`.
- yaat:
  - check the SpeechSandbox `--local` builds;
  - split `pwsh -Command` chains;
  - use `--env` for the routing census regeneration.
- yamv: split `pwsh -Command` chains.
- delve-the-dungeon:
  - switch the strikes hint to `--env` once GATE-979 lands;
  - pass `--cd` for csharpier from another worktree.

### This repo

- `.gate/local.toml`: `cargo nextest run *--no-run*`. The suite builds a rig runs on this PC stay local, declared, without `-Local`.

### Truly local, no booking

These need no booking:

- claude-config's image drill (this PC's hypervisor and private keys);
- yaat SpeechSandbox runs (the private corpus and this PC's GPU);
- sherpa-onnx CUDA builds and smokes;
- world-of-intrigue's `7z` of this PC's Unreal install;
- the rigs;
- the `git` and `prek` families;
- short families below the routing floor, each named in the Sort with its comparison.
