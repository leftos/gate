# Local-run census, 2026-10-27

The first run of the `local-run-census` skill (`.claude/skills/local-run-census/`): every heavy command family the gate journal shows running on this PC over the last 7 days, sorted into the three states of [routing-a-repo.md](../routing-a-repo.md#which-repos-and-which-commands) — ready to route, blocked on work, or truly local — with the evidence for each.

## Window

The last 7 days, 2026-10-20T19:33Z to 2026-10-27T19:33Z, read from the 19 journal archives, `events.1.jsonl` and `events.jsonl`. The archives keep only `release` lines, so the `notRouted` values, the fallback reasons and the `--local` table cover only the runs queued since 2026-10-27T04:49Z; for older runs the reason a run stayed local is not in the journal, and the sort below judges those from their commands and the repos' `remote.json` patterns.

Runs from `.claude-wt` worktrees are named `claude-config`. A repo shown as `(gone)` ran from a folder that no longer exists and is not a worktree of a known layout (removed copies of a repo, such as the `redcheck`, `c2copy` and `tc3d-227-mut` folders the Sort names); `(no repo)` is a folder in no git repo.

Routing overhead per repo, from families with both local and routed runs (routed median minus local median): yaat `dotnet test` 10.9 s, `dotnet build` 70.1 s, `dotnet format` 17.7 s; godot-mcp `dotnet build` 23.4 s; towercab-3d `cargo clippy` 47.2 s, `pnpm run` 17.8 s; delve-the-dungeon `dotnet build` 18.0 s; yamv `dotnet build` 29.8 s; ops-tower `cargo clippy` 11.9 s; opening-hand `dotnet build` 24.1 s. For gate and rustling-tulip the routed medians are below the local ones.

## Heavy families

| Repo | Family | Local | Routed | Unfinished | Local s total | Local median s | Routed median s | Local verdicts | notRouted | Fallbacks |
|---|---|---|---|---|---|---|---|---|---|---|
| yaat | `git commit` | 240 | 0 | 0 | 54341.3 | 206.8 | - | passed 220, failed 19, timed-out 1 | - | - |
| rustling-tulip | `pwsh .../rt.ps1` | 81 | 0 | 0 | 30113.0 | 339.6 | - | passed 67, failed 13, stalled 1 | - | - |
| godot-mcp | `pwsh .../hidden-desktop.ps1` | 169 | 201 | 5 | 28268.3 | 126.9 | 240.5 | passed 161, failed 8 | - | busy 29, desktop 9, full 1 |
| yaat | `pwsh tools/test-all.ps1` | 42 | 230 | 1 | 25529.0 | 604.0 | 653.5 | failed 23, passed 17, stalled 2 | - | - |
| towercab-3d | `cargo test` | 276 | 0 | 0 | 22879.7 | 62.0 | - | failed 146, passed 129, stalled 1 | --local 2 | - |
| towercab-3d | `pnpm vitest` | 452 | 0 | 0 | 10666.7 | 15.3 | - | passed 286, failed 165, stalled 1 | - | - |
| race-anywhere | `build RaceAnywhereEditor` | 35 | 0 | 0 | 10648.7 | 31.2 | - | passed 32, failed 3 | - | - |
| yaat | `dotnet test` | 198 | 1368 | 0 | 8743.9 | 14.3 | 25.1 | passed 113, failed 83, timed-out 2 | --local 5 | - |
| yaat | `dotnet build` | 191 | 777 | 0 | 7879.4 | 27.7 | 98.3 | passed 169, failed 22 | - | - |
| yaat | `dotnet run` | 174 | 81 | 0 | 6994.8 | 23.5 | 23.4 | passed 119, failed 55 | --local 3 | - |
| race-anywhere | `unrealeditor` | 58 | 0 | 0 | 5809.9 | 27.5 | - | passed 49, failed 9 | - | - |
| claude-config | `uv run pytest` | 63 | 0 | 0 | 4675.6 | 42.0 | - | passed 43, failed 20 | - | - |
| gate | `cargo nextest` | 45 | 1032 | 1 | 4518.3 | 79.2 | 64.4 | passed 36, failed 9 | --local 10 | - |
| rustling-tulip | `cargo build` | 27 | 0 | 0 | 4448.4 | 94.0 | - | passed 24, failed 3 | - | - |
| towercab-3d | `pnpm run` | 61 | 138 | 0 | 4062.6 | 18.0 | 35.8 | passed 56, failed 5 | - | full 1 |
| towercab-3d | `git commit` | 99 | 0 | 0 | 3835.9 | 16.6 | - | passed 93, failed 6 | - | - |
| godot-mcp | `dotnet build` | 244 | 723 | 1 | 3769.7 | 8.2 | 31.6 | passed 238, failed 6 | --local 29 | version 1 |
| yamv | `dotnet test` | 91 | 269 | 0 | 3503.9 | 17.2 | 48.8 | passed 45, failed 40, stalled 6 | - | - |
| godot-mcp | `dotnet test` | 85 | 112 | 0 | 3059.1 | 29.0 | 140.4 | failed 45, passed 40 | - | - |
| ops-tower | `git commit` | 69 | 0 | 0 | 2949.8 | 26.7 | - | passed 66, failed 3 | - | - |
| godot-mcp | `pwsh .../run.ps1` | 22 | 44 | 0 | 2826.5 | 106.4 | 173.8 | passed 21, failed 1 | - | busy 6, desktop 3 |
| gate | `cargo build` | 65 | 0 | 0 | 2679.8 | 32.4 | - | passed 61, failed 4 | --local 6 | - |
| towercab-3d | `uv run python scripts/shipping/data-generation/extract-airport-buildings.py` | 2 | 0 | 0 | 2581.9 | 1291.0 | - | passed 2 | - | - |
| rustling-tulip | `pwsh ./rt.ps1` | 14 | 0 | 0 | 2538.4 | 179.6 | - | passed 11, failed 3 | - | - |
| race-anywhere | `raceanywhere` | 10 | 0 | 0 | 2537.2 | 347.2 | - | passed 8, failed 2 | - | - |
| godot-mcp | `git commit` | 77 | 0 | 0 | 2434.3 | 22.5 | - | passed 69, failed 8 | - | - |
| race-anywhere | `pwsh tools/unreal/import-tile-set.ps1` | 4 | 0 | 0 | 2407.3 | 239.0 | - | passed 3, failed 1 | - | - |
| race-anywhere | `build RaceAnywhere` | 1 | 0 | 0 | 2228.4 | 2228.4 | - | passed 1 | - | - |
| rustling-tulip | `cargo run` | 25 | 0 | 0 | 2100.1 | 82.7 | - | passed 25 | - | - |
| race-anywhere | `uv run pytest` | 4 | 0 | 0 | 2065.4 | 396.1 | - | passed 4 | - | - |
| yaat | `pwsh -Command` | 17 | 0 | 0 | 2040.7 | 43.4 | - | passed 13, failed 4 | - | - |
| delve-the-dungeon | `git commit` | 19 | 0 | 0 | 2037.7 | 114.0 | - | passed 19 | - | - |
| towercab-3d | `cargo clippy` | 55 | 43 | 0 | 1998.4 | 9.0 | 56.2 | passed 45, failed 10 | --local 3 | - |
| opening-hand | `git commit` | 33 | 0 | 0 | 1987.1 | 55.4 | - | passed 32, failed 1 | - | - |
| rustling-tulip | `cargo nextest` | 5 | 1333 | 1 | 1912.4 | 172.1 | 89.0 | failed 3, passed 2 | - | - |
| in-the-sky | `git commit` | 35 | 0 | 0 | 1794.3 | 43.2 | - | passed 34, failed 1 | - | - |
| ops-tower | `cargo nextest` | 36 | 280 | 0 | 1736.0 | 29.1 | 44.7 | passed 32, failed 4 | - | - |
| herdr-gpui | `just ci` | 1 | 7 | 0 | 1586.0 | 1586.0 | 191.6 | failed 1 | - | - |
| claude-config | `pwsh tools/collie-patched-update/test-collie-patched-update.ps1` | 5 | 0 | 0 | 1532.0 | 319.1 | - | passed 3, failed 2 | - | - |
| godot-mcp | `prek run` | 28 | 0 | 0 | 1505.5 | 54.2 | - | passed 23, failed 5 | - | - |
| yaat | `pwsh .../test-all.ps1` | 2 | 0 | 0 | 1501.9 | 750.9 | - | failed 1, passed 1 | - | - |
| race-anywhere | `pwsh tools/unreal/package.ps1` | 3 | 9 | 0 | 1462.4 | 594.1 | 397.0 | failed 1, stalled 1, passed 1 | - | - |
| race-anywhere | `unrealeditor-cmd` | 2 | 0 | 0 | 1360.2 | 680.1 | - | failed 2 | - | - |
| ue-mcp | `npm run` | 7 | 7 | 0 | 1284.9 | 90.6 | 284.6 | passed 6, failed 1 | - | - |
| in-the-sky | `dotnet test` | 104 | 149 | 0 | 1183.3 | 8.5 | 7.3 | failed 56, passed 48 | - | version 2 |
| yamv | `yamv.core.tests 10` | 5 | 0 | 0 | 1179.3 | 302.3 | - | failed 3, passed 1, timed-out 1 | - | - |
| towercab-3d | `node scripts/shipping/build/vnas.js` | 23 | 0 | 0 | 1179.1 | 21.5 | - | passed 19, failed 4 | --local 3 | - |
| delve-the-dungeon | `dotnet build` | 17 | 2095 | 0 | 1120.8 | 55.7 | 73.7 | passed 16, failed 1 | gitignored input 3 | - |
| rustling-tulip | `git push` | 23 | 0 | 0 | 1042.9 | 39.1 | - | passed 23 | - | - |
| delve-the-dungeon | `godot_console` | 16 | 556 | 0 | 1031.4 | 31.1 | 89.5 | passed 13, log-failure 2, stalled 1 | - | - |
| ue-mcp | `pwsh -Command` | 5 | 0 | 0 | 1007.4 | 255.5 | - | passed 3, failed 2 | - | - |
| herdr-gpui | `cargo test` | 9 | 114 | 0 | 1003.0 | 85.9 | 267.8 | passed 7, failed 2 | - | - |
| rustling-tulip | `cargo check` | 25 | 0 | 0 | 966.6 | 14.6 | - | passed 13, failed 12 | - | - |
| yaat-server | `git commit` | 8 | 0 | 0 | 931.7 | 85.5 | - | passed 6, failed 2 | - | - |
| godot-mcp | `pwsh tools/hidden-desktop.ps1` | 5 | 0 | 0 | 897.8 | 53.7 | - | failed 3, passed 2 | - | - |
| race-anywhere | `pwsh ra.ps1` | 17 | 0 | 0 | 829.1 | 31.7 | - | passed 16, failed 1 | - | - |
| herdr-gpui | `cargo clippy` | 2 | 87 | 0 | 789.0 | 394.5 | 120.3 | passed 2 | - | - |
| towercab-3d | `cargo build` | 8 | 0 | 0 | 732.2 | 59.5 | - | passed 6, failed 2 | - | - |
| delve-the-dungeon | `git` | 6 | 0 | 0 | 717.7 | 114.5 | - | passed 6 | - | - |
| herdr-gpui | `cargo build` | 2 | 0 | 0 | 691.9 | 345.9 | - | passed 2 | - | - |
| gate | `git commit` | 133 | 0 | 0 | 666.9 | 1.7 | - | passed 132, failed 1 | - | - |
| towercab-3d | `uv run python .../build_media.py` | 2 | 0 | 0 | 618.8 | 309.4 | - | passed 2 | - | - |
| delve-the-dungeon | `dotnet format` | 6 | 776 | 0 | 596.4 | 87.9 | 35.4 | passed 6 | - | busy 2 |
| yamv | `git commit` | 14 | 0 | 0 | 580.4 | 37.4 | - | passed 14 | - | - |
| yaat | `dotnet format` | 23 | 251 | 0 | 535.6 | 14.1 | 31.5 | passed 22, failed 1 | - | busy 1 |
| delve-the-dungeon | `dotnet test` | 13 | 834 | 0 | 471.8 | 32.1 | 41.7 | passed 8, failed 5 | - | full 1 |
| rustling-tulip | `git commit` | 3 | 0 | 0 | 466.4 | 112.1 | - | passed 3 | - | - |
| collie | `bun run` | 15 | 0 | 0 | 399.9 | 3.7 | - | passed 15 | - | - |
| rustling-tulip | `pwsh -Command` | 5 | 0 | 0 | 390.3 | 28.2 | - | passed 4, failed 1 | - | - |
| towercab-3d-vnas | `cargo test` | 2 | 0 | 0 | 388.3 | 194.2 | - | passed 2 | - | - |
| yaat | `env YAAT_MENU_GOLDEN_REGENERATE=1` | 4 | 0 | 0 | 383.4 | 92.5 | - | failed 4 | - | - |
| godot-mcp | `pwsh .../suspend-rig.ps1` | 7 | 0 | 0 | 379.4 | 48.2 | - | failed 4, passed 3 | - | - |
| .claude | `uv run pytest` | 5 | 0 | 0 | 357.9 | 50.3 | - | passed 3, failed 2 | - | - |
| towercab-3d | `uv run python .tmp/tc3d-54-clips/convert_set.py` | 2 | 0 | 0 | 351.9 | 175.9 | - | failed 1, passed 1 | - | - |
| rustling-tulip | `pwsh .tmp/red-run.ps1` | 1 | 0 | 0 | 333.6 | 333.6 | - | failed 1 | - | - |
| ops-tower | `pwsh tools/browser-test.ps1` | 5 | 166 | 0 | 332.6 | 33.5 | 72.5 | passed 4, failed 1 | - | - |
| towercab-3d | `pnpm install` | 19 | 0 | 0 | 325.9 | 15.5 | - | passed 19 | - | - |
| yamv | `dotnet build` | 35 | 622 | 0 | 307.2 | 6.7 | 36.5 | passed 24, failed 11 | - | - |
| race-anywhere | `node .../index.js` | 2 | 0 | 0 | 305.2 | 152.6 | - | passed 2 | - | - |
| gate | `cargo check` | 12 | 0 | 0 | 302.6 | 23.3 | - | passed 7, failed 5 | - | - |
| delve-the-dungeon | `pwsh tools/test-all.ps1` | 2 | 7 | 0 | 296.9 | 148.5 | 356.1 | passed 1, failed 1 | - | - |
| towercab-3d | `cmd /c` | 1 | 0 | 0 | 295.0 | 295.0 | - | failed 1 | - | - |
| rustling-tulip | `pwsh .tmp/run-tests.ps1` | 8 | 0 | 0 | 290.1 | 37.9 | - | passed 5, failed 3 | - | - |
| godot-mcp | `uv run pytest` | 8 | 0 | 0 | 288.2 | 34.5 | - | failed 5, passed 3 | - | - |
| towercab-3d | `pwsh -Command` | 7 | 0 | 0 | 282.4 | 26.3 | - | passed 6, failed 1 | - | - |
| gate | `bash -c` | 2 | 0 | 0 | 272.9 | 136.4 | - | passed 2 | - | - |
| yaat | `git cherry-pick` | 4 | 0 | 0 | 262.8 | 13.4 | - | passed 4 | - | - |
| towercab-3d | `cargo doc` | 21 | 0 | 0 | 258.2 | 7.7 | - | passed 21 | - | - |
| godot-mcp | `dotnet publish` | 33 | 0 | 0 | 226.4 | 4.4 | - | passed 33 | - | - |
| ops-tower | `cargo clippy` | 6 | 200 | 0 | 221.4 | 16.1 | 28.0 | passed 4, failed 2 | - | - |
| delve-the-dungeon | `dotnet run` | 17 | 0 | 0 | 220.3 | 9.3 | - | passed 16, failed 1 | - | - |
| yaat-server | `prek run` | 2 | 0 | 0 | 206.3 | 103.2 | - | failed 1, passed 1 | - | - |
| opening-hand | `dotnet test` | 9 | 71 | 0 | 201.8 | 15.1 | 14.9 | passed 5, failed 4 | - | - |
| gate | `cargo doc` | 3 | 0 | 0 | 190.5 | 5.6 | - | passed 3 | - | - |
| towercab-3d-vnas | `cargo clippy` | 2 | 0 | 0 | 181.4 | 90.7 | - | passed 2 | - | - |
| towercab-3d | `bash .tmp/mut/run.sh` | 1 | 0 | 0 | 176.8 | 176.8 | - | passed 1 | - | - |
| gate | `cargo clippy` | 3 | 772 | 1 | 155.6 | 56.5 | 31.8 | passed 2, failed 1 | - | - |
| ops-tower | `pwsh -Command` | 5 | 0 | 0 | 140.8 | 8.3 | - | passed 3, failed 2 | - | - |
| claude-config | `wsl Ubuntu` | 3 | 0 | 0 | 134.7 | 4.8 | - | failed 1, passed 1, stalled 1 | - | - |
| rustling-tulip | `pwsh .tmp/rt305_real.ps1` | 1 | 0 | 0 | 130.5 | 130.5 | - | passed 1 | - | - |
| yamv | `yamv.core.tests` | 1 | 0 | 0 | 125.8 | 125.8 | - | stalled 1 | - | - |
| ops-tower | `cargo check` | 4 | 0 | 0 | 122.1 | 15.5 | - | passed 4 | - | - |
| yaat-server | `dotnet test` | 2 | 8 | 0 | 114.1 | 57.1 | 62.3 | passed 2 | - | - |
| (gone) | `dotnet test` | 3 | 0 | 0 | 111.5 | 28.9 | - | failed 3 | - | - |
| yaat | `dotnet` | 6 | 0 | 0 | 103.8 | 8.1 | - | passed 6 | - | - |
| herdr-gpui | `wsl Ubuntu` | 3 | 58 | 0 | 102.4 | 28.9 | 145.9 | failed 2, passed 1 | - | - |
| yaat | `dotnet restore` | 18 | 0 | 0 | 102.0 | 3.0 | - | passed 18 | - | - |
| delve-the-dungeon | `docker build` | 4 | 0 | 0 | 101.6 | 25.4 | - | passed 4 | - | - |
| yaat | `yaat.client.tests 1` | 2 | 0 | 0 | 98.7 | 49.3 | - | passed 2 | - | - |
| towercab-3d | `pwsh .../gate-49.ps1` | 2 | 0 | 0 | 96.4 | 48.2 | - | failed 1, passed 1 | - | - |
| towercab-3d | `cargo check` | 1 | 0 | 0 | 96.2 | 96.2 | - | failed 1 | - | - |
| gate | `pwsh -Command` | 1 | 0 | 0 | 92.6 | 92.6 | - | passed 1 | - | - |
| rustling-tulip | `cargo clippy` | 1 | 543 | 0 | 87.5 | 87.5 | 40.3 | passed 1 | - | - |
| yaat | `pwsh .tmp/repeat646.ps1` | 2 | 0 | 0 | 85.9 | 42.9 | - | passed 2 | - | - |
| ops-tower | `cargo build` | 1 | 0 | 0 | 82.3 | 82.3 | - | passed 1 | - | - |
| yaat | `probe` | 1 | 0 | 0 | 79.0 | 79.0 | - | passed 1 | - | - |
| towercab-3d | `git` | 2 | 0 | 0 | 78.8 | 39.4 | - | passed 2 | - | - |
| yamv | `wsl Ubuntu` | 5 | 0 | 0 | 74.4 | 16.7 | - | passed 4, log-failure 1 | - | - |
| yaat | `pwsh .tmp/rig/watch-run.ps1` | 2 | 0 | 0 | 67.1 | 33.5 | - | failed 1, passed 1 | - | - |
| gate | `cargo test` | 1 | 0 | 0 | 57.2 | 57.2 | - | passed 1 | - | - |
| yamv | `node tools/dev/page-check/check.mjs` | 7 | 0 | 0 | 56.9 | 6.1 | - | passed 4, failed 3 | - | - |
| yaat | `git` | 4 | 0 | 0 | 56.2 | 14.4 | - | passed 4 | - | - |
| in-the-sky | `blender` | 4 | 0 | 0 | 55.6 | 13.8 | - | passed 4 | - | - |
| rustling-tulip | `pwsh .tmp/rt305_app.ps1` | 1 | 0 | 0 | 55.4 | 55.4 | - | passed 1 | - | - |
| towercab-3d | `uv run python .tmp/mutate.py` | 2 | 0 | 0 | 54.1 | 27.1 | - | passed 2 | - | - |
| claude-config | `git commit` | 13 | 0 | 0 | 52.9 | 2.6 | - | passed 13 | - | - |
| ops-tower | `cargo doc` | 2 | 0 | 0 | 50.6 | 25.3 | - | passed 2 | - | - |
| towercab-3d-vnas | `git commit` | 3 | 0 | 0 | 48.6 | 19.2 | - | passed 3 | - | - |
| delve-the-dungeon | `dotnet msbuild` | 38 | 0 | 0 | 41.9 | 0.6 | - | passed 38 | - | - |
| delve-the-dungeon | `dotnet csharpier` | 10 | 904 | 0 | 40.7 | 3.0 | 9.5 | passed 10 | - | - |
| towercab-3d-vnas | `cargo doc` | 2 | 0 | 0 | 40.6 | 20.3 | - | passed 2 | - | - |
| godot-mcp | `dotnet csharpier` | 13 | 224 | 0 | 35.7 | 2.0 | 21.3 | passed 13 | - | - |
| in-the-sky | `bash .../pr68-build-test.sh` | 1 | 0 | 0 | 35.0 | 35.0 | - | passed 1 | - | - |
| yamv | `pwsh -Command` | 4 | 0 | 0 | 33.5 | 8.7 | - | passed 3, failed 1 | - | - |
| yaat | `yaat.client.ui.tests *ClientMenuHostGroundTests*` | 1 | 0 | 0 | 31.3 | 31.3 | - | failed 1 | - | - |
| yaat | `pwsh .tmp/sids.ps1` | 1 | 0 | 0 | 28.9 | 28.9 | - | passed 1 | - | - |
| yaat | `yaat.sim.tests *ProbeH_*` | 1 | 0 | 0 | 28.9 | 28.9 | - | failed 1 | - | - |
| godot-mcp | `pwsh .../install.ps1` | 5 | 0 | 0 | 27.7 | 2.5 | - | passed 5 | - | - |
| (gone) | `cargo test` | 1 | 0 | 0 | 26.3 | 26.3 | - | failed 1 | - | - |
| godot-mcp | `dotnet run` | 2 | 0 | 0 | 22.4 | 11.2 | - | passed 2 | - | - |
| yamv | `cmd /c` | 1 | 0 | 0 | 22.4 | 22.4 | - | failed 1 | - | - |
| yamv | `pwsh scripts/publish.ps1` | 3 | 0 | 0 | 21.0 | 7.3 | - | passed 3 | - | - |
| in-the-sky | `bash -c` | 1 | 0 | 0 | 20.2 | 20.2 | - | passed 1 | - | - |
| .claude | `git commit` | 1 | 0 | 0 | 19.3 | 19.3 | - | passed 1 | - | - |
| delve-the-dungeon | `dotnet publish` | 1 | 0 | 0 | 18.5 | 18.5 | - | passed 1 | - | - |
| yaat | `bash .../rebase-additive-loop.sh` | 1 | 0 | 0 | 17.3 | 17.3 | - | passed 1 | - | - |
| claude-config | `pwsh tools/load-burn/test-load-burn.ps1` | 1 | 0 | 0 | 15.4 | 15.4 | - | passed 1 | - | - |
| collie | `bun install` | 10 | 0 | 0 | 15.1 | 0.4 | - | passed 10 | - | - |
| opening-hand | `dotnet build` | 1 | 372 | 0 | 15.1 | 15.1 | 39.1 | passed 1 | - | - |
| yaat | `git rebase` | 4 | 0 | 0 | 14.6 | 3.2 | - | failed 2, passed 2 | - | - |
| collie | `bun` | 5 | 0 | 0 | 14.2 | 2.5 | - | passed 5 | - | - |
| towercab-3d | `pnpm exec` | 3 | 0 | 0 | 13.1 | 3.6 | - | passed 3 | - | - |
| rustling-tulip | `prek run` | 2 | 0 | 0 | 12.2 | 6.1 | - | passed 2 | - | - |
| rustling-tulip | `cargo fmt` | 2 | 0 | 0 | 10.9 | 5.5 | - | passed 2 | - | - |
| agent-park | `git commit` | 6 | 0 | 0 | 10.7 | 1.8 | - | passed 6 | - | - |
| yaat | `uv run python .../mcp_probe.py` | 7 | 0 | 0 | 9.2 | 1.5 | - | passed 7 | - | - |
| yaat | `yaat.client.tests *HoldShortMenu*` | 1 | 0 | 0 | 7.4 | 7.4 | - | failed 1 | - | - |
| yaat-server | `dotnet build` | 1 | 27 | 0 | 7.4 | 7.4 | 56.8 | passed 1 | - | - |
| in-the-sky | `ffmpeg` | 1 | 0 | 0 | 7.1 | 7.1 | - | passed 1 | - | - |
| claude-config | `bash .../lint.sh` | 1 | 0 | 0 | 7.0 | 7.0 | - | passed 1 | - | - |
| ops-tower | `wsl Ubuntu` | 1 | 0 | 0 | 6.8 | 6.8 | - | failed 1 | - | - |
| ops-tower | `git` | 10 | 0 | 0 | 5.0 | 0.4 | - | failed 9, passed 1 | - | - |
| yaat | `yaat.sim.tests` | 2 | 0 | 0 | 4.0 | 4.0 | - | unknown 1, passed 1 | - | - |
| in-the-sky | `ffmpeg 24` | 1 | 0 | 0 | 3.2 | 3.2 | - | passed 1 | - | - |
| yaat | `yaat.sim.tests *SecondTaxiGivenOffThe28RPavement*` | 1 | 0 | 0 | 3.2 | 3.2 | - | failed 1 | - | - |
| towercab-3d | `cargo fmt` | 2 | 0 | 0 | 3.0 | 1.5 | - | passed 2 | - | - |
| yamv | `npm` | 2 | 0 | 0 | 2.3 | 1.1 | - | passed 2 | - | - |
| yaat | `pwsh .../load-burn.ps1` | 2 | 0 | 0 | 2.2 | 1.1 | - | failed 2 | - | - |
| godot-mcp | `git cherry-pick` | 1 | 0 | 0 | 2.0 | 2.0 | - | passed 1 | - | - |
| yaat-server | `dotnet restore` | 1 | 0 | 0 | 2.0 | 2.0 | - | passed 1 | - | - |
| yaat | `dotnet csharpier` | 1 | 19 | 0 | 1.8 | 1.8 | 10.3 | passed 1 | - | - |
| yaat | `dotnet msbuild` | 1 | 0 | 0 | 1.7 | 1.7 | - | passed 1 | - | - |
| opening-hand | `dotnet csharpier` | 1 | 101 | 0 | 1.3 | 1.3 | 7.5 | passed 1 | - | - |
| yaat-server | `git cherry-pick` | 2 | 0 | 0 | 0.7 | 0.4 | - | passed 2 | - | - |
| yaat | `yaat.crcdriverhook.tests` | 1 | 0 | 0 | 0.6 | 0.6 | - | passed 1 | - | - |
| yamv | `pwsh .../load-burn.ps1` | 1 | 0 | 0 | 0.6 | 0.6 | - | failed 1 | - | - |
| (gone) | `dotnet csharpier` | 0 | 1 | 0 | 0 | - | 10.8 | - | - | - |
| (gone) | `dotnet format` | 0 | 1 | 0 | 0 | - | 189.0 | - | - | - |
| (gone) | `dotnet run` | 0 | 1 | 0 | 0 | - | 783.2 | - | - | - |
| agent-park | `cargo build` | 0 | 2 | 0 | 0 | - | 124.2 | - | - | - |
| agent-park | `cargo clippy` | 0 | 3 | 0 | 0 | - | 68.2 | - | - | - |
| agent-park | `cargo nextest` | 0 | 5 | 0 | 0 | - | 33.7 | - | - | - |
| gate | `bash tools/builder/test-install-macos.sh` | 0 | 3 | 0 | 0 | - | 17.8 | - | - | - |
| gate | `git add` | 1 | 0 | 0 | 0.0 | 0.0 | - | passed 1 | - | - |
| godot-mcp | `dotnet format` | 0 | 40 | 0 | 0 | - | 50.6 | - | - | - |
| in-the-sky | `dotnet build` | 0 | 547 | 1 | 0 | - | 28.2 | - | - | - |
| in-the-sky | `dotnet csharpier` | 0 | 155 | 0 | 0 | - | 3.5 | - | - | - |
| in-the-sky | `dotnet format` | 0 | 299 | 0 | 0 | - | 9.0 | - | - | - |
| opening-hand | `dotnet format` | 0 | 149 | 0 | 0 | - | 16.6 | - | - | - |
| opening-hand | `godot_console` | 0 | 16 | 0 | 0 | - | 74.9 | - | - | - |
| opening-hand | `uv run pytest` | 0 | 3 | 0 | 0 | - | 13.6 | - | - | - |
| vatsim_control_recs | `uv run pytest` | 0 | 5 | 0 | 0 | - | 139.0 | - | - | - |
| yaat-server | `dotnet format` | 0 | 1 | 0 | 0 | - | 29.4 | - | - | - |
| yamv | `dotnet format` | 0 | 39 | 0 | 0 | - | 40.6 | - | - | - |

## `--local` runs

| Repo | Family | Reason | Runs |
|---|---|---|---|
| godot-mcp | `dotnet build` | GATE-557: a routed itest build leaves a nothing-back record, and the parallel lanes then build one tree at once | 29 |
| gate | `cargo nextest` | the rig needs this PC's gate.exe | 9 |
| gate | `cargo build` | the installer builds the gate on this PC | 6 |
| towercab-3d | `cargo clippy` | branch predates TC3D_CHECK_BUILD; build needs the gitignored converter exe and dist/ | 3 |
| towercab-3d | `node scripts/shipping/build/vnas.js` | branch predates TC3D_CHECK_BUILD; build needs the gitignored converter exe and dist/ | 3 |
| towercab-3d | `cargo test` | branch predates TC3D_CHECK_BUILD; build needs the gitignored converter exe and dist/ | 2 |
| yaat | `dotnet run` | LayoutInspector output read locally | 2 |
| gate | `cargo nextest` | GATE-813 rig needs this PC's gate.exe and a suite built from the clean tree | 1 |
| yaat | `dotnet run` | HTML render must land in .tmp | 1 |
| yaat | `dotnet test` | TickRecorder output must land in .tmp | 1 |
| yaat | `dotnet test` | remote run captures no stdout of passing tests; bare-route dump probe writes locally | 1 |
| yaat | `dotnet test` | remote run captures no stdout of passing tests; node-ref route dump probe writes locally | 1 |
| yaat | `dotnet test` | remote run captures no test stdout; route node-id probe needs a file the test host writes | 1 |
| yaat | `dotnet test` | remote run captures no test stdout; route node-id probe needs live output | 1 |

- Window: 2026-10-20T19:50:50+00:00 to 2026-10-27T19:50:50+00:00 (UTC).
- Files read: 19 archives, events.1.jsonl, events.jsonl; unreadable: none.
- Skipped lines: 0; duplicate lines left out: 3076.
- Runs with neither a queue nor a release line in the window: 140; heavy runs lent or covered, with no release of their own, left out: 0.
- The archive keeps only release lines, so notRouted, the fallbacks and the --local table cover only the runs queued from 2026-10-27T04:49:35+00:00.

## Sort

Proposed by the census run; the orchestrator confirms each row when it books it. "Routes already" means the family routes and its local runs carry no reason the journal kept: nothing to change, unless a repeat census shows them again.

### Ready to route

| Repo | Family | Evidence and the change |
|---|---|---|
| yaat | `pwsh tools/test-all.ps1` | 230 routed as `pwsh tools/test-all.ps1`; 38 of 42 local runs are `pwsh -NoProfile -File tools/test-all.ps1`, which no yaat pattern matches (local median 604 s, 25,529 s in all). Add `pwsh -NoProfile -File tools/test-all.ps1` and `pwsh -NoProfile -File tools/test-all.ps1 *` to `repos.yaat`. |
| yaat | `pwsh .../test-all.ps1` | 2 runs naming the script by absolute path; covered by the same pattern once run with `--cd` from the worktree. |
| rustling-tulip | `pwsh .../rt.ps1`, `pwsh ./rt.ps1` | 95 local, 0 routed, median 340 s and 180 s (32,651 s in all). The patterns are exact (`pwsh -NoProfile -File ./rt.ps1 test`), so `test -Validate` (61 runs) never matches. Add `pwsh -NoProfile -File ./rt.ps1 test *` and `pwsh ./rt.ps1 test *`; the `native-e2e` runs (29) are under blocked on work. |
| towercab-3d | `cargo test` | 276 local, 0 routed, median 62 s (22,880 s in all), longer than the 47 s routing adds to the repo's `cargo clippy`. No `cargo test` pattern in `repos.towercab-3d`. Add `cargo test`, `cargo test *`, and an `inputs` glob for the converter exe and `dist/` (the `--local` reason of 2 runs: "build needs the gitignored converter exe and dist/"). |
| rustling-tulip | `cargo check` | 25 local, 0 routed, median 14.6 s; the repo's routed `cargo clippy` and `cargo nextest` medians are below their local ones, so routing costs it nothing measurable. Add `cargo check` and `cargo check *` to `repos.rustling-tulip`. |
| race-anywhere | `uv run pytest` | 4 local, median 396 s; no pattern. Add `uv run pytest*` to `repos.race-anywhere`. |
| claude-config worktrees | `uv run pytest` | 61 local runs across `.claude`, `cc-*`, `linear*` and `fix-*` trees, medians 5-413 s; `repos.claude-config` holds only the GL probe. Add `uv run --directory tools/* pytest*`, `uv run --directory skills/* pytest*` and `uv run --project tools/* pytest*`, after checking the router names the `.claude-wt` worktrees' repo `claude-config`. |
| godot-mcp | `uv run pytest` | 8 local, median 34.5 s, above the repo's 23.4 s overhead; no pattern. Add `uv run --quiet --with * pytest *` to `repos.godot-mcp`. |
| yaat | `dotnet run` | 173 local, 77 routed, median 23.6 s. The pattern covers only `dotnet run --project tests/*`; 128 runs are of `tools/*` projects. The tools that write output read locally (3 `--local`: "output read locally", "must land in .tmp") need `outputs` globs: book under blocked on work. The `tests/*` local runs route already. |
| yaat, yaat-server, yamv, delve-the-dungeon, opening-hand, in-the-sky, godot-mcp, ops-tower, gate, rustling-tulip, herdr-gpui, ue-mcp | the matched .NET, cargo and npm families: yaat `dotnet test`, `dotnet build`, `dotnet format`, `dotnet csharpier`; yaat-server `dotnet test`, `dotnet build`; yamv `dotnet build`, `dotnet test`; race-anywhere `pwsh tools/unreal/package.ps1`; delve-the-dungeon `dotnet build`, `dotnet test`, `dotnet format`, `dotnet csharpier`, `pwsh tools/test-all.ps1`; opening-hand `dotnet test`, `dotnet build`, `dotnet csharpier`; godot-mcp `dotnet csharpier`; ops-tower `cargo clippy`, `pwsh tools/browser-test.ps1`; gate `cargo clippy`; rustling-tulip `cargo nextest`, `cargo clippy`; herdr-gpui `cargo clippy`, `cargo test`, `cargo build`, `just ci`, `wsl Ubuntu`; ue-mcp `npm run` | Routes already: every local run matches a pattern of its repo and carries no reason in the live journal (the older runs' reasons are not kept: fallbacks, Windows-only paths, outside-worktree words). The large ones: yaat `dotnet test` 198 local against 1,364 routed, `dotnet build` 191 against 779. |
| in-the-sky | `dotnet test` | Routes already (145 routed); 104 local with every command naming its project by absolute path, which stays local when the path is outside the run's worktree. Local and routed medians are equal (8.4 s and 7.3 s). |
| godot-mcp | `dotnet test` | 84 local, 109 routed. The repo's pattern names only `tests?GodotMcp.Tests?GodotMcp.Tests.csproj*`; 83 of the 84 local runs name their test project by absolute path, which the relative pattern does not match. Run them with a worktree-relative `--project` from the worktree (`--cd`), and through `tools/hidden-desktop.ps1` where they need its desktop. |
| godot-mcp | `pwsh .../hidden-desktop.ps1`, `pwsh tools/hidden-desktop.ps1`, `pwsh .../run.ps1` | 202 + 44 routed; 198 local, all but 4 naming the script by absolute path (31,687 s; `hidden-desktop.ps1` median 127 s). The desktop patterns list each filter class, so a class or argument outside the list stays local; with 9 `desktop` and 34 `busy` fallbacks kept in the live window, see blocked on work. |

### Blocked on work

| Repo | Family | Blocker |
|---|---|---|
| godot-mcp | `pwsh .../hidden-desktop.ps1`, `pwsh .../run.ps1` | Desktop routing capacity: 34 `busy` and 9 `desktop` fallbacks in the live window alone, over 198 local runs (31,687 s). A builder desktop that serves the itest lanes in parallel, and patterns that take any `--filter-class` instead of listing each. |
| godot-mcp | `dotnet build` | 29 `--local` runs: "GATE-557: a routed itest build leaves a nothing-back record, and the parallel lanes then build one tree at once". 248 local against 706 routed. |
| rustling-tulip | `pwsh .../rt.ps1 native-e2e`, `cargo run` (the `shot` example), `cargo build` | The native app's end-to-end and screenshot runs (29 + 25 + 27 runs; the `cargo run` and `cargo build` ones alone 6,548 s) build and launch the native app on this PC: a desktop route for rustling-tulip, and an outputs list for the release build the PC launches. |
| race-anywhere | `build RaceAnywhereEditor`, `build RaceAnywhere`, `unrealeditor`, `unrealeditor-cmd`, `raceanywhere`, `pwsh tools/unreal/import-tile-set.ps1`, `pwsh ra.ps1`, `node .../index.js` | Unreal editor builds whose binaries the PC then launches (35 + 1 runs, 12,877 s) and game or editor runs with a window and GPU (70 runs, 9,707 s): needs GATE-696 or an outputs list for the editor build, and a desktop route with a GPU for the game runs. The tile import, `ra.ps1 build` and the node build tool (4 + 17 + 2 runs, 3,541 s) drive the same editor. Only `tools/unreal/package.ps1` routes today. |
| towercab-3d | `pnpm run vite:build`, `cmd /c`, `pwsh -Command`, `pnpm exec` | The vite build's `dist/` is an input the Rust builds read on this PC (the `--local` reason above); a build whose output the PC uses needs an `outputs` glob for `dist/` before it can route. 37 runs, about 1,060 s. |
| towercab-3d | `cargo build`, `cargo check --features tauri/custom-protocol` | 9 runs, median 59.5 s and 96 s: the Tauri app build the PC then launches, which waits on GATE-696 or an outputs list for `src-tauri/target`. |
| yaat | `dotnet run` (tools), `dotnet test` (`--local` ones) | 8 `--local` runs: "remote run captures no stdout of passing tests" (4) and outputs that must land in `.tmp` (4). A routed run that returns passing tests' output, and `outputs` globs for the tools' `.tmp` files. |
| yaat | `env YAAT_MENU_GOLDEN_REGENERATE=1` | 4 runs, median 92.5 s: the gate cannot route an `env` prefix; the golden files it writes would need an `outputs` glob and the variable an `--env`. |
| delve-the-dungeon | `godot_console --import` | 12 of 16 local runs are `--import` by absolute Godot path; the patterns take `*Godot_console* --headless --path src?Delve.Client --import*`, so these name the project by absolute path or another tree. Check the callers' word form; 556 routed. |
| delve-the-dungeon | `dotnet build` (`gitignored input`) | 3 runs journalled `gitignored input`: an input past the 64 MiB, 2000-file or sibling limit. |
| in-the-sky | `blender`, `ffmpeg`, `ffmpeg 24` | Blender and ffmpeg are not on the builders; all 6 runs are `.tmp/explore` one-offs, so no booking unless they recur. |
| delve-the-dungeon | `docker build` | 4 runs: Docker is not on the builders; book only if the server rehearsal becomes routine. |

### Truly local

| Repo | Family | Reason |
|---|---|---|
| every repo | `git commit`, `git push`, `git rebase`, `git cherry-pick`, `git add`, `git` (`-C … commit`, `rebase --continue`), `prek run` | A `git` or `prek` command writes this checkout's refs and stays local silently (routing-a-repo.md). The largest: yaat `git commit` 240 runs, 54,341 s, median 207 s; the time is the repo's commit hooks. |
| gate | `cargo build`, `cargo nextest` (`--no-run`), `cargo test --no-run` | The installer's build of `gate.exe` for this PC (6 `--local`: "the installer builds the gate on this PC", 48 runs of `--release -p gate -p gate-desktop`) and rig builds that need this PC's `gate.exe` (10 `--local`). The family `cargo nextest` routes (1,025 runs). |
| gate, ops-tower, towercab-3d, rustling-tulip | `cargo check`, `cargo doc`, `cargo fmt` (rustling-tulip: `cargo fmt` only) | Short checks: medians 1.5-25 s, below the 47 s routing adds to towercab-3d's `cargo clippy`. Two are borderline: ops-tower `cargo check` (median 15.5 s against 11.9 s of overhead) and gate `cargo check` (23.3 s, where routing costs gate nothing measurable), which could route with a pattern if wanted. |
| towercab-3d | `pnpm install` | 19 runs, median 15.5 s: installs into this checkout's `node_modules` for the local runs. |
| towercab-3d | `cargo clippy`, `node scripts/shipping/build/vnas.js` | Local median 9.0 s and 21.5 s against 47.2 s routing adds to `cargo clippy` (routed median 56.2 s): the repo's warm local build beats the push. |
| towercab-3d | `pnpm vitest`, `pnpm run check` | Local medians 15.3 s and 18.0 s against the 17.8 s routing adds to `pnpm run typecheck` (routed median 35.8 s). |
| godot-mcp | `pwsh .../install.ps1`, `dotnet publish` | An install of the server and skills into this PC's folders (5 runs) and the publish that feeds it into folders outside the worktree (33 runs, median 4.4 s). |
| godot-mcp | `pwsh .../suspend-rig.ps1`, `dotnet run` (`.tmp/rig290`) | Rigs that freeze or time this PC's own processes. |
| yaat, yamv, cc-422-tools | `pwsh .../load-burn.ps1`, `pwsh tools/load-burn/test-load-burn.ps1`, `pwsh .tmp/rig/watch-run.ps1`, `pwsh .tmp/repeat646.ps1` | Load and repeat rigs measuring this PC. |
| cc-484, yamv, ops-tower | `wsl Ubuntu` | The command runs in this PC's WSL distro, in its home folder (a home-folder clone of yamv, the runner's `ci-local`). |
| yaat, yaat-server, yamv | test executables run directly (`yaat.client.tests …`, `yaat.sim.tests …`, `yaat.client.ui.tests …`, `yaat.crcdriverhook.tests`, `yamv.core.tests …`), `dotnet <dll>`, `probe`, `dotnet msbuild`, `dotnet restore` | The built test hosts and probes of this checkout's `bin`, run on the build the PC made; `dotnet restore` (median 3 s) and `msbuild -getItem` (median 0.6 s) are shorter than any routing overhead. |
| collie | `bun run`, `bun install`, `bun` | Medians 0.3-3.8 s; `bun` is run by absolute path to this PC's install. |
| delve-the-dungeon | `dotnet msbuild`, `dotnet run`, `dotnet publish` | `msbuild -getItem` queries (38 runs, median 0.6 s), bundle and probe tools reading `.tmp` files (17 runs, median 9.3 s), and a publish into `.tmp`. |
| yamv | `node tools/dev/page-check/check.mjs`, `npm`, `pwsh scripts/publish.ps1`, `pwsh -Command`, `cmd /c` | Medians 1.1-22 s, below yamv's 30 s overhead; the publish produces the PC's own installers. |
| ops-tower | `pwsh -Command` (`cargo doc`), `cargo build` (`fixture_server`), `cargo nextest --test http` | `cargo doc` median 8.3 s; the fixture server the PC's browser tests run; the http pair tests listen on this PC's interfaces (`.gate/local.toml` already keeps `cargo nextest run *--test http*`, 36 local runs). |
| scratch runs | rustling-tulip `pwsh .tmp/*.ps1`, `pwsh -Command`; gate `bash -c`, `pwsh -Command`; towercab-3d `uv run python .tmp/*.py`, `bash .tmp/mut/run.sh`, `pwsh .../gate-49.ps1`, `uv run python …/extract-airport-buildings.py`, `…/build_media.py`; towercab-3d-vnas, tc3d-227-mut `cargo test`, `cargo clippy`, `cargo doc`; yaat `pwsh -Command`, `pwsh .tmp/sids.ps1`, `bash …/rebase-additive-loop.sh`, `uv run python …/mcp_probe.py`; in-the-sky `bash …/pr68-build-test.sh`, `bash -c`; ue-mcp `pwsh -Command`; redcheck, c2copy `dotnet test`; `.claude` `bash …/lint.sh` | One-off scripts and copies of a repo outside any worktree the router knows: each is sorted with the command it wraps, and none recurs enough to book. The two data-generation scripts (towercab-3d, 2 runs each, median 1,291 s and 309 s) write the repo's assets, which would need outputs: rerun the census before booking them. |

## Bookings

- yaat `pwsh tools/test-all.ps1`, `pwsh .../test-all.ps1`: GATE-833 (added)
- rustling-tulip `pwsh .../rt.ps1`, `pwsh ./rt.ps1` (`test`), `cargo check`: GATE-834 (added)
- towercab-3d `cargo test`: GATE-835 (added)
- race-anywhere `uv run pytest`: GATE-837 (added)
- claude-config worktrees' `uv run` families: GATE-404 (commented)
- godot-mcp `uv run pytest`, `dotnet test`: GATE-836 (added)
- godot-mcp desktop routing capacity (`pwsh .../hidden-desktop.ps1`, `pwsh .../run.ps1`): GATE-844 (added)
- godot-mcp `dotnet build` `--local` after GATE-557: GATE-838 (added)
- rustling-tulip native app runs (`rt.ps1 native-e2e`, `cargo run`, `cargo build`): GATE-736 (commented)
- race-anywhere Unreal editor builds and towercab-3d Tauri build (outputs the PC launches): GATE-696 (commented)
- race-anywhere Unreal game and editor runs (desktop route with a GPU): GATE-839 (added)
- towercab-3d vite build (`dist/` as outputs): GATE-840 (added)
- yaat `dotnet run` (tools) and `dotnet test` (`--local` probes): GATE-841 (added)
- yaat `env YAAT_MENU_GOLDEN_REGENERATE=1`: GATE-842 (added)
- delve-the-dungeon `godot_console --import`: GATE-439 (commented)
- delve-the-dungeon `dotnet build` (`gitignored input`): GATE-439 (commented)
- the journal gap (archives keep only `release` lines): GATE-843 (added)
