---
name: route-local-runs
description: Use when the local-run watch reports pending shapes, when a gate session starts and arms its watches, when a heavy gate ran on this PC that looks as if it should have routed, or when asked why a command ran locally or to sweep or fix local heavy runs.
---

# Route local runs

A gate session keeps a background watch on the journal. When a new kind of heavy gate runs on this PC for 60 s or more, the watch ends and reports it, and the session decides that **shape** at once: routes it, books what blocks it, or records why it stays local. The owner is not asked; each `remote.json` change is listed for them afterwards.

The weekly `local-run-census` sorts whole families. This skill works on exact commands as they appear, and shares the census's three states.

## Arm the watch

From the main checkout, as a background command (`run_in_background`), beside the inbox watch:

```
uv run --no-project python -I .claude/skills/route-local-runs/watch_local.py watch
```

It exits 0 with a report when something is pending, and 1 after 8 hours with nothing pending; re-arm it after either, once the report is handled. A shape stays in every report until it is marked, so decide them all before re-arming. `scan` takes the same options and reports at once without waiting. State is `.tmp/local-runs-state.json`; losing it only means shapes are looked at again.

## Decide each pending shape

Read the report largest first. In it, `seconds` sums the shape's runs in this report; `routed` counts this repo's routed runs of the same family; `matched` is `no`, `since` (a pattern added after the run) or `yes` (a pattern was in force and the run stayed here anyway).

Take the first row that holds, then record the decision with `watch_local.py mark <id>... --verdict routed|local|booked --note "<why>"`, which takes several ids.

| # | The report shows | Decision |
|---|---|---|
| 1 | `STILL-LOCAL-AFTER-FIX` | The fix did not take. Find why, book it, mark `booked`. |
| 2 | `matched=since` | Covered now. Mark `routed`, note `pattern added since`. |
| 3 | `fellBack=` | A route failed and fell back. Book it with the reason, mark `booked`. |
| 4 | `reason=declared local` | The repo keeps it here in `.gate/local.toml`. Mark `local`, its reason there as the note. |
| 5 | `reason=--local: <text>` where the text names something no work can move off this PC | Mark `local`, the reason as the note. |
| 5a | `reason=` any other value: `--local` with a removable reason, `sibling absent`, `gitignored input`, `another worktree` | The gate said why it stayed. Book that cause, mark `booked`. |
| 5b | `matched=yes` and `reason=none` | A matching command stayed here with no reason given. Book it, mark `booked`. |
| 6 | The command drives this PC's own state: a window, a GPU session, an install into this PC's folders, a rig or measurement of its own processes, a build whose exes are started on this PC by a repo with no `outputs` for them | Mark `local`, the reason as the note. |
| 7 | The repo is `(no repo)` | No repo owns it. Mark `local`, note `no repo`. |
| 7a | The repo is not a key of `remote.json`'s `repos` | **Add** one step under GATE-192 (Remote routing for every caller repo) for the repo, unless an open one names it, mark `booked`. |
| 8 | The shape holds `.tmp/*` | Scratch work. The shape hides the script, so read the name from the journal, and add up `seconds` across every shape of one script, counting the parts of one job (`rerun_a` to `rerun_d`) as one. Under 1800, mark `local`, note `scratch`. At 1800 or more, read the script: if row 6 fits, take it; else book asking how the repo should run this job routed, mark `booked`. A script no longer on disk is judged by its arguments. |
| 9 | The clear-route test below passes | Apply the pattern, mark `routed`. |
| 10 | Anything else | Book what blocks the route, mark `booked`. |

A reason work can remove never makes a shape `local` (census, "Sort every heavy family with local runs").

**Book by where the work sits.** A change in the caller repo (a tracked wrapper script, an `inputs` or `.gate/stage.toml` declaration) is a repo inbox request to that repo. A change in the gate or a builder is a High GATE issue. One booking covers every shape with the same cause: mark them all with it.

**A GATE booking is then worked, in this session.** Booking records the cause; it does not finish the shape. Start the issue once the batch is decided, under the usual orchestrator rules, and stop only for what needs the owner or another repo.

## The clear-route test

All three must hold:

1. **The repo routes its kind.** `routed` is above 0, or `nearest` names a pattern with the same first two words; for a shell or interpreter, the same script.
2. **Nothing here needs what it makes.** It is a test, lint, check, format or analysis command, or a simulation that only prints; or it is a build, and the repo's routed builds of the same project already bring back what it makes. A script that does both counts as a build. Read the repo's `outputs` in `remote.json`. Windows outputs started on the PC must come from a Windows builder (`gate-nextup`, "What the Mac mini runs").
3. **The builder can run it.** The repo has no `.gate/local.toml` pattern for it, every tool it names is on the builder (`gate check --builder <host>`), and it reads nothing outside the worktree that `docs/routing-a-repo.md` says a repo must declare.

A repo that already routes needs no routing trial for one more pattern: the next run is the trial, and row 1 catches it.

## Apply a pattern

1. Write the narrowest pattern that covers the shape: the shape's words, each `#` as `*`, and each path separator as `?`, since a run may spell it either way.
2. Never write a pattern that matches on the tool or verb alone (`cargo build *`).
3. Run `pwsh -NoProfile -File tools/gate.ps1 remote add repos <repo> "<pattern>" --dry-run`, read the line, then run it without `--dry-run`. The tool writes its own backup.
4. Mark the shape `routed`, the pattern as the note. If the command runs here again, the report flags it `STILL-LOCAL-AFTER-FIX`.

## Report

After a batch, one short message to the owner: each pattern added (repo and pattern), each issue booked, and how many shapes were marked `local`. Say nothing when every shape was marked `local`.

The repo is public. An issue names the repo and the family, never a raw shape, since shapes hold machine paths. Never quote a host, address or user from `remote.json`.

## Common mistakes

- Marking a shape `local` because routing it needs work. That is `booked`.
- Reporting a batch of bookings and stopping. A booked GATE issue is worked next.
- Widening a pattern to catch the next variation. A new variation is a new shape, decided when it appears.
- Leaving the watch ended after a report: it does not re-arm itself.
