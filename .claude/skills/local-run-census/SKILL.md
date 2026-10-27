---
name: local-run-census
description: Use when reviewing which gate runs on this PC over a window (the last week by default) could move to the builders, when re-checking a repo's routing coverage after its trial, or when asked which heavy command families still run locally and why.
---

# Local-run census

Every heavy command family in the gate journal ends in one of three states ([routing-a-repo.md](../../../docs/routing-a-repo.md#which-repos-and-which-commands)): it routes, it is booked with what blocks it, or it stays local for a reason named beside it. The census reads the journal for a window, puts every heavy family with local runs into one of those states with its evidence, and books each finding.

## Run the query

```
uv run --no-project python -I .claude/skills/local-run-census/census.py [--days N | --since ISO] [--journal DIR] [--json OUT]
```

Run it through the launcher (`-Slot light -TimeoutSeconds 600`) and always pass `--json`: the gate prints only the tail of a long report.

Rebuild the Markdown from the JSON with `uv run --no-project python -I -c "import json, pathlib, sys; sys.path.insert(0, '.claude/skills/local-run-census'); import census; pathlib.Path('.tmp/census.md').write_text(census.render(json.loads(pathlib.Path('.tmp/census.json').read_text(encoding='utf-8'))), encoding='utf-8', newline='\n')"` from the repo root.

The default window is the last 7 days; `--journal` defaults to `%LOCALAPPDATA%\gate\v1`. Tests: `uv run --no-project python -I -m unittest discover -s .claude/skills/local-run-census -p "test_*.py"`.

What the script counts:

- **A run** is the lines sharing one `(pid, processStart)`. Its kind, repo and command come from its `queue` line, else from its `release` (every line carries the common fields, and the archive keeps only `release` lines). A run is **routed** when its `release` has `remote: true`, **local** when its `release` has no `remote`, and **unfinished** when it has no `release`; unfinished runs are left out of the time figures. A `fallback` reason counts against the run's family.
- **The repo**: a `<repo>.wt` segment of `cwd` names `<repo>`, and a `.claude-wt` segment names `claude-config`; else the main checkout's folder name from `git rev-parse --git-common-dir` (a submodule's names its superproject, and `.claude` is `claude-config`). A folder git names no repo for is `(no repo)`, and a folder that is gone is `(gone)`: a folder's own name is never printed.
- **Left out**: runs whose `cwd` holds `gate-selftest`, and heavy runs lent or covered by a parent's slot with no `release` of their own (the footer counts them).
- **The window** keeps a run by its `release` time, else its `queue` time: every line is read first, so a run queued before the window and released in it counts.
- **The family**: the program (file name, lower-cased, `.exe`/`.cmd`/`.bat` stripped) plus, for a shell or interpreter (`pwsh`, `powershell`, `bash`, `sh`, `python`, `py`, `node`, `cmd`), its first script word, else its `-c`/`-Command`/`/c`.

  For `uv`, `uv run` plus its target (with the script rule when the target is `python`); for `cargo`, the subcommand after any `+toolchain`; for any other program, its first word that is no flag, path or file name. An absolute script path shows as `.../<name>`.
- **Per (repo, family), heavy runs only**: local, routed and unfinished counts; total and median local `wallSeconds`; median routed `wallSeconds`; local verdicts; `notRouted` values; fallback reasons. Sorted by total local seconds.
- **The `--local` runs** of every kind, by repo, family and `reason`.

The report names repos and families only: it never prints a `cwd`, `log`, `host` or `key`, and free-text reasons have absolute paths replaced by `<path>`. The repo is public, so keep it that way in the note.

**The archive keeps only `release` lines.** `notRouted`, the fallback reasons and the `--local` table cover only the runs whose `queue` is still in `events.jsonl` or `events.1.jsonl`; the report's last line gives that start. A family's older local runs therefore show no reason: judge them from their commands, not from an empty `notRouted` cell.

## Sort every heavy family with local runs

Read `remote.json` (`%LOCALAPPDATA%\gate\remote.json`) and each repo's `.gate/local.toml` under its main checkout, read-only, and compare the local runs' commands with the repo's patterns. Never quote a host, address or user from `remote.json`.

| State | Evidence that puts a family there | The note names |
|---|---|---|
| **ready to route** | No `notRouted` reason left, or one a config change removes: a `remote.json` pattern the repo lacks, an `outputs` or `inputs` glob. A family that already routes and whose local runs carry no reason is here too, with no change. | The config change, or "routes already". |
| **blocked on work** | A reason work can remove: a tool or SDK the builder lacks, a fallback reason that repeats, an outputs list that would need to grow, a `--local` reason showing a routing gap, a window or GL the desktop route can serve. | The blocker in one line, with the run count. |
| **truly local** | The command names this PC's own state (its WSL state, an install into this PC's folders, a rig or measurement of this PC's own processes, a `git` or `prek` command writing this checkout's refs), or its local median is shorter than what routing adds. | The reason. For the time case: the same repo's routed median minus local median of a comparable family, and which family. |

A reason work can remove never makes a family truly local: it is a blocker to book. A one-off scratch command (a `.tmp/` script, a probe project) is sorted like the command it wraps, and needs no booking when nothing in the repo runs it again.

## Book each finding

| State | Booked as |
|---|---|
| ready to route | The `remote.json` change (back the file up first, change only the repo's keys, as routing-a-repo.md says), or an Urgent or High step under GATE-192. |
| blocked on work | A GATE issue, High, under the gate repo's focus fence, citing the runs (repo, family, counts, window). |
| truly local | The repo's `.gate/local.toml` pattern, or a row in routing-a-repo.md's table, with the reason. `git` and `prek` need neither: they stay local silently. |

## The note

`docs/research/YYYY-MM-DD-local-run-census.md`, holding:

1. The window and what the journal held for it (the report's closing lines).
2. The heavy-family table and the `--local` table, as the script printed them.
3. `## Sort`: every heavy family with local runs, its state and its evidence in one line. Families that share one state and one reason may share a row that names each of them.
4. `## Bookings`: each booking made (issue, config change or `local.toml` line), filled by whoever books.

## Common mistakes

- Reading an empty `notRouted` cell as "no reason": for runs older than the live journal the reason is not kept.
- Comparing a local median with the routed median of a different repo: routing overhead differs per repo (push size, outputs, builder).
- Sorting a whole family by its routed runs: a family that routes can still hold a local sub-command (a `--no-run` rig build, a Windows-only project) that needs its own state.
- Copying a raw command into the note: commands hold machine paths; use the family and describe the rest.
