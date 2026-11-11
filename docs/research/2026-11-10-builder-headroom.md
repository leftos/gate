# GATE-1181: Windows builder headroom during the full and busy bursts

Measured 2026-11-10 (UTC), read-only, from the PC's gate journal (`events.jsonl`, `events.1.jsonl` and `archive\`, release lines back to 2026-10-11) and read-only queries on both builders. Scripts: [`headroom1181.py`](2026-11-10-builder-headroom/headroom1181.py), [`desktop1181.py`](2026-11-10-builder-headroom/desktop1181.py) and [`busy1181.py`](2026-11-10-builder-headroom/busy1181.py), run with `uv run --no-project python -I <script>` from the repo root over the journal of the day.

## Short answer

The Windows builder was out of CPU when the pool was genuinely full of CPU work, but most of the time the pool was full, it was full of desktop runs that were queued and doing nothing.

- At 10 active heavy runs the builder leaves a heavy run a median 10% of its cores and runs it 2.5 times slower than its own median. Throughput stops growing at about 7 active runs.
- On 2026-11-10, when the PC's 10 Windows slots were all held, a mean of 5.0 were held by godot-mcp `run.ps1 itest` runs. These take the builder's single desktop lock, so about 4.0 of them were waiting for the desktop while holding a remote slot.
- All 26 `busy` fallbacks on 2026-11-10 had another routed run of the same work tree on a builder at the time. That is contention for the work folder's lock, decided mostly in the PC's own key-lock wait (`Run::take_key`), which a queued desktop run held, not capacity. Neither more slots nor a longer wait fixes it.
- The Mac sat almost idle: on 2026-11-10 it held a mean of 0.64 of its 6 slots.

**Recommendation: option 3. Do not raise `slots`.** Separately, book the desktop-lock queue as its own fix (see the end).

## Hardware

| | Windows builder | Mac |
|---|---|---|
| CPU | AMD Ryzen 9 9950X, 16 cores / 32 logical | Apple M5 Pro, 15 logical (`hw.perflevel0.logicalcpu` 5, `hw.perflevel1.logicalcpu` 10) |
| RAM | 61.4 GB, plus a 12 GB page file | 24 GB |
| Remote slots (`remote.json`) | 10 | 6 |
| Live snapshot (about 23:55 UTC, not during a burst) | 17.8 to 27.8 GB free over two reads; 43% CPU | load average 4.74 / 4.93 / 10.19 |

## What the journal says about builder load

A routed run's `release` line carries `adjustedSeconds`, the load-adjusted seconds the box counted for the command, and `phases.command`, the run call's wall time.

Their ratio is the run's mean **free share** on the box. For a heavy run it is `clamp((cores − cores other processes held) / cores, 0.05, 1)`, measured by the box's own sampler (design.md, the free-share rule). It is the builder's CPU headroom as the box saw it, other gate runs included. 1.0 means the other processes left the box idle; 0.10 means they held 90% of the cores.

**Slowdown** is a run's `phases.command` divided by the median for the same repo and the same sent words on the same builder. The median comes from passed runs outside every burst window, with at least 3 samples and a median of at least 5 s.

**Slots held** is reconstructed from the release lines: each routed run held its remote slot from `time − heldSeconds` to `time`.

### CPU against active runs

This covers non-desktop heavy runs on the Windows builder since 2026-11-01. They are bucketed by the number of runs active at each run's midpoint: the non-desktop runs, plus at most one desktop-locked run, since only one can hold the desktop.

| active runs | runs | median free share | p25 free share | median slowdown | throughput (active ÷ slowdown) |
|---|---|---|---|---|---|
| 1 | 2,107 | 0.89 | 0.84 | 0.88 | 1.1 |
| 2 | 4,079 | 0.79 | 0.67 | 0.95 | 2.1 |
| 3 | 5,074 | 0.67 | 0.47 | 1.01 | 3.0 |
| 4 | 4,632 | 0.54 | 0.33 | 1.09 | 3.7 |
| 5 | 3,625 | 0.42 | 0.22 | 1.20 | 4.2 |
| 6 | 2,383 | 0.32 | 0.16 | 1.36 | 4.4 |
| 7 | 1,531 | 0.25 | 0.12 | 1.43 | 4.9 |
| 8 | 916 | 0.18 | 0.09 | 1.91 | 4.2 |
| 9 | 461 | 0.16 | 0.08 | 1.99 | 4.5 |
| 10 | 354 | 0.10 | 0.06 | 2.53 | 4.0 |

The builder's useful output levels off at about 4 to 5 "unloaded runs' worth" from 6 to 7 active heavy runs. Each run past that slows every other run without finishing work any sooner.

The throughput column is rough, because the command mix differs between buckets. Still, the free-share column falls steadily and has no knee, so the box has no idle CPU left at 10.

## The bursts on 2026-11-09 and 2026-11-10

A burst is a cluster of `full` or `busy` fallbacks less than 10 minutes apart. The window runs from the first fallback's start to the last one's end.

**Desktop** counts the godot-mcp `pwsh -NoProfile -File run.ps1 itest[ -*]` runs that held slots. These match the `patterns` (locked) list in godot-mcp's `.gate/desktop.toml`, and only one at a time can hold the builder's desktop.

**Queued** is the mean count beyond the first. **Free share** covers the burst's non-desktop heavy runs, weighted by command time.

| burst (UTC) | fallbacks | Win held mean | desktop held mean (max) | of those queued | other runs held | Mac held mean (max) of 6 | free share | median slowdown (n) |
|---|---|---|---|---|---|---|---|---|
| 11-09 18:31–18:33 | 2 full | 9.3 | 0.8 (1) | 0.0 | 8.5 | 0.0 (0) | 0.20 | 3.56 (7) |
| 11-10 03:24–03:27 | 1 busy | 5.2 | 2.2 (3) | 1.2 | 3.1 | 0.8 (1) | 0.44 | 1.16 (9) |
| 11-10 04:23–04:29 | 2 busy | 6.7 | 2.9 (3) | 1.9 | 3.8 | 0.0 (0) | 0.48 | 1.32 (25) |
| 11-10 05:12–05:16 | 1 busy | 7.5 | 3.4 (5) | 2.4 | 4.2 | 0.0 (0) | 0.64 | 1.28 (15) |
| 11-10 05:43–05:47 | 1 busy | 4.5 | 2.5 (3) | 1.5 | 2.0 | 0.0 (0) | 0.62 | 1.22 (9) |
| 11-10 07:42–07:46 | 1 busy | 8.0 | 3.2 (4) | 2.2 | 4.8 | 0.3 (1) | 0.33 | 1.62 (15) |
| 11-10 08:08–08:11 | 1 busy | 3.4 | 3.0 (3) | 2.0 | 0.4 | 0.0 (0) | 0.75 | 0.92 (5) |
| 11-10 08:26–08:37 | 3 busy | 3.7 | 2.3 (3) | 1.3 | 1.4 | 1.5 (2) | 0.57 | 1.17 (7) |
| 11-10 08:49–09:16 | 5 busy | 3.3 | 2.9 (4) | 1.9 | 0.4 | 0.1 (1) | 0.87 | 0.87 (24) |
| 11-10 16:02–16:06 | 3 full | 10.0 | 2.9 (3) | 1.9 | 7.1 | 3.0 (4) | 0.24 | 2.42 (8) |
| 11-10 16:27–17:21 | 12 full, 8 busy | 8.4 | 6.0 (8) | 5.0 | 2.4 | 0.2 (1) | 0.47 | 1.21 (92) |
| 11-10 19:09–19:11 | 1 busy | 3.6 | 0.0 (0) | 0.0 | 3.6 | 2.0 (2) | 0.76 | 1.35 (1) |
| 11-10 20:19–20:22 | 1 busy | 4.5 | 3.0 (3) | 2.0 | 1.5 | 0.0 (0) | 0.48 | 1.24 (8) |
| 11-10 20:38–20:44 | 1 busy | 3.1 | 0.0 (0) | 0.0 | 3.1 | 0.1 (1) | 0.43 | 1.09 (10) |
| 11-10 21:04–21:13 | 1 busy | 7.3 | 1.0 (1) | 0.0 | 6.3 | 0.5 (2) | 0.28 | 2.35 (33) |

Notes:

- The free-share column comes from `desktop1181.py`, which leaves desktop runs out. The slowdown column comes from `headroom1181.py`, which includes them. godot-mcp `run.ps1 itest` runs show slowdowns of 5 to 33 times with free shares of 0.01 to 0.1. That is time queued for the desktop, not CPU starvation, so the medians above lean high in desktop-heavy windows.
- The desktop wait is confirmed from two godot-mcp logs, `X:\dev\godot-mcp\.tmp\rerun-scratchrunner-main.txt` and `X:\dev\godot-mcp\.tmp\rerun-526-1.txt`. Each says `gate: remote waiting for the builder's desktop: held by godot-mcp ...`, and its `command` phase (171 s, 114 s) is well over the inner steps' own times (61 s, 89 s). So a `run.ps1 itest` run holds its PC remote slot, the PC's key lock on its work folder and the builder's work lock while it waits for the desktop.

### Who held the Windows slots

Mean slots held over the window:

| burst | holders |
|---|---|
| 11-09 18:31 | rustling-tulip 4.4 (cargo nextest 3.6), world-of-intrigue 3.9 (dotnet format/build, uv run), career-driver 0.9 |
| 11-10 16:02 | godot-mcp 3.5 (`run.ps1 itest` 2.9), rustling-tulip 3.0 (nextest 2.0, clippy 1.1), career-driver 1.1, gate 1.0 (nextest), delve-the-dungeon 0.9, world-of-intrigue 0.5 |
| 11-10 16:27 | godot-mcp 5.8 (`run.ps1 itest` 5.6), rustling-tulip 0.9, career-driver 0.9 (Build-GodotTemplate 0.4), world-of-intrigue 0.3, gate 0.2, yaat 0.2, delve-the-dungeon 0.2 |
| 11-10 21:03 | godot-mcp 1.8, yaat 1.8 (dotnet test 1.1, build 0.7), career-driver 1.2, gate 0.9, world-of-intrigue 0.8, rustling-tulip 0.7 |
| the busy-only bursts | godot-mcp 2.4 to 4.6, almost all `run.ps1 itest`, plus a mix of dotnet build/format from career-driver, world-of-intrigue, delve-the-dungeon and yaat |

### Reading the bursts

- **Genuine CPU saturation.** In three windows the slots were full of CPU work and the box had no headroom: 11-09 18:31 (8.5 non-desktop runs, free share 0.20, builds 3.5 to 9.8 times slower), 11-10 16:02 (7.1 plus one desktop holder, free share 0.24, median slowdown 2.4) and 11-10 21:03 (6.3, free share 0.28, median slowdown 2.35). Raising `slots` there would only have split the same cores more ways.
- **Slots full of the desktop queue.** The biggest burst was 11-10 16:27–17:21, with 12 of the day's 15 `full` fallbacks and 2,253 s run here. In that window a mean of 6.0 slots (up to 8) were held by `run.ps1 itest` runs, 5.0 of them queued behind the desktop. Only 2.4 other runs were active, and the box left them 0.47 of its cores. Across the whole of 11-10 (06:00–24:00 UTC), in the moments all 10 slots were held, a mean of 5.0 were desktop runs and 4.0 of those were queued.
- **`busy` is not capacity.** All 26 `busy` fallbacks on 11-10 (833 s run here) had another routed run of the same work tree on a builder at the time (`busy1181.py`). The `busy` fallbacks are mostly decided on the PC, in the key-lock wait (`Run::take_key`), where a run waits behind another run of the same tree that holds the key; the builder's own work lock is a second guard that a run reaches only once it holds the key.

  22 of the 26 were godot-mcp, almost all colliding with that tree's own `run.ps1 itest`, which holds the key lock, and the work lock, while it waits for the desktop. Most of them waited 180 to 300 s before running here, against the 60 s remote wait.

## Day counts

From release lines' `fellBack`:

| day | full | full s here | busy | busy s here | Win share of time at 10 held | Win mean held | Mac mean held of 6 |
|---|---|---|---|---|---|---|---|
| 11-01 | 0 | 0 | 5 | 2,481 | 0.0% | 1.85 | 0.40 |
| 11-02 | 0 | 0 | 1 | 812 | 0.0% | 1.22 | 0.38 |
| 11-03 | 0 | 0 | 2 | 152 | 0.3% | 2.42 | 0.07 |
| 11-04 | 2 | 101 | 1 | 1 | 0.3% | 2.16 | 0.74 |
| 11-05 | 0 | 0 | 1 | 34 | 0.4% | 2.40 | 0.28 |
| 11-06 | 1 | 51 | 2 | 21 | 1.3% | 2.34 | 0.65 |
| 11-07 | 0 | 0 | 1 | 11 | 0.0% | 0.56 | 0.00 |
| 11-08 | 0 | 0 | 1 | 12 | 0.0% | 0.58 | 0.01 |
| 11-09 | 2 | 94 | 0 | 0 | 1.7% | 2.10 | 0.12 |
| 11-10 | 15 | 2,253 | 26 | 833 | 3.8% | 3.40 | 0.64 |

The time shares and means are sampled every 30 s from 06:00 to 24:00 UTC. The 11-10 figures run to about 23:59 UTC, so they are higher than the issue's first comment, which was counted earlier in the day.

## The Mac's idle share

- On 11-10 the Mac held a mean of 0.64 of 6 slots and had 3 or more free 98% of the time. It never had all 6 held that day.
- During the Windows `full` bursts it held 0.0 (11-09 18:31), 3.0 (11-10 16:02) and 0.2 (11-10 16:27) on average.
- Its heavy runs since 11-01 had a median free share of 0.64.
- Its routed set is narrow. `dotnet test *` reaches it only for in-the-sky, in-the-sky-ops, opening-hand, yaat and yaat-server. delve-the-dungeon reaches it for format and csharpier only, and career-driver for Blender jobs only.
- None of the commands that fell back `full` (godot-mcp dotnet build/test, career-driver dotnet build, delve-the-dungeon dotnet test, world-of-intrigue dotnet build/test, gate cargo clippy/nextest, rustling-tulip cargo nextest) has a Mac pattern.

## Builder-side history

- The journal holds no memory figure for routed runs; release lines carry CPU only.
- The Windows builder has no running Data Collector Set (`logman query` lists none), so it keeps no counter history.
- In its System log, the Resource-Exhaustion-Detector has no events since 11-08, and there are no error or warning events at all since 11-09: no low-memory or commit-limit warnings during the bursts.
- Its Application log since 11-09 holds app crashes only: Godot.exe, rustling-tulip-native.exe and python.exe access violations or stack-buffer overruns, plus two per-process `RADAR_PRE_LEAK_64` reports (wasm-opt.exe 11-10 04:50, Intrigue.Content.Tests.exe 11-09 08:10). None is a system memory event.
- The page file's peak use since the 10-28 boot is 8.8 GB of 12 GB. So memory was tight at least once in that time, but nothing dates it.
- The SRUM database (`C:\Windows\System32\sru\SRUDB.dat`) exists and would hold hourly per-process CPU and memory. Reading it needs an administrator copy of a locked file, so I did not read it.

## What I could not measure

- **Memory during the bursts.** Nothing on either side records it with a time. All I can say is that the System log shows no memory pressure since 11-09.
- **The box's own CPU outside gate runs.** The free share counts every other process, but only while some run's command is sampling.
- **Runs without a release line** (still running at the journal's end, or killed without release) are missing from slot occupancy. The archive keeps only `release` events, so `fallback` and `take` lines from before 11-10 14:42 local are gone.
- **Desktop classification** uses each repo's `.gate/desktop.toml` locked `patterns`. A desktop run's queue time is inferred from its `command` phase, which includes the wait; the two logs above confirm it, but there is no per-run figure.
- **Whether the Mac can run the other repos' `dotnet test`** (Godot or Windows-only test dependencies) is not checked here.

## Recommendation

Pick **option 3: move more repos' `dotnet test *`, and their `dotnet build *`, to the Mac.**

- The Windows builder is CPU-bound whenever its slots are really full of work: free share 0.10 and slowdown 2.5 at 10 active runs. Its throughput is flat from about 7 active runs.
- The Mac sat at a mean of 0.64 of 6 slots on 11-10, including through the bursts.
- The repos whose runs fell back `full` and that saturated the box in its three CPU-bound windows have no Mac patterns: world-of-intrigue, career-driver, delve-the-dungeon dotnet build/test, and rustling-tulip and gate cargo. The dotnet ones are the candidates, after checking each repo's tests run on macOS.

**Option 1: do not raise `slots`.** The CPU table says 10 is already past the knee. Raising it would let in more CPU work that slows everything else, and in the desktop-queue bursts the extra slots would fill with more queued `itest` runs.

**Option 2: a longer remote wait helps little.** In the main burst, slots freed only as godot-mcp `itest` runs finished, and those take about 1,000 s each. The fallbacks were mostly runs of 4 to 170 s. Waiting minutes for a slot would often cost more than running here.

**Outside the three options, the biggest lever is the desktop queue.**

- Only one locked desktop run can use the builder at a time, but each queued one holds a PC remote slot and the PC's key lock on its work folder (and the builder's work lock) for its whole wait. That was 4 to 5 of 10 slots in the moments the pool was full on 11-10, and it caused most of the 26 `busy` fallbacks.
- Two possible fixes, which are a design choice for the issue: the PC holds at most one remote slot for locked desktop runs per builder, and queues the rest locally before taking a slot; or a run waiting for the desktop holds no slot until it gets it.
- Moving godot-mcp's `run.ps1 itest` to a `hidden` desktop, as its `itest-csharp` already is, would also remove the queue, if its tests allow.
