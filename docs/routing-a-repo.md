# Routing a repo

How a caller repo's heavy gates come to run on a builder: what goes in `remote.json`, how a routing trial runs, and what stays on the PC. The format of `remote.json` and the router's rules are in [design.md](design.md#remotejson); this page is the procedure.

## Which repos and which commands

Every repo that runs heavy gates is in scope, and every heavy command family in the gate journal ends in one of three states:

- it routes;
- it is booked with what blocks it (a GATE issue, High when it blocks the repo's adoption);
- it stays local, but only for one of two reasons, named beside the family.

  Either it cannot run anywhere but this PC (a command that names this PC's own WSL state, such as a `/mnt/x` path or a distro folder only this PC has, an install into this PC's own folders, a git operation that writes this checkout's refs: any command whose first word is `git` or `prek`, a `git commit` included, stays local silently whatever its hooks need), or its run time is measured as shorter than what routing adds: the push, the outputs, the compression and the call overhead, taken from the repo's routed runs in the journal.

  A reason that work can remove does not keep a family local. It is a blocker to book. Examples: a tool or SDK the builder lacks, a window or GL the desktop route can serve, a `--local` whose recorded reason shows a routing gap, an outputs list that would need to grow.

Coverage is judged from the journal's history of gated commands per repo, not from the commands guessed to matter. Light-slot runs never route (the router considers heavy runs only), so a repo whose gates are all light, short checks stays local without an entry.

A caller never adds `--local` (`-Local`) to a gate unless that command has been tried routed and failed. It then reruns locally only what failed and always reports the failure to the gate repo's inbox with the command, the gate's lines and the log path.

The gate repo fixes the routing or rules that the command stays local, and a command ruled local is kept local by the router through the repo's `.gate/local.toml` ([A repo with a command that stays on this PC](#a-repo-with-a-command-that-stays-on-this-pc)), so callers do not carry the flag.

Every `--local` run must give `--reason "<why>"` (the gate refuses it otherwise). A heavy one is journalled as `notRouted: --local` with the reason on its `queue` event, which is how stray uses are found (a command kept here by `.gate/local.toml` is journalled `notRouted: declared local`, never `--local`); a light one writes the reason without `notRouted`. The reason is also in its claim and ticket and on ops-tower's Gates page.

A command run from one worktree that names a path in another worktree of a listed repo stays local, saying `gate: not routed: the command works in <tree>; run it with --cd <tree>`. Run it again with `--cd <tree>` to route it from that worktree ([design.md, Routing](design.md#routing)). A path in an unlisted repo, on a network drive or in no worktree keeps the plain `is outside the worktree` line.

## Adding a repo

1. Read the repo's own gate commands (its CLAUDE.md, nextup profile, justfile or package scripts) and the tools each needs: toolchains and their pins (`.gate/toolchain.toml`, `rust-toolchain.toml`, `global.json`), shells and utilities, package managers.
2. Set the builder up ([Builder setup](#builder-setup)). A session installs what a routed repo needs, at the repo's pinned version, and changes the builder's configuration (PATH, ACLs, environment) directly over ssh.

   A change to the builder's toolchains, tools, work root or git config is also made in `tools/builder/install-toolchains.ps1` in the same item, so a new builder gets the same setup. A change to the machine's provisioning goes to claude-config as a repo inbox note with what changed and the exact commands. Run `gate check --builder` from the repo.
3. Back up `remote.json` beside it (`remote.json.bak-<UTC stamp>`), then add the repo's patterns under `repos.<repo>`, plus `outputs`, `inputs`, `siblings`, `desktop`, `warmup` or `sendBuild` consent where the repo needs them. Re-read the file just before writing and change only the repo's own keys: other sessions edit it too.

   `sendBuild` is an array of repo names: a routed `dotnet test <project> -c <configuration> --no-build` of a listed repo sends the PC's build of that project to the builder and keeps `--no-build` there, where an unlisted repo has the word dropped and the builder builds ([design.md, Routing](design.md#routing)).

   A test report under a `TestResults` folder (an MTP TRX) comes back only through a glob that names the `TestResults` folder, such as `tests/*/bin/*/*/TestResults/*.trx`, and only after a passing command. `**` and `*TestResults*` bring back nothing under it.
4. Run one routing trial (below). The repo counts as routed once its trial passes.

## Builder setup

A builder's setup has two halves. claude-config's private provisioning does the machine: first boot, the WSL platform and its reboot, Defender, Developer Mode, the Application event log ACL, autologon, the D: Dev Drive, OpenSSH and the tailnet, the Windows Update policy, RustDesk and Process Explorer.

This repo's `tools/builder/install-toolchains.ps1` does the rest: the toolchains and tools, the sccache server and WSL keep-alive tasks, the git config for gate runs, the MSYS keeper task, the gate work root and the work-folder prune task.

- **Order on a new builder:** claude-config's provisioning, then the installer, then `gate push`.
- **Copy:** `scp tools/builder/* {builder}:C:/Users/{builder user}/` puts the scripts in the builder user's home, since claude-config's provisioning leaves `C:\build-box` writable by Administrators and SYSTEM only. From the elevated session, copy the exact files (`install-toolchains.ps1` and `wsl-setup.sh`, never a wildcard) into `C:\build-box\gate\` before running the installer. `wsl-setup.sh` must sit beside the script.
- **Run:** over ssh, elevated (an administrator's ssh session), under Windows PowerShell 5.1: `powershell -NoProfile -ExecutionPolicy Bypass -File C:\build-box\gate\install-toolchains.ps1 -User {builder user} [-GateRoot D:\gate-remote] [-SccacheDir D:\sccache] [-SccacheCapBytes N] [-Only {step prefix},...]`.
- **Parameters:** `-User` is required and names the builder user: the scheduled tasks run as it and the write-deny ACLs name it. `-GateRoot` is the gate's work root, `-SccacheDir` sccache's disk cache and `-SccacheCapBytes` its size cap. `-Only` runs just the steps whose names start with one of the given prefixes, then the version summary (`-Only Summary` runs only the summary).
- **As the builder user:** run it in the builder user's own ssh session, since the user-profile installs go to the running user. Run it only while no gate is running on the builder.
- **Exit codes:** a missing or unknown `-User` exits 2, a `-Only` prefix that matches no step exits 2 before anything is written, and a session that is not elevated exits 1 with `install-toolchains: not elevated; ...`.
- **Steps**, each a valid `-Only` prefix, run in this order: Git, PowerShell, .NET SDK 10.0.401, .NET SDK 8.0.425, .NET Framework 4.8 Developer Pack, .NET workload wasm-tools, Node.js, uv and prek, Python, Visual C++ Redistributable 14.51, Visual Studio Build Tools 2022 (C++), Rust, cargo-nextest and just.

  The remaining steps are WSL Ubuntu (registers and starts the `gate-wsl-keepalive` logon task and waits for the VM before the elevated setup), sccache, Git usr\bin on the builder user Path, Bun, Godot, FFmpeg, pnpm, Playwright browsers, Git system config for gate runs, Smart App Control off, keeper: MSYS sleep task, Gate work root, Work-folder prune task and Summary.
- **Tasks:** `\build-box\sccache-server`, `build-box-msys-keeper`, `gate-wsl-keepalive` and `\build-box\gate-prune-work`. All four run as the builder user at the Limited run level: all but `gate-wsl-keepalive` by S4U, whether or not the user is logged on, and `gate-wsl-keepalive` in the user's interactive logon.

  The sccache server's log is `%LOCALAPPDATA%\build-box\sccache-server.log` in the builder user's profile, not under `C:\build-box`; the step refuses a `LOCALAPPDATA` that holds a quote or bracket.
- **Work-folder prune task:** the step `Work-folder prune task` (`-Only Work-folder`) registers `\build-box\gate-prune-work`, which runs every 6 hours from 00:17 with a 2 h limit, IgnoreNew and StartWhenAvailable. Its command runs inline, with no script file and module auto-loading off: `C:\Program Files\gate\bin\gate.exe box prune --root <GateRoot>` ([design.md, The box side](design.md#the-box-side), `prune`).

  Its output goes to the Application log under the event source `gate-prune`, event ID 1: each output line is an Information event, and a closing event `gate box prune exited {code}; {drive}: free {n} GB` (`(no exit code)` when there is none) is Information on exit 0 and Warning otherwise.

  The step logs `event source gate-prune: already registered` or `registered in the Application log`, and `task \build-box\gate-prune-work registered: ...`, adding that the task `will fail until a gate push installs C:\Program Files\gate\bin\gate.exe` when that exe is not there yet.
- **Gate work root:** the step refuses a root that is a link (reparse point) or a file. When the root's security is not already right it sets the owner to Administrators (`icacls /setowner *S-1-5-32-544`) and applies a protected DACL: SYSTEM, Administrators and the builder user with full control, inherited by objects and containers, no mandatory label.

  The box's preflight judges the same state before it touches anything and refuses a root another account may write in, a folder above it another account may delete, a reparse point at the root or its parent, a `subst` drive and a network volume.

  It says `gate: the builder's work root is not locked down: <why>; run the installer's "Gate work root" step (install-toolchains.ps1 -Only "Gate work root")`, and a routed run then runs here (`gate check --builder` says it as a missing item). A root open in another process with delete access is only busy: retry when it is closed.

  It then reads the security back and fails unless it is exactly that, owner included; when it already is, it logs `already protected` and changes nothing. A routed command's restricted token passes access checks through the builder user's own SID, so neither Authenticated Users nor Users needs a grant.
- **claude-config's `C:\build-box` rules:** its provisioning re-asserts the `C:\build-box` DACL on every run and strips explicit ACEs under it. The gate half therefore re-applies anything it needs under `C:\build-box\gate` on every run: an ACE set on the root that inherits survives, an explicit ACE on a child does not.

  Any SYSTEM or highest-run-level task the gate half adds whose action names a script under `C:\build-box` goes through claude-config's `C:\Program Files\build-box\run-pinned.ps1`; the gate half adds none today.
- **Smart App Control off:** in Evaluation, Smart App Control asks the cloud for the reputation of every unsigned image the first time it is mapped, whatever the Defender exclusions, so each freshly built exe in the work root waited 1.5-16 s before its first start (0.05 s with it off). The step sets `VerifiedAndReputablePolicyState` to 0 and refreshes the policies with `CiTool`; Defender's real-time, cloud and behaviour protection stay as they are. A policy still enforced after the refresh asks for a restart of the drained builder.
- **Test:** `pwsh -NoProfile -File tools/builder/test-install-toolchains.ps1`, run through `tools/gate.ps1 -Slot light`. It installs nothing and needs no admin.
- **The Unreal engine install is not in the installer.**

### A macOS builder

A macOS builder (an Apple-silicon Mac, a dedicated builder nobody works on) is set up by `tools/builder/install-macos.sh`, run as the builder user over ssh. The box gets onto the Mac by `gate push`, which builds it there ([design.md, `gate push`](design.md#gate-push)): the Mac needs git, cc (the Xcode Command Line Tools, `xcode-select --install`) and the Rust step below, and the PC needs a clean, committed `tools/install.ps1` install, whose `source.json` names the commit the Mac builds. The box lands in `<root>/bin/gate-box`, and the build's checkout, target folder and `build.log` live in `<root>/gate-push`.

`remote.json`'s `builders` array (an entry with `os` `macos` and a `root`) is read and routes ([design.md, remote.json](design.md#remotejson)), and the transport, payload layout, path map and PC caches follow the entry's profile.

The words a macOS builder is sent are spelled for the Mac: the recognised paths use `/`, and a relative word written with `\` (`src\Duel.sln`) is sent with `/` when the snapshot carries that file or folder, in its exact letter case. A word that still holds a `\` (a regex, a list, script text after `bash -c` or `pwsh -Command`, a path the repo does not carry) keeps the command off the Mac: it runs on a Windows builder or here, silently. A Windows builder is sent the words as written. Details: [design.md, Routing](design.md#routing).

- **Toolchain steps** need no sudo and land under `--prefix` (default `~/gate-tools`), Rust under `CARGO_HOME` and `RUSTUP_HOME`: .NET SDK 10.0.401 and 8.0.425, the wasm-tools workload, PowerShell, Node.js, pnpm, uv and prek, Rust, cargo-nextest, Git LFS, Godot 4.7.2 mono, Bun. FFmpeg is skipped: no official source publishes a macOS arm64 build with a checksum.
- **Login PATH** writes a marked block to `~/.zprofile`. The box reads its commands' PATH from the builder user's login shell, since a non-login ssh command lacks the prefix's tools.
- **Git config** sets `core.autocrlf false` and `gc.pruneExpire never`. **Gate work root** (`--root`, default `~/gate-remote`) makes the root, `work` and `bin` at mode 700; the box refuses a root another account may write in.
- **System** runs only as `sudo bash install-macos.sh --system --user <builder user> --only System`: no sleep, restart after power loss, wake on network, the open-file limits and Remote Login. Without `--system` it prints the commands it would run.
- **Flags and exits** follow the Windows installer: `--only <prefix>,...` (case-insensitive prefixes, then the summary; `--only Summary` alone), `--list`, exit 2 on a bad argument or a prefix that matches no step, exit 1 when a step failed. Every step logs `OK`, `SKIP` or `FAIL` to stdout and `<prefix>/install.log`.
- **Test:** `bash tools/builder/test-install-macos.sh`, on this PC or the Mac; it installs nothing. `bash tools/builder/test-build-box-macos.sh` runs the script `gate push` sends (`MAC_SCRIPT`) against a scratch root with a real cargo build of the box; it runs on macOS only.

`gate check --builder` hints at the installer for a missing node, pwsh, ffmpeg, netfx-sdk or a rust with no minimum: the hint names `tools/builder/install-toolchains.ps1` and points to this section.

### Several builders

List each further builder in `remote.json`'s `builders` array, beside `host`. A builder that refuses a routed run or cannot be reached hands the run to the next builder that matches the command, then `host`, and the PC runs it only when none takes it.

Which refusals hand the run on, the order, the one wait shared by every builder and the lines the run says are in [design.md, The next builder](design.md#routing); a repo owner adds nothing beyond the list.

## The routing trial

A trial runs the repo's full command routed, once, and assumes a local run passes: it never runs the full command locally to compare. If the routed run fails, only the failing steps or tests are rerun locally, with the narrowest command that reproduces them:

- **A step that fails locally too** is the repo's own failure. The pattern stays routed, since routed and local agree, and the failure goes to the repo as an inbox note.
- **A step that passes locally but fails routed** is a routing failure. It is fixed in the builder's setup and that step alone is retried routed, once. If it still fails, the repo's patterns come back out of `remote.json` and the failure is booked as a GATE issue.

A first routed run in a new work folder is cold, as is the first run after the builder cleared what a killed or lost run left in it, and a pair's first routed run (the folder has no pair layout yet). The gate raises its ceiling to `min(2 x ceiling, ceiling + 1800 s)` and says so. The trial still wants a `--timeout` near what a cold build takes, since the raise is at most half an hour more; the repo's ordinary routed runs keep their usual ceilings. Run one heavy trial at a time.

## A repo with a command that needs the builder's desktop

A repo with a routed command that needs the builder's logged-on desktop (a GUI test run) declares its `-like` patterns in a tracked `.gate/desktop.toml`, `patterns = ["dotnet test *Ui*"]`, at most 32, matched against the same rewritten words as the repo's `repos` patterns. The PC consents in `remote.json`: `"desktop": {"<repo>": ["dotnet test *Ui*"]}`, each pattern listed exactly as the file spells it, letter case included. The first declared pattern a command matches decides, and a command no pattern matches asks for no desktop.

Without the consent the command runs here with `gate: not routed: desktop pattern '<pattern>' not allowed in remote.json`.

A `.gate/desktop.toml` the gate cannot use keeps every routed command of the repo here with `gate: not routed: .gate/desktop.toml: <problem>`, so fix it before relying on routing. A desktop run holds a remote slot while it waits for the desktop, up to the usual 60 s remote wait. Format and rules: [design.md](design.md#remotejson).

## A repo with a command that stays on this PC

A repo with a command that needs this PC (its WSL state, a service or device only this PC has) declares its `-like` patterns in a tracked `.gate/local.toml`, `patterns = ["dotnet test *Lan*"]`, at most 32, matched against the same rewritten words as the repo's `repos` patterns. No consent in `remote.json` is needed, since the file only keeps commands here.

A command it matches runs here with `gate: not routed: .gate/local.toml keeps it here (<pattern>)` and is journalled `notRouted: declared local`; the check runs right after the repo's patterns match, before the siblings and desktop checks.

A `.gate/local.toml` the gate cannot use keeps every routed command of the repo here with `gate: not routed: .gate/local.toml: <problem>`, so fix it before relying on routing. A repo that cannot commit the file keeps it in its declarations folder ([design.md](design.md#remotejson)). `gate check` does not report it.

## A repo whose builds read its own variables

A routed run sends none of the caller's environment but the `--env` pairs, and warns when the caller holds a variable that run drops. The warning knows the build variables (`NuGetAudit`, `RUSTFLAGS` and the `NUGET_`, `MSBUILD`, `CARGO_BUILD_` and `CARGO_PROFILE_` families); a repo whose commands read others declares their name prefixes in a tracked `.gate/env.toml`, `prefixes = ["COLLIE_"]`, at most 32, each ASCII letters, digits and `_`, none starting `GATE_`.

The line `gate: a routed run does not send the caller's <names>; --env NAME sends it` then names the caller's variables starting one too, so a caller adds `--env NAME` where the command needs it: the gate never sends a declared variable by itself, since it may hold a secret.

A file the gate cannot use says `gate: .gate/env.toml: <problem>; only the build variables are watched` and the run still routes. The file may also sit in the declarations folder; `gate check` does not report it ([design.md](design.md#command-environment)).

## A repo whose git hooks run heavy checks

A heavy gate started inside a git hook stays local silently unless `remote.json` names its repo in `hooks`, an array of repo names matched in any letter case: `"hooks": ["<repo>"]`. A value that is not an array of non-empty strings consents to no repo, and the rest of the file is still read ([design.md, remote.json](design.md#remotejson)).

The key is for check-only hook commands (a build, clippy, a format check). A routed fixer's patch comes back to the worktree under prek's stash, so a fixing hook stays local.

The hook entry calls the repo's launcher, `pwsh -NoProfile -File tools/gate.ps1 run ... -- <heavy command>`, never a bare `gate run`, as every other call does (scripts, docs and agents' own commands alike): a hook runs on every machine that commits, and on a clone without gate.exe the launcher runs the command directly. The repo's patterns must match that command.

The `git commit` that runs the hook is gated on `--slot heavy` when its hooks build or run analyzers, else on `--slot light`: the commit holds that slot for all its hooks, and a `--slot none` commit runs on a light slot and says to pass one of the two.

A heavy commit covers its hook's gate, which still routes and takes its own remote slot; a light commit does not cover a heavy hook gate, which takes a heavy slot of its own (and routes). The commit's end-of-run line says whether the hooks fit the slot. The `git commit` itself stays local.

Only hooks that run with `GIT_INDEX_FILE` set (pre-commit and commit-msg) need the `hooks` consent: a pre-push hook's gate routes by the repo's patterns like any other run. Every gated repo carries a committed copy of this repo's `tools/gate.ps1`; claude-config's `tools/sync-launchers.ps1` gives a first copy to each repo it lists that has none and keeps every copy current, and `~/.claude/tools/gate.ps1` is the same wrapper for a caller outside any repo.

## A repo whose short runs a cold folder would push past their ceiling

A repo whose short routed runs (a filtered test of a minute) would meet a cold builder work folder declares warm-up builds in a tracked `.gate/warmup.toml`, and `remote.json` consents to it under `warmup` ([design.md, gate warm](design.md#gate-warm)).

Routed runs go to the builder from a new worktree's first run, warm or not: the first run in a new work folder is cold, with its ceiling raised, and says to prewarm the next new worktree with `gate warm`. Run `gate warm` in a new worktree before its first routed run to find the folder warm.

## A repo that builds with Unreal Engine

The engine is not in the worktree and not on the builder's `PATH`: the builder holds its engines side by side under `GATE_UNREAL_ROOT`, one subfolder per version named `x.y.z`, each with `Engine\Build\Build.version` ([design.md, Tool probes](design.md#the-box-side)). The engine install is not part of `tools/builder/install-toolchains.ps1`: a session installs an engine by hand over ssh. `gate check --builder` shows the versions it lists.

- **The pin.** The repo pins the engine in `.gate/toolchain.toml`, `unreal = "5.8"`, an exact pin like godot's: `"5.8"` takes any 5.8.x and `""` any version. A builder without a matching engine keeps the run here with the mismatch line. The gate picks the highest matching engine and the box sets `UE_ROOT` to its folder for every command of the run.

  A repo whose build reads another variable for the engine (ue-mcp's `UE_MCP_TEST_ENGINE_ROOT`) names it in the manifest's `[env]` table, `UE_MCP_TEST_ENGINE_ROOT = "unreal"`, and the box sets it to the same folder; a PC engine path passed with `--env` is not rewritten for the builder, and `UE_ROOT` itself is refused there. `gate check` shows an `[env]` entry the gate cannot use.
- **The routed command is a tracked wrapper script.** It builds absolute `-project=` and `-archivedirectory=` paths itself, because `RunUAT.bat` `pushd`es into the engine and the router sends worktree paths as relative ones. It reads the engine from `UE_ROOT`, with its own fallback for the PC, where nothing sets it.
- **The .NET Framework SDK.** A manifest naming `unreal` at any version also needs a `netfx-sdk` of 4.6.0 or later, the SDK `UnrealBuildTool` looks for, which the gate adds itself. The installer's step `.NET Framework 4.8 Developer Pack` provides it, and a builder without it keeps the run here with the mismatch line. `gate check` here judges it too, unless `GATE_UNREAL_ROOT` is unset on this PC, where it says it is not checked.
- **The archive** goes to a gitignored folder outside `.tmp/`, named in the repo's `outputs`, because outputs never come back from `.tmp/`.
- **What stays local:** commandlets that write into ignored content, a packaged-game run that needs a GPU when the builder has none, and the editor.
- **Git LFS.** A repo with LFS-tracked files routes with them: the PC sends the objects the snapshot's pointers name to the builder's per-repo LFS store, and the box checks them out with no download.

  The builder needs git-lfs 3.1.3 or later whenever a tracked `.gitattributes` names the LFS filter (a builder without it keeps the run here, `parity`), and the PC must hold the objects: run `git lfs fetch` first, else the run falls back `lfs` with a log line naming the missing object. See [design.md, Routing](design.md#routing) (Git LFS).

## Build files that read back as zeros

When a caller reports build files that are the right size but all zero bytes, check for a crash of the PC before reading any gate code. Three things point to one:
- System events 41, 6008 or 1001 near the time.
- A run of NUL bytes in the PC's journal (`events.jsonl`, [design.md](design.md#journal)).
- A gate with a `take` line and no `release`.

A crash loses data that was not yet written to every drive at once. NTFS can keep a file's new size while its contents never reach the disk. The gate never leaves a file like that: the outputs install unpacks into a stage it removes on any error, and it replaces a worktree file only by renaming a complete, verified copy over it.

## Taking a builder out for maintenance

A reboot, an install that needs one, or any other work that would break a run on the builder starts by draining it, never by waiting for it to go idle while it stays in `remote.json`: new runs keep arriving, so that wait has no end.

1. Back up `remote.json` beside it (`remote.json.bak-<UTC stamp>`) and save the builder's exact entry. Then, in one quick read-modify-write, take the builder out by removing `host`: routed runs go to `host` or a `builders` entry, and `hosts` names only the builders `gate push` and `gate check --builder` reach. With no `host` left, every gate on the PC runs here and says the routing file is unusable, which is expected while the builder is out.
2. Wait for the runs already on the builder to end. No `gate.exe` box process and no routed command may still be live under the builder user, besides your own ssh session. There is no cutoff, since each routed run ends at its own ceiling. A run still live past the longest routed ceiling is looked into, never cut off by the reboot.
3. Do the work. After a reboot, confirm that the console session logged back on (autologon), which the desktop route needs.
4. Put the builder back with the saved entry, in a read-modify-write that keeps every other key as it stands then, and check it with `gate check --builder <host>`. If anything blocks the work partway, put the builder back first, so routing never stays off.
