# Routing a repo

How a caller repo's heavy gates come to run on a builder: what goes in `remote.json`, how a routing trial runs, and what stays on the PC. The format of `remote.json` and the router's rules are in [design.md](design.md#remotejson); this page is the procedure.

## Which repos and which commands

Every repo that runs heavy gates is in scope, and every heavy command family in the gate journal ends in one of three states:

- it routes;
- it is booked with what blocks it (a GATE issue, High when it blocks the repo's adoption);
- it stays local, but only for one of two reasons, named beside the family. Either it cannot run anywhere but this PC (a WSL distro on `/mnt/x`, an install into this PC's own folders, a git operation that writes this checkout's refs: any command whose first word is `git` or `prek`, a `git commit` included, stays local silently whatever its hooks need), or its run time is measured as shorter than what routing adds: the push, the outputs, the compression and the call overhead, taken from the repo's routed runs in the journal.

  A reason that work can remove does not keep a family local. It is a blocker to book. Examples: a tool or SDK the builder lacks, a window or GL the desktop route can serve, a `--local` with no recorded cause, an outputs list that would need to grow.

Coverage is judged from the journal's history of gated commands per repo, not from the commands guessed to matter. Light-slot runs never route (the router considers heavy runs only), so a repo whose gates are all light, short checks stays local without an entry.

A caller never adds `--local` (`-Local`) to a gate unless that command has been tried routed and failed. It then reruns locally only what failed and always reports the failure to the gate repo's inbox with the command, the gate's lines and the log path. The gate repo fixes the routing or rules that the command stays local, and a command ruled local is kept local by the router, so callers do not carry the flag. Every `--local` run is journalled as `notRouted: --local` on its `queue` event, which is how stray uses are found.

A command run from one worktree that names a path in another worktree of a listed repo stays local, saying `gate: not routed: the command works in <tree>; run it with --cd <tree>`. Run it again with `--cd <tree>` to route it from that worktree ([design.md, Routing](design.md#routing)). A path in an unlisted repo, on a network drive or in no worktree keeps the plain `is outside the worktree` line.

## Adding a repo

1. Read the repo's own gate commands (its CLAUDE.md, nextup profile, justfile or package scripts) and the tools each needs: toolchains and their pins (`.gate/toolchain.toml`, `rust-toolchain.toml`, `global.json`), shells and utilities, package managers.
2. Set the builder up. A session installs what a routed repo needs, at the repo's pinned version, and changes the builder's configuration (PATH, ACLs, environment) directly over ssh. After each change it sends claude-config a repo inbox note with what changed and the exact commands, so `tools/build-box/install-toolchains.ps1` follows and a new builder gets the same setup. Run `gate check --builder` from the repo.
3. Back up `remote.json` beside it (`remote.json.bak-<UTC stamp>`), then add the repo's patterns under `repos.<repo>`, plus `outputs`, `inputs`, `siblings`, `desktop` or `warmup` consent where the repo needs them. Re-read the file just before writing and change only the repo's own keys: other sessions edit it too.
4. Run one routing trial (below). The repo counts as routed once its trial passes.

## The routing trial

A trial runs the repo's full command routed, once, and assumes a local run passes: it never runs the full command locally to compare. If the routed run fails, only the failing steps or tests are rerun locally, with the narrowest command that reproduces them:

- **A step that fails locally too** is the repo's own failure. The pattern stays routed, since routed and local agree, and the failure goes to the repo as an inbox note.
- **A step that passes locally but fails routed** is a routing failure. It is fixed in the builder's setup and that step alone is retried routed, once. If it still fails, the repo's patterns come back out of `remote.json` and the failure is booked as a GATE issue.

A first routed run in a new work folder is cold: the gate raises its ceiling to `min(2 x ceiling, ceiling + 1800 s)` and says so, so the trial still wants a `--timeout` near what a cold build takes, since the raise is at most half an hour more; the repo's ordinary routed runs keep their usual ceilings. Run one heavy trial at a time.

## A repo with a command that needs the builder's desktop

A repo with a routed command that needs the builder's logged-on desktop (a GUI test run) declares its `-like` patterns in a tracked `.gate/desktop.toml`, `patterns = ["dotnet test *Ui*"]`, at most 32, matched against the same rewritten words as the repo's `repos` patterns. The PC consents in `remote.json`: `"desktop": {"<repo>": ["dotnet test *Ui*"]}`, each pattern listed exactly as the file spells it, letter case included. The first declared pattern a command matches decides,

Without the consent the command runs here with `gate: not routed: desktop pattern '<pattern>' not allowed in remote.json`.

A `.gate/desktop.toml` the gate cannot use keeps every routed command of the repo here with `gate: not routed: .gate/desktop.toml: <problem>`, so fix it before relying on routing. A desktop run holds a remote slot while it waits for the desktop, up to the usual 60 s remote wait. Format and rules: [design.md](design.md#remotejson).

## A repo whose git hooks run heavy checks

A heavy gate started inside a git hook stays local silently unless `remote.json` names its repo in `hooks`, an array of repo names matched in any letter case: `"hooks": ["<repo>"]`. A value that is not an array of non-empty strings consents to no repo, and the rest of the file is still read ([design.md, remote.json](design.md#remotejson)).

The key is for check-only hook commands (a build, clippy, a format check). A routed fixer's patch comes back to the worktree under prek's stash, so a fixing hook stays local.

The hook entry calls the gate itself, `gate run ... -- <heavy command>`, and the repo's patterns must match that command. The `git commit` that runs the hook is gated on `--slot none`, so it does not cover its hook's gate and the hook's gate takes its own slot (and routes). The `git commit` itself stays local.

## A repo whose short runs a cold folder would push past their ceiling

A repo whose short routed runs (a filtered test of a minute) would meet a cold builder work folder declares warm-up builds in a tracked `.gate/warmup.toml`, and `remote.json` consents to it under `warmup` ([design.md, gate warm](design.md#gate-warm)). Until the folder is warm, a matched run stays here and starts the warm-up in the background; the runs after it route.

## Taking a builder out for maintenance

A reboot, an install that needs one, or any other work that would break a run on the builder starts by draining it, never by waiting for it to go idle while it stays in `remote.json`: new runs keep arriving, so that wait has no end.

1. Back up `remote.json` beside it (`remote.json.bak-<UTC stamp>`) and save the builder's exact entry. Then, in one quick read-modify-write, take the builder out: remove it from `hosts`, or remove `host` when it is the only builder. With no `host` left, every gate on the PC runs here and says the routing file is unusable, which is expected while the builder is out.
2. Wait for the runs already on the builder to end. No `gate.exe` box process and no routed command may still be live under the builder user, besides your own ssh session. There is no cutoff, since each routed run ends at its own ceiling. A run still live past the longest routed ceiling is looked into, never cut off by the reboot.
3. Do the work. After a reboot, confirm that the console session logged back on (autologon), which the desktop route needs.
4. Put the builder back with the saved entry, in a read-modify-write that keeps every other key as it stands then, and check it with `gate check --builder <host>`. If anything blocks the work partway, put the builder back first, so routing never stays off.
