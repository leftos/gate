# gate design

`gate.exe` runs a heavy command (a build, a test suite, a formatter) under a machine-wide queue of CPU slots, so the agent sessions and people sharing one Windows PC take its cores in turn. It watches each command for stalls and runaway time and kills the command's whole process tree when one trips, and it can send allowlisted heavy commands to a remote builder.

This document is the gate's design: the CLI, the output callers may parse, the files and kernel objects other processes see, the crate layout and the test harness. Terms used in a project sense are in the [glossary](GLOSSARY.md).

The design keeps the PowerShell gate's behaviour wherever no ruling changes it. Such a detail says "kept" and cites its item in the [behaviour inventory](research/2026-10-09-powershell-gate-inventory.md) (C1, S2, Q1 and so on), which holds the full rule, its edge cases and the self-test cases that pin it today.

## Goals and non-goals

Goals:

- **A local gate first.** Everything a single PC needs (slots, queueing, the critical drain, nesting, the watchdog, the journal) works with no remote builder configured and no network.
- **Remote builders optional.** A machine may route allowlisted heavy commands to builders it names in its own state root. Nothing about any builder (host, address, user, key) lives in this repo, its history or its issues.
- **Reusable by anyone.** The install script and these docs work for any Windows user; no path, host or account of the author's is assumed.
- **A Windows binary.** `gate.exe` and the Win32 layer build for Windows only.
- **Pure crates build everywhere.** The crates that hold formats, arithmetic and string rules build and unit-test on Linux as well, so their tests run in CI and WSL without a Windows host.

Non-goals:

- Sharing slots with the PowerShell gate. The two generations share the default root's slot mutexes and take turns on them; each cutover drains the PowerShell gates first ([Cutover](#cutover)).
- A daemon. Every gate is a short-lived process; coordination is through named kernel objects and files in the state root.
- Running on Linux or macOS. Only the pure crates do.
- Configurable failure markers. The marker regex is built in ([Log and tail](#log-and-tail)).

## CLI

The CLI is subcommands with GNU-style flags: `--name value` or `--name=value`, and `--` ends the gate's options. The first word picks the parser. `--version`, `--help`, `-h` and `help` are matched first; then a subcommand name selects the CLI below, and any other first word starting with `-` selects the [compatibility layer](#compatibility-layer). No arguments at all print the usage on stderr and exit 2.

A `--help` or `-h` before the `--` of a `run`, `stop-tree`, `cutover`, `push`, `check`, `warm`, `burn` or `expect` line also prints the usage on stdout and exits 0, whatever else the line holds; after the `--` it belongs to the command (`gate run --slot light --timeout 5 --log x.log -- tool --help` runs `tool --help`).

| Subcommand | Purpose |
|---|---|
| `gate run` | Run one command under a slot and the watchdog. |
| `gate stop-tree` | Stop a process tree, reaching the gate jobs inside it. |
| `gate cutover` | Switch this machine from the PowerShell gate to `gate.exe`, draining it first. |
| `gate push` | Copy the installed `gate.exe` to every builder `remote.json` names. |
| `gate check` | Say what this PC lacks for the repo in the current folder, and how to install it. |
| `gate burn` | Spin the CPU for a set time, for the load-burn tool. |
| `gate expect` | Print the expected time of a command here and on each builder, from the durations store. |
| `gate warm` | Warm the builder's work folder of the worktree the gate is started in, by running its `.gate/warmup.toml` commands there ([`gate warm`](#gate-warm)). |
| `gate box` | Hidden: the builder side of a routed run. |
| `gate --version` | Print `gate <package version>+<build id>` and exit 0 ([Build id](#build-id)). |
| `gate --help`, `gate -h`, `gate help`, `gate <subcommand> --help` | Print the usage on stdout and exit 0. |

An unknown subcommand, an unknown flag or a missing value prints what is wrong on its own `gate: ` line, then the usage, and exits 2 before any slot is queued (kept, C1). Every problem found is printed, one per line: invalid values in the order the words came, then the missing inputs in the order slot, timeout, log, command. A state-root failure ([State root](#state-root)) exits 2 with its one line and no usage.

Rules both parsers share:

- An option given twice is a usage error.
- A value option takes the next word unless that word is `--`; an empty value counts as missing.
- A whole number is ASCII digits only: no sign, space or fraction. Timeout, stall, tail and the `--test-*-ms` values are at most 2147483647.
- `--slot` values compare case-insensitively.
- `--slot auto` is refused as an invalid value until GATE-8 builds it. `--slot none` is accepted.
- `--timeout` takes a whole number or `auto:<n>` (`auto` in any ASCII case), whose `n` follows the whole-number rule. A bare `auto` says `gate: --timeout auto needs a floor in seconds: auto:<n>`, and a bad floor `gate: --timeout must be a whole number above 0, got 'auto:0'` or `gate: --timeout must be at most 2147483647, got 'auto:2147483648'`.
- A gate option that is not valid Unicode is a usage error; the words after `--` are passed through as the caller gave them.
- A word before `--` that is not an option is a usage error (`gate: unexpected argument <word>`).

A valid `gate stop-tree` stops the tree ([`gate stop-tree`](#gate-stop-tree)).

A valid `gate run` starts the command: it refuses a batch word (exit 2), refuses a `--log` inside the state root (exit 2, `gate: the log <path> is inside the gate's state folder <root>`; the paths are compared canonical, through links and junctions, after canonicalising the nearest existing folder), refuses a `--cd` folder it cannot enter (exit 2, [Output lines](#output-lines)), takes the log's mutex, prepares the log, queues, takes a slot, starts the command in the job `Local\gate[-<h>]-job-<gate pid>`, waits for it, reaches the verdict and releases the slot.

A `none` run takes no slot and starts at once ([Slots](#pools-and-names)).

The watchdog samples the command and kills its job on a stall, at the ceiling and at the backstop.

### `gate run`

`gate run --slot heavy|light|critical|none --timeout <seconds>|auto:<seconds> --log <path> [options] -- <command> [args...]`

`--slot`, `--timeout`, `--log` and the command are required and have no default, because a gate with no ceiling holds its caller for as long as the command hangs (kept, C1). Each missing or unreadable input is named on its own line before the usage.

| Flag | PowerShell option | Meaning | Source |
|---|---|---|---|
| `--slot heavy\|light\|critical\|none` | `-Slot` | The slot kind ([Slots](#slots)); `none` takes no slot and is for a command that only dispatches or watches remote work. | ruling |
| `--timeout <seconds>` | `-TimeoutSeconds` | The ceiling in load-adjusted seconds, a whole number above 0 (W3). `auto:<n>` makes `n` a floor: each take resolves the ceiling to `max(n, ceil(3 x the command's adjusted p99))`, capped at 2147483647, and to `n` with no adjusted p99 ([Output lines](#output-lines), [design/gate-durations.md](design/gate-durations.md), part 4). The new CLI only: `-TimeoutSeconds` refuses it. | ruling |
| `--log <path>` | `-Log` | The command's log; a relative path is read against the caller's working directory once, at start, before `--cd` (P5). | ruling |
| `--stall <seconds>` | `-StallSeconds` | The stall window, default 120 (W2); the gate passes it as given, and the remote client raises a value under 15 to 15 on a routed run (F2). | kept (C1, F2) |
| `--tail <lines>` | `-Tail` | Lines of the log printed at the end, default 20 (V3). | kept (C1, V3) |
| `--no-markers` | `-NoMarkers` | The command's status is the verdict; the marker scan is off (V2). | ruling |
| `--local` | `-Local` | Never route this run to a builder (F1). | kept (F1) |
| `--state-dir <path>` | none | Use another [state root](#state-root); a relative path is read against the caller's working directory, before `--cd`. | ruling |
| `--cd <folder>` | `-Cd` | Run the command in `<folder>` and route from it ([Run folder](#run-folder)). | ruling (GATE-323) |
| `--bring-tracked <glob>` | none | A routed run's fetched output that git tracks and `<glob>` matches is installed rather than skipped (Outputs install, "Tracked members"); repeatable, and the one option that is not refused when given twice. The launcher's PowerShell form (`-Log …`) does not take it: use the `gate run` form. | ruling |

The words after `--` reach the command exactly as the caller passed them ([Command launching](#command-launching)).

#### Run folder

`--cd <folder>` (a relative folder is read against the caller's working directory) makes the gate enter `<folder>` once, after `--log` and `--state-dir` are made absolute against the caller's folder and before any slot is taken. From then on `<folder>` is the folder the gate was started in: [routing](#routing) reads its worktree, the command's working folder and program lookup, a fallback's local run, and the journal's and claim's `cwd` all take it. A relative `--log` or `--state-dir` stays the caller's.

It is how a caller in a main checkout gates a command that names another worktree's files: `--cd <worktree>` routes the run as if the gate had been started there. Given twice it is refused (`gate: --cd is given twice`). `gate expect` and `gate check` do not take it (unknown option). The compatibility layer reads it as `-Cd`, which is the one new option it takes (the launcher's callers need it); the frozen PowerShell gate refuses `-Cd`. Nothing is said on success.

### `gate stop-tree`

`gate stop-tree <pid>` takes one process snapshot (pids, parents and start times, no command lines) and builds the tree: the pid plus its descendants, a child counting only when its creation time is readable and at or after its parent's, so a recycled pid adopts no one (a child whose creation time is unreadable is left out with its subtree).

It terminates the job of every gate in the tree first, then the tree's processes deepest first, skipping itself and any process whose creation time changed since the snapshot (the pid was reused) (kept, X6). The members of a terminated gate job share a 2 s grace to exit on their own, and only the processes stop-tree had to terminate itself are counted.

A job or process it stops ends with status 124. Gate jobs are found under the resolved [state root](#state-root), named as the job of each gate pid in the tree, so `--state-dir` and `GATE_STATE_DIR` are honoured.

A failed terminate is a failure only if the process is still alive 1 s later. It prints `gate: stopped <n> gate job(s) and <m> process(es) under <pid>` on stdout and each failure as `gate: could not stop: <reason>` on stderr, and exits 0 when nothing failed and 1 when anything did. A gate whose routed run holds an ssh call prints `gate: stopped 0 gate job(s) and 2 process(es) under <pid>`: the ssh call's job is anonymous, so stop-tree ends ssh by pid.

A reason reads `<Op>(<arg>) failed with Win32 error <n>: <message>`, where `<Op>` is `OpenJobObjectW`, `TerminateJobObject`, `OpenProcess` or `TerminateProcess`, or `listing the processes of <job name> failed: QueryInformationJobObject failed with Win32 error <n>: <message>`.

A pid with no live process prints `gate: stop-tree: no live process has the id <pid>` and the usage, and exits 2, as does a missing or invalid pid.

### `gate cutover`

`gate cutover` takes no words; its usage line is `       gate cutover`, after `gate stop-tree <pid>`. It reads `%LOCALAPPDATA%` only: `GATE_STATE_DIR` is ignored, an extra word prints `gate: unexpected argument <word>` and any option, `--state-dir` included, prints `gate: unknown option <opt>`, each followed by the usage, exit 2.

A missing or empty `LOCALAPPDATA` prints `gate: cutover needs LOCALAPPDATA` and exits 2. What it does and why is [Cutover](#cutover); this is what it says. Every line goes to stderr.

1. Copy the counts. A failure prints `gate: cutover: cannot copy the slot counts: <error>` and exits 1 with no flag written. A PowerShell counts file that is rejected prints `gate: cutover: ignored <path>: <reason>; gates use the default counts` and the cutover goes on.
2. Write the flag. A failure prints `gate: cutover: cannot write the cutover flag: <error>` and exits 1. Success prints `gate: cutover: flag written; new gates now start gate.exe`.
3. Drain, below.
4. Print `gate: cutover: done; every gate now runs gate.exe`, then `gate: cutover: to go back to the PowerShell gate, delete <LOCALAPPDATA>\gate\cutover.flag`, and exit 0.

The drain polls every 2 s. Its first list is `gate: cutover: waiting for 1 PowerShell gate to finish:` (or `... <n> PowerShell gates to finish:`), then one line per gate in `since` order, `gate:   pid <pid>  <what>  <command>`.

- `<what>` joins with `, ` any of `heavy slot <i>`, `light slot <i>`, `remote slot <i>`, `critical claim`, `waiting (<kind>)` and `holds the machine (critical)`. A critical gate's claim and its holding ticket are one gate.
- `<command>` is the claim's command, else the process's command line, else `(command line unreadable)`.

After the list, each scan says, in this order:

- `gate: cutover: pid <pid> finished (<n> left)` for each pid that left;
- for each new pid, `gate: cutover: late PowerShell gate pid <pid> started after the flag: <command>` when its earliest `since` is later than the flag time, else `gate: cutover: also waiting for pid <pid>  <what>  <command>`; each pid is named once;
- `gate: cutover: cannot read <n> claim or ticket file(s) under <LOCALAPPDATA>\gate; they are not waited on`, said once;
- `gate: cutover: still waiting for <n> PowerShell gate(s) (<s> s)` every 30 s from the start of the drain, while gates remain.

The drain ends only when every scan across a 5 s confirmation window finds no gate; a scan that finds one resets the window. Ctrl+C prints `gate: cutover: interrupted; the flag stays set, and running gate cutover again resumes the wait` and exits 0xC000013A. A rerun resumes; the take-back is deleting the flag by hand.

Test-seams builds accept hidden options on `cutover`: `--test-poll-ms`, `--test-heartbeat-ms`, `--test-confirm-ms` and the switch `--test-cutover-interrupt-after-list` (raises the interrupt once the first list is printed). `--test-print-request` prints `{"mode":"cutover","localAppData":...,"testSeams":{"pollMs","heartbeatMs","confirmMs","interruptAfterList"}}`.

### `gate push`

`gate push` takes no words; its usage line is `       gate push`, after `gate cutover`. A word prints `gate: unexpected argument <word>` and any option, `--state-dir` included, prints `gate: unknown option <opt>`, each followed by the usage, exit 2. It reads `%LOCALAPPDATA%` only; `GATE_STATE_DIR` is ignored. What it does is [Install](#install); this is what it says.

It copies the installed `%LOCALAPPDATA%\gate\bin\gate.exe` to each builder, one after another, in the order `host` then `hosts` of `remote.json` ([remote.json](#remotejson)). The routing file must be one routing accepts, so `repos` and the other keys are checked as routing checks them. Every builder is tried, whatever happened to an earlier one. The exit is 1 if any builder failed or nothing could be pushed, else 0.

With nothing to push or nowhere to push it, `gate push` tries no builder, prints one line on stderr and exits 1:

- `push: no remote.json at <path>`;
- `push: ignored <path>: <reason>`, the router's reason text for an unusable file, or for a bad `hosts` ([remote.json](#remotejson));
- `push: no installed gate at <path>; run tools/install.ps1`;
- `push: no remote.json: LOCALAPPDATA is not an absolute folder`, for an unset, empty or relative `LOCALAPPDATA`;
- `push: could not read the version of <path>`, when the installed exe's `--version` gives no `gate <version>` line or takes over 30 s;
- `push: cannot read <path>: <error>`, when the installed exe cannot be read;
- `push: an install is in progress on this PC; nothing was sent`, when `tools/install.ps1` holds this PC's `push.lock`;
- `push: <bin>\push.lock is a link; nothing was sent`, and `push: could not lock <bin>: <error>; nothing was sent` for any other refusal of the lock. The link text is also said for a `push.lock` that is a folder, a junction or a second name of another file (a hard link); the open never follows a link.

`gate push` holds `%LOCALAPPDATA%\gate\bin\push.lock` on this PC, open and unshared, while it reads the installed exe's bytes and runs its `--version`, and lets go before the first builder, so it never reads a gate.exe that `tools/install.ps1` is replacing.

`<v>`, `<old>` and `<new>` are the whole text after `gate ` in `gate.exe --version`, build id included, as `tools/install.ps1` reads it; the local one is read once, before the first builder. Per builder, on stdout:

1. `push: <host>: sending gate <v>`, before the transfer.
2. Then one of `push: <host>: gate <v> already installed` (an identical SHA-256: nothing was replaced), or `push: <host>: replaced gate <old>` followed by `push: <host>: installed gate <new>` (`replaced` only when a gate was there before), or on stderr `push: <host> failed: <reason>`.

A note on stderr never fails the push: `push: <host>: could not delete <path>: <message>` names an aside or stale temporary copy the builder could not delete, `push: <host>: could not delete the aside copies in <dir>: <message>` says the folder could not be listed after a verified install, and `push: <host>: could not list <dir>: <message>` says the same of the count of kept aside copies.

`push: <host>: could not stop the version read of <path>: <message>` says a version read past its bound could not be killed (the read still gives no version).

After the builder's other stdout lines, a push that ends with `gate.exe` missing or one it could not verify left in place, and with aside copies in the folder, says `push: <host>: kept <n> aside copies: gate.exe is not verified` on stdout, `<n>` counting every `gate.exe.*.old` present, this push's own included. It is never a failure by itself: an exe identical to the installed one that does not answer `--version` still says `already installed` and exits 0, with this line after it.

A failure reason is one of: `timed out after <n> s`, `no answer`, ssh's last stderr line, `cannot feed the call: <error>`, or the builder script's own.

The builder script's own reasons are `the copy could not be read: <exception message>`, for bytes that do not decompress; `the copy arrived damaged`; `<dir> is a link`; `<dir>\push.lock is a link` (also for a folder, a junction or a hard link there); `another gate push is installing on this builder`; `could not lock <dir>: <message>`; `the builder needs pwsh 7.2 or later (it has <version>)`.

`could not prepare the install: <message>` says the script's small C# type for its file checks could not be added (constrained language, no compiler), before anything is touched. Any other failure says the failing step's message.

`the installed copy does not match`, `the installed gate.exe does not run` (the installed copy hashes right but gives no `gate <version>` line within 30 s), or `could not install <path>: <message>` when the installed copy cannot be read after the move or the move into place fails, ends with `; the previous gate.exe is back in place` or `; the previous gate.exe is at <aside>`.

On a first install it ends instead with `; the copy was removed and no earlier gate.exe existed` or `; the copy at <path> could not be removed: <message>`. A first install whose move of the temporary copy into place fails with no gate.exe there says `could not install <path>: <message>; nothing was installed and no earlier gate.exe existed`.

The move back acts only while `gate.exe` is missing or still this push's own file, the one with the temp copy's NTFS file id (`FILE_ID_INFO`: the volume serial number and the 128-bit id), which the script reads from the handle that wrote the temp copy and which the rename into place keeps; the hash does not decide it.

An id that cannot be read is tried three times, 300 ms apart, and a file gone at any try or after the last counts as missing, never as another's. The script reads the id through a small C# type it adds with `Add-Type`, which costs about 0.3-0.5 s on a push that installs; the same-exe path never adds it. The move back never overwrites a `gate.exe` that is missing.

A `gate.exe` another process placed meanwhile, a by-hand install say, is left as it is and the reason ends with `; another gate.exe was placed meanwhile and was left as it is`, followed on a push that moved an old gate aside by `, the previous gate.exe is at <aside>`.

Builder text is escaped as the remote client escapes it and cut to 4096 bytes.

Each builder is one ssh call, run in a kill-on-close job with stdin piped and bounded at 300 s.

A timeout stops only the local ssh, so the builder's install may still finish.

The call is `pwsh -NoProfile -NonInteractive -OutputFormat Text -EncodedCommand <bootstrap>` through the transport's ssh words ([Routing](#routing)), with `-o RequestTTY=no` ahead of them so stdin never passes through a pseudo-terminal.

The bootstrap is a short fixed PowerShell line that reads stdin's first line and runs it as the install script, so the call stays within the 8127 characters cmd takes (8191, less 64 for sshd's own `cmd.exe /c`, `%LOCALAPPDATA%` counted as 100) however long the script grows, which a unit test checks.

Stdin is line 1, the install script as base64 of its UTF-8 with this PC's SHA-256 put in, then the exe as base64 text of its gzip (flate2's best level). The push call is not asked for ssh compression: the bytes are already compressed.

The builder needs pwsh 7.2 or later, whose .NET the file checks need; an older one fails the push with `the builder needs pwsh 7.2 or later (it has <version>)` before anything is touched. The script decompresses the copy with .NET `GZipStream`, carries this PC's SHA-256 of the exe and refuses decompressed bytes that differ, reports on `gate-push: ` lines that this side maps to the `push:` lines, and exits 1 after a failure.

Two pushes to one builder at once are refused: the script holds `push.lock` in the builder's `%LOCALAPPDATA%\gate\bin` open, unshared, for the whole install, so the second push fails at once with `another gate push is installing on this builder`, touching nothing, the cleanup of aside copies included.

The open handle is the lock: `push.lock` stays as a plain file after a push and never blocks a later one, and a `push.lock` that is a link, a folder, a junction or a second name of another file is refused with `<dir>\push.lock is a link`: the lock is opened without following a link and the opened handle is checked.

`tools/install.ps1` and the PC side of `gate push` take the same `push.lock` in their own bin folder with the same open, so an install, a push's read of the exe and a push to a builder never change or read one folder's gate.exe at once ([Install](#install)).

On the builder the script installs as `tools/install.ps1` does ([Install](#install)), in its own `%LOCALAPPDATA%\gate\bin`, which it creates when missing and refuses when it is a link: it first deletes the stale `gate.exe.<pid>.tmp` copies of other pids that no process holds, and the aside copies (`gate.exe.*.old`) only after the installed gate is verified.

It says the same version when `gate.exe` already has the same SHA-256; else it writes the exe to `gate.exe.<pid>.tmp` (created new), moves the old one aside to `gate.exe.<stamp>.<pid>.old`, moves the temporary file into place without force, checks the SHA-256 of the installed copy and that it answers `--version` with `gate <version>`, and puts the aside copy back when anything failed, the check's own read included; with no old exe to move back, it removes the installed copy.

An installed gate is verified when its SHA-256 matches and `--version` answers, and only then does the cleanup run: it deletes every aside copy no process holds but this push's own, which stays as the last good gate. A push that ends with `gate.exe` missing or unverified, a failed one and an identical exe that does not answer included, deletes no aside copy.

Each version read on the builder is bounded at 30 s.

The ssh a push runs is `ssh.exe` found along `PATH`, never `ssh.cmd` or `ssh.bat`. Test-seams builds accept hidden options on `push`: `--test-push-ssh <path>` (the program each builder's call runs in place of the ssh found along `PATH`) and `--test-push-bound-ms <n>` (the 300 s bound, in milliseconds above 0). `gate push` has no `--test-print-request`.

### `gate check`

`gate check` takes no words but `--builder <host>`; its usage line is `       gate check [--builder <host>]`, after `gate push`. With `--builder` it checks one builder instead of this PC ([With `--builder`](#with---builder)).

A word prints `gate: unexpected argument <word>` and any option, `--state-dir` included, prints `gate: unknown option <opt>`, each followed by the usage, exit 2. It says what this PC lacks for the repo the working folder is in, and how to install it; it installs nothing and runs only version commands (each tool's own, and `dotnet --list-sdks`).

The repo is the git worktree root of the working folder, as routing finds it.

Outside a git worktree it prints `check: not in a git repository` on stderr and exits 1; a working folder or git that cannot be read prints `check: <why>` on stderr and exits 1. The repo's `.gate/toolchain.toml`, `.gate/siblings.toml` and root `global.json` are read from the worktree on disk, so an uncommitted edit counts.

The two `.gate` files are read as routing reads `.gate/siblings.toml`: a `.gate` folder or file that is a link or junction, or a file over 64 KiB, is refused. `global.json` is read the same way, a link refused and 64 KiB the most.

A file that cannot be read whole (not one that reads with problems) fails the check: the summary says it and the exit is 1, however the items came out.

On stdout, each line prefixed `check: here: `, `here` being this PC, in this order:

1. The header `check: here: <repo> at <root>`, `<repo>` being the name routing gives the repo and `<root>` the worktree root as git spells it. With no `.gate/toolchain.toml` the line `check: here: <repo> declares no toolchain` follows; the SDK pin and the siblings are still checked.
2. One line per item, every item printed, the ones that are fine included: the manifest's tools in manifest order, then the .NET SDK, then the workloads, then the `remote.json` item when it applies, then the siblings.
3. One `install: here: <subject>: <command>` hint line per missing or too-old item, in item order.
4. The summary: `check: here: nothing missing` or `check: here: <n> missing, <n> too old`, with `, <n> skipped` added when any item was skipped; with nothing missing and some skipped it is `check: here: nothing missing (<n> skipped)`. When a file could not be read it is `check: here: could not read <files>; <n> missing, <n> too old`, `, <n> skipped` added as above, the files comma separated in the order `.gate/toolchain.toml`, `.gate/siblings.toml`, `global.json`.

The problems of the files go to stderr, one per line, before the item lines: `check: .gate/<file>: <problem>`, `<file>` being `toolchain.toml` or `siblings.toml` and `<problem>` the text the file's reader gives ([Toolchain manifest](#routing), [Sibling worktrees](#routing)), or why the file could not be read at all (`.gate is not a folder`, for the manifest, when `.gate` is a file). A file with problems is still used for what reads, and the rest is still checked.

The `global.json` ones are `check: global.json: <reason>`, `<reason>` being `is larger than 64 KiB`, `cannot read it: <error>`, `is a link, a junction or not a file`, `is not UTF-8`, `does not parse as JSON`, `its sdk is not an object`, `its sdk.version is not a version dotnet reads`, `its sdk.rollForward is not a policy gate knows` or `its sdk.allowPrerelease is not true or false`.

The tool lines, for each tool of the manifest, probed as the box probes the tools of a preflight ([The box side](#the-box-side), Tool probes), all together within one 10 s bound, on this PC's own `PATH` with the worktree's own folders dropped, a program found that still lies inside the worktree refused:

- `<tool> <found> ok (needs <min> or later)`, `<min>` written as `major.minor.patch`, when the found version is the minimum or later, and `<tool> <found> too old (needs <min> or later)` when it is below it;
- `<tool> missing (needs <min> or later)`, or `(needs it present)` for a tool declared with `""`;
- `<tool> <found> ok` for a tool declared present at any version, and `<tool> ok (version not readable)` for a present one whose version the probe could not read;
- `<tool> skipped: found, version not readable (needs <min> or later)` when a minimum is declared and the tool answered with no version;
- `<tool> skipped: <why>` when the probe gave no answer, `<why>` being the probe's own reason (`no answer in 10 s`, `cannot run <program>: <error>`, and for `godot` the reasons of its probe, such as `cannot run <install>: <install> lies inside the worktree` for a `GATE_GODOT_ROOT` inside the repo, [Tool probes](#the-box-side));
- `tools.<name> skipped: not a tool gate knows` for a name the manifest declares that gate has no probe for, which runs nothing.

The SDK line comes only when the root has a `global.json` with an `sdk.version`; one with no `sdk` key, or an `sdk` with no `version`, pins nothing and prints none.

With a pin that names a version it is `.NET SDK <version> (rollForward <policy>) ok` when one of this PC's SDKs serves the pin by the rules of the parity check ([Parity](#routing)), else `.NET SDK <version> (rollForward <policy>) missing (this PC has <list>)`, the list being the SDKs `dotnet --list-sdks` gives or `none`; the words are the parity check's, so the PC's and a builder's lines agree.

The SDK list is read with the folders inside the worktree dropped from `PATH`. A `global.json` whose pin cannot be read (a reason above) says `.NET SDK skipped: global.json has no readable sdk pin`, which counts as skipped, and the file counts as unreadable; a list that cannot be read `.NET SDK skipped: cannot read this PC's SDK list: <why>`.

A manifest that lists workloads gets one line, `workloads <ids, comma separated> not checked yet`, which counts as neither skipped nor missing. The tools in `.config/dotnet-tools.json` and a `Godot.NET.Sdk` pin are not checked and say nothing.

Each sibling the repo declares is checked as routing checks it, and gets one line:

- `sibling <name> not allowed in remote.json`, counted missing, when this PC's `remote.json` does not allow the repo that sibling (`siblings`, [remote.json](#remotejson)); its folder is not looked at. Consent is asked only when `remote.json` is usable and lists the repo; with no usable file, or one that does not list the repo, routing would never carry a sibling and every sibling is checked for its folder alone.
- `sibling <name> ok (<path>)` when its folder, `../<name>` of the worktree root as routing spells it, is a real folder with a `.git` (a folder, or the file of a linked worktree) that is no link or junction.
- `sibling <name> missing (no git worktree at <path>)` when nothing is there or it holds no `.git`.
- `sibling <name> <reason>`, counted missing, for a refused folder, worded as routing words it: `is a link or a junction`, `is not a folder`, `has a .git that is a link or a junction`, or `is the worktree's own folder` for a name that is the root's folder name in any ASCII letter case.
- `sibling <name> skipped: <why>` when its folder cannot be read.

When the repo declares siblings and `remote.json` lists the repo but its `siblings` value cannot be used, the line `check: here: remote.json: <problem>` (control characters escaped, counted missing) comes before the sibling lines, and every sibling is not allowed. A repo that declares none says nothing of it.

The install hints, for the items that have one:

| Item | Hint |
|---|---|
| `rust` | `winget install Rustlang.Rustup`; with a minimum, `rustup default <min>` when rust is too old, and `winget install Rustlang.Rustup, then rustup default <min>` when it is missing |
| `cargo-nextest` | `cargo install cargo-nextest --locked` |
| `node` | `winget install OpenJS.NodeJS` |
| `pnpm` | `corepack enable pnpm` |
| `pwsh` | `winget install Microsoft.PowerShell` |
| `ffmpeg` | `winget install Gyan.FFmpeg` |
| the .NET SDK | `.NET SDK: winget install Microsoft.DotNet.SDK.<major>`, the pin's major |
| a missing sibling | `sibling <name>: clone it to <path>` |

A tool not in the table (`godot`) gets no hint line, nor does a sibling that is not allowed or refused, nor the `remote.json` item.

The exit is 0 when nothing is missing or too old and every file read, whatever was skipped; 1 when anything is missing or too old, or the check could not run (not in a git repository, an unreadable `.gate/toolchain.toml`, `.gate/siblings.toml` or `global.json`, `.gate` not a folder); 2 for a usage error. Test-seams builds accept the hidden `--test-probe-limit-ms <n>` (the 10 s probe bound, in milliseconds above 0), which `--builder` refuses.

#### With `--builder`

`gate check --builder <host>` says what one builder lacks for the repo in the current folder. It needs one host (`gate: --builder needs a value`; given twice, `gate: --builder is given twice`), which must be a builder `remote.json` lists (`host` or `hosts`), matched ignoring ASCII case and printed in `remote.json`'s spelling. Any other host prints `gate check: <host> is not a builder remote.json lists` and exits 2.

The repo is found as for the local check (outside a git worktree is `check: not in a git repository`, exit 1). A `remote.json` that is missing, ignored or cannot be found is a stderr line with exit 1: `check: <host>: remote.json not found at <path>`, `check: <host>: remote.json is ignored: <reason>` or `check: <host>: remote.json cannot be found: LOCALAPPDATA is not an absolute folder`.

It runs the box's preflight once over ssh with this repo's real work-folder key and the manifest's tools, bounded at 120 s, with no retry and no PC-side lock. A routed run of the same worktree running at the same time may collide with it (GATE-270). The preflight creates the same folders a routed run's preflight would.

On stdout, each line prefixed `check: <host>: `, in this order:

1. The header `check: <host>: <repo> (work folder <key>)`.
2. `check: <host>: created <folder>` for each folder the preflight made, at most 20, then `check: <host>: and <n> more created folders`.
3. The items, worded as on this PC with `<host>` for `here`: the SDK reads `.NET SDK <version> (rollForward <policy>) missing (<host> has <list>)`, the list cut at 20 entries and then `, and <n> more`; `.NET SDK skipped: <host> sent no SDK list`, with `: <last stderr line>` when there is one; a tool that did not answer is `check: <host>: <tool> skipped: the builder gave no answer`; the workloads are `not checked yet`.
4. The install hints `install: <host>: <tool>: <command>`: `cargo install cargo-nextest --locked`, `npm install -g pnpm@<min>` (`npm install -g pnpm` with no minimum), `rustup toolchain install <min> && rustup default <min>`, `dotnet-install.ps1 -Version <v>`, and `run the build-box toolchain script on <host>` for node, pwsh, ffmpeg and a rust with no minimum.
5. The summary.

When the repo's `.gate/siblings.toml` names a sibling or cannot be read, one line `check: <host>: siblings skipped: a routed run lays them out` comes instead of the sibling lines and counts as skipped.

A failure is one stderr line, exit 1, and nothing else on stdout:

- `check: <host>: unreachable: <ssh's last error line>` for ssh status 255, and on the timeout `check: <host>: unreachable: no answer in <limit>`;
- `check: <host>: no answer in <limit>, and ssh could not be stopped: <error>`;
- `check: <host>: runs gate <theirs>, this PC runs <ours>; run gate push`, a version mismatch printing only this line;
- `check: <host>: has no gate box that takes this call (status <n>); run gate push`;
- `check: <host>: preparing <root> failed (status <n>): <line>`, `<root>` being the builder's work root;
- `check: <host>: cannot run ssh: <error>`, and `check: <host>: <why>` for any other failure of the call.

Text from the builder is cut to 4 KiB and escaped. The exit is 0 when nothing is missing, 1 when something is missing or the check could not run (not in a git repository, unreachable, a different build, no box, `remote.json` missing or unusable), 2 for a usage error.

Test-seams builds accept, with `--builder` only, `--test-remote-transport`, `--test-remote-ssh` (which also needs `--test-remote-transport`) and `--test-remote-timeout-ms`, as the routed client reads them ([Black-box suite harness](#black-box-suite-harness)).

### `gate burn`

`gate burn [--cores <n>] --seconds <s>` is the CPU burn of `~/.claude/tools/load-burn/load-burn.ps1`, which runs it inside a critical gate and applies the owner's 120 s burn cap itself. One process spins `n` threads on a busy loop at the priority it inherited, for `s` seconds. It takes no slot and writes no claim, journal line or log; `GATE_STATE_DIR` and `--state-dir` do not apply.

- `--seconds` is required and a whole number from 1 to 180 (the 120 s burn cap that load-burn.ps1 enforces plus 60 s of margin, so `gate burn` itself allows up to 180 s); a larger value, a huge one included, is refused.
- `--cores` is a whole number above 0, at most this machine's logical processors, and defaults to all of them.
- Usage errors, each on its own `gate: ` line before the usage, exit 2: `gate: missing --seconds <seconds>`, `gate: --seconds must be a whole number from 1 to 180, got '<v>'`, a `--cores` that is not a whole number above 0, `gate: --cores must be at most this machine's <n> logical processors, got <m>`, an unknown option, a word that is not an option, and an option given twice.

It says `gate: burn: <n> thread(s) for <s> s` on stdout once, when the threads are running, and exits 0 at its deadline.

It also ends when the process that started it ends: `gate: burn: the parent process <pid> ended` on stderr, exit 1. A parent it cannot watch is exit 1 on stderr: `gate: burn: the parent process <pid> is not running` (also when the pid was reused by a process that started after the burn did), `gate: burn: cannot open the parent process <pid>: <why>`, `gate: burn: cannot wait for the parent process <pid>: <why>`, or `gate: burn: this process is not in the process table, so its parent is unknown`.

**`gate run --burn`** (GATE-160), planned, not built: `gate run --slot critical --burn [--cores <n>] [--owner-approved] --timeout <s> --log <path> -- <command...>` takes over the parts load-burn.ps1 supplies around `gate burn` (the slot, the command, the cap), so that gate.exe is the only path.

- `--burn` needs `--slot critical`; with any other slot it is a usage error (exit 2). Once the critical slot is held, the burn starts beside the command at Normal priority and stops when the command ends, on every kill path too (stall, ceiling, Ctrl+C). `--cores` defaults to every logical processor and is capped at the machine's count, as in `gate burn`.
- The 120 s cap moves into the gate: with `--burn`, a `--timeout` above 120 is refused with exit 2 before any slot is taken, unless `--owner-approved` is passed. `--owner-approved` means the owner said yes to that one burn; a brief or a profile never stands in for it, so no configuration turns it on.
- Two lines frame the command: one before it saying how many burn threads run on how many logical cores, one after it saying the burn stopped. A burn still running after the stop turns a passing run into exit 1.
- The PowerShell spelling (`-Burn`, `-Cores`, `-OwnerApproved`) is accepted by the compatibility layer while the launcher's form stays.
- Plain `gate burn` stays as the subcommand `gate run --burn` builds on: one process that spins the CPU, takes no slot, runs no command and enforces no 120 s cap (its limit is the 180 s above).

### `gate expect`

`gate expect [--state-dir <path>] [--slot heavy|light|critical] -- <command> [args...]` prints how long the command would take in the current folder, from the durations store ([design/gate-durations.md](design/gate-durations.md)). Its usage line is listed after `gate burn`. It is a pure reader: it queues for nothing, takes no slot or mutex, writes no journal line, and neither writes nor rebuilds the store.

- `--state-dir` and `GATE_STATE_DIR` resolve the [state root](#state-root) as for `gate stop-tree`.
- `--slot` defaults to `heavy`; `critical` keys the `crit` class. `none` and any other value are refused: `gate: --slot must be heavy, light or critical, got '<value>'`, then the usage, exit 2.
- Other usage errors, each on its own `gate: ` line before the usage, exit 2: `gate: missing the command after --`, `gate: --slot is given twice`, `gate: --state-dir is given twice`, `gate: unknown option {word}` and `gate: unexpected argument {word}`.
- It keys the command as a `gate run` of the same words in the same folder would: the same git call with the folder-shape fallback, the same temp root, class and exact-then-family estimate. The one difference is that it keys the words as given, so a dotnet `--no-build` that a local run would drop is keyed with it.

It prints one line per place with an estimate on stdout, the local one first, then each builder in host order: `gate: expected about {n} {s|min} (usual {p50}, p90 {p90}, {runs} runs[ of similar commands]) here`, and for a builder the same text ending ` on {host}`. It exits 0.

`no history` on stdout, exit 0, when no place has an estimate, when the store is missing, and, silently, when it is of an older or newer key version. A torn or unreadable store also prints `no history` and exits 0, with `gate: cannot read {path} ({reason}); expected times are unavailable` on stderr. A working directory that cannot be read prints `gate: cannot read the working directory: {error}` on stderr and exits 1.

The launcher maps no `-Expect`: the compatibility layer refuses it, and callers run `gate.exe expect -- <command>` directly.

### `gate warm`

`gate warm [--cd <folder>] [--background] [--state-dir <path>]` warms the builder's work folder of the worktree the gate is started in: it runs the commands of the worktree's `.gate/warmup.toml` there and keeps how that ended in the worktree's warm-up record.

It takes no command, `--log`, `--timeout` or `--local`: the commands come from the file, the log is `remote\warm\<key>.log` and the ceiling is the file's `timeout`. A warm-up takes no slot and writes no journal event. It runs the warm-up even when the work folder is already warm.

- `--cd <folder>` enters the folder as `gate run --cd` does ([Run folder](#run-folder)), with the same refusals, which exit 2. Whatever folder of the worktree the gate starts in, the warm-up is worded and laid out from the worktree root (the gate makes the root its working folder), so its commands, their path rewrite and its work folder are those of a routed run started at the root, and the call's subfolder is empty.
- `--background` keeps a warm-up that does not start quiet: no consent, no warm-up file and a warm-up already running say nothing and exit 0. It detaches nothing; whatever starts a warm-up in the background does that. Nothing starts one from a route yet.
- `--state-dir` and `GATE_STATE_DIR` resolve the [state root](#state-root) as for `gate stop-tree`.
- A test-seams build also takes `--test-warm-wait-ms <n>`, which replaces the file's ceiling as the length of the key mutex wait and of the cap wait, and the remote client's seams a routed run takes (`--test-remote-client`, `--test-remote-transport`, and the others that need the transport), but not `--test-print-request`.
- Exit codes: 0 warm (also, with `--background`, a warm-up that did not start for no consent, no file or a warm-up already running); 2 a usage or precondition problem; 1 a warm-up that failed or was refused, a failed record write, and, without `--background`, a warm-up already running; `0xC000013A` on Ctrl+C. A word other than the options above is a usage error, exit 2.

It works in this order, each step ending the warm-up where it fails.

1. The state root, then `--cd`, then the worktree root, then `remote.json`, then the repo's consent (`warmup` in `remote.json`; one that cannot be read is no consent), then the routing target (a repo `remote.json` does not list, or siblings that cannot be used, end here), then the warm-up file's first read: through the `.gate` reader, parsed and hashed, each command worded for the builder as a routed run words it (`builder_words`). This read gives the hash of the first `warming` record only.
2. The warm-up lock `remote\warm\<key>.lock`, taken for the warm-up's whole life: created when missing, opened without sharing and without following a link, required to be a plain file, its open tried again every 20 ms for 200 ms while another handle holds it, which rides out a route's probe. A lock still held ends the warm-up without touching the record. In the foreground it says `gate: a warm-up of <host> is already running` and exits 1; with `--background` it says nothing and exits 0.
3. The warm-up's log `remote\warm\<key>.log`, made new: an earlier one is removed, then the new one is created as a plain file without following a link and held open sharing reading and writing but not deletion, so nothing can put a link in its place while the client appends to it by its path.
4. The `warming` record, with the first read's hash, this process's pid and start and the PC gate's build id.
5. The remote client chosen and `ready()`, then the key's mutex waited for up to the file's ceiling (the client's `hold_key_waiting`), then the cap lock `remote\warm.lock` waited for up to the same length in slices of 100 ms, looking at the console interrupt between them.

   A key or cap not got in time ends the warm-up `failed`, class `refused`, with the reason `another routed run held the work folder past the wait` or `another warm-up held the warm-up cap past the wait`. A Ctrl+C during either wait deletes the record.

   A client that is not ready ends it `failed`, class `refused`, with the client's line as the reason.
6. The late recheck, in this order: `remote.json` read again, consent, the builder host, the routing entry, the siblings, then the warm-up file read again. The commands, the `WarmHash` and the final record come from this read alone, not from the first. A recheck that finds one of them gone or changed withdraws the warm-up: the record is deleted, the builder is not called, and the end says its line (quiet with `--background`, except a file problem or a target problem, which say their line).
7. The client's warm call ([Warm-up call](#routing)) from the worktree root, with the late read's commands and hash and the late file's `timeout` as its ceiling.

   The record is written from how the client says the warm-up ended alone, never from its status: `warm` for a confirmed warm-up, `failed` with the client's class and reason, deleted for a Ctrl+C (exit `0xC000013A`), and `failed`, class `refused`, with the client's detail (else `the client did not start the warm-up`) for a call that said nothing of its end.

   A final write that fails says its line, deletes the `warming` record and exits 1. Its cap lock, key mutex and warm-up lock are let go last.

The key mutex wait, the cap wait and the client's backstop on the run call (five ceilings and 60 s) bound the warm-up's life at about seven ceilings plus 60 s and the git limits; it has no deadline of its own, and a stale `warming` record counts as live only while its lock is held.

The lines, each on stderr and escaped to one line where it carries a path, a repo, a host or a reason:

- Ends before the record, exit 2: `gate: warm: LOCALAPPDATA is unset or not absolute, so there is no remote.json`, `gate: warm: <path> does not exist`, `gate: warm: <path> cannot be used: <reason>` (both for `remote.json`), `gate: warm: the folder is not in a git worktree`, `gate: warm: cannot find the git worktree: <why>`, `gate: warm: cannot enter the worktree root <path>: <error>`, `gate: warm: <repo> is not listed in remote.json`, and the target's siblings line.
- No consent, exit 2 (quiet with `--background`, exit 0): `gate: warm: <repo> has not consented to warm-ups in remote.json` and `gate: warm: remote.json's warmup cannot be read: <reason>`.
- No file, exit 2 (quiet with `--background`, exit 0): `gate: warm: <root>\.gate\warmup.toml does not exist`.
- A file problem, exit 2: `gate: warm: .gate\warmup.toml <problem>` (the first problem, then ` (and <n> more)`), `gate: warm: .gate\warmup.toml command <n> starts with <word>, which a routed run never sends` (a command a routed run keeps on this PC) and `gate: warm: .gate\warmup.toml command <n> names <word>, outside the worktree`.
- Withdrawn at the recheck, exit 2 (quiet with `--background`, exit 0): `gate: warm: remote.json's builder changed from <host> to <host>`, `gate: warm: remote.json's entry for <repo> changed` (also for a changed work folder key or inputs), `gate: warm: the siblings of <repo> changed while the warm-up waited`, and the consent and file lines above.
- Ends that write no record, exit 1: `gate: warm: cannot make <folder>: <error>`, `gate: warm: cannot take <lock>: <why>`, `gate: warm: cannot remove the earlier the warm-up log <path>: <error>`, `gate: warm: cannot create the warm-up log <path>: <error>`, `gate: warm: cannot use the warm-up log <path>: <error>` and `gate: warm: cannot write the warm-up record <path>: <error>`.
- `gate: warm: cannot delete the warm-up record <path>: <error>` joins the line of a final write that failed or a record a withdrawal could not delete.
- `gate: cannot install the Ctrl+C handler, so Ctrl+C ends the gate at once: <error>`, and the verdicts: `gate: warm-up of <host> passed` (exit 0) and `gate: warm-up of <host> failed: <reason>` (exit 1). The client's own lines and the box's go to the console and the warm-up's log as a run's do.

### `gate box`

Hidden from the usage and from `--help`. It is the builder side of a routed run, run over ssh on the builder ([The box side](#the-box-side)).

`gate box preflight|run|stop|boot|outputs|inflight --stdin` takes exactly one verb and the word `--stdin`, and no `--state-dir`: the request is the first line of stdin, and every path comes from it. Any other shape (no verb, no `--stdin`, any other word in its place, an unknown verb, an extra word) prints `usage: gate box preflight|run|stop|boot|outputs|inflight --stdin (the base64 JSON request as stdin's first line)` alone on stderr and exits 2.

### Later subcommands

Further modes arrive as subcommands of the same binary, each designed with its issue: `gate status` and `gate drain`. New options, such as `--timeout auto:<n>` (GATE-16) and `--slot auto` (GATE-8), exist only in the new CLI, never in the compatibility layer. The one exception is `--cd` (GATE-323), which the compatibility layer reads as `-Cd` because the launcher's callers need it.

### Compatibility layer

`gate.exe` also accepts the PowerShell gate's argument form, so the cutover breaks no caller:

`gate -Log <path> -TimeoutSeconds <n> -Slot heavy|light|critical|none [-StallSeconds <n>] [-Tail <n>] [-NoMarkers] [-Local] [-Cd <folder>] -- <command>` and `gate -StopTree <pid>`.

- It accepts exactly the options `-Log`, `-TimeoutSeconds`, `-Slot`, `-StallSeconds`, `-Tail`, `-NoMarkers`, `-Local`, `-Cd` and `-StopTree`, compared case-insensitively, and maps each to the flag in the table above (`-StopTree <pid>` to `gate stop-tree <pid>`).

  Any other option, or a new-CLI flag in the old form (`-StateDir`, `--state-dir`, `--slot`), is a usage error (exit 2). `-TimeoutSeconds` refuses `auto:<n>`: `gate: -TimeoutSeconds must be a whole number above 0, got 'auto:900'`. The one exception is the hidden `--test-print-request` of a `test-seams` build ([Black-box suite harness](#black-box-suite-harness)).
- `--` is required before the command; the command never starts at the first word that is not an option.
- `-StopTree <pid>` (or `-StopTree:<pid>`) must be the only option.
- A value option never takes one of the nine option names as its value: `-Log -NoMarkers` is `-Log` with no value, then `-NoMarkers`.
- The colon form `-Name:value` in one word is read as that option with that value (C2). A switch's colon form is read by its value, compared case-insensitively: `-NoMarkers:true`, `-Local:$true` set it, `-Local:false`, `-Local:$false` clear it, and any other value is a usage error. This fixes the PowerShell gate's reading of every `-Switch:<value>` as true (C2, CC-194).
- Values are validated as in the new CLI; the messages may name either spelling. The missing-slot line reads `gate: missing --slot heavy|light|critical|none` or `gate: missing -Slot heavy|light|critical|none`, and a bad slot says `gate: <option> must be heavy, light, critical or none, got '<value>'`.
- The layer is removed once no caller uses it. Until then, every reader of command-line text (ops-tower's activity join, the hooks that inspect a gated command) matches both spellings.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | The command exited 0 and its log holds no failure marker. `gate check` found nothing missing or too old (with `--builder`, nothing missing). |
| `N` | The command's own non-zero status, passed through whole: the gate exits with the command's 32-bit exit code (300 stays 300; 0xFFFFFFFF reads as -1 where a caller reads it signed), and the FAILED line and the journal show it signed (a routed run whose connection was lost reports 255, RC7). |
| 1 | The command exited 0 but its log holds a failure marker (never with `--no-markers`, V2); or the gate itself failed after the command started (X4); or it failed before queueing (the working directory, the command's environment, the log's folder or an earlier log that cannot be removed); or a routed run passed but its changes or outputs could not be brought back (RC9, D1); or `gate check` found something missing or too old, or could not run (outside a git worktree; with `--builder`, also an unreachable builder, a different build, no box, or a `remote.json` missing or unusable). |
| 2 | Usage: a missing, unknown or invalid input; a `.cmd` or `.bat` word the gate refuses (C5); a `--log` inside the state root; a critical gate under a heavy or light slot marker (K7). Nothing is queued. `gate box` also uses 2 for a refused payload and for a refused subfolder or batch word ([The box side](#the-box-side)). |
| 124 | The watchdog killed the command: STALLED, TIMED OUT or BACKSTOP (X2), for a routed run by the builder (TIMED OUT, BACKSTOP) or the client (STALLED, BACKSTOP); or a routed run's fetch outlived its transfer ceiling: TRANSFER TIMED OUT. |
| 127 | The command could not be started (C7); the slot is released at once. `gate box run` uses it for the same failure. |
| 75 | A builder refused a routed run (`gate box`): busy (`gate box run` finding its work folder's lock held), unreachable, parity, rebooted, tools, layout (the box could not clear or lay out its work folder), version. The PC side reads the refusal and falls back to a local run, so a caller sees 75 only as a command's own status, or as a refusal that carried no reason (T5). |

The codes are kept (V4), with 75 added for refusals. 124 is the status coreutils `timeout` uses, so no caller mistakes a kill for the command's own status; a command that itself exits 124 is told apart in the journal's `verdict`.

## Output lines

Every line the gate says is written and flushed as it is said, never held until exit, so a running or killed gate's lines are already visible. Every line the gate writes for itself starts `gate: ` and goes to stderr and to the end of the log (V1), except the passed line, the stop-tree summary and the `gate burn` start line, which go to stdout (V3, X6). Three forms are the published interface that callers may parse; they keep their wording, and a change to them is a breaking change:

| Line | Where | Meaning |
|---|---|---|
| `gate: STALLED: ...`, `gate: TIMED OUT ...`, `gate: BACKSTOP: ...` | at line start in the log and on stderr | The watchdog killed the command; the exit is 124. Match `^gate: (STALLED\|TIMED OUT\|BACKSTOP)`. A routed run's builder says `gate: TIMED OUT on <host> after ...` and `gate: BACKSTOP on <host>: ...` ([Routing](#routing)). A gate wrapping a gate reports the inner kill through its own marker scan (V2). |
| `gate: TRANSFER TIMED OUT after <n> s fetching the outputs` (or `fetching the sync-back`) | at line start in the log and on stderr | A routed run's fetch after the command outlived its transfer ceiling of 1800 s, counted from the fetch's own start; the exit is 124 and the release verdict `transfer-timed-out`. It is a failure marker of the log scan; it does not match the `TIMED OUT` of the command's ceiling. |
| `gate: waiting for the machine (critical)` | stderr | A critical gate is queued and waiting for the machine (K5); the stable part is the prefix, and the status after it is prose. A critical gate that takes the machine at once says nothing. |
| `Full output: <path>` | the end of the verdict line: `gate: passed in ... Full output: <path>` on stdout, `gate: FAILED (status <n>). Full output: <path>` on stderr | Where the whole log is (V3). |

Every other `gate: ` line is prose: it informs people and agents and may change in any release. That includes the passed and FAILED lines apart from their `Full output:` tail, the wait lines, the heartbeat lines, the diagnostics block and the stop-tree summary.

The lines of a run that starts its command:

- Passed, on stdout: `gate: passed in <w> s (load-adjusted <a> s, ceiling <n> s). Full output: <log>`, `<w>` and `<a>` in whole seconds rounded half to even. Once the sampler failed there is no load-adjusted time, and the line reads `gate: passed in <w> s (load-adjusted unknown: the sampler failed, ceiling <n> s). Full output: <log>`.

  A command that ends with status 0 after its clock reached the ceiling (a gate starved between samples can miss the kill) still passes, and the line names the ceiling it ran past: `ceiling <n> s` becomes `past its ceiling of <n> s`. The clock is the one the ceiling kill reads, the load-adjusted time, or the wall time once the sampler failed, compared as the kill compares it (reached or passed).
- The cores hint, on stdout just before the passed line, when its conditions hold ([Cores hint](#cores-hint)): `gate: used <avg> cores on average (peak <peak>, <free> free) on a light slot; work that keeps more than 2 busy belongs on heavy` or `... on a heavy slot; work that keeps under 1.5 busy fits on light`.

  Two notes go to the log only, and the hint is left out: `gate: cannot read the command's CPU at its end: <error>; its cores are unknown` and `gate: cannot tell whether a gate under this one took a slot of its own: <error>; its cores are unknown`.
- The cores hint of a run on no slot, in the same place and under the same conditions, has no free cores: `gate: used <avg> cores on average (peak <peak>) on no slot; work that keeps 1.5 or more busy belongs on light or heavy`.
- Failed, on stderr: `gate: FAILED (status <n>). Full output: <log>`. A command that exited 0 with a marker in its log first says `gate: command exited 0 but its log reports a failure -> <log>` on stderr, and its status is 1.
- On a non-zero status, up to 40 `<line number>:<line>` marker lines on stdout, before the tail.
- A start failure, on stderr and in the log: `gate: could not start <word> (<resolved full path>): <system message>`, exit 127. The parenthesis is left out when the resolved path is the word itself, and a program found nowhere gets the message of error 2 with no parenthesis. `gate: cannot read the system folder, which holds cmd.exe: <error>` and `gate: cannot make <path> a full path: <error>` are start failures too.
- A run end that cannot end the job's leftovers, said at most once, on stderr and in the log: `gate: cannot end the run's job: <reason>`, the reason `the process snapshot listed no processes`, `<n> of its processes still ran after 5 passes`, or the first process that could not be opened, judged or ended (`reading the parent of <pid>` among them). The run's status and verdict stand.

  After the pass cap's reason comes `; first: <first problem>`: the first problem of the last pass that ended processes, then of the counting pass, so a terminate refused up to the cap is said; a problem of an earlier pass is not.
- A gate failure after the slot is taken: `gate: the gate failed while running the command: <message>`, exit 1. The job is terminated with 124, with its follow-up lines `gate: and could not terminate the command's job: <error>`, `gate: the command was still running 5 s after its job was terminated` or `gate: cannot wait for the command after terminating its job: <error>`.
- Failures before queueing, exit 1: `gate: cannot read the working directory: <error>`, `gate: cannot build the command's environment: <error>`, `gate: cannot create the log's folder <dir>: <error>` and `gate: cannot remove the earlier <path>: <error>`.
- A `--cd` folder the gate cannot enter, on stderr only, exit 2, before any slot and with nothing queued or journalled, the flag named as the caller spelled it (`--cd` or `-Cd`): `gate: <flag> <value> is not a folder` (nothing is there, or a file); `gate: <flag> <value> is not a folder on a drive` (a UNC, `\\?\` or device value, refused before any filesystem call); `gate: cannot run in <flag> <value>: <error>` (any other OS error, such as access denied).
- A gate that cannot read its own start, exit 1, before it writes any ticket, claim, slot marker, pause marker, wake event, covered marker or journal line: `gate: cannot read this gate's own start: <error>; not running <command>`.

  The gate reads its start a second way (`process_start_of` of its own pid, the source the sweepers judge liveness by) when the first read fails, and `<error>` is the second read's error, or `<first error>; then the process has no start` when that read finds no process start. A gate therefore always holds its real start.
- A log in use, exit 1, before queueing and with no journal line: `gate: the log <path> is in use by another gate`, when another gate holds the log's mutex ([Kernel object names](#kernel-object-names)). A mutex a killed gate left is taken silently. `gate: cannot take the mutex <name> of the log <path>: <error>`, exit 1, when the mutex cannot be opened or tried.
- Routing, on stderr and in the log, with the command running here: `gate: ignored <path>: <reason>; running locally` when `remote.json` is there but unusable (not a file, unreadable or refused), and `gate: not routed: <reason>` when a heavy gate that would route stays local.

  The second line is said for `--local` (`gate: not routed: --local`), a word outside the worktree and its declared siblings (`gate: not routed: <word> is outside the worktree`), a folder that holds nothing the snapshot sends (`gate: not routed: <sub> holds nothing the snapshot sends`).

  It is also said for a word of the command that is not valid Unicode (`gate: not routed: a word of the command is not valid Unicode`), or a fact that could not be read (`gate: not routed: cannot run git: <error>`, or the working directory or git's output).

  These lines keep a matched command here for siblings: a problem with the repo's `.gate/siblings.toml` (`gate: not routed: .gate/siblings.toml: <problem>`, control characters and reordering marks escaped), a declared sibling with no worktree beside the root (`gate: not routed: sibling <name> not found beside the worktree`), a root folder name the builder cannot lay out (`gate: not routed: the worktree folder name <leaf> cannot be laid out on the builder`).

  Also a declared sibling `remote.json` does not allow: `gate: not routed: sibling <name> is not allowed in remote.json`, or `siblings <a>, <b> are not allowed in remote.json` for more, with `: <problem>` added when the `siblings` value cannot be used.

  A sibling folder that cannot be carried says `gate: not routed: sibling <name> <reason>`, `<reason>` being `is a link or a junction`, `is not a folder`, `has a .git that is a link or a junction`, `is the worktree's own folder` or `is the same folder as sibling <other>`.

  The remote client keeps a routed run here, after the route, with `gate: not routed: <who> <why>`, status 75 and the fallback reason `client`, then `gate: remote <host> client could not start: <who> <why>; running here`. `<who>` is the worktree root's folder name for the main repository and `sibling <name>` for a sibling.

  A run of one repository can say `<leaf> is not the folder the route was decided for` (the root's folder id is not the one the route read) and `<leaf> cannot be checked: <error>`.

  A run that carries siblings holds every folder first and can say, for the root or a sibling, `is a link or a junction`, `is not a folder`, `has a .git that is a link or a junction`, `is not the folder the route was decided for`, `differs in case from its declared name`, `cannot be held: <error>` and `cannot be held: its path is not valid Unicode`.

  For the root alone: `is under a folder that is not the one the route was decided for`, `cannot be held: it is on a network volume` and `has no parent folder`.

  Once git is asked where each repository is: `<who> is not the top folder of its git repository: git names <path>`, `<who> shares its git repository with <other>` (<other> the earlier repository sharing its common git folder, by folder id), `<who> has a git folder reached through a link or a junction`, `<who> cannot be checked: <error>` (a git or common folder that cannot be opened or read).

  Then `<who> cannot be laid out on the builder: <why>` (the box's reader refused the pair; <why> is its usage line without `gate: `, the main repository's name when no one sibling is at fault), `sibling <name> cannot be sent: <why>` (a sibling's git call or push failed) and, after a save or the end of the run, `<who> moved while it was held` and `<who> changed its git location while it was held`.

  A lock another routed run holds past the lock wait says `gate: not routed: <who> is busy with another routed run`. A pair's root spelled other than it was at its check has no git location recorded, and fails the run with `gate: the remote run failed: the git location of <root> was not recorded at its check`, status 1 and no fallback.

  A gate that is not heavy, has a covering parent, matches no pattern or has no `remote.json` says nothing.
- A local run that drops its dotnet `--no-build`, on stderr and in the log, before it takes a slot: `gate: dropped --no-build: <reason>; building on a heavy slot`, or `gate: dropped --no-build: <reason>; building on the parent gate's <heavy|critical|light> slot` for a gate covered or lent a slot ([Routing](#routing)).
- A routed run, on stderr and in the log, says first `gate: routed <given> as <name>` when it sent the command's first word, which named a program by an absolute path outside the worktree, as the program's name (`<given>` the word as given, `<name>` the name sent, each escaped; [Routing](#routing), the in-worktree path rewrite). After that line and any `gate: dropped --no-build: the last build may have run on the other machine` and `gate: outputs: none brought back for a --no-build command` lines, it says `gate: routed with siblings: <name> (<path>), ...` when the repo declares siblings.

  The line names each sibling in declared order with its root as routing spells it. While it waits for a remote slot: `gate: waiting for a remote slot on <host> (<n> busy)` and `gate: still waiting for a remote slot on <host> (<s> s)`, as a local wait says them, ETA included.

  A run the builder does not take says why and goes on here: `gate: remote <host> <reason>; running here`, with `<reason>` one of `full` (no remote slot within 60 s), `busy`, `unreachable`, `parity`, `rebooted`, `tools`, `layout`, `version`, `inputs` (the run's ignored inputs could not be sent, or the builder's work folder no longer held them at the run) or `cold` (the builder's work folder was not warm for the run's warm-up file; the client says it for a call that needs a warm folder, and no route makes such a call yet), and `gate: remote <host> client could not start: <detail>; running here` when the remote client cannot start.

  A routed run whose repo declares `inputs` says `gate: inputs: <n> file(s), <X.X> MB (<Y.Y> MB compressed) in <Z.Z> s` once a put has sent files.

  Four lines go to the log only: `gate: inputs: <n> file(s) unchanged` (the manifest's entries, said when the builder needed none), `gate: inputs: <glob> matched nothing`, `gate: inputs: <path> is in the snapshot; not sent as an input` and `gate: inputs: <folder> is another repository; not sent as an input`. The reason of an `inputs` fallback is the log line `gate: remote inputs not sent: <reason>` ([Routing](#routing), Inputs).

  A remote part the gate cannot start says the second line before it takes a remote slot when ssh is missing, else after it ([Routing](#routing), The remote client's run).

  A Ctrl+C while waiting for the remote slot says `gate: interrupted while waiting for a remote slot`. One during the client's call, or raised before it, ends the gate with `gate: interrupted before the command started` and no fallback, even when the client refused the run; the remote slot is released.

  A Ctrl+C while the client waits to start ends the gate the same way with no slot to release, and journals an `abandon` after the `queue` line, not a `release`.

  A routed run that passed says `gate: passed in <w> s on <host> (load-adjusted <a> s, ceiling <n> s; command <c> s, push <p> s[, inputs <i> s][, sync-back <s> s][, outputs <o> s]). Full output: <log>`, with `past its ceiling of <n> s` in place of `ceiling <n> s` as the local line has it.

  In it `<w>` is the attempt's whole wall time; `<a>` is the load-adjusted time the builder counted for the command, the clock `--timeout` bounds; `<c>` is the run call's own wall time; the push is always named, the inputs calls (diff and put together), the sync-back and the outputs fetch only when they ran, in the order they ran, each in whole seconds rounded half to even.

  With no figure from the builder (its sampler failed) the clause reads `load-adjusted unknown: the sampler failed` and the past-ceiling check uses `<c>`. The client's status is the gate's own, passed through as `N`.
- The stale line, on stderr and in the log, before the passed line, when a routed dotnet build (verb `build`, `test`, `publish`, `pack`, `run` or `msbuild`, with no `--no-build` of its own) passed and did not bring everything back: `gate: routed build left stale: <folders>`.

  The folders are every PC-side folder (no cap) that a judging glob matches and no brought-back glob covers, judged per file: a folder whose every matching file came back is not named. The judging globs are the run's `outputs`, or the default globs when the repo's entry is absent or `[]`, plus the defaults `**/bin/**` and `**/.godot/mono/temp/bin/**`. The folders are sorted, relative to the worktree root, `/`-separated, escaped and joined with `, `.

  A pair (a run with siblings) names each sibling's output folders after the main's, written `../<sibling>/<folder>` (`../lib/src/App/bin`), judged by the same globs with nothing brought back, since no sibling's output ever comes back; the siblings follow in declared order.

  A glob names the folder where its last plain folder part ends, at the first place from the left where its plain parts match whole path components: `**/bin/**` and `**/bin/**/*.dll` name each `src/App/bin`, `out/**/App.dll` names `out`, `publish/App.exe` names `publish`; a glob with no plain folder part (`**`, `*.dll`, `**/*.dll`) names nothing. The line may name a folder this build did not touch.

  It is not said after a failed build (an exit 0 whose log reports a failure included), a fallback or a command that is not a build, and it neither writes nor clears the nothing-back record. A walk of the worktree that fails says `gate: routed build may have left stale output; could not list it: <reason>` instead, after the folders line.

  A pair's sibling whose walk fails says `gate: routed build may have left stale output in ../<sibling>; could not list it: <reason>` on a line of its own, after the folders line and after the main's failure line; the folders of the trees that could be listed are still named. A run of one repo says the same lines as without siblings.
- A critical gate, on stderr and in the log: the `waiting for the machine (critical)`, `still waiting for the machine (critical)`, `holds the machine (critical)` and `interrupted while waiting for the machine` lines, in [Critical drain and nesting](#critical-drain-and-nesting).
- Nesting, on stderr and in the log, from the slot marker in the gate's environment: the `running under` (including the lend's `while a critical request drains`), `ignored GATE_SLOT_HELD`, `could not judge GATE_SLOT_HELD` and `does not cover` lines, the barrier's `waiting for a heavy slot behind the critical request` and `waiting for a light slot beside the critical request` lines, and the refusal of a critical gate, all in [Critical drain and nesting](#critical-drain-and-nesting).

  The late take adds `the parent gate <pid> ended; ...` and `cannot watch the parent gate ...` lines, in the same section.
- The gate prints no `queued for` or `took ... slot` line, for a local, a routed or a no-slot run: the queue and the take are in the journal, and a wait longer than 30 s says `gate: still waiting for a <kind> slot (<s> s)`, with the busy count and an ETA when every slot is busy ([Slots](#slots), step 6). A no-slot run never waits.
- The expected-time line, on stderr and in the log, once a run that takes a slot has written its claim and woken the waiters and before the command's output: `gate: expected about <n> s (usual <n> s, p90 <n> s, <runs> runs)`, or in whole minutes from 90 s: `gate: expected about <n> min (usual <n> min, p90 <n> min, <runs> runs)`. A local light or heavy take says it, a critical gate after its holds line, a routed run at its remote take, and a fallback a second one at its local take.

  `about` is the mean wall time over the estimate's window (the last 30 passing runs or those within 14 days, whichever holds fewer), `usual` the median of the 5 newest, and every figure is rounded and floored at 1 s (or 1 min). An estimate from the family key ends `<runs> runs of similar commands)`. A key with fewer than 5 passing runs, a `--slot none` run, and a run a parent's slot covers or lends say nothing ([design/gate-durations.md](design/gate-durations.md), Estimate).
- The ceiling lines, on stderr and in the log, right after the expected-time line (at each take, a lent take included): `gate: ceiling 42 min (3x this command's p99 of 14 min, above the given 15 min)` only when history raised an auto ceiling above its floor, then `gate: the ceiling 10 min is below this command's load-adjusted p90 of 14 min` when the resolved ceiling, given or auto, is below the estimate's adjusted p90.

  The ceiling is in load-adjusted seconds, so the hint compares it with the adjusted p90 alone: an estimate without one says no hint, however long its wall p90.

  Their figures are the expected-time line's: `<n> s` below 90 s, whole minutes from 90 s. An estimate from the family key reads `similar commands'` in place of `this command's`, and a family key whose p90 is above 2x its p50 does not raise the ceiling.
- The hang hint, once a run, on stderr and in the log: `gate: still running after 42 min, 3x its usual p90 of 14 min` (`similar commands' usual p90` for a family key), `k` rounded down, from the run's passing 3x the p90 of its take's estimate. A local run counts wall time less paused time, checked at each watch sample; a routed run counts the wall time since the builder's start marker reached the client, said live. It is never said for a usual p90 under 1 s and kills nothing.
- The durations store's notes, on stderr and in the log. Said once a run: `gate: cannot read <path> (<reason>); expected times are unavailable` for a store that is torn or cannot be read, said before the run queues, so it can come before a waiting line, and `gate: could not write <path> (<reason>)` for a failed store write, which leaves the run's exit code as it was.

  `gate: cannot rebuild <store path> (<reason>); expected times are unavailable` when the run's claim on a rebuild fails other than by another gate holding it.

  A rebuild says `gate: <note>` for each of `skipped <n> release line(s) of <file> that do not parse`, `could not read <path>: <error>`, `could not list the archive folder <dir>: <error>` and the swap's own notes, and `gate: could not remove <marker path>: <error>` when an interrupted rebuild cannot remove its marker.
- Notes that never fail the gate: `gate: cannot remove <log>.err: <error>`, `gate: cannot remove the log <path> of the attempt: <error>` (a routed run that fell back could not delete the log the client wrote), `gate: cannot append <log>.err to the log: <error>` and `gate: cannot read this gate's priority class, so the command runs below normal: <error>`.

## Slots

### Pools and names

A slot is a named mutex in the logon session's `Local\` namespace, held by the gate for the command's whole run (kept, S1, S3). A gate killed while holding a slot is the crash recovery: the next gate takes the slot over, where a semaphore's count would be lost for good. It sees the takeover in either of two ways, the mutex coming back abandoned or the slot's claim naming a gate that is no longer alive ([Abandoned slots](#abandoned-slots)).

| Pool | Mutex names (default root) | Who takes it |
|---|---|---|
| heavy | `Local\gate-slot-<i>` | `--slot heavy`; a critical gate takes every heavy slot. |
| light | `Local\gate-light-slot-<i>` | `--slot light`; a critical gate takes every light slot but the beside allowance of its count (K2). |
| remote | `Local\gate-remote-slot-<i>` | a routed heavy run, for the length of its remote attempt (T5). |
| critical | `Local\gate-critical` | a critical gate, for its whole run, so two critical requests run one at a time in arrival order (K1). |

The pools never wait on each other, so a serial test run does not queue behind builds (S1). A gate takes and releases its mutexes on one OS thread, since Windows mutex ownership is per thread.

A `none` run is in no pool: it takes no slot, mutex, ticket, claim or covered marker, never waits, wakes no waiters and starts at once, even while a critical gate holds the machine. It is for a command that only dispatches or watches remote work (`gh run watch`, a deploy script polling a server, a remote image build).

It still has its job, so `gate stop-tree` reaches it, and it runs at BelowNormal. A critical drain does not wait for it, it is not in the take's `otherClaims`, and the barrier never holds it.

### Counts

The default count of a pool is `max(1, floor((logical processors - 1) / threads))`, with threads 4 for heavy and 2 for light (kept, S2). `slot-counts.json` in the state root overrides either count with a whole number from 1 to 64. It is re-read on every pass of the wait loop, so a count changed while gates wait applies to them, and a gate holding a slot the new count leaves out keeps it.

A missing file or key means the default; an unreadable or invalid value is ignored for that pool with one line, `gate: ignored <path>: <reason>; using <n> <kind> slots`, said once per wait and only for the waiting gate's own kind (S2). A processor count the system will not give is taken as 1, with `gate: cannot read the processor count: <error>; counting 1`, said once. The remote pool's size comes from `remote.json` (T1).

### Taking a slot

1. A gate that needs a slot creates its wake event, then writes its arrival [ticket](#ticket), both before its first try, so no wake is lost. A gate a parent's slot covers takes no slot and writes no ticket (N1). A gate its light parent lends the slot while a critical request drains (N2) writes no ticket when it is lent before it queues, only a lend marker ([Claim](#claim)); one lent as it waits has queued with a ticket, which it deletes when lent, writing its lend marker and waking the waiters.
2. Waiters of one kind take slots in ticket `arrived` order, ties broken by pid. A gate tries a slot only when fewer live tickets of its kind are ahead of it than the pool has free slots (indices whose mutex no handle holds, a probe that fails counting as not free), not counting a ticket whose gate already holds a live claim (it holds a slot and only failed to delete its ticket), so a try never takes the slot an earlier waiter would get.

   It then tries indices 0 up to the count in order and takes the first free one. A live critical ticket that arrived before the gate (by `arrived`, then pid; the barrier, Q3) narrows that: a heavy gate tries no slot, and a light gate tries any light slot, and none once as many light gates run beside the critical request as the beside allowance (K3). See [Critical drain and nesting](#critical-drain-and-nesting).
3. A take deletes the gate's own ticket, journals `take`, writes the claim and wakes the waiters, so the next in line tries at once. A release deletes the claim, journals `release`, lets the mutex go and wakes the waiters. A critical gate takes the machine instead ([Critical drain and nesting](#critical-drain-and-nesting)): its `take` and `release` list every slot it holds, and it writes one claim, `gate-critical.json`, and none for the slots.
4. A waiter sleeps on its own wake event, `Local\gate-wake-<pid>-<start>` (auto-reset). A gate that takes or releases a slot, or deletes a dead gate's ticket, signals the wake event of every live ticket in its state root. An auto-reset event stays signalled until its waiter wakes, so no wakeup is lost between a waiter's check and its wait. A wake that cannot be opened or set is not said: the fallback poll covers it.
5. Each wait has a fallback poll as its timeout (2 s, kept from S4's interval). The poll re-reads the counts and the tickets, which deletes a dead waiter's ticket so it cannot hold the queue (Q1), and picks up a count changed by hand. A pass that found the [beside mutex](GLOSSARY.md) held by another gate (a light gate's, a lend's, a critical gate's) sleeps 100 ms at most, since that gate wakes the waiters only once it took or lent something.
6. A waiter prints `gate: waiting for a <kind> slot (<n> busy)` once, on the first pass whose own [probe by open](#probe-by-open) finds none of the pool's slots free, where `<n>` is how many slots that same probe reports held, followed by `, the first frees in about <eta>` when the ETA below is known.

   A gate that waits only behind earlier tickets while a slot is free for it does not print it. A gate held back by a critical request prints the barrier line instead, on its first pass ([Critical drain and nesting](#critical-drain-and-nesting)).

   It prints `gate: still waiting for a <kind> slot (<s> s)` every 30 s from its queueing, on stderr and in the log, so a gate around it does not read the wait as a stall (kept, S4). When a probe of the last pass's slots finds every one busy, the line reads `(<s> s; <n> busy)`, with the ETA before the `)`. Its watchdog starts only once it holds a slot.

   The ETA, on the heavy, light and remote queue lines and these heartbeats, names the soonest expected end among the busy slots, `started + expected.p50` of each holder's live claim. It reads `, the first frees in about <eta>`, or once that end has passed `, the first is <over> past its usual <p50>, ceiling <ceiling>`, the ceiling being the claim's `timeoutSeconds`.

   It is left out, and the line is as without it, when any busy slot has no live claim carrying `expected` or any slot of the probe could not be probed. A gate held back by a critical request, and a gate with a free slot behind earlier tickets, prints no ETA. Each figure is `<n> s` below 90 s and otherwise whole `<n> min`, never below 1, its unit picked by itself.

   `waitedSeconds` in its `take` counts from its `queue` line; the local `take` of a routed run that fell back counts from the `fallback` line, as the PowerShell gate does, except after a `full` fallback, whose local `take` counts from the run's `queue` line (as the PowerShell gate did for it).

   After a `full` fallback the wait's schedule, its still-waiting lines and its `take` or `abandon` `waitedSeconds` all count from that one instant, the run's `queue` line, so the `(<s> s)` includes the remote wait. The first still-waiting line of a wait entering from a fallback comes at the next whole heartbeat period past what it has already waited, never at once: for a 60 s remote wait and a 30 s heartbeat, the first line comes at 90 s.
7. A problem a pass meets is said as a prose line, once per name or path per wait: `gate: could not take <name>: <error>`, `gate: cannot probe <name>: <error>`, `gate: cannot create the wake event <name>: <error>` (the gate then waits on the poll alone), `gate: cannot wait on the wake event <name>: <error>`, `gate: cannot write the ticket <path>: <error>`, `gate: cannot write the claim <path>: <error>`.

   More: `gate: cannot read the tickets in <dir>: <error>`, `gate: cannot read the claims in <dir>: <error>`, `gate: ignored the ticket <path>: <reason>`, `gate: cannot check the gate of <path>: <error>` and `gate: cannot delete the ticket|the claim|the dead gate's ticket <path>: <error>`.

   A lend marker adds `gate: cannot write the lend marker <path>: <error>` (the gate is not lent and queues), `gate: cannot delete the lend marker <path>: <error>`, `gate: ignored the lend marker <path>: its name names no gate` and `gate: cannot delete the dead gate's lend marker <path>: <error>`, the last two said once.

   A dead gate's ticket or covered marker whose delete is refused with access denied is passed over unsaid and never counts as a delete, so it wakes no waiter. The usual cause is another gate deleting the same file (a [delete pending](GLOSSARY.md), which refuses every open and delete), and that gate sends the wake. A dead gate's file that can never be deleted stays in place unsaid and is tried again each pass; it never blocks the queue, since a dead ticket is never ahead.

   A gate under a parent's slot says `gate: cannot write the covered marker <path>: <error>`, `gate: cannot rewrite the covered marker <path>: <error>` and `gate: cannot delete the covered marker <path>: <error>`, and the survey says `gate: cannot read the covered markers in <dir>: <error>` and `gate: cannot delete the dead gate's covered marker <path>: <error>`, each of the last two once; the run goes on after each.

   A gate that looks for a stand-in says the read line too ([Critical drain and nesting](#critical-drain-and-nesting)).

Ordering by ticket replaces the PowerShell gate's free-for-all among waiters of one kind (Q2), and the wake events replace its 2 s polling as the way a waiter learns of a release.

### Abandoned slots

A slot counts as taken from a gate that died holding it in either of two cases: its mutex comes back abandoned (S3), or the take is clean but the slot's claim names a pid and start that are no longer alive.

The gate says `gate: took <name>, which a killed gate had left held` on stderr and in the log, and its journal `take` line carries `abandoned`, the list of slot names it took that way, so the journal counts how often gates die holding a slot (GATE-30). An abandoned journal mutex counts as owned and is said the same way, and the line appended under it carries `abandoned` with the journal mutex's name, whatever its event.

The second case exists because a killed gate leaves its claim and Windows frees a mutex with its last handle: the abandoned result is seen only when another handle to the name was open at the kill ([Probe by open](#probe-by-open)). The dead-claim check belongs to the slot loop, and runs on a clean take of a slot only.

A claim that is missing or unreadable is no takeover: the gate overwrites it with its own. A claim is dead by the [liveness rule](#claim).

### Probe by open

A slot is held while any handle to its name exists: a reader opens the name with `SYNCHRONIZE` access and closes it at once (kept, S5). ops-tower uses the probe to show a slot held with no claim, the critical gate uses it to count light gates beside it (K3), and a waiter uses it for the busy count of its first wait line.

A name the prober may not open (an elevated gate made it with an access list this process is refused) counts as held, since the refusal proves the name exists; any other failure is the prober's error.

`gate-state`'s `Win32MutexProbe`, which the dashboard uses, reads the other way round: a mutex is closed only when no process holds the name, and access denied (an elevated gate's mutex) or any other failure counts as open, so the dashboard shows such a slot as Held.

A gate that is trying a slot holds a handle for an instant, so a probe can read a transient "held"; only waiters with a free slot for them try ([Taking a slot](#taking-a-slot)), which keeps such reads rare. The same rule bounds crash recovery: a mutex comes back abandoned only if another handle to its name was open when its owner was killed, so a take cannot rely on the abandoned result alone ([Abandoned slots](#abandoned-slots)).

## State root

One folder holds every file the gate writes: `%LOCALAPPDATA%\gate\v1\` by default, beside the install folder `%LOCALAPPDATA%\gate\bin\` and the PowerShell gate's own files in `%LOCALAPPDATA%\gate\`, so the two generations' files never mix.

`--state-dir <path>` overrides it, else the `GATE_STATE_DIR` environment variable; the flag wins over the variable. An empty `GATE_STATE_DIR` or `LOCALAPPDATA` counts as unset. A relative root is made absolute against the caller's working directory, before `--cd`, and the root need not exist: nothing is created under it by the CLI alone.

A root that cannot be resolved exits 2 with one line and no usage: `gate: LOCALAPPDATA is not set; give the state root with --state-dir or GATE_STATE_DIR`, `gate: cannot read the LOCALAPPDATA variable` (both only when neither the flag nor the variable gave a root), `gate: cannot read the state root <path>`, `gate: cannot make the state root <path> absolute: <error>`, or `gate: cannot make the default state root <path> absolute: <error>`.

A `--log` that cannot be made absolute is `gate: cannot make the log path <path> absolute: <error>`.

The root replaces the PowerShell gate's test variables `GATE_TEST_SLOT_PREFIX`, `GATE_TEST_SLOT_COUNTS`, `GATE_TEST_REMOTE_CONFIG` and `GATE_TEST_JOURNAL_BYTES`: a test gets its own counts file and journal by giving its gate its own root, and its own routing file by giving its gate its own `LOCALAPPDATA` (the routing file is `%LOCALAPPDATA%\gate\remote.json` whatever the root), and tests rollover by writing a journal near the rollover size into that root.

A root is the default when its full path, normalised (made absolute, a leading `\\?\UNC\` turned into `\\` and a leading `\\?\` removed, `/` turned into `\`, lower-cased, trailing `\` removed), equals that of `%LOCALAPPDATA%\gate\v1` normalised the same way. The default root's kernel object names are the PowerShell gate's, so during the cutover drain both generations take turns on one set of slots ([Cutover](#cutover)).

For any other root, every kernel object name carries `<h>`, the first 8 lower-case hex digits of the SHA-256 of the UTF-8 bytes of that normalised path, so a test root never touches the machine's real slots. A gate running under a non-default root passes the root on to its command as `GATE_STATE_DIR`, so a gate nested in that command uses the same root.

### Files

| Path under the root | Writer | Readers | Notes |
|---|---|---|---|
| `slot-counts.json` | ops-tower's Gates tab, people | every waiting gate, ops-tower | [Counts](#counts). |
| `remote.json` | people | the router, ops-tower (`slots`) | Builders and routing (T1); read from `%LOCALAPPDATA%\gate\remote.json`, beside the `v1\` folder, whatever the state root is. |
| `slots\<name>.json` | the gate holding the slot, written after its `take` line | other gates, ops-tower | [Claim](#claim); `<name>` is the mutex name without `Local\` and the root hash, plus `gate-critical.json` for a critical gate, its one claim for the whole machine (kind `critical`, index 0), written after its `take` line; the heavy and light slots it holds get none. |
| `slots\gate-lent-<parent pid>-<pid>-<start>.json` | a gate lent its light parent's slot, from before its command starts to after it ends | a critical gate's drain; ops-tower does not read it | [Claim](#claim) (the lend marker). Claim readers skip it. |
| `waiting\<pid>-<start>.json` | the waiting gate | other gates, ops-tower | [Ticket](#ticket). |
| `covered\<pid>-<start>.json` | a gate running under a parent gate's slot | other gates (the survey), ops-tower | [Covered marker](#covered-marker). |
| `paused\<parent pid>-<parent start>-<pid>-<start>.pause` | a gate waiting behind a critical request under a parent gate's slot | its parent gate's watchdog | Empty file; [Critical drain and nesting](#critical-drain-and-nesting) (kept, N3). |
| `events.jsonl`, `events.1.jsonl` | every gate, under the journal mutex | ops-tower, the durations store's rebuild, people | [Journal](#journal). |
| `archive\events-<yyyyMMddTHHmmssZ>.jsonl.gz` | the gate whose append rolled the journal | the durations store's rebuild | [Archive](#archive). |
| `durations.json` | every slot-taking gate that reaches a verdict, inside the journal mutex right after its `release` line; a rebuilding gate swaps one in | gates, `gate expect` (no lock); ops-tower does not read it | The learned durations store, `{"v":1,"keyVersion":1,"keys":{…}}`, at most 30 samples per key ([design/gate-durations.md](design/gate-durations.md), Store). Written to a temporary sibling and moved over. A gate rebuilds a missing, stale or torn file from the journal and the archive once it has released, outside the journal hold, leaving out `--slot none` runs; a run whose release was filed into a current store does not rebuild, and the store is read again just before the rebuild claim. A store whose `keyVersion` is above this build's is read as no history, and neither filed into nor rebuilt. |
| `durations.rebuilding`, `durations.rebuilding.<pid>.tmp` | the gate rebuilding the store | other gates | JSON `{"pid":<pid>,"start":<ms>}`; one gate rebuilds at a time, and a marker whose holder is dead or that cannot be read is taken over. Both exist only while a rebuild runs; a Ctrl+C during it swaps nothing and removes the marker. |
| `backfill\<pid>.json` and the backfill bar record | gates | gates | Reserved for backfill guests (GATE-7; [design/gate-durations.md](design/gate-durations.md)). |
| `remote\<key>.outputs-pending` | the Rust remote client | the same | Empty file. The Rust client writes it after the push and before a routed run's run call when the run has output globs, or when the route withheld its outputs (a dotnet `--no-build` command the builder built; that marker no end clears), so a run refused at the preflight or by the parity check leaves none, removes it after an accepted `gate-remote-outputs: 0`, an install that skipped no file or only files it cannot rewrite, or a refusal of the archive's members for what they are, and sends `OutputsFull` while it stands ([The remote client's run](#routing)). |
| `remote\<key>.outputs-hashes` | the Rust remote client | the same | The hash cache: one outputs manifest line, `{"files":{"<path>":{"size":…,"modified":…,"sha256":"…"}}}`, of the files of the worktree's last built manifest last written at least 2 s before that build. Saved after each manifest the client builds, and by a complete outputs install (one that skipped no file but ones it cannot rewrite), for the files it put in place, through `<key>.outputs-hashes.<pid>.tmp` and a rename; one that cannot be read, is not a manifest line or is over 64 MiB gives no hash and is replaced by the next save, and deleting the file resets the cache ([The remote client's run](#routing), Manifest time). |
| `remote\<key>.inputs-hashes` | the Rust remote client | the same | The inputs' hash cache, in the outputs cache's shape and saved as it is, kept apart from it since the outputs saves drop every file the outputs walk no longer finds. Saved after an inputs manifest the client builds changes it; a file whose hash the pack finds wrong is dropped from it, and deleting the file resets the cache ([The remote client's run](#routing), Inputs). |
| `remote\<key>.nothing-back` | the gate, after a routed dotnet build ends on the builder without its outputs known to be brought back, or, when none stands, a routed dotnet `--no-build` command whose word the builder dropped | every gate (the `--no-build` drop and the clear) | JSON `{"v":1,"host":"…","configuration":"…","framework":"…","runtime":"…"}`; the last three are optional and come from the routed build's own `-c`, `-f` and `-r` words. A passing local build of the whole worktree with the same three removes it, as does a passing routed build of that kind that brought all its outputs back ([Routing](#routing)). Written with create-new and the reparse-point flag, and read the same way, 4096 bytes at most; one that cannot be read counts as present. |
| `remote\<key>.outputs-capped` | the Rust remote client, at an outputs fetch that passed the fetch cap | the same | The outputs cap record: JSON `{"v":1,"hits":<n>}`, the fetches in a row of that worktree that passed the cap. A second hit in a row fails the run; any other end of its outputs fetch but a Ctrl+C during it, and a box that packed nothing, removes it; one that cannot be read counts no hit ([The remote client's run](#routing), Outputs fetch). |
| `remote\warm\<key>.json` | `gate warm` for the work folder `<key>` (`gate-state` has the writer and reader) | the route's warm-up judge | The warm-up record: JSON `{"v":1,"host":"…","state":"…","at":"<ISO>","hash":"…","gate":"…","pid":<n>,"started":"<ISO>"}`, then `failure` and `reason` when, and only when, `state` is `failed`. `state` is `warming`, `warm` or `failed`; `failure` is `commands` (the commands failed or ran out of time) or `refused` (refused before any command ran); `hash` is the 64 lower-case hex digits of the SHA-256 of the raw bytes of the worktree's `.gate/warmup.toml`, a byte order mark included; `gate` is the build id of the PC gate that wrote it; `pid` and `started` are the warming process's, for the lines only. Written whole or not at all like the nothing-back record; a record that breaks a rule of its shape is refused on read and on write. ops-tower does not read it. |
| `remote\warm\<key>.log`, `remote\warm\<key>.lock` | `gate warm` | the route's warm-up judge (the lock) | The warm-up's log, made new for each warm-up, opened without following links and held open without delete sharing, and the lock it holds for its whole life, opened without sharing: a `warming` record counts as live only while the lock is held. |
| `remote\warm.lock` | `gate warm` | other warming processes | The lock that lets one warm-up at a time run on this PC, taken after the key's mutex and waited for up to the warm-up file's ceiling. |
`<start>` everywhere in a name is the process start time in Unix epoch milliseconds. Claims, tickets, counts, the nothing-back record and the outputs cap record are written to a temporary sibling (`<file>.<pid>.<n>.tmp`, where `<n>` is a per-process counter so two threads of one process never share one) and moved over the target, so a reader never sees a torn file (kept, L1, Q1).

A `remote-cutover.flag` or `remote-cutover.draining` left in a state root is ignored: nothing reads or writes either file.

The temporary sibling is made new, a link or junction at its name opened as itself, and an entry that already stands at a name is left and the next name tried. A write never removes an entry it did not create: after 16 names it fails with `something already stands at each of 16 temp names beside <path>`.

### Kernel object names

| Object | Default root | Non-default root |
|---|---|---|
| heavy slot | `Local\gate-slot-<i>` | `Local\gate-<h>-slot-<i>` |
| light slot | `Local\gate-light-slot-<i>` | `Local\gate-<h>-light-slot-<i>` |
| remote slot | `Local\gate-remote-slot-<i>` | `Local\gate-<h>-remote-slot-<i>` |
| critical | `Local\gate-critical` | `Local\gate-<h>-critical` |
| critical-beside | `Local\gate-critical-beside` | `Local\gate-<h>-critical-beside` |
| journal mutex | `Local\gate-journal` | `Local\gate-<h>-journal` |
| waiter's wake event | `Local\gate-wake-<pid>-<start>` | `Local\gate-<h>-wake-<pid>-<start>` |
| command's job | `Local\gate-job-<gate pid>` | `Local\gate-<h>-job-<gate pid>` |
| box command's job | `Local\gate-remote-<key>-<nonce>` | the same; the name is not root-dependent |
| box command's stop event | `Local\gate-remote-<key>-<nonce>-stopped` | the same; the name is not root-dependent |
| log's mutex | `Local\gate-log-<l>` | `Local\gate-<h>-log-<l>` |
| routed lock | `Local\gate-routed-<k>` | `Local\gate-<h>-routed-<k>` |
| worktree lock | `Local\gate-worktree-<v>-<f>` | `Local\gate-<h>-worktree-<v>-<f>` |

The critical mutex is taken by a critical gate once no earlier live critical ticket exists, and held for its whole run. An abandoned one counts as taken ([Abandoned slots](#abandoned-slots)).

The critical-beside mutex (the beside mutex) is tried once, never waited on, and held briefly. A local light gate holds it from before it counts the light gates beside a critical request and tries its slot to after it writes its claim. A gate lent its light parent's slot holds it from its lend decision to after it writes its lend marker. A critical gate holds it around its count of the light gates and its light takes, and around its count of the lend markers and the rewrites of its ticket. So no light or lent gate between its take and its claim or marker is missed by a count. A pass that finds it held by another gate has no room and looks again within 100 ms. One a killed gate left held is taken with the `took <name>, which a killed gate had left held` line.

A mutex that cannot be opened (access denied, or a name another kind of object holds) is said once as `gate: could not take <name>: <error>`. A local light gate then takes its slot without it when no critical ticket is live, and has no room when one is; a critical gate holds nothing on that pass and tries again, and a gate lent a slot is not lent, so it queues.

The box command's job name comes from the payload's `Job` (`<key>` is the payload's `Key`, `<nonce>` is 12 hex digits); `gate box` only checks its form and that it ends in the payload's key, and never derives it.

`<l>` is the log hash: 16 lower-case hex digits, the first 8 bytes of the SHA-256 of the lower-cased, normalised absolute log path. Each run takes its log's mutex without waiting, before it readies the log, and holds it until after the slot release and the final append to the log, so two gates never write one log; a second gate fails at once ([Output lines](#output-lines)).

`<k>` is the key hash: 16 lower-case hex digits, the first 8 bytes of the SHA-256 of the lower-cased work-folder key. The Rust remote client takes the routed lock without waiting, before the remote slot, and holds it for the whole remote attempt ([Routing](#routing)).

`<v>` and `<f>` are the worktree lock's folder id: the volume serial number as 16 lower-case hex digits and the 128-bit file id as 32, read from the worktree root opened following links, so two spellings of one folder share one lock.

Every routed run takes it for its main root and each carried sibling before its snapshots and holds it until its last push is saved, and a sync-back takes it again for each worktree it has a back commit for, each waiting for another run's hold ([Routing](#routing), The worktree lock).

The environment carries one more name: `GATE_SLOT_HELD`, the slot marker ([Critical drain and nesting](#critical-drain-and-nesting)).

## Formats

### Common rules

- The claim, ticket, counts and journal files are JSON with the PowerShell gate's field names (kept, L1, Q1, S2, J1), each object carrying `v`, the format version, as a whole number. Every format starts at `v: 1`.
- Machine times are Unix epoch milliseconds as JSON numbers. Times people read (`time`, `started`, `processStart`) stay ISO 8601 UTC strings ending in `Z`.
- Files are read as strict JSON (RFC 8259: no comments, no trailing commas, no unquoted keys) with case-sensitive keys. A file with a syntax error, a byte order mark, bytes that are not UTF-8, a top level that is not an object, a missing required key, a wrong type, a duplicate key at any depth or a `v` that is missing or not the number 1 is unreadable.

  The reader takes the path each format's section names for an unreadable file. The journal is the exception: its rule is per line ([Journal](#journal)). Unknown keys are ignored, so an optional field is added without a `v` change; a rename, removal, retyping or change of meaning raises `v`.
- The reason a file is unreadable is one of `not JSON: <message>`, `not JSON: not UTF-8`, `duplicate key "<key>"`, `missing key "v"`, `unknown format v <value>`, `not a JSON object`, the message of the missing key or wrong type, or the io error. It is the `<reason>` of the gate's ignore line.
- `gate-state` is the one implementation of every format; `gate.exe` and ops-tower both use it, and its fixtures (`crates/gate-state/tests/state/fixtures/`) are the reference examples. The strict rules above are the `gate_state::v1` module's, the formats `gate.exe` writes.

  The crate's other readers (`claims.rs`, `state.rs` and the counts reader beside `v1`) read the PowerShell gate's files leniently, and ops-tower uses them while the cutover flag does not exist. ops-tower follows the flag: its `StateInputs.generation` (`Generation::PowerShell` or `Generation::V1`) is looked up on every `live_inputs()` call from `%LOCALAPPDATA%\gate\cutover.flag`, the switch the launcher uses ([Cutover](#cutover)).

  In the `PowerShell` generation the state and the history read `%LOCALAPPDATA%\gate\`; in `V1` they read `%LOCALAPPDATA%\gate\v1\`, and a missing `v1\` folder is an empty state.

  In `V1` a slot's row also reads the claims in the PowerShell root's `slots\` folder (`%LOCALAPPDATA%\gate\slots\`) while the slot's mutex is open and no live `v1\` claim holds it: a PowerShell gate holding the slot shows `busy` with that claim, its processes and last line.

  A mutex that cannot be probed counts as open, so a slot whose mutex an elevated gate holds shows as Held.

  A claim whose pid is absent from the process snapshot but whose `processStart` is later than every process's creation time in the snapshot counts as live, only while the slot's mutex is open. A claim file whose read fails with access denied (delete-pending) or not found (deleted after the listing) is skipped, never a stale claim; the cutover drain reads claims the same way.

  In `V1` the readers take `gate.exe`'s formats: the claims (their `SlotRow.claim` carries `"v":1`), the tickets and the counts file are read as the strict `v1` formats, and the journal is the one `gate.exe` writes.

  The waiters in `queue[]`, `pools[].waiting` and `critical.waiting` come from the tickets, not from process command lines: `queue[]` is ordered by (`arrived`, `pid`) with `arrived` as ISO text of 7 fractional digits and a lower-case `kind`; a waiter's `since` is its gate's start and its `commandLine` is the gate's command line from the process table; `pools[].problem` is worded for `gate.exe`'s files.

  `DashboardState.covered`, its last key, lists the gates running under a parent gate's slot, from the [covered markers](#covered-marker): `{pid, start, parentPid, command, cwd, since}`, with `start` and `since` as ISO text of 7 fractional digits, sorted by (`since`, `pid`). It holds live markers only, judged by the tickets' pid-and-start check, and is always empty for the `PowerShell` generation.

  A `V1` read of the counts takes `v1\slot-counts.json` beside the PowerShell counts path (with `GATE_TEST_SLOT_COUNTS`, `<that file's folder>\v1\slot-counts.json`) and reads it strictly ([Counts file](#counts-file)).
- Text is UTF-8 with no BOM and LF line ends.

### Claim

`slots\<name>.json`, written by the gate holding the slot, deleted before the slot's mutex is released so it never deletes the next holder's claim (kept, L1).

| Field | Type | Meaning |
|---|---|---|
| `v` | number | 1 |
| `kind` | string | `heavy`, `light`, `critical` or `remote`; a `none` run writes no claim |
| `index` | number | slot index; 0 for `gate-critical.json` |
| `pid` | number | the gate's pid |
| `processStart` | string | the gate's start time, ISO UTC |
| `started` | string | when it took the slot, ISO UTC |
| `cwd` | string | the gate's working directory: the `--cd` folder when given |
| `command` | string | the command's raw words (the program unresolved), each quoted as for CreateProcess (C4) even for a batch target and joined by spaces, as the PowerShell gate's `Format-CommandText` writes them, such as `cmd /c echo "a b" "x\"y"` |
| `log` | string | the log's full path |
| `timeoutSeconds` | number | the ceiling as the take resolved it ([`gate run`](#gate-run)); the writer refuses a value JSON cannot carry (not finite) |
| `remote`, `host`, `key` | bool, string, string | a remote slot's claim only (L1) |
| `expected` | object | optional, last: what the durations store expects of the run, `{"p50": <s>, "p90": <s>, "n": <runs>, "key": "<key>"}` with the seconds to 3 decimals, rounded half to even; absent for a run without history. It is the builder's estimate for a remote take and `crit`'s for a critical gate. The wait lines read `p50` ([Slots](#slots), step 6); `null` reads as absent, any other non-object is unreadable |

The file name `<name>` is `gate-slot-<i>`, `gate-light-slot-<i>`, `gate-remote-slot-<i>` or `gate-critical`, the same whatever the root: a non-default root's claims sit in that root's own `slots\` folder, so the file name carries no root hash.

A claim is live while its `pid` runs with that `processStart` (L2), the start compared in whole milliseconds. A killed gate leaves its file; the next holder overwrites it and readers treat it as stale. A gate that cannot write its claim says so once and runs on (L1).

**The lend marker.** A gate lent its light parent's slot while a critical request drains ([Critical drain and nesting](#critical-drain-and-nesting), the lend) holds no slot and writes no claim. It writes `slots\gate-lent-<parent pid>-<pid>-<start ms>.json` instead, named after its parent gate and itself, in the claim's format with kind `heavy` and index 0 (its `expected` is there when the gate was lent as it waited).

It writes it under the beside mutex before its command starts, deletes it after the command ends, and wakes the waiters. Whether a marker is live is judged by the gate its name names, never by its content; a dead gate's marker is deleted by the next survey. A name of another shape is said and passed over.

The readers of claims skip it, since only the files named `gate-<slot>.json` are claims. A critical gate's drain counts each live one as a heavy holder it waits for. ops-tower's `gate-state` readers skip it.

The liveness rule is the same for claims and [tickets](#ticket). A pid with no running process, or one that has exited, is dead; exited is judged by a zero-time wait on the process, never by its exit code, since a gate whose command exited 259 (`STILL_ACTIVE`) passes that code through. Only a process that refuses the wait's `SYNCHRONIZE` right while granting the query right is judged by its exit code (none does on a standard machine, elevated gates included).

A pid the checking gate may not query is dead too, since it is another user's process reusing a dead gate's pid. Any other failure to check counts as live, said once per path, since a gate deletes nothing it cannot judge. A gate never judges its own ticket or claim (same pid and start).

### Ticket

`waiting\<pid>-<start>.json`, written before a gate's first try, deleted once it holds its slot or ends (kept, Q1, with the [ordering rule](#taking-a-slot)).

| Field | Type | Meaning |
|---|---|---|
| `v` | number | 1 |
| `kind` | string | `heavy`, `light`, `critical`; a `none` run writes no ticket |
| `arrived` | number | first arrival, epoch ms; a routed run that falls back keeps its first arrival (K6) |
| `pid` | number | the gate's pid |
| `startTicks` | number | the gate's process start, epoch ms (the field keeps its name) |
| `holds` | bool | true only for a critical gate that holds the machine: once every slot it takes is held and no lend marker is live, it rewrites its ticket with `true`, under the beside mutex, and keeps the ticket until it ends; a rewrite that fails is no hold, and the next pass tries again |
| `lendsClosed` | bool | optional, written only when true: a critical gate that holds every slot it takes while gates lent a light parent's slot still run writes it once, under the beside mutex, so no light parent lends its slot again and the gate waits only for the lends already made; it is replaced by `holds` when the lends have ended. A ticket without it reads as false, and a build that does not know the key ignores it |
| `remote` | bool | present and true only while queued for the remote pool |
| `expected` | object | optional, last: the same object as a claim's `expected`, written when the gate has history for its command; a critical gate's rewritten holding ticket keeps it; `null` reads as absent |

Waiters of one kind queue by (`arrived`, `pid`). The ticket reader lists only `*.json` files in `waiting\` (a `*.tmp` file is skipped), and leaves out a ticket deleted between the listing and the read, since a gate that took its slot deletes it.

A ticket whose gate is dead by the [liveness rule](#claim) is ignored and deleted by the first gate that finds it, which then wakes the other waiters; the reader itself deletes nothing. An unreadable ticket is ignored but left in place, said once; it also stops a lend that would rest on the critical mutex alone, since it may be a critical request's that refuses lends ([Critical drain and nesting](#critical-drain-and-nesting), the lend).

ops-tower learns that a gate is waiting from these tickets, read through `gate-state`; it does not scan process command lines.

### Covered marker

`covered\<pid>-<start>.json`, written by a gate running under a parent gate's slot ([Critical drain and nesting](#critical-drain-and-nesting)) right after its `covered` journal line, so ops-tower sees a gate that takes no slot and writes no ticket or claim. A `none` run under no covering parent writes no marker: ops-tower's live Gates view, built from claims, tickets and covered markers, shows it only through the journal.

| Field | Type | Meaning |
|---|---|---|
| `v` | number | 1 |
| `pid` | number | the covered gate's pid |
| `processStart` | number | the covered gate's process start, epoch ms |
| `parentPid` | number | the pid of the parent gate whose slot covers it; a gate lent its light parent's slot, which holds no claim, may be that parent (its lend marker names it) |
| `command` | string | the command text, as the claim's [`command`](#claim) |
| `cwd` | string | the gate's working directory: the `--cd` folder when given |
| `since` | number | when the marker was written, epoch ms |
| `kind` | string | optional: the covered gate's own slot kind (`heavy`, `light`, `critical` or `none`), the kind of slot it takes if its parent gate ends first; `none` after the gate gave up watching its parent |

A written marker looks like `{"v":1,"pid":...,"processStart":...,"parentPid":...,"command":...,"cwd":...,"since":...,"kind":"light"}`. The gate rewrites it once, with `kind` `none`, when it cannot watch its parent gate and so stays covered by it to its end. Only a marker whose `kind` is `heavy` or `light` is a stand-in ([Critical drain and nesting](#critical-drain-and-nesting)); a missing or unknown `kind` never is.

The gate deletes the marker when it releases, after a panic and on drop. It writes none when it cannot read its working folder, the condition that also leaves out the `covered` journal line. A gate whose own start cannot be read never gets this far: it refuses to run (see Output lines).

A marker whose gate is dead by the [liveness rule](#claim) is deleted by the next gate's survey, the pass that deletes dead tickets; the reader itself deletes nothing. The reader (`gate_state::v1::read_covered`) returns the good markers in `covered\`, in file-name order, whose name `<pid>-<start>.json` matches the pid and start in the body; a file that is unreadable, misnamed or gone between the listing and the read is skipped without a word, and a missing folder is empty.

### Counts file

`slot-counts.json`: `{"v": 1, "heavy": <n>, "light": <n>}`, both keys optional, each a whole number from 1 to 64 (kept, S2). People and ops-tower write it; `gate.exe` only reads it.

ops-tower's Gates tab writes through `gate-state` (`write_slot_counts`, `apply_slot_count`) on every edit: the PowerShell gate's `%LOCALAPPDATA%\gate\slot-counts.json` first, then `gate.exe`'s `%LOCALAPPDATA%\gate\v1\slot-counts.json` as `{"v":1,"heavy":<n>,"light":<n>}`, creating `v1\`, whether or not the cutover flag exists. A failure on the first file leaves the second as it was. Counts that are empty delete both files.

A missing file, or a missing key, gives the default silently. A file unreadable as a whole (including one without `v`, reason `missing key "v"`) gives both pools their default, each with its own ignore line. A value that is not a whole number from 1 to 64 gives that pool alone its default, with the reason `"<kind>" must be a whole number from 1 to 64`. Every line is `gate: ignored <path>: <reason>; using <n> <kind> slots`, built in one place for the `v1` and the PowerShell-era readers.

### Journal

`events.jsonl` holds one JSON object per line, appended under the journal mutex (`Local\gate-journal`, [Kernel object names](#kernel-object-names)) with a 2 s wait (kept, J1). The events are `queue`, `take`, `release`, `abandon`, `covered`, `parent` and `fallback`. `kind` is `heavy`, `light`, `critical` or `none`.

`gate.exe` writes `covered` for a gate running under its parent's slot; `gate.exe` writes `parent` for a gate lent its light parent's slot while a critical request drains. A gate nested in a lent gate is covered by it and writes `covered` with the lent gate as `parentPid`.

Every line starts with `v`, `time`, `event`, `pid`, `processStart`, `kind`, `cwd` (the `--cd` folder when given) and `command` (the same text as the claim's [`command`](#claim)), in that order, then the event's own fields, as the PowerShell gate writes them (J1); a routed run adds `remote` (true), `host` (the ssh destination) and `key` (the remote work folder's key) after the event's own fields and before `abandoned`, on its `queue`, `take`, `release`, `abandon` and `fallback` lines, and on none after the `fallback`.

The journal keeps ISO 8601 UTC strings for `time` and `processStart` (7 fractional digits, ending in `Z`); it never held .NET ticks, so the move of claims, tickets and markers to epoch milliseconds does not touch it. `gate.exe` takes `processStart` from `GetProcessTimes`, in whole microseconds, so its 7th fractional digit is 0. Seconds fields (`heldSeconds`, `waitedSeconds`, `wallSeconds`, `adjustedSeconds`) are rounded to 3 decimals.

`v` is a line-level rule, unlike the other formats: the writer always writes `"v":1` first; a reader skips a line whose `v` is present and not 1, and reads a line with no `v` as the PowerShell gate's format until the cutover's drain (GATE-9) makes `v` required.

The event's own fields:

| Event | Fields after the common ones |
|---|---|
| `queue` | `log` (the log's full path), `notRouted` (written right after `log`, only when a heavy gate's caller kept it local with `--local`, with the value `--local`; other reasons for a local run write none) |
| `take` | `slots` (the slot names taken; a critical gate lists every heavy and light slot it holds; a `none` run lists none, `[]`, with `waitedSeconds` 0.0), `waitedSeconds`, optionally `parentPid` (also on the late take of a covered gate), and for a critical gate `counts`, `otherHeld`, then `otherClaims` or `snapshotError` |
| `release` | `slots` (the same list), `heldSeconds`, `status` (null when the gate failed before its command had one), `wallSeconds`, `adjustedSeconds`, optionally `avgCores`, `peakCores`, `freeCores`, `verdict` |
| `abandon` | `waitedSeconds`: the gate ended while still queued (Ctrl+C while waiting for a slot, or, for a routed run, while its client waited to start) |
| `parent`, `covered` | `parentPid`; a lent gate writes `parent` after its `queue` when it was lent the slot as it waited, and alone, with no `queue`, when it was lent it before it queued |
| `fallback` | `reason`, then `remote`, `host`, `key`: it ends the routed attempt, so a remote `take` that falls back gets no `release`; a `full` fallback, and a `client` fallback with no client built, follow no remote `take` |

A release with no run facts writes `wallSeconds: null`, `adjustedSeconds: null` and `verdict: "unknown"`.

Fields beyond J1's:

- any event: `abandoned`, the last field, a list of names present only when there were any ([Abandoned slots](#abandoned-slots); GATE-30). On a `take` it is the slots taken from a gate that died holding them, followed by the journal mutex's name when that came back abandoned; on any other event it is just the journal mutex's name.
- `take`: a critical gate's `take` carries, after `waitedSeconds` and in this order, `counts` (`{"heavy": <n>, "light": <n>}`, the counts of the pass that took the machine), `otherHeld` (the same shape: the slots within the counts that other gates hold), then `otherClaims` or `snapshotError`; `abandoned` stays last (K5, GATE-38).

  `otherClaims` is a list of `{pid, kind, index, command, file}`, one per live claim of another gate in this root's `slots\` folder, sorted by file name: only the `gate-*.json` files, the remote pool's claims and this gate's own left out, `file` the claim's file name. `snapshotError` replaces it only when the claims folder cannot be listed, and holds the reason.
- `take`: `parentPid`, an integer after `waitedSeconds` (and a critical take's snapshot fields) and before the remote fields and `abandoned`, written by a gate under a parent whose slot did not cover it ([Cores hint](#cores-hint)), or under a no-slot parent, and by a covered gate that takes a slot late once its parent ended ([Critical drain and nesting](#critical-drain-and-nesting)), where it names the parent that ended, or the stand-in that ended last.

  `gate-state`'s reader accepts it and does not treat the run as on the parent's slot, except for a covered run, which it reads as a run with a slot of its own.
- `take` of a `none` run: `"kind":"none"` on its `queue`, `take` and `release`, and a `take` with `"slots":[]` and `"waitedSeconds":0.0`, no `abandoned`, no machine fields, and `parentPid` only under a live no-slot parent. Its `release` has `"slots":[]` and is written like any run's, unlike a covered or lent run's. `v` stays 1. The history reader gives the run `kind` `none`, no `slot`, and `started` from the `take`.
- `release`: `avgCores`, `peakCores`, `freeCores`, optional numbers of one decimal after `adjustedSeconds` and before `verdict` ([Cores hint](#cores-hint)). They are omitted when there is no unpaused sample, the sampler failed, the read of the command's CPU at its end failed, a gate under the run took a slot of its own (or the machine), the journal could not be read to tell, or the run was routed.

  Covered and lent runs write no `release`, except a covered run that took a slot late, which writes one when it lets that slot go: its `slots` the one it took, its `heldSeconds` counted from that take.
- `release`: `wallSeconds` (the command's wall time less the seconds paused behind a critical request; a routed run's whole remote wall time), `adjustedSeconds` (load-adjusted seconds, 3 decimals, a number for every run that started; for a routed run the builder's figure, null when it said none; null for a run whose sampler failed) and `verdict`, one of `passed`, `failed`, `log-failure`, `stalled`, `timed-out`, `backstop`, `transfer-timed-out`, `unknown` (kept, J5).

  `transfer-timed-out` is a routed run's fetch after the command cut at its transfer ceiling.

  A routed run is a kill only when the gate killed it; a command that exits 124 on its own is `failed`.
- `fallback`: `reason`, one of `full` (no remote slot within the 60 s wait), `busy`, `unreachable`, `parity`, `rebooted`, `tools` (T5), `layout` (the box could not clear or lay out its work folder), `version` ([The box side](#the-box-side)), `client` (the remote client could not start, as when it finds no usable ssh), `inputs` (the run's ignored inputs could not be sent, a sibling pair declared inputs, or the box refused the run because its work folder no longer held the inputs sent) and `cold` (the builder's work folder was not warm for the run's warm-up file; `FallbackReason::Cold` writes the word, and the client refuses a call that needs a warm folder this way, but no route makes such a call yet).

  The `take` after it is the run's local one, untagged, its `waitedSeconds` counted from the `fallback`, except after `full`, where it counts from the run's `queue` line; a Ctrl+C that ends that local wait writes the same figure on its `abandon`.
- a routed run's `take`, `release` and `abandon` carry the tag; its `release` has the builder's `adjustedSeconds` (null when it said none) and a `wallSeconds` of the whole remote call, and the verdict `timed-out` or `backstop` after a kill by the builder or by the client's backstop.

The verdict comes from the run's end: a kill line matching `^gate: (STALLED|TIMED OUT|BACKSTOP|TRANSFER TIMED OUT)\b` gives `stalled`, `timed-out`, `backstop` or `transfer-timed-out`; otherwise a command status of 0 with a non-zero gate status is `log-failure`, a status of 0 is `passed`, anything else `failed`; `unknown` when there are no run facts.

The reader starts a run from a lone `parent` line that names a parent: state `parent` and `parentPid`, no queued or started time, and the status and held time of the parent's run. A `parent` line naming no parent is skipped.

A lone `covered` line is its own state, `covered` (`RunState::Covered`), distinct from a run that lent its slot: the run has `parentPid`, no queued or started time, the kind and first time of the `covered` line, and the status and verdict of the parent's released run.

The parent's run is the run of that pid whose first line is the latest at or before the `covered` line, whether or not it had taken its slot by then, so a gate covered by a stand-in reads its end from the stand-in's `release`. A gate covered by a lent gate finds no released run, since a lent gate journals no `release`, so its row has no status, held time or verdict. A covered run is never counted in the summary.

A covered run that also journals a `take` of its own (it took a slot late) is no longer a lone `covered` line: it reads as a run with its own take, `running` while its gate runs, `unrecorded` once the gate is gone without a `release`, and released with its own `release`, its status, held time and verdict its own. It counts toward the 50 newest runs, outside the covered quota, and the per-command held-time summary leaves it out, its held time covering only the stretch after the parent ended.

The history that ops-tower serves (`HistoryRun`, `GET /api/gate/history`) carries `verdict`, `wallSeconds` and `adjustedSeconds`, null when a line has none, and `abandoned`: every name any line of the run lists (`parent` and `covered` lines included), in line order without repeats, always present and empty when there are none.

The history lists the 50 newest runs that are not covered, and at most the 50 newest covered runs newer than the oldest of them: at most 100 runs, newest first by the time of each run's first journal line (its queue line, or the `parent` or `covered` line of a gate that never queued), then by pid and process start.

A run fallen back onto or lent its parent's slot counts toward the 50; a covered run does not (one that took a slot late does), so a parent with many nested covered gates never crowds other runs out. The 7-day prune keeps every run the history lists, and the parent run of each listed covered or fallen-back run, whose row takes its status, held time and verdict from it.

It also carries `pid` (a number), `processStart` (the ISO string from the journal) and `running` (a bool, true while the run's gate is live by the same pid-and-start check as the tickets', for every run): a covered run's `state` stays `covered` while it runs.

It also carries, after `slot`, `slots` (the slot names the run took, such as `["Local\\gate-slot-0"]`, `[]` when none) and `machine` (null, or for a critical gate's `take` `{counts, otherHeld, otherClaims}` or `{counts, otherHeld, snapshotError}`). A `fallback` line clears `slot`, `slots`, `machine`, `started` and `waitedSeconds`.

A run a fallback sent back after it took a remote slot (`rebooted`, `unreachable` mid-run, or any other fallback after a remote `take` line) keeps that take as `remoteTake`: `{"started": <ISO>, "waitedSeconds": <number or null>, "fellBackAt": <ISO>}`, the remote `take`'s time and wait and the `fallback` line's time (`RemoteTake` in `gate-state`).

The key is omitted for every other run: a `full`, key-lock `busy` or `client` fallback before any remote take, and a run that never fell back. A later remote-tagged line of the run (a new routed attempt) clears it.

ops-tower reads the journal of the generation the cutover flag selects ([Common rules](#common-rules)).

The history key is `gate_state::normalize_command` of a run's `command`, so the runs of one command in different worktrees count together. It splits the command as the Microsoft C runtime reads arguments (space and tab outside quotes separate words, `"` opens or closes a span, `""` inside a span is one literal quote, a backslash run before `"` is halved with the quote literal when the run is odd), the first word included.

Each word's rules then run on its unquoted value: `--manifest-path`, `--directory`, `--dir` or `-C` with an absolute path after it is dropped with the path, a word starting `.tmp/` or `.tmp\` becomes `…`, and an absolute path with a `.wt` component and one more after it becomes what follows that component, with `/` separators, or `.` when nothing does.

Each word is written back as `quote_arg_str` in `gate-args` writes it and the words are joined by single spaces, so a quoted spelling of a tree flag, scratch path or worktree path keys the same as the plain one.

The journal writer says these prose lines, each prefixed `gate: `:

- `could not write the journal <path> (<reason>)`, once per run, whose reasons include `<name> was not free within 2 s`;
- `could not move <events.jsonl> over <events.1.jsonl>: <reason>; appended to it as it is`, once per run;
- `took <name>, which a killed gate had left held`, for an abandoned journal mutex ([Abandoned slots](#abandoned-slots));
- the archive's lines ([Archive](#archive)).

Ordering is part of the interface: `release` is written before the slots are let go, so no later `take` precedes it; `fallback` is written while the routed run still holds its remote slot; a `take` with no `release` or `fallback` after it is a gate killed outright (J1). Runs are keyed by (`pid`, `processStart`).

Rollover, torn lines and failures are kept: a journal of 5,242,880 bytes (5 MiB) or more is moved over `events.1.jsonl` before an append, so it rolls at or above the cap, and a move a reader blocks leaves it in place and is said once (J2); a torn last line gets a line feed before the next (J3); a line that cannot be written is said once and skipped, and the gate runs on (J4). The cap and the 2 s mutex wait are constants; a test writes a near-cap journal into its own root.

### Archive

The gate whose append rolled the journal archives the rolled `events.1.jsonl`'s `release` lines, gzipped, to `<state root>\archive\events-<yyyyMMddTHHmmssZ>.jsonl.gz`, counting and skipping lines that do not parse, then deletes archives older than 180 days and stray temporary files older than a day.

It does this before it releases the journal mutex, in the same hold as the append that rolled (J5), so a second roll can never replace the file before it is archived. The hold stays short: the archive parses only each line's `event` and does not flush to disk.

A gate killed between the roll and the archive, or an archive write that fails, loses that roll's release lines from the archive: no later gate re-archives `events.1.jsonl`, and the next roll replaces it. They stay in `events.1.jsonl` until then.

Two rolls within one second collide on the archive name: the second archive fails and the failure is said once. The writer says, as prose lines:

- `archived the release lines of <path>; skipped <n> line(s) that do not parse`;
- `could not archive the release lines of <path> in <dir>: <error>`;
- `archived the release lines of <path> but could not delete <temp>: <error>`;
- `could not delete old archive <name>: <error>`.

### remote.json

`remote.json` keeps the PowerShell gate's format (T1), read by `gate-router` as [Routing](#routing) describes: `host`, `slots`, `repos` and the optional per-repo `outputs`, `inputs` and `siblings` and the optional `warmup`, read afresh by each routed gate and ignored as a whole with one line when unusable. A host of one letter is refused (`host must not be a single letter, which a git host:path URL reads as a drive`).

The optional `hosts` is an array of strings naming further builders, which only `gate push` reaches; a routed run goes to `host` alone. Each entry passes the checks `host` does, with the same reasons worded `hosts entries ...`.

The push order is `host`, then `hosts` in file order, an entry equal to an earlier one in any letter case skipped. A `hosts` that is not an array of strings, or holds a bad entry, never affects routing: `gate push` alone refuses the file with `push: ignored <path>: <reason>`.

`warmup` is an array of repo names, matched in any letter case, that consent to the repo's `.gate/warmup.toml` warming its builder work folders (`Config::warmup_consented`); a repo it does not name, or a file without the key, has no consent. A value that is not an array of non-empty strings consents to none and leaves the rest of the file usable, and `warmup_consented` gives its reason. Nothing reads the consent yet.

ops-tower reads only `slots`.

A repo's own `.gate/toolchain.toml`, which declares the tools and workloads a routed run's builder needs, is a separate file in the snapshot, not part of `remote.json`: [Routing](#routing) (Toolchain manifest) describes its format.

A repo's `.gate/warmup.toml`, which declares the warm-up commands, is read by `gate_router::warm::read_warmup`: `commands`, an array of 1 to 8 commands, each a non-empty array of non-empty strings, and the optional `timeout`, whole seconds from 1 to 86400, 1800 when missing. A leading byte order mark is skipped, any other key is a problem, and any problem means no warm-up. `gate warm` reads the file ([`gate warm`](#gate-warm)); nothing starts a warm-up from a route yet.

A warm-up command is worded for the builder as a routed run's words are. A command whose first word is `git` or `prek`, named by any path or sent as that program's name (`C:\x\git.exe.exe status`), is a file problem, `gate: warm: .gate/warmup.toml command <n> starts with <word>, which a routed run never sends`; a first word naming another program by a path outside the worktree is sent as the program's name ([Routing](#routing), the in-worktree path rewrite), not a file problem.

The judge, `gate_router::warm::judge`, says where a command that would otherwise route goes, given the repo's consent, the file, the work folder's record and whether its warm-up lock is held; it does not run yet. In order: no consent or no file routes; a file problem runs here saying `gate: not routed: .gate/warmup.toml: <problem>`; a held lock means a warm-up is running and the run stays here with `gate: not routed: <host> is warming the work folder for this worktree`; an unreadable record runs here with `gate: not routed: the warm-up record <path> cannot be read: <error>`; no record, a `warming` record with a free lock, or a `warm` record of another host or file starts a warm-up, the run staying here with `gate: not routed: starting a warm-up of the work folder for this worktree on <host>`; a `warm` record of this host and the file's hash routes. A `failed` record of this host, file hash and gate build keeps the run here until its backoff has passed, 30 minutes after `commands` and 60 seconds after `refused`, with `gate: not routed: the warm-up failed <n> min ago: <reason>` (`less than a minute ago` under a minute); a failure of another host, file or build, or one written in the future, has no backoff and starts a warm-up. Every `<…>` text is escaped to one line.

 It carries no `v`; its format moves with the remote port (GATE-10).

## Critical drain and nesting

All kept: what a critical gate asks for and the order of its drain (K1), the light slots a critical gate leaves to the beside allowance (K2), light gates beside a critical gate (K3), priorities (K4), the take line and its snapshot (K5), a routed run falling back in its first place (K6), the refusal of a critical gate under a heavy or light marker (K7), nesting under a slot marker (N1), lending while a critical request drains (N2) and pause markers (N3).

Changes from the PowerShell forms:

- The slot marker is `GATE_SLOT_HELD=<kind>:<pid>:<start>` with `<start>` in epoch ms, `<kind>` one of `heavy`, `light`, `critical` and `none`. Under a non-default root it gains the root hash, `<kind>:<pid>:<start>:<h>`, and a gate honours a marker only when its hash matches its own root (no hash for the default root). A test gate running under a real gate's marker therefore takes its own slots.
- The pause folder lives in the state root, so a test root isolates it.
- Light gates beside a critical gate and the critical gate's own waits use the wake events and the queue order of [Taking a slot](#taking-a-slot).

What `gate.exe` does of it today is part A, nesting under the slot marker (N1, K7), part B1, the critical request's own drain and take (K1, K2, K5), the barrier (Q3), the light gates beside a critical request (K3), the lend (N2) and the pause markers' writer and reader (N3), all below. The watchdog reads the markers on every sample.

- A gate reads `GATE_SLOT_HELD` once, before routing and before the log mutex (`nesting.rs`). A live parent whose slot covers the gate's kind (heavy covers heavy, light and none, light covers light and none, critical covers all four) gives `gate: running under the <held kind> slot of gate <pid>`: the gate takes no slot, writes no ticket or claim, and journals one `covered` line (identity fields, then `parentPid`; no `queue`, `take` or `release`).

  It then keeps a [covered marker](#covered-marker) until it releases, so ops-tower sees it run.

  Its command gets the parent's marker verbatim, and a heavy gate covered by any parent is never routed.
- Under a critical parent the command runs at High priority and the watchdog measures it as a critical gate's, until a late take gives the gate a slot of its own kind.
- A parent is live by the three-way liveness dead-gate cleanup uses (`slots::Liveness`): gone means no such process, a start mismatch or access denied; any other error reading its start counts as running, with no extra line.
- A malformed marker says `gate: ignored GATE_SLOT_HELD='<text>', which is not <kind>:<pid>:<start>; taking a <kind> slot`, with every character outside `[A-Za-z0-9:]` in `<text>` shown as `?` and at most 64 characters, then `...`.

  A marker whose parent is gone says `gate: ignored GATE_SLOT_HELD='<marker>': gate <pid> is no longer running; taking a <kind> slot`. A marker hashed for another root, or unhashed under a non-default root, is ignored silently. Each of these gates then takes its own slot, unless a stand-in is found for it (below); a no-slot gate says `...; running on no slot` in place of `taking a <kind> slot`.
- A marker is honoured only when it binds to a real parent gate's job, since any process can set the variable. It binds when the parent gate's named job holds this gate itself, asked first from this gate's own handle without a snapshot, or, walking up one process snapshot without command lines, the first ancestor the job holds. Each process is checked by its creation time against pid reuse, and a job the gate may only list is enough.

  Under `cmd` then `pwsh` (a git hook's pwsh) the binding is found two ancestors up.
- A marker that does not bind says `gate: ignored GATE_SLOT_HELD='<marker>': this gate does not run inside the command of gate <pid>; taking a <kind> slot` (`running on no slot` for a no-slot gate), and the gate runs as one under no parent, never refused, a critical gate included, unless a stand-in is found for it (below), which replaces this line.

  A marker is unbound when no job has the parent's name, the chain ends with no member, a process was reused, an ancestor's start is missing from the snapshot, the job name is held by another kind of object, or access to the job is denied while the marker's gate, by pid and start, is not among this gate's ancestors.
- A binding that cannot be judged covers: access to the job denied while the marker's gate is among this gate's ancestors (an elevated parent), a membership query that fails, or a snapshot that does not list this gate.

  A process on the walk that cannot be opened is unbound, not unjudged: the snapshot read its start through the same open, so it changed after the snapshot. The gate first prints `gate: could not judge GATE_SLOT_HELD='<marker>' (<reason>); running under the <held kind> slot of gate <pid>`, and then goes on as under a bound marker.
- The binding guards against an inherited, copied or leaked marker, not against a same-user process that sets out to forge one (GATE-144 tracks hardening).
- **A covered gate whose parent gate ends first** (the late take, `crates/gate/src/late.rs`). A gate job has no kill-on-close, so a nested gate the parent's command backgrounded, or one whose parent was killed from Task Manager, would keep running uncounted after the parent's slot is released and taken by another gate; a covered gate therefore takes a slot of its own, or runs under a live stand-in's.

  A heavy or light covered gate holds a waitable handle on its parent (`WaitableProcess` in `crates/gate-win32/src/tree.rs`, as `burn.rs` uses it), opened as its watch begins and looked at on every step of the watch loop. A `none` or critical gate takes no late slot and watches nothing.

  When the parent ends while the command still runs, the gate looks for a stand-in (below); finding none it says `gate: the parent gate <pid> ended; taking a <kind> slot for the rest of this run`, queues a ticket of its own kind with the run's first arrival, and takes a slot as soon as one frees. The journal records a `take` with `parentPid` naming the ended parent, the claim is written, and the run's end journals a `release` of that slot and lets it go as any slot is.

  The take happens beside the running command, on the thread that will release the slot, never through `Hold`/`take_own` before it: the command never pauses while it waits. Until a slot frees the machine is one gate over its pool, and a command that ends while the gate still waits deletes the ticket and says nothing more. The wait's `still waiting` lines are the ordinary ones.

  Under a critical parent that ended, the command had run at High as a critical gate's; once the gate holds its own slot (or runs under a stand-in's) the watch measures it as its own kind, stops raising the servers to High, and lowers every process of the command's job still at High to BelowNormal.

  A process it cannot lower, or a job whose processes it cannot list, is said: `gate: could not lower the command's process <pid> from High: <error>` and `gate: cannot list the command's processes to lower them from High: <error>`.

  A parent that cannot be watched is said once, and the run stays covered by it to its end: `gate: cannot watch the parent gate <pid> (<reason>); this run stays covered by it` for an open or start read that fails and for access denied to a live or unknown parent, and `... (no covered marker); this run stays covered by it` for a run that wrote no covered marker, since a gate started inside its command could not find it to run under its slot.

  The gate then rewrites its covered marker with `kind` `none` ([Covered marker](#covered-marker)) so no gate runs under it in the parent's place.

  **The stand-in.** A stand-in is a live covered gate, heavy or light by its marker's `kind`, whose command holds this gate and whose own slot kind covers this gate's, standing in for the ended parent.

  The command holds the gate when the candidate's job holds this gate's own process, else when one walk up this gate's ancestors in a process snapshot finds the candidate's job holding one (the same test that binds a marker); a binding that cannot be judged binds nothing, since queueing is the safe failure. Candidates are tried outermost first (by start, then pid), those whose kind covers this gate's before the others, and a gate whose liveness is gone is passed over.

  This gate then stays covered, says `gate: the parent gate <ended pid> ended; running under the <held> slot of gate <A>` (`<held>` is the stand-in's own kind), watches the stand-in in turn, and names it in `parentPid` should it take a slot itself, so one build tree holds one slot.

  A stand-in that cannot be watched is passed over with `gate: cannot watch gate <pid> (<reason>) to run under its slot in place of the parent gate <ended pid>`, and the search goes on. A covered folder that cannot be read says `gate: cannot read the covered markers in <dir>: <error>` and finds none.

  A gate started after its marker's gate ended, or whose live parent's command no longer holds it (its job closed as it ended), is taken up by a stand-in at start-up the same way, and says `gate: running under the <held> slot of gate <A>` in place of the `ignored GATE_SLOT_HELD=...` line.

  A critical gate there is refused as under a live heavy or light gate (below). A heavy gate whose only stand-in is light says `gate: the light slot of gate <A> does not cover a heavy gate; taking a heavy slot`, and a light gate turned heavy by a dropped `--no-build` searches again for a heavy stand-in.

  A parent ends the processes its command left in its job once the command exits, but spares a `gate.exe` there, its launcher, its process tree and its job ([Run end](#watchdog)), so the late take above still covers a covered gate the command left running.
- A critical gate under a heavy or light marker is refused (K7) on stderr only, exit 2, before the log mutex, with nothing queued or journalled: `gate: --slot critical cannot run under the <held kind> slot of gate <pid>: it would wait for that slot for good; run the critical command outside that gate` (`-Slot critical` through the PowerShell spelling).
- A heavy gate under a light marker takes its own slot and says `gate: the <held kind> slot of gate <pid> does not cover a <kind> gate; taking a <kind> slot` just before its local take (after a Local route or a fallback, before its wait lines); a gate routed to the builder never says it.
- **No slot.** A `none` run takes nothing and is never held by the drain or the barrier. Its marker is `GATE_SLOT_HELD=none:<pid>:<start>[:<h>]`. A gate under a `none` marker (the case `Nesting::Beside`) takes its own slot as a top-level gate would: a critical child takes the machine and is never refused (K7 applies to heavy and light markers only).

  Such a gate prints no nesting line and is never lent a slot, and it keeps the pause marker for its no-slot parent's stall window. Its `take` names `parentPid`.

  A `none` gate under a heavy, light or critical parent is covered: it journals `covered` with kind `none`, prints `gate: running under the <held kind> slot of gate <pid>` and hands the parent's marker on.
- **One request at a time.** A critical gate queues a ticket of kind `critical` and runs after every earlier live critical ticket, in arrival order, each through the critical mutex ([Kernel object names](#kernel-object-names)). An abandoned critical mutex counts as taken, with the `took <name>, which a killed gate had left held` line.
- **The drain.** With the mutex held, the gate waits for every earlier live heavy ticket, and for every earlier light ticket when the light count less the beside allowance is above 0. Remote tickets are ignored, and so is a ticket whose gate already holds a live claim and only failed to delete its ticket.
- **The take.** It then takes every heavy slot of the counts as they stand, and on every pass the free light slots, lowest index first, until it holds all but the beside allowance of the light count: the allowance is `min(2, light count)`, the light gates that may run beside the critical request, whichever light slots they hold. It does not choose slots to leave; the slots it leaves free are the beside slots of this drain.

  It takes light slots only on a pass where no more light gates than the allowance run, so it drains past those within the allowance. It counts and takes holding the beside mutex ([Kernel object names](#kernel-object-names)): the light gates running are the light slots of the count some gate has open that it does not hold (a gate between its take and its claim included, and a slot that cannot be probed counted as running) and the other gates' live light claims past the count.

  A pass that cannot take the mutex or list the claims folder takes nothing, and one that took a slot wakes the waiters.

  A slot a killed gate left is a takeover, journalled in `abandoned`, and the gate deletes that gate's claim, since it writes none for the slot.
- **The hold.** Once every slot it takes is held and no gate runs lent its light parent's slot, the gate rewrites its ticket with `holds: true`, keeps it until it ends, and writes the one claim `slots\gate-critical.json`. Its `take` and `release` list the slots held ([Journal](#journal)).

  It counts the lend markers and rewrites the ticket under the beside mutex, so a gate lent a slot after the count sees the ticket and is not lent. A rewrite that fails is said once, is no hold and is tried again on the next pass; so is a claims folder that cannot be listed or holds an entry that cannot be read.

  While lend markers are live the gate rewrites its ticket once with `lendsClosed: true` and waits for those lent gates to end. No light parent lends its slot again, so the drain waits only for the lends already made.
- **The barrier.** A gate that finds a live critical ticket that arrived before its own (by `arrived`, then pid; its own ticket never counts) is held back by the earliest such request, whether or not that request holds the machine yet. Gates that arrived before the request run first and are waited for. A heavy gate tries no slot. A light gate tries any light slot while fewer light gates than the beside allowance run beside the request, and none once that many do.

  The light gates beside a request are the other gates' live light claims, whichever slots they hold, counted by a gate that holds the beside mutex from its count to the write of its own claim, so a gate between its take and its claim is counted. A pass that finds the mutex held by another gate has no room. The allowance is taken from the light count as it stands on each pass. A remote wait has no barrier.
- **The lend (N2).**

  A heavy gate under a light parent (the one nesting case where the parent's slot does not cover the gate) is lent the parent's slot, whichever light slot the parent holds, takes no slot of its own and says the lend line, while a critical request is pending and none refuses a new lend.

  A request is pending when another gate has a live critical ticket, else when the critical mutex is open; the mutex is not read when a ticket could not be read, since it may be a critical request's. A request refuses lends when its ticket says `holds` or `lendsClosed`. A lend that rests on the critical mutex alone is not made after an unreadable ticket, and the gate queues.

  Once the critical gate holds every slot it takes and closes its lends, nothing is lent, and the child waits behind it like any gate.

  The rule is checked before the gate queues, when the gate is lent without routing, queue line or ticket, and on every pass of its wait once it has queued, when it deletes its ticket and wakes the waiters. A lend is decided holding the beside mutex, which the gate holds until its lend marker ([Claim](#claim)) is written; one that cannot take the mutex is not lent that pass, and one whose marker cannot be written is not lent: the gate queues, or waits on.

  The marker stays from before the command to its end, and a critical gate holds the machine only once no marker is live. The lent gate hands its command its own marker, `GATE_SLOT_HELD=heavy:<its pid>:<its start>[:<h>]`, not the parent's. Every gate nested in its command is therefore covered by the lend: it journals `covered` with the lent gate as `parentPid`, says `gate: running under the heavy slot of gate <pid>`, and asks for no slot or lend of its own. A critical gate nested there is refused (K7), naming the lent gate. The lent gate journals `parent` with `parentPid` and no `take` or `release`; a gate lent as it waited has its `queue` before the `parent`.
- **The pause marker (N3).** A gate waiting for a slot under a parent gate's marker that does not cover it keeps the empty file `paused\<parent pid>-<parent start>-<pid>-<start>.pause` in the state root. It writes the file on every pass of the wait while a live critical ticket that arrived before its own exists (the barrier), setting its last-write time explicitly each time, so the file's age shows the gate is alive.

  It deletes the file when that stops and however the wait ends: take, lend, timeout, Ctrl+C or a panic. A write or delete that fails is said once per wait, each, and the wait goes on.
- **Reading the pause markers.** `PauseReader` (`pause.rs`) answers on each watchdog sample whether a gate under this one waits behind a critical request. No `paused` folder means no pause.

  A marker counts when its name parses, names a parent the gate reads for (below), its child gate still runs with the start in the name (a start that cannot be read counts as not running), it was written at most 30 s ago (exactly 30 s is live) and the child's ticket exists. A marker gone while it is read is skipped. On the first marker that counts, the answer is whether a critical request is pending (a live critical ticket, else the critical mutex open).

  A folder, the waiting tickets or the critical mutex that cannot be read is an error the caller says once, and the parent pauses nothing: the watchdog says it once per watch, counts that sample as not paused, and reads again on the next.

  A gate reads for itself (its own pid and start) and, when its parent's slot covers it, also for that parent, since the gates under its command name their markers after the parent whose marker the command gets. A lent gate hands its command its own marker, so it reads only for itself; every gate nested in it is covered, waits for nothing and writes no pause marker. A marker named after that parent counts only when its waiting gate runs in this gate's job: a sibling the parent started beside this gate names it too, and must not keep a hung gate from being stalled.
- **Lines**, on stderr and in the log:
  - `gate: waiting for the machine (critical): <h> heavy and <l> light slots held (<r> light slot|light slots beside it), <n> earlier waiters`, the first time a pass falls short; a critical gate that takes the machine on its first pass says nothing.
  - `gate: still waiting for the machine (critical): <status> (<s> s)`, with the same status, on each heartbeat.

  `<r>` is the beside allowance, `min(2, light count)`. The slots other gates hold count each live lend marker as a heavy holder.

  On both lines, when no earlier waiter and no earlier critical gate is ahead and every holder the drain waits for has a usual, the status ends `, the last frees in about <eta>`, or `, the last is <over> past its usual <p50>, ceiling <ceiling>`, naming the latest expected end instead of the soonest, in the figures of [Slots](#slots), step 6.

  The holders are the heavy slots some gate has open, the light slots past the count that live claims hold, and of the light slots some gate has open, all but the allowance of those whose usual ends come soonest, each by its live claim's `expected`, and each live lend marker by its own `expected`. It is given when only lent gates block, and dropped when a marker cannot be read or has none.
  - `gate: holds the machine (critical) at <yyyy-MM-ddTHH:mm:ss.fffffffZ> after <s.d> s: counts heavy <H>, light <L>; other gates hold <h> heavy and <l> light slots (<r> ... beside it); other live claims: <pid> <kind> <index> <command>, ...`, the claims ending as `none` or `unknown (<reason>)` when they could not be listed.
  - `gate: waiting for a heavy slot behind the critical request of gate <pid>`, from a heavy gate the barrier holds back, on its first pass whether or not a slot is free, with `<pid>` the earliest request's gate; then `gate: still waiting for a heavy slot (<s> s)` on each heartbeat.
  - `gate: waiting for a light slot beside the critical request of gate <pid>`, from a light gate the barrier holds back, likewise, then `gate: still waiting for a light slot beside the critical request of gate <pid> (<s> s)`.
  - `gate: running under the <held kind> slot of gate <pid> while a critical request drains; taking no slot`, from a lent gate, said before its command starts.
  - `gate: interrupted while waiting for the machine`, on Ctrl+C: the journal gets `abandon`, everything taken is released and the exit is 0xC000013A.
  - `gate: cannot read the claims in <dir>: <error>`, said once, for the claims folder or an entry of it that cannot be read. An entry that cannot be read keeps the critical gate from holding the machine on that pass, since it may be a lend marker.
  - The lend marker lines of [Slots](#taking-a-slot), step 7: `cannot write the lend marker`, `cannot delete the lend marker`, `ignored the lend marker` and `cannot delete the dead gate's lend marker`.
  - `gate: could not write the pause marker <path> (<reason>)` and `gate: could not delete the pause marker <path> (<reason>)`, each said once a wait.
  - `gate: could not read the pause markers in <folder> (<reason>); a gate under this one pauses nothing`, and the same with `the waiting tickets in <folder>` or `the critical mutex <name>`, from the watchdog, once per watch; likewise `gate: could not read the processes in this gate's job (<reason>); a gate under this one pauses nothing`.

## Watchdog

All kept, with each knob's default unchanged:

- The sampler (W1): every 5 s while the command runs, it reads, in order, the pause state, the log and `.err` sizes, the job's pids, one process snapshot, then the WSL VM, the system times and the job's accounting; nothing goes through WMI.

  Each sample carries two stamps: `began`, taken before its reads, and `at`, taken after them, next to the system times and the job's accounting. The `interval`, the load-adjusted clock, the paused time, the free share and the exit use `at`; the PowerShell gate stamps once, before its reads.
- Stall (W2): no output, no CPU (at least 1% of one core over the window), no new process in the job, no WSL VM CPU and no nested gate paused behind a critical request, for `--stall` seconds: `gate: STALLED: ...`.

  The quiet time runs from the last progress sample's `at` to the latest sample's `began`, not to the moment the watcher checks: a watcher starved of CPU wakes late, and the time it spends in its reads, and after them up to the check, is not the command's quiet time, so the next sample decides.

  A quiet command is therefore killed up to one sample period after its window, plus the read time of the last progress sample and of the latest sample, never before; the PowerShell gate measures to the check and can kill earlier. The ceiling and the backstop read the check's own time.

  Progress as the watchdog crate counts it resets the stall clock when the sum of the log and `.err` sizes changed (grew or shrank), the CPU of the job and of the processes outside it under its members (the escaped tree, below) since the last progress reached 1% of one core over the stall window together, a pid appeared in the job that the last sample did not have, a process outside it appeared that the last sample did not have (by pid and start), or a nested gate is paused.

  The stall clock also resets, for a `wsl` command, when the WSL VM's CPU counts as progress (see WSL below).

  A build server outside the command's job and its escaped tree is not progress: only the job's own processes and the escaped tree count, a per-run server inside the job or under a member included.
- The load-adjusted clock and the ceiling (W3): each sample adds `interval * free`, the interval running between the samples' `at` stamps, to the clock `--timeout` runs on (for `auto:<n>`, the ceiling the take resolved; the backstop below uses it too); a sample taken while a nested gate is paused adds to the paused time instead; `gate: TIMED OUT after ...`.

  The free share is 1 for a High-priority command; otherwise `busy` is the share of the interval's system time that was not idle, the cores others held are `max(0, busy * cores - own cores)` with `own cores` the CPU gained over the interval by the job and by the escaped tree (below), and `free = clamp((cores - others) / demand, 0.05, 1)`. `demand` is every core, `min(cores, 2)` for a light gate, or 1 for a gate on no slot, so its ceiling stays near wall time on a loaded PC.
- The backstop (W4): wall time less paused time reaching 5 times the ceiling, whatever the load: `gate: BACKSTOP: ...`. One sample's precedence is STALLED, TIMED OUT, BACKSTOP.
- Sampler failure (W5): `gate: sampler failed: <message>; watching by wall time only`, then the backstop alone watches. The pause markers are still read, and a paused stretch, that of the failing sample included and running to the failure's own fresh stamp, is left out of the backstop's wall time.
- WSL (W6): for a command whose first word is `wsl` or `wsl.exe` (a path allowed, case-insensitive), the VM's CPU since the last progress reaching a tenth of one core over the stall window counts as progress. The VM's CPU is not added to the command's own CPU, since the VM is the only signal of the work inside it; a `wsl` relay among the job members' descendants is an escaped process like any other, and its CPU counts as the command's own.

  The reader is `gate_win32::wsl_vm_cpu` (`times.rs`): the sum of kernel and user time over every process named `vmmemWSL`, read in-process through `NtQuerySystemInformation(SystemProcessInformation)`, never through a child process (an unelevated `OpenProcess` on the VM process is denied, and so is HCS). Plain `vmmem` is not read, since it can be any Hyper-V VM.

  The undocumented entry layout is checked: a matched entry whose create time is zero or more than a day past now, or whose times are negative, is an error.

  No `vmmemWSL` (a cold start) is silent: there is nothing to count, and the next reading is a fresh baseline, as it is when the VM restarts between samples (its CPU fell).

  A read that fails is said once per run, by the first sample whose read fails, on stderr and in the log: `gate: WSL VM CPU unavailable (<message>); retrying each sample, and until a read succeeds this wsl command counts only output and job CPU as progress`. A failed read at the watch's start alone says nothing, and every sample retries.
- A command that exits on its own before the kill keeps its status (W7).
- Build servers: the per-run server settings of [Command launching](#command-launching) (the run id as `SharedCompilationId` and `MSBUILDNODEHANDSHAKESALT`, `MSBUILDUSESERVER=0`, `DOTNET_CLI_USE_MSBUILD_SERVER=0`, `MSBUILDDISABLENODEREUSE=1`), so each run has its own compiler server and MSBuild nodes. No gate classifies, lowers or raises a build server.

  The stall check and the load figures count the job's own processes and the escaped tree only: a build server that is neither in the job nor under one of its members is neither progress nor the command's own CPU in the free share, and a server in the job, or escaped from a member, is one of its processes like any other.

    Each run has build servers of its own because a shared one is not the run's to end or to count: a server another run started would be neither in this run's job nor under its members, so a run's end could not wait on its whole tree, its CPU could not be told from a neighbour's, and a kill could not tell which server it may end.

    A server a member starts lives in the member's job, since none of the launchers (MSBuild's node launcher, Roslyn's `BuildServerConnection`) asks for breakaway and the gate's job allows none, so the run's end ends it with the job.

    The approach rejected is sparing shared servers: classifying build server images and command lines, counting their CPU only while the job held a build client, lowering or raising their priority, and leaving them out of the escaped-tree kill.

    It needed a classifier that tracks every toolchain's server names, and it could not tell a server a run needs from one that merely runs. Node reuse stays off so MSBuild nodes end with the invocation, and the compiler server's pipe id is the run's, so it is reused within the run only.

  The watch hands the pure crate raw facts: the job's CPU, the escaped tree's per-process CPU and the VM's CPU as an option (`None` for a command that is not a `wsl` one, or with no VM).
- Kill, on a STALLED, TIMED OUT or BACKSTOP alike: the diagnostics block (X3), then `TerminateJobObject(job, 124)` and the end of the processes that left the job (below), `gate: terminated the job's <n> process(es)` (with the escaped processes named, below), `gate: diagnostics written to <log>`, a 5 s wait for the command and exit 124 (X2).

  A failure inside the gate while the command runs terminates the job and ends the escaped processes first (X4); one after the command's own exit ends none of them.
- **Run end.** Once the command exits, clean or failing, and before the verdict is reached, the gate ends every process the command left in its job, with exit code 124 (`end_leftovers` in `crates/gate/src/run.rs`), so a leftover's last output is in the log the verdict's marker scan reads and a `gate: cannot end the run's job: …` line comes before the verdict line. The exit status and verdict are unchanged.

  It spares every job member whose image is `gate.exe` (compared ASCII case-insensitively): a covered gate the command left running (GATE-101's late slot), a nested gate on its own slot, and any program named `gate.exe`.

  Each is spared with its process tree, its own job (looked up under the ender's state root, then under the default root, `Local\gate-job-<pid>`, so a nested gate on the default root is found by an ender on another `--state-dir`) and each process above it that is a member of the job (its launcher, such as `cmd` or `pwsh`, hung or not, up to the first process that is not a member, the command's own).

  Each spared gate ends its own job when its own command exits, and its launcher waits on it.

  A `conhost.exe` member (its name compared ASCII case-insensitively like `gate.exe`) is never ended and is spared on its own: a console host exits by itself with its last client, and ending one would take the console away from a spared gate.

  It lists the job again after each pass, up to 5 passes, and a member is ended only when its own handle's start and parent equal the start and parent the pass's process snapshot lists for its pid, so a pid taken by a new process since the snapshot, or one whose start or parent cannot be read, is passed over and counted until a later pass; a pass that only passed over such members is followed by another.

  A pass terminates every leftover first, then waits once for all of them, up to 1 s from the first wait; a refused terminate (`TerminateProcess(<pid>) failed …`) is reported only if that process is still running when the wait ends.

  An empty process snapshot ends nothing, and a process whose start or job membership cannot be read is left running. A gate killed hard still leaves its job's processes running, since the job has no kill-on-close. The kill, stall, ceiling and Ctrl+C paths end the whole job, as above.
- The processes that left the job: a packaged desktop app started by a process of the job without package identity runs in a job of its own, outside the command's ([Command launching](#command-launching)), and so does what it starts. Every kill of a `gate run` command (a stall, the ceiling, the backstop, Ctrl+C and the stopper after a gate failure) also ends them, in `crates/gate/src/escaped.rs`:

  - Before the job is terminated, while the process that started them still runs, one snapshot and the job's members read before and after it give the processes that are not members and are descendants of a member, parents first. A descendant counts only when its start is at or after its parent's, so a recycled pid adopts no one. A job with no members takes no snapshot.
  - An escaped `VBCSCompiler.exe`, MSBuild-node `dotnet.exe` or `dotnet.exe` whose command line cannot be read is ended like any other, with everything under it; a server that descends from no member is not found. A process whose start, or whose parent's, cannot be read is not judged and is left running.
  - After the job is terminated each is opened, checked through its own handle to be the process the snapshot saw and to be outside the job, terminated and waited for, parents first; one more pass ends the children they started meanwhile. One 5 s deadline covers both passes, and the second waits only for what is left.
  - A gate among them is ended through its own named job first ([Kernel object names](#kernel-object-names), under the resolved state root), then as a process.
  - `gate: terminated the job's <n> process(es) and <m> that left it (<image> <pid>, ...)` names at most 5, then `, and <k> more`; `<m>` counts only the processes that ended, and the images are escaped. Without any it is the plain `gate: terminated the job's <n> process(es)`.
  - `gate: could not judge <n> process(es) under the job; they were left running`, once per kill, counts the processes that could not be judged (one that has ended, or whose pid no process holds, is not counted).
  - `gate: could not end <image> <pid>, which left the job: <reason>` for a process that could not be terminated or waited for, and the same line for a nested gate whose job could not be opened or terminated. `gate: could not look for processes that left the job: <reason>` when the snapshot listed no processes or the system time is outside the range a timestamp holds.
  - On Ctrl+C the look happens as the interrupt arrives, and the terminate adds what a second look finds.
  - The watch records the escaped tree (below), and every kill of a running command also ends the recorded processes with what is under them, though the process of the job that started them has ended, and the children started after the last sample.
  - These passes wait only while a process that left the job has not ended, at most 5 s for both together. So a stall, ceiling or backstop kill takes at most about 5 s for them plus the 5 s wait for the command, and a Ctrl+C whose command still runs about 15 s in all (the 5 s it gives the command, these passes, then the 5 s wait).
  - The watch counts the tree while the command runs. Each sample takes the processes outside the job under its members the way the kill's look does (members read before and after the snapshot), and records them in `escaped::Recorded`, in place of the last sample's record: each one's pid, creation time and the parent it was seen under.

    A recorded process the snapshot still lists as itself (by pid and start) stays recorded and the walk goes on below it after the launcher in the job has ended; one whose start, or whose parent's, is unknown is dropped. Each one's CPU is read by pid and start (`Recorded::cpu`); a read that fails once keeps the last read, so the process is neither dropped nor counted anew.

    That CPU counts as the command's own, unconditionally: in the journal's `avgCores`, `peakCores` and `freeCores`, the cores line, the no-slot hint and the load-adjusted clock, which no longer takes it for other processes' load. An escaped process that lives under one sample is seen only when its launcher, a member of the job, had ended before the sample (the job's memory of its members, below).
  - The run's job (only it: the box run, box tools, push and remote launch jobs have none) remembers the members that ended before a sample could list their children, through an I/O completion port associated with the job before the first process is assigned (`Job::remember`, `Job::departed` and `Job::forget` in `crates/gate-win32/src/job.rs`).

    `Job::remember` starts a thread (`job-memory`) that waits on the port 100 ms at a time and reads each new member's start the moment its `JOB_OBJECT_MSG_NEW_PROCESS` message arrives, keeping its handle open while it runs. The handle is closed, the exit time kept, when the member's `JOB_OBJECT_MSG_EXIT_PROCESS` or `JOB_OBJECT_MSG_ABNORMAL_EXIT_PROCESS` message arrives, or after a wait that came back empty. Samples and the kill's look only read the memory.

    Dropping the job removes the port association, sets a closing flag, posts a close message and waits for the thread to end; the port stays open until then, and a thread that misses the message sees the flag when its wait ends.

    A failure in the thread is stored, and `Job::departed` returns it with the members it has (`Departures`): the job then no longer remembers the processes that join it, so a kill may miss a process that left the job under one that ended. A member whose start or exit cannot be read is dropped and reported the same way, the first three by pid and the rest as `<n> more member(s) could not be read`.

    The watch says so once per run, with the sample that first sees it (`gate: the job's memory failed, so a kill may miss a process that left the job under one that ended: <why>`) and goes on sampling; the kill's look goes on with the members the memory has and adds `gate: could not look for processes that left the job: the job's memory failed: <why>`. Microsoft documents delivery as best effort, so a member can be missing from the memory.
  - Each remembered member has its pid, its start, read while its process object lives, and its exit time, read from the handle held open while it runs. The first moment it can have started a process is the later of its start and the job's creation, so a process put in the job from outside takes nothing it started before the job existed; its start alone tells it from a later holder of its pid.

    A member found gone (it had ended, and its process object was gone or a process outside the job held its pid, when the thread opened it) is not remembered and not reported. Without its start no stretch of time bounds what it started: on a busy port, a stranger that held the pid before it could have started a process inside any window guessed for it.

    It happens on ordinary runs, to a command or member that ended at once, so saying it would print the memory-failed line where nothing failed. A pid whose holder cannot be told is treated the same way.

    A process whose parent pid is a departed member counts as escaped only when it started from that member's first moment to its exit, both included, and before any new holder of the pid started, unless the holder is the member itself (a start equal to the member's). A child whose start is unknown is counted in `could not judge` only when no process holds the departed member's pid now, or the holder is the member itself.

    A departed entry is forgotten once a sample or the kill's look that began after it ended has walked it (`Job::forget`, by pid, start and exit), unless that look's snapshot listed nothing, as a failed one does. Nothing of a forgotten member is kept, so the memory stays bounded.
  - A stall, ceiling or Ctrl+C kill ends these processes with what is under them and counts them in the `and <m> that left it` line; a gate failure after the command's own exit ends none. The diagnostics block lists the departed members' children and their descendants.
  - The stall check adds the CPU the escaped tree gained since the last progress to the job's CPU against the 1% floor, and a new escaped process (by pid and start) is progress; both reset on progress. So a command whose work runs there is not stalled or timed out while it is busy. The other kill sites (`gate box stop`, the remote client, `gate push`) do not end them yet (GATE-140).
  - **Every kill site ends the escaped tree** (GATE-140), planned, not built: the box's stop, the remote client's kill and `gate push`'s kill end the escaped processes the way a `gate run` kill does, so the sites agree on what is ended.

    The escaped-tree kill spares no build server and no shared daemon, since each run's build servers live in its own job ([Command launching](#command-launching)); whether `mspdbsrv` or a .NET Framework MSBuild node can leave the job is unmeasured (GATE-143). The one process it treats apart is a gate's, whose own command's job is terminated before it is ended.
- The diagnostics block (X3) is gathered before the job is terminated and goes to the log only, between the command's output and the kill line. A command that exits during the gathering keeps its own status and writes no block. Its lines:
  - `gate: diagnostics`, then `gate:   job <pid> parent <ppid> <name>: cpu <s> s, working set <mb> MB: <command line>` for each process of the job (up to 40, then `gate:   ... and <n> more job processes`), and `gate:   outside job ...` lines for the descendants outside the job (up to 20), the recorded escaped processes and their descendants after them.
  - Up to six sample lines, `gate:   sample at <w> s (load-adjusted <a> s, free <p>%): log <n> B, err <n> B, job cpu <s> s, <n> processes, escaped cpu <s> s`, or `gate:   samples: none` when no sample was taken. `<w>` is the sample's `at` stamp.

    A `wsl` command's sample line also carries `wsl vm cpu <p>%` before the process count: the VM's CPU in percent of one core since the previous reading (`Watch::last_vm_percent`), `wsl vm cpu unavailable` when that sample's read failed, and 0% with no VM. Any other command shows nothing there.
  - `gate:   ... diagnostics cut at <n> ms` when the 2000 ms gather budget ran out. A gathering that failed writes `gate: diagnostics` then `gate: diagnostics failed: <message>` in its place.
  - The working set comes from `gate_win32::working_set` (`None` for a gone or exited process). stderr gets one line, `gate: diagnostics written to <log>` (also in the log), after the terminated line; the block's lines are not on stderr.
- A command still running 5 s after its job was terminated is reported: `gate: the command was still running 5 s after its job was terminated`. A job that cannot be terminated says `gate: could not terminate the command's job: <error>`. A killed run is not scanned for markers, and its release verdict is `stalled`, `timed-out` or `backstop` by the kill line that ended it.
- What a sample reads: the job's CPU by accounting (exited members included), the job's pids created at or after the command's start (a pid that answers access denied is not a member), the log's and `.err`'s sizes, and the system times.

  It also takes one process snapshot with the job's members read before and after it, and reads the CPU of the escaped tree under the members (`crates/gate/src/watch.rs`).

  A critical run counts as High priority for the free share, and a light gate's demand is `min(cores, light threads)`.
- Ctrl+C and Ctrl+Break (X5): the gate handles them itself and prints `gate: interrupted; stopping the command` during a run. The command got the same Ctrl+C through the shared console, so the gate waits up to 5 s for it to exit on its own and exits with its status, then terminates whatever is left in the job and the processes that left it.
- A command still running after those 5 s gets `gate: the command was still running 5 s after the interrupt; terminating its job`, the job is terminated with 0xC000013A and the gate exits 0xC000013A. Either way the verdict skips the marker scan and the release is journalled, with verdict `failed` when the status is 0xC000013A.
- Ctrl+C between taking the slot and launching the command: `gate: interrupted before the command started`, no command runs, the exit is 0xC000013A and the release verdict is `unknown`.
- Ctrl+C while queued: `gate: interrupted while waiting for a <kind> slot`, the ticket is deleted, the waiters are woken, an `abandon` line is journalled and the exit is 0xC000013A. No `still waiting` line follows it. A critical gate says `gate: interrupted while waiting for the machine` and also lets go of everything it took so far.
- A console close ends the gate at once, because Windows ends the process when the handler returns; the dead-gate cleanup reclaims its ticket and claim ([Abandoned slots](#abandoned-slots)). If the handler cannot be installed the gate says `gate: cannot install the Ctrl+C handler, so Ctrl+C ends the gate at once: <error>`.

The arithmetic (stall progress, the free share, the clock, the three kills and their lines, the box heartbeat) is a pure crate driven by samples and an injected clock; the Win32 layer takes the samples. The cores hint's average, peak, free cores and verdict are in it too. The kill lines are:

- `gate: STALLED: no output and no CPU for <quiet> s (machine free <p>% on average); killed <command> and its children`
- `gate: TIMED OUT after <timeout> s of load-adjusted time (wall <wall> s, machine free <p>% on average); killed <command> and its children`
- `gate: BACKSTOP: <wall> s of wall time (5 x <timeout> s ceiling, load-adjusted <adjusted> s, machine free <p>% on average); killed <command> and its children`, where the middle clause reads `machine load unknown: the sampler failed` after a sampler failure.

`<p>` is the free share averaged over the wall time the ceiling counted (paused seconds left out), a whole number rounded half to even, 100 while nothing is counted; the seconds are rounded the same way. After a sampler failure only the backstop is read.

A builder counts a routed run's ceiling with the same arithmetic ([The box side](#the-box-side)) and says the lines with the host: `gate: TIMED OUT on <host> after <timeout> s of load-adjusted time (...)` and `gate: BACKSTOP on <host>: <wall> s of wall time (...)`, the host left out (`gate: TIMED OUT after ...`) when the payload names none; its stall stays the client's.

The box heartbeat (BW9) is the box wrapper's command-time heartbeat, not the wait heartbeat: a pump thread writes `gate-remote-heartbeat: <cpu ms>` every period, and while a command is watched only when the CPU used since the last beat is at least 1% of the time elapsed since it. A failed CPU read writes `gate: remote heartbeat stopped: <message>` and ends the heartbeats, never the run. `gate box run` is the crate's caller ([The box side](#the-box-side)).

## Command launching

- **The argv arrives byte-exact.** `gate.exe` reads its own command line (`GetCommandLineW`), splits it by the Microsoft C runtime's rules, and takes every word after the first `--` as the command, unchanged (C3). The PowerShell binder workarounds (C3's rejoined colon words and flattened arrays) do not exist.
- **C-runtime quoting.** The words are written into one CreateProcess command line so the C runtime reads each back as one argv entry: an empty word or one holding whitespace or a quote is wrapped, `"` becomes `\"`, and a backslash run before a quote or at the end is doubled (kept, C4).

  Splitting the written line gives back the words, for every list of argument words; a property test holds that. A program word must not hold a quote or end in a backslash, because the C runtime reads argv[0] without backslash escapes (the PowerShell gate has the same limit).
- **`.cmd` and `.bat` targets** run as `"<system folder>\cmd.exe" /d /v:off /s /c "<line>"`, the path written C4-quoted and each word quoted for cmd.exe (C5). The system folder is `GetSystemDirectoryW`'s; `%ComSpec%` and `%SystemRoot%` are not read, a deliberate difference from the PowerShell gate. A target is a program whose extension is `.cmd` or `.bat` in any case, after the trailing dots and spaces of its file name are dropped.

  A word holding `"` is written `""` for a batch file, which programs on the old msvcrt.dll read differently from the C runtime's `\"`. This is a known limit shared with the PowerShell gate.

  The gate refuses, with exit 2 before any slot is queued, a word that cmd.exe would split, cut or expand, the batch file's own path included:

  - a word holding a CR, LF or U+0000;
  - `%NAME%` of a variable set in the command's environment, names compared case-insensitively over all of Unicode and the lookup answering for the exact environment block the command gets;
  - any `%=…%` name (`%=ExitCode%`, `%=C:%`), set or not, since cmd.exe's hidden variables appear in no environment listing;
  - a dynamic name cmd.exe expands though no variable is set: `%CD%`, `%DATE%`, `%TIME%`, `%RANDOM%`, `%ERRORLEVEL%`, `%CMDCMDLINE%`, `%CMDEXTVERSION%`, `%HIGHESTNUMANODENUMBER%`, `%__CD__%`, `%__APPDIR__%`, and `%COMSPEC%`, `%PATHEXT%` and `%PROMPT%`, which cmd.exe sets for itself when the block it is started with lacks them;
  - the substring and substitution forms `%NAME:~a,b%` and `%NAME:a=b%` of a set or dynamic name, and a whole `%NAME:…%` name that is itself set.

  A lone `%` and an unset name pass. The gate and the box side share this one implementation. The refusal is one line, `gate: a .cmd or .bat command cannot take a word holding a line break or a %NAME% cmd.exe would expand: <word>`.
- **Program resolution.** The program must resolve to an absolute path (kept, C6):

  - A name with a directory part is read against the working folder, `.` and `..` folded and separators written `\`; a drive-qualified or UNC name is kept as written.
  - A name with an extension but no directory is looked for as written in each absolute PATH folder.
  - A bare name is looked for in PATH × PATHEXT, the folders outer and the extensions inner, as `Get-Command` orders them.
  - The current folder and relative PATH entries are never searched, and an extensionless shim is never found.

  The program's path is then made a full path (`.` and `..` folded) before the batch check, and that path is the application.

  A name found nowhere is a start failure. Unlike the PowerShell gate, which lets CreateProcess search for the program (the current folder among others), the gate never passes an unfound name on. This is deliberate, for security: a repo cannot plant a `git.exe` at its root. The `gate-win32` layer passes the path as `lpApplicationName`, so CreateProcessW searches no folder (not the gate's, the current one, System32 or PATH).

  A start failure prints `gate: could not start <word> (<resolved>): <message>` and exits 127, releasing the slot (C7); the lines are in [Output lines](#output-lines).

  A program the gate's own tool probes and SDK read find along `PATH` (`program_output` in `remote/git.rs`) and judge against the worktree is followed through links, and the file the link leads to is the image started. Its command line's first word stays the name it was found by, not the target's, because a multi-call program picks what to be from argv[0] alone: rustup's `rustc.exe` and `cargo.exe` are links to `rustup.exe`, and started by the target's name they would answer as rustup. A batch target still runs through the system folder's cmd.exe, given the target.

  The worktree refusal covers only the file launched: a program it starts in turn (a multi-call proxy, a shim) is not judged.
- **Process creation.** The `gate-win32` layer is the contract the run uses (`Launch` → `Suspended` → `Running`). The command is created suspended with an explicit environment block, assigned to the gate's job `Local\gate-job-<gate pid>`, then resumed (kept, X1, P1). Creation flags are `CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT` plus the priority class, with no new console and no new process group.

  The job has no limits: no breakaway, no kill-on-close. (This is `gate run`; the box's command is created with `CREATE_BREAKAWAY_FROM_JOB` and a restricted non-elevated token, [The box side](#the-box-side).) (A `test-seams` build's `--test-job-breakaway` makes the job allow breakaway, `JOB_OBJECT_LIMIT_BREAKAWAY_OK`, still without kill-on-close: `Job::create_breakaway`.)

  It holds the command and what the command's descendants start, except a packaged desktop app started by a non-packaged descendant, which runs in a job of its own with what it starts; a kill ends those too ([Watchdog](#watchdog)). A job name already held is refused. If assigning to the job or resuming fails, the command is terminated with exit code 1 first, and a suspended command dropped without being resumed is terminated with exit code 1.

  The desktop-app policy `PROCESS_CREATION_DESKTOP_APP_BREAKAWAY_DISABLE_PROCESS_TREE` is set, as the PowerShell gate sets it. Per Microsoft's UpdateProcThreadAttribute remarks it matters only to Win32 apps packaged through the Desktop Bridge, whose children it keeps inside the desktop app runtime environment.

  So a packaged app the gate starts itself keeps its tree in the job: the command.

  `PipedLaunch` creates its command with the policy too and, on a system that cannot set it (before Windows 10 1703), without it, saying so in `Suspended::note`.

  A packaged desktop app started by any non-packaged descendant (the Store's pwsh or `winget` under a `cmd`, a git hook's pwsh) runs in a job of its own, outside the gate's, and no documented control keeps it in: only a direct child of the gate, assigned explicitly, with the policy, stays in. `IsProcessInJob(h, NULL)` and the job's `TotalProcesses` tell it in one run; `winget --version` under `cmd` is the packaged reproducer on Windows 11 and Server 2025.

  The layer refuses a `.cmd` or `.bat` application (any case, trailing dots and spaces ignored): the caller wraps batch targets in `"<system folder>\cmd.exe" /d /v:off /s /c "<line>"` itself. It refuses a NUL in the application, command line, working folder, log path or `.err` path before anything is opened.

  The log and `<log>.err` are each opened fresh. Any file at the path is deleted first (a link itself, never its target), then created with `CREATE_NEW` and `FILE_FLAG_OPEN_REPARSE_POINT | SECURITY_SQOS_PRESENT | SECURITY_ANONYMOUS`; it must be a disk file, not a reparse point, with exactly one name.

  Both are shared for read, write and delete, so a process the command left running does not block the next run on that log; a second gate on a live log is kept out by the log's mutex ([Kernel object names](#kernel-object-names)). An orphan's old, delete-pending file makes the create fail loudly rather than be shared. stdin is `NUL`, opened `OPEN_EXISTING`.

  It inherits exactly three handles, through `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`: stdin from `NUL`, stdout to the log and stderr to `<log>.err`. The gate's own handles to them are not inheritable: the command inherits copies made under the spawn lock, which exist only while the lock is held, and a failed copy is a setup error, `copy a handle for the command to inherit: <message>`.

  The spawn lock is process-wide and is held while the gate starts any process, so only the launched command inherits the copies. A process the gate starts itself goes through `gate_win32::spawn`, which holds the lock for the start alone; one started any other way could inherit the copies if it started while they are open.

  The command shares the gate's console, so Ctrl+C reaches it.

  A nested gate is told from the slot marker in its environment, honoured only when the marker binds to the parent's job ([Critical drain and nesting](#critical-drain-and-nesting)), not from its own job alone: its job nests inside its parent's when the nested gate is in the parent's job, but under a `cmd` and then a packaged pwsh, or a git hook's pwsh, it is in no job of the parent's, and the kill reaches it as one of the processes that left the job.

  The start-failure message is the system message for the CreateProcessW error code, as `Win32Exception.Message` gives it, with no `(os error N)` suffix. That failure alone maps to status 127; every other start failure is the gate's own.
- **Priority.** BelowNormal, or the gate's own class when that is already BelowNormal or Idle; High for a critical gate and any gate under a critical marker (kept, P2).
- **Environment.**

  Every command gets the gate's environment with these set over it (names compared case-insensitively): `GATE_SLOT_HELD=<kind>:<pid>:<start_ms>[:<h>]` (the held slot's marker, `none` too, or the parent's for a covered gate; a gate lent its light parent's slot holds none and hands `heavy:<its pid>:<its start>[:<h>]`, naming itself), `SharedCompilationId` and `MSBUILDNODEHANDSHAKESALT`, both the run id, `MSBUILDUSESERVER=0` (over an inherited value), `DOTNET_CLI_USE_MSBUILD_SERVER=0` (kept, P3), `MSBUILDDISABLENODEREUSE=1` (over an inherited value), and `GATE_STATE_DIR=<root>` under a non-default root.

  Under the default root an inherited `GATE_STATE_DIR` is removed. The gate's own environment is never changed.

  The run id is `gate-{pid}-{start ms}`, with `-{root hash}` added under a non-default state root. Each nested gate has its own id. So each run gets its own .NET compiler server, and MSBuild's node reuse is off: with it on, the nodes idle about 15 minutes inside the command's job, so anything in the command waiting on its whole tree would hang. The gate ends what the run left at its [end](#watchdog).

  The batch refusal's `%NAME%` check answers for this exact block.

  The block is built with names matched and sorted as Windows does (`CompareStringOrdinal`, case-insensitive over all of Unicode), and a later pair wins.
- **Working directory.** The caller's (P5), or the `--cd` folder when given ([Run folder](#run-folder)).

## Log and tail

- The log is kept as the command's exact bytes. The log's directory is created, an earlier log and `<log>.err` are removed at start, stdout goes to the log and stderr to `<log>.err`, and at the end the `.err` bytes are appended to the log byte for byte (once) and `.err` is deleted. The gate's own lines follow, starting on a line of their own: every line it said on stderr, and the passed line. So one file holds the output, then the errors, then the gate's lines (kept, V1).

  The gate's lines are the run's notes, gathered while it goes and appended to the end of the log. The verdict lines are appended after the tail is printed, so the log ends with them and the printed tail does not hold them. A hard-killed gate's lines are only on stderr.
- Lines end at CR LF, LF or a lone CR, as .NET's `ReadLine` ends them, for the marker scan and the tail. Both decode the log as UTF-8 lossily: an invalid byte reads as U+FFFD. The gate writes the tail to stdout as UTF-8 (through the wide console API when stdout is a console).
- The failure-marker regex is built in (kept, V2): `^Build FAILED\.|error CS\d+|: error |Test run summary: Failed!|^\s*failed: [1-9]|gate: (TIMED OUT|STALLED|BACKSTOP|TRANSFER TIMED OUT)`.

  It is matched case-insensitively, per line. A zero exit whose log matches becomes status 1 with `gate: command exited 0 but its log reports a failure -> <log>`; on any non-zero status up to 40 matching lines are printed to stdout as `<line number>:<line>`; a killed run is not scanned. `--no-markers` turns the scan off.
- The scan reads the log streaming and never whole: at most 64 KiB of any one line, and it stops after 40 matches. The tail is read backwards from the end of the log, at most 1 MiB of it; when the last line is longer than that window, the tail shows `… ` followed by its last 4096 characters, starting on a whole character. Each printed tail or marker line is cut at 4096 characters, ending ` …`.
- At the end the last `--tail` lines go to stdout, taken from the log without the lines of a kill's diagnostics block (dropped by position, so the command's own lines are never lost), then the verdict line ([Output lines](#output-lines)) (kept, V3).
- A panic after the slot is taken terminates the job, appends the `.err` file and the gate's notes to the log (when completing the log fails, the notes are kept for the next write), deletes the claim and journals `release` with `status` null and `verdict` unknown.

## Routing

Routing is off on a machine with no `remote.json` in `%LOCALAPPDATA%\gate\`: nothing is read and nothing is written.

- **What routes** is kept (T2): a heavy gate, not `--local`, under no covering or lending parent, outside a git hook (`GIT_INDEX_FILE` set to a non-empty value), whose first word is not `git` or `prek`, in a git worktree of a repo `remote.json` lists, whose words with in-worktree paths rewritten (T3) match one of that repo's patterns.

  A matched command still runs here, saying why, when a word names a path outside the worktree (a drive-relative, root-relative, share, device or climbing form included, see [the path rewrite](#routing)) or the gate's folder holds nothing the snapshot sends. The one exception is a first word naming a program by an absolute path, which the path rewrite sends as the program's name unless it refuses the name.

  `--no-build` is dropped from the builder's words of a dotnet `test` or `run` command, saying so (T4); `build`, `msbuild`, `pack` and `publish` keep it everywhere. The fallback here drops the same words after every refusal, rebuilding locally as the builder would have, because an earlier routed step (a repo's test-all build) may have built only on the builder, so a later `dotnet test --no-build` that falls back here would find no build output.

  A step that really did build here pays a second local build on a refusal, a cost accepted. A fallback that drops no word reuses the first plan.

  A dotnet command is one whose first word's file name without extension is `dotnet`, or is `pwsh` or `powershell` with `--` followed by a `dotnet` word (a script handed a dotnet command to run), each compared in any letter case. A dotnet command's own words run from its `dotnet` word to its next `--`, and only they are read: a `--no-build` after that `--`, or before a wrapping script's `--`, belongs to another program and stays, so plain `dotnet test -- --no-build` keeps the word.

  A dotnet command with a word starting with `@` (a response file) is opaque, since the gate cannot see its words: it drops no word, gets no `SourceRevisionId` stamp, writes no nothing-back record and clears none.

  A run that stays local (the Local route) drops the same `--no-build` words before it takes a slot, when it would find no build here, saying `gate: dropped --no-build: <reason>; building on a heavy slot`, or `gate: dropped --no-build: <reason>; building on the parent gate's <heavy|critical|light> slot` under a covering or lending parent. A routed run is never dropped here. `<reason>` is one of two:
  - `the last build ran on <host> and brought nothing back`, when the worktree's nothing-back record stands (`<host>` is the record's builder, else `the builder`).
  - `<project> has no obj\project.assets.json here`, asked only when no record stands.

  A dropped run is a build, so it takes a heavy slot whatever kind it named: its queue, take and release journal lines, its ticket and its claim show kind `heavy` and the command without `--no-build`. A light child of a light parent is no longer covered after the drop, and a heavy run the drop makes uncovered asks its light parent again whether it lends its slot. The covered, ignored-marker and could-not-judge lines are said after the drop is decided and name the final kind.

  The obj check covers the project the command names (a project file, or `--project`), else the projects a named `.sln`, `.slnx` or `.slnf` lists, else the worktree's tracked `*.csproj`, `*.fsproj` and `*.vbproj` (`git ls-files`).

  The check is skipped, and the word kept, when the worktree root's `Directory.Build.props` or `Directory.Build.targets` names `UseArtifactsOutput`, `ArtifactsPath`, `BaseIntermediateOutputPath` or `MSBuildProjectExtensionsPath`. Solution entries that are not plain names inside the worktree are skipped.

  Inside is judged by canonical path, both sides resolved: another spelling of the worktree (a `subst` drive, a junction) counts as inside, and a project folder inside the worktree that is a junction leading outside counts as outside.

  Props and solution files are read only as regular files up to 1 MiB; otherwise the gate says `gate: cannot read <path>: it is a link, a junction or not a file` or `gate: cannot read <path>: it is larger than 1 MiB`, and the word is kept. Printed paths escape control characters. When git fails the gate says `gate: cannot find the command's git worktree: <error>` or `gate: cannot list the worktree's projects: <error>` and keeps the word.

  The git calls of the drop and the clear run with `LC_ALL=C`. The clear prints nothing when git is missing or the folder is outside a repository.

  The nothing-back record ([State root](#files), [Glossary](GLOSSARY.md)) is written by every routed dotnet build (verb `build`, `test`, `publish`, `pack`, `run` or `msbuild`, with no `--no-build` of its own) that ends on the builder, a failed, killed or interrupted run included, unless the run's outcome says its outputs were all brought back.
  The Rust remote client says `yes` only when the outputs archive installed complete (skipping at most files it cannot rewrite), or when the box packed nothing for a PC whose sent manifest held files, and no name was skipped (the box's `gate: remote outputs: skipped ` line); every other end of the outputs step is `no`, and an end that never reaches the outputs step is `unknown`. No and unknown count alike: the record is written.

  A routed dotnet build that passed (status 0) with its outputs all brought back removes the record instead, as a passing build here would: only when its words are whole (they name a solution, or nothing at the worktree root), its verb is not `msbuild`, and its `-c`, `-f` and `-r` match the record's; otherwise the record is left as it was, not rewritten. A record that cannot be removed says `gate: cannot remove <path>: <error>`, as the clear here does.

  A passing routed dotnet build that did not bring everything back also says which folders it left stale ([Output lines](#output-lines)); that line reads the outputs outcome and never changes the record.

  A routed dotnet `--no-build` command whose word the builder dropped (the builder built, and nothing comes back) writes the record when none stands, whichever client ran it and whether or not the repo has output globs, with the builder and the `-c`, `-f` and `-r` its words set; it never rewrites or clears a record that stands.

  When the record's presence cannot be read it says `gate: cannot read <path>: <error>` and writes nothing. When the route withheld the run's outputs (the repo has output globs), the Rust client also writes the outputs pending marker for it ([The remote client's run](#routing)), so the next routed run of the worktree sends a full manifest.

  A fallback after a refusal writes none. A record that cannot be written says `gate: cannot write <path>: <error>`.

  A passing build here clears the record only when all three hold: its verb is not `msbuild`; it names a solution, or names nothing and runs at the worktree root; its `-c`, `-f` and `-r` words match the record's, ignoring letter case, an absent word equal to an absent one. A build that names one project, or runs from a subfolder, does not clear it. A record that cannot be removed says `gate: cannot remove <path>: <error>`.

  A record that cannot be read counts as present, saying `gate: cannot read <path>: <error>`: a record wrongly taken as present costs one build here, one wrongly taken as missing a `--no-build` run that finds no build.

  Each routed run pushes a new snapshot commit, and the .NET SDK stamps the source revision into every assembly's informational version, so without a pinned revision a routed build recompiled every project on every run.

  A routed dotnet command whose verb (the next non-empty word after `dotnet`, in any letter case) is `build`, `test`, `publish`, `pack`, `run` or `msbuild` therefore gets `-p:SourceRevisionId=<the worktree's HEAD sha>` on the builder's words only: after the dotnet command's own words, before its own `--`, else at the end (a wrapped dotnet command gets it before dotnet's own `--` too).

  The word is not added when one of the dotnet command's own words already sets `SourceRevisionId` (`-p:`, `/p:`, `-property:`, `/property:` or `--property:`, in any letter case, alone or in a list separated by `;` or `,`). The sha comes from `git rev-parse --verify -q HEAD` in the worktree root, asked only of a command that would get the word; when that fails or the branch is unborn the run routes without it.

  The SDK keeps an explicit value (its git targets fill `SourceRevisionId` only when it is empty) and a `-p:` global property overrides a repo's own `<SourceRevisionId>`, so the builder's DLLs read `1.0.0+<HEAD sha>` as a local build's do, and a no-change routed build compiles nothing. The fallback here, the journal and the claim keep the command as given, and no output line says the word was added.
- **The decision is `gate-router`'s**, pure, over facts the caller gives it and the decision asks for lazily, in this order. A light or critical gate, or one under a covering or lending parent, stays local silently and reads nothing. `--local` stays local, saying `gate: not routed: --local`. Only then is `remote.json` read: no file stays local silently, and a file that is not a file, cannot be read or is refused stays local saying `gate: ignored <path>: <reason>; running locally`.

  After the file: a git hook, a command with no non-empty word, or a first word whose file name without extension is `git` or `prek` (`git.exe` and a full path included, in any letter case) stays local silently, as does a matched command whose first word the path rewrite sends as a program's name that is `git` or `prek` (`C:\x\git.exe.exe status`).

  Git is asked only then: a folder in no worktree (any non-zero `git rev-parse` exit), a repo the file does not list (the main checkout's folder name as a repo name, in any letter case) and words no pattern of the repo matches stay local silently, the first matching pattern winning. A fact that cannot be read stays local saying `gate: not routed: <reason>`. A machine with no `remote.json` costs no git call.

  The repo's `.gate/siblings.toml` is read once the file lists the repo, before the words are rewritten, so the rewrite knows the siblings; a gate that stays local before that point never reads it.

  A command no pattern matches stays local silently whatever the file holds.

  Only a matched command acts on what the file says, in this order, each keeping the run here with its line.

  First a problem with the file (`gate: not routed: .gate/siblings.toml: <problem>`, the first problem, on one line, control characters and reordering marks escaped).

  Then, for declared siblings: a name the builder cannot lay out (the same `.gate/siblings.toml:` line, checked again whatever the reader checked, before consent).

  Then a declared sibling `remote.json` does not allow (`gate: not routed: sibling <name> is not allowed in remote.json`; every sibling is not allowed when the `siblings` value cannot be used, the line then ending `: <problem>`).

  Then a root folder name the builder cannot lay out (`gate: not routed: the worktree folder name <leaf> cannot be laid out on the builder`); a sibling named like the root's own folder (`gate: not routed: .gate/siblings.toml: siblings[<i>] is the worktree's own folder: "<name>"`).

  Then each sibling's folder, in declared order, the first fault stopping the look: no worktree beside the root (`gate: not routed: sibling <name> not found beside the worktree`), or a refusal (`gate: not routed: sibling <name> <reason>`, [Output lines](#output-lines)).

  Last, by folder id: a sibling whose folder is the worktree's own (`is the worktree's own folder`) or an earlier sibling's (`is the same folder as sibling <other>`), reached under two names.

  A sibling's folder is present when it is a real folder that is a direct child of the root's parent, `<parent of the root>\<name>`, opened by its name inside the parent without following a link (a link or junction is refused as `is a link or a junction`, a file as `is not a folder`), holding a `.git` that is a folder or a linked worktree's file and not a link or junction (`has a .git that is a link or a junction`); with no `.git`, or nothing there, it is missing.

  A failure to read says `gate: not routed: cannot read the sibling folder <path>: <error>`.

  The folder ids are the volume serial and 128-bit file id of each open folder. A repo with no file declares no siblings and nothing changes, except that the root's own folder id is read for every matched command, siblings or none, once the declared siblings are found.

  A root whose id cannot be read keeps the run here with `gate: not routed: cannot read the id of the worktree folder <root>: <error>`, since the worktree lock of every routed run is named by it, and the client checks the folder it opens against it.

  git is found along the gate's own `PATH` (absolute entries only) and run by its full path from the gate's folder, with the `GIT_*` variables that point it at another repository, index or config removed and `core.fsmonitor` off and `core.hooksPath` pointing at a folder that cannot exist (inside the git program's own file), so a gate in a hook that exports `GIT_DIR` still routes and no hook a repository's config names runs.

  Each call gets 60 s, after which git is killed and the gate runs here saying `gate: not routed: git did not answer within 60 s` (in whole milliseconds, `1 ms`, when `--test-git-limit-ms` sets a limit that is not a whole number of seconds).

  The limit is long because a loaded PC took git over 10 s in tiny repos, which kept heavy gates here exactly when the builder would help most; a git that truly hangs costs a heavy gate a minute.

  `gate run` answers those facts in `route.rs`, before it queues and before any command starts. The routing file is `%LOCALAPPDATA%\gate\remote.json` (no file when `LOCALAPPDATA` is unset or empty). The hook rule reads `GIT_INDEX_FILE`: set and non-empty means a git hook, which never routes.

  The worktree comes from one `git rev-parse` run in the gate's folder, which is the `--cd` folder when given: a caller in a main checkout gates a worktree's command with `--cd <worktree>`. It reads (root, prefix below the root and common git folder), and a folder below the root asks `git ls-files` whether the snapshot would send anything there (tracked or untracked and not ignored, and still on disk). A `subst` drive is followed through `gate_win32::device_target`. A word that is not valid Unicode keeps a command that would route here, saying so.

  The queue line carries `notRouted` `--local` for a heavy `--local` gate and the remote tag for a routed one. When the remote client has a part missing, the routed run falls back before it takes a remote slot: the `fallback` line has reason `client`, the gate says `gate: remote <host> client could not start: ...; running here`, and the later `take` and `release` carry no remote fields.
- **The routing file's text** is JSON with `//` and `/* */` comments and trailing commas allowed outside strings, and a leading byte order mark skipped, as the PowerShell gate's reader allows. Keys match in any letter case, and unknown keys are ignored.

  An object holding two keys that differ only in letter case anywhere in the file refuses the whole file, with the PowerShell reader's error text. Single-quoted strings are refused (`Conversion from JSON failed with error: ...`), a recorded deviation: the PowerShell reader accepts them.

  `host` is an ssh alias or `user@host`, each part of ASCII letters, digits, `.`, `_` and `-` and not starting with `-`: the host reaches ssh's argument list, a git `host:path` URL and ssh_config `%h` expansion, and only that shape is safe in all three.

  Any other host refuses the file (`host must be an ssh alias or user@host of letters, digits, '.', '_' and '-', not starting with '-'`), a recorded deviation: the PowerShell reader accepts any non-empty string. An IPv6 literal or a `host:port` is written as an alias in `~/.ssh/config`.

  `slots` is a whole number 1 to 16 (1 when missing), `repos` an object whose values are arrays of non-empty `-like` patterns, and `outputs` an optional object of the same shape whose globs may not hold `[` or `]`; a repo with no `outputs` entry brings back `**/bin/**` and `**/.godot/mono/temp/bin/**`.

  `inputs` is an optional object of the same shape with the same `[`/`]` ban (`is not a valid input glob: '[' and ']' are not allowed`), read by the same table reader; a repo with no `inputs` entry sends nothing, and there is no default list. The route carries the repo's list as `RemoteRun.inputs` and `RemoteCall.inputs`, kept for a dotnet `--no-build` command, whose outputs alone are withheld.

  A cargo command instead (its first word's file stem is `cargo` in any case; one a `pwsh` script is handed after `--` is not) brings back `target/*/*.exe`, `target/*/*.dll` and `target/*/*.pdb`: each profile folder's own files, never `deps/`, `build/` or `incremental/`. A repo's own `outputs` list replaces either default.

  `siblings` is an optional object of the same shape as `outputs`, but each value an array of folder names the builder can lay out (`is_layout_name`): the sibling worktrees the machine lets that repo carry, its repo name matched in any letter case and each sibling name exactly. A repo with no entry is allowed none.

  A `siblings` that is not an object, or holds a value that is not an array of such names, never refuses the file: it allows no sibling, and the rest of the file is read as usual ([Sibling worktrees](#routing)).
- **The `-like` matcher** is PowerShell's wildcard match, case-insensitive, over the words joined by single spaces. It folds case by simple case folding. Inside a `[...]` set, a non-ASCII member whose .NET case-insensitive equivalents differ (the Kelvin sign and `k`) can match in PowerShell and not here.
- **The in-worktree path rewrite** (T3) finds a path wherever a drive letter, a colon and a separator start one inside a word (`--flag=X:\a`, `-p:Name=X:\a`, a script's text), each start judged on its own.

  A path runs to the first of `, ; " ' ) | & < >`. In a word that holds one of those or a line break (a script word: a script's text or a list) white space ends a path too; in any other word, one argument, a path runs through its spaces.

  The router keeps this `subst` model for anyone who routes from a subst drive; its tests use a made-up drive letter, never a mapping of the machine they run on.

  A drive other than the root's is followed through `subst` mappings as a chain, at most 26 hops with a visited set, and the root's own letter is never asked; a cycle, an unreadable device or a target that is not a `\??\` path gives no spelling, and the run stays local.

  A bare first word (no separator, drive or `.`/`..`) becomes `./name` only when that file exists in the gate's folder; otherwise it stays bare, for the builder's `PATH`.

  A first word that names a program by an absolute path outside the worktree and its siblings is sent as the program's name, the file name with a trailing `.exe` removed, so the builder finds it on its own `PATH` (a Godot or dotnet install named by its full path). The run says once, first among its routed lines, `gate: routed <given> as <name>` (both escaped), and the fallback here, the journal and the claim keep the words as given. Such a word is never written `./name`.

  The name is refused, and the word stays outside (the run stays here with `gate: not routed: <word> is outside the worktree`), when the file name holds any of `* ? " < > |`, white space or a line break, or is empty once `.exe` is removed; when any path segment after the drive (folders and file name) holds `:`, holds `~` followed by an ASCII digit (a short name), ends in `.` or a space, or is empty, `.` or `..`; or when the word holds a second drive path (`X:\a\x.exe;C:\other`).

  With siblings declared, a path inside a sibling's root (every spelling the root can have, `subst` drives included) is sent as `..` once for each level of the gate's folder below the worktree root and once more, then `/<name>` as declared and the rest, with the separator the in-worktree edit uses.

  A relative path is inside a sibling when its `..` climb exactly to the root's parent and then enters a declared sibling (names compare in ASCII letter case only; `yaat-server2` is not `yaat-server`). Any other climb above the root, a word naming the parent itself, and leaving a sibling again (`../yaat-server/../x`) are outside, and keep the run here with `gate: not routed: <word> is outside the worktree`.

  An absolute path whose `..` leaves the root it starts below and lands in the worktree or in a declared sibling is sent as where it lands; landing anywhere else is outside.

  Windows stops `..` at the root of a drive or share, so a `..` past that root is outside, never a climb to the root's parent. The floor comes from the spelling of the gate's folder: a folder spelled on a drive, a `\\?\X:` path or a UNC share whose root is the worktree root or a folder below it sits fewer folders below that root than it sits below the worktree root, and a relative climb past the difference is outside.

  A path on a bare `subst` drive that stands for a root (`S:\..`) never leaves that root either.

  Every word is judged for outside at each path start: the start of the word, after white space or a line break, after `, ; " ' ) | & < > ( =`, after a leading `@` (and an `@` right after any path start), and after the colon of a switch that starts its white-space token or follows a `"`, `'` or `,` (`-bl:`, `/p:`). The forms below keep the run here with the same `is outside the worktree` line. A command's first word that names a program by an absolute path is judged first, as above: it is sent as the program's name, or, when the name is refused, it is outside like any other path.

  Drive-relative text is a letter and `:` not followed by `\`, `/` or `:`, anywhere in a word or at any path start (`C:..\..\secret`, `-bl:C:..\x.binlog`); `-c:Release`, `/t:Build`, `$env:PATH` and `crate::mod` are not. Root-relative text is a single `\` at a path start, or a single `/` only where a value begins (after `=`, a switch colon or `@`); `/p:...`, `/c` and nextest's `test(/^run::/)` stay switches or text.

  A share or device path at any path start is outside: two separators, or a drive right after `\\?\`, `\\.\`, `//?/` or `//./`. Relative text that climbs out of the worktree (or past the drive-root floor above) is outside at every path start, not only at the word's.
- **The work folder** on the builder is named `<repo name>-<key>`, the worktree root's last segment cut to letters, digits, `-` and `_` and 8 hex digits of the SHA-256 of the lower-cased root ([Glossary](GLOSSARY.md), work-folder key).
- **The routed run's life** is kept (T5) and built in `remote/mod.rs` and `run.rs`: its ticket is heavy with `remote: true` and queues apart from the local waiters, which never count it ahead of them, nor it them; it waits up to 60 s for a remote slot (`--test-remote-wait-ms` in a test build); it takes one with a claim of kind `remote` of its own and calls the remote client with the route, the log and the limits (`--stall` as given, the interrupt).

  A client that runs the command to its end hands back its status, which the gate passes through, and the run releases the slot with the remote tag. On `full` (no slot within the wait), a refusal (status 75 with a reason) or a client that cannot start, it journals `fallback` while still holding the remote slot (none was taken on `full`), deletes the claim, lets the slot go, and runs here, taking a heavy slot with its first arrival kept in the ticket.

  The attempt's lines stay in the log once, as notes: the client's own bytes in the log are removed, and the local run writes its log afresh.
- **The remote client** (GATE-43) is built in parts.

  The seam a routed run calls is `RemoteClient` in `crates/gate/src/remote/mod.rs` (the call, the outcome, the scripted client); part A, the snapshot, and part B1, the transport, the routed lock, the preflight, the push and the run call, and part B2, the sync-back, part C1, the watchdog, the stop, Ctrl+C, the sync-back's deadlines and the output cap, and part C2, the lost connection and the reboot, are built, and every routed run uses them (The remote client's run, below).

  The snapshot copies the real index to `<git path>/gate-remote-<pid>.index` (a leftover copy and its `.index.lock` are removed first; the copy is empty when the repo has no index). With `GIT_INDEX_FILE` on the copy it runs `git add -A`, `git write-tree` and `git commit-tree --no-gpg-sign` with no parent, under the identity `gate-remote <gate-remote@localhost>` and with `GIT_AUTHOR_DATE` and `GIT_COMMITTER_DATE` removed.

  The copy is always removed, and the real index and the worktree are never touched, so both are byte-identical before and after a snapshot. The snapshot takes a list of roots, one a repository: the main one, then a pair's siblings in declared order.

  For a pair every git call about a repository is given `GIT_CEILING_DIRECTORIES` set to the root's parent, so git never reaches a repository above it.

  Once the repository's place is checked (its top folder is its root, see [Output lines](#output-lines)), each later call is given `GIT_DIR`, `GIT_WORK_TREE` and `GIT_COMMON_DIR` outright, as git printed them, so a `core.worktree`, a `commondir` or a `gitdir:` written after the check cannot point it elsewhere; the place is asked again after each save and a different one keeps the run here.

  Every git call runs with `core.hooksPath` off. A clean filter in a repository's own config still runs during the snapshot's `git add`: the repository's config is the user's, an accepted risk.

  The bridge commit has the snapshot's tree and, as its parent, the newest local `refs/gate-remote/*` not ending `-back`; with none it has no parent. The push writes `refs/gate-remote/<key>` once the builder has the snapshot.

  Every git call of the client goes through `remote/git.rs`, which the routing decision's git calls share: it removes the `GIT_*` location variables, applies the routing decision's time limit (`--test-git-limit-ms`), starts git through `remote/launch.rs` (suspended, assigned to an anonymous kill-on-close job of its own and resumed, with its output on pipes read by a thread per stream), reads and drops stderr where nothing reads it and ends git's whole job on Ctrl+C.

  The calls the client reads (`Git::output`: the sync-back's and the snapshot's) read git's output up to 16 MiB and keep the last 64 KiB of its stderr; a call past the cap is ended and fails with `git <args> failed in <folder>: its output passed 16 MiB`. The routing decision's calls are not capped. A Ctrl+C that ends git is an interrupt.

  The sync-back's `update-ref` and `apply` and the snapshot's `update-ref` run on a hidden console and answer to no interrupt, so a Ctrl+C cannot end git while it holds a ref lock in the git folder.

  Once the fetched back commit is confirmed as this snapshot's, every step up to a successful `apply` (the changed-file listing, the worktree check, the patch and the `apply` itself) that fails or is interrupted first keeps the back commit at `refs/gate-remote/<key>-back` and says so:
  - a failure: `gate: the remote run failed: <reason>; the changes are kept: see git diff <snapshot 8 hex> <back 8 hex>`;
  - an interrupt: `gate: the run was interrupted; the changes are kept: see git diff <snapshot 8 hex> <back 8 hex>`, with the interrupt's own status;
  - a keep that fails too: `gate: the remote run failed: <reason>; the changes could not be kept either: <why>`, or after an interrupt `gate: the remote run failed: the run was interrupted, and the changes could not be kept: <why>` (status 1). When the fetched objects could not be moved into the repository (Sync-back, below), the owner line is said first and `<why>` is `see the line above`.

  A patch that applied but cannot be removed fails the run (`gate: the remote run failed: cannot remove <patch>: <error>`) and keeps nothing: the changes are already in the worktree.

  Once git has exited, its readers get 2 s to reach their pipes' ends (the routing decision's calls, `--test-git-drain-ms` of them), else the call fails with `cannot read git's output: it did not end within <n> s of git's exit`, `<n>` being the call's own drain in whole seconds. A failure line carries the last 10 of git's lines joined by ` | `, escaped and cut to the last 4 KiB at a whole escape.

  Of the test seams, `--test-remote-snapshot` and `--test-remote-transport` give the client a stand-in. The first makes the snapshot of each worktree of the call, the main one then a pair's siblings in declared order, says `gate: remote snapshot <sha> bridge <sha or none>` on stderr and in the log, one line a worktree, and ends with status 0.

  A snapshot that fails says `gate: the remote run failed: git <args> failed in <root> (status <n>): <git's lines joined by ' | '>` and ends with status 1, with no fallback.

  Ctrl+C ends it with `STATUS_CONTROL_C_EXIT` and no line. A log that cannot be written gives status 1, with no fallback.
- **The remote client's run** (GATE-43, parts B1, B2, C1 and C2) is built in `remote/{client,parity,payload,retry,syncback,transport,watchdog}.rs`, in every build. Every routed run goes through the Rust client over ssh (`RustClient` in `remote/client.rs`, chosen in `remote/mod.rs`).

  In a build with the `test-seams` feature, `gate run --test-remote-transport local:<folder>` stands in for the builder with a local folder, and `--test-remote-snapshot` and `--test-remote-client` take precedence over the ssh client.

  The Rust client's builder root is `D:\gate-remote`, and its ssh is `ssh.exe` found along the gate's own `PATH`: `ssh.cmd` and `ssh.bat` are never looked for, in the client and in `gate push` alike ([`gate push`](#gate-push)). ssh is looked up only when a run routes, in the client's `ready()` as the routed attempt starts, before any key lock, ticket or slot; a run that never routes never looks for it.

  The client's ssh calls start in the Windows system folder with `NoDefaultCurrentDirectoryInExePath=1`, and every git call the gate makes carries that variable (`NO_CURRENT_FOLDER` in `remote/git.rs`), so neither ssh nor a program it starts by name is found in the gate's working folder, which may be the worktree.

  The client runs the file the ssh found leads to through links, and refuses it when that file lies inside the run's worktree, with the same check the dotnet read has.

  A client that cannot start falls back `client` and says `gate: remote <host> client could not start: <why>; running here`; a run with no usable ssh says `cannot run ssh: <message>` before any key lock, ticket or remote slot. A `cannot run ssh: <path> lies inside the worktree` is said after the remote slot is taken, since the worktree is known only to the run call.

  A repo name that is not `[A-Za-z0-9_-]+` is never sent to the builder: the push and the sync-back fail the run with `cannot push to the repo '<name>': a repo name is letters, digits, '_' and '-'`, as a client failure (`gate: the remote run failed: ...`).

  The phases run in this order. Any `gate: the remote run failed` line ends the run with status 1 and no fallback; Ctrl+C at any point after the snapshot ends the run as an interrupt.
  - **Lock.** The run takes the routed lock, `Local\gate-[<root hash>-]routed-<16 hex>` ([Kernel object names](#kernel-object-names)), before the remote slot, with a zero wait, and holds it for the whole remote attempt.

    A lock held elsewhere says `gate: remote <host> busy; running here` and falls back `busy` before any queueing, so two routed runs of one worktree give a deterministic `busy`, not a race. A lock that cannot be opened or taken says `gate: cannot open the mutex <name>: <error>` or `gate: cannot take the mutex <name>: <error>`, then the busy line.
  - **Hold.** The run takes the worktree locks of its main root and each carried sibling ([Sync-back](#routing), The worktree lock) and, for a pair, holds its folders ([Sibling worktrees](#routing)); one it cannot hold keeps the run here ([Output lines](#output-lines)).
  - **Snapshot.** Part A's snapshot and bridge commit, for each repository of a pair.
  - **Run line.** The run call's cmd line is the fixed `"%LOCALAPPDATA%\gate\bin\gate.exe" box run --stdin`, the same whatever the run holds; every other `gate box` call's line differs only in its verb. The request, base64 of compact UTF-8 JSON, is the first line of the call's stdin, followed for the run call by the outputs manifest line, so no request size is capped by a command line. The box reads the request line with a cap of 16 MiB, newline included ([The box side](#the-box-side)).
  - **Preflight.** `gate box preflight` over ssh. Status 255, ssh's own status for a lost connection, says `gate: remote <host> unreachable: <last stderr line>` and falls back `unreachable`; the line is the last `\r` update of the last stderr line, its first 4 KiB, escaped (Escaping, below). Status 9009 or 2 (no `gate box` verb there) says `gate: not routed: <host> has no gate box that takes this run (preflight status <n>)` and falls back `version`.

    Any other non-zero status says `gate: preparing <builder root> on <host> failed (status <n>)` and ends the run with that status. A reported version text other than this PC's whole one, build id included, says `gate: not routed: <host> runs gate <theirs|unknown>, this PC <ours>; run gate push` and falls back `version`; the builder's version is its first 4 KiB, escaped.

    The client also reads the preflight's `gate: remote warm <none|64 lower-case hex>` line, only the line right after the last `gate: remote version` line: anything else there, no such line, or a warm line elsewhere reads as no warmth.

    A call that needs the work folder warm with a given hash and whose preflight does not name exactly that hash falls back `cold` here, after the version check and before parity and the push, so nothing is sent. A warm-up and a plain run do not compare the line. No run makes a call that needs warmth yet.
  - **Parity.** After the preflight and before the push, `remote/parity.rs` checks that the builder's `gate: remote sdks` line (from the preflight) has what the snapshot needs. A builder with no sdks line, or a tree that is not .NET (no `global.json`, `.sln`, `.slnx` or `.csproj` anywhere in the snapshot), skips the SDK part of the check; the tools of the repo's `.gate/toolchain.toml` are judged either way (Toolchain manifest, below).

    A lacking builder says `gate: remote toolchain mismatch: <repo> needs <need>; <host> has <sdks>` once for each need, in the order the needs are listed, then a single `gate: remote <host> parity; running here`, and falls back `parity` (status 75).

    A need is an SDK or a tool the manifest declares; each kind words the builder's side of its own line (for an SDK, its SDKs, or `none`; for a tool, the version the builder found, or `none`). The SDK needs come first, then the tools' in the manifest's order.

    `<need>` is `.NET SDK <version> (rollForward <policy>)` for a pin, the policy defaulting to `patch`, or `a .NET <major> SDK` without one, where the major is that of this PC's newest SDK (none listed: no check). A pin is met by dotnet's own rule for the nine rollForward policies and `allowPrerelease`, versions read as `x.y.znn` (feature band and patch).

    A .NET tree with no readable pin is refused unless the builder has an SDK of this PC's newest major, after `gate: global.json has no readable sdk pin; checking the SDK major only`. A `global.json` has no readable pin when it is not UTF-8, is past the git runner's 16 MiB cap, or has member names not in dotnet's exact case (`sdk`, `version`, `rollForward`, `allowPrerelease`).

    It also has none when its version is not three-part (`8.0`, `8.0.100.1`, `v8.0.100` and a version with surrounding spaces are not), when its `rollForward` is not a string naming one of the nine policies (matched ignoring case), or when its `allowPrerelease` is not a JSON boolean.

    `//` and `/* */` comments outside strings are stripped before parsing, a block comment counting as one space. Only the first JSON value is read, as dotnet reads it: a stray `}`, other text or a second value after it is ignored. A root `global.json` is found whatever the letter case of its name.

    This PC's SDKs come from `dotnet --list-sdks`; if that cannot be read the run says `gate: cannot read this PC's SDK list: <why>; checking no SDK major` and checks no major. A snapshot whose file listing passes 16 MiB, or holds a path that is not UTF-8, says `gate: cannot list the snapshot's files: <why>; skipping the toolchain check` (`<why>` the over-cap line, or `a path is not valid UTF-8`), and the run goes remote unchecked.

    **Bounded SDK read**: `dotnet --list-sdks` runs like the client's git calls, through the same start-and-wait function in `remote/git.rs`: in an anonymous kill-on-close job of its own, on the gate's console, with the run's interrupt and a 30 s bound (the test seam `--test-dotnet-limit-ms` lowers it), its standard output read up to 16 MiB and the last 64 KiB of its standard error kept.

    A `dotnet` that hangs (a broken install, a first-run prompt) is killed with its whole job at the bound. One that exits non-zero gives the reason `dotnet --list-sdks exited with status <n>`, followed, when it wrote any, by `: ` and the last lines of its standard error as a failed git call's line carries them (at most 10, joined by ` | `, escaped, at most 4096 bytes), so a broken SDK shows its cause.

    This PC's read refuses a `dotnet` whose file, followed through links, lies inside the worktree, after the `PATH` folders inside it are dropped: the reason is `cannot run dotnet: <path> lies inside the worktree`.

    A `PATH` holding an unmatched `"` reads every folder after the quote as one, where no `dotnet` would be found, so it is refused rather than read as an empty list: `PATH holds an unmatched quote, so the folders after it read as one`. Both reach the run as `gate: cannot read this PC's SDK list: <why>; checking no SDK major`.

    The builder's `gate box preflight` reads its own list through the same function (`sdk_listing` in `remote/git.rs`), with no interrupt ([The box side](#the-box-side)); only the `PATH` it searches differs, as below.

    It runs in the system folder (`GetSystemDirectoryW`), not the repo root nor the temp folder: `TMP`/`TEMP` can point back into the repo and any process of the user can write there, while cmd.exe searches the working folder for each bare command in a `dotnet.cmd`. Its environment also sets `NoDefaultCurrentDirectoryInExePath=1`. A `dotnet.cmd` found on `PATH` runs through the system folder's cmd.exe.

    `dotnet` is found on `PATH` only: `find_program` never searches the working folder for a bare name, and a `PATH` folder inside the worktree is skipped, compared by canonical path so a junction does not get around it (an `npm run` or an activated venv puts repo folders first on `PATH`). The dotnet the run starts sees that `PATH` too. The search on `PATH` is outside the 30 s bound, which starts with dotnet's launch.

    The builder's preflight skips no `PATH` folder: the box's work folder holds the snapshot this PC sent, and this PC already refuses its own repo's folders.

    Past the bound the list counts as unreadable and the run says `gate: cannot read this PC's SDK list: dotnet --list-sdks did not answer in 30 s; checking no SDK major` (a bound that is not a whole number of seconds is said in milliseconds, as the git limit is: `in 200 ms`).

    An output past 16 MiB counts as unreadable too (`dotnet --list-sdks wrote more than 16 MiB`), as does a dotnet that cannot be looked for or started, or a system folder that cannot be read (`cannot run dotnet: <why>`), or one that exits non-zero (`dotnet --list-sdks exited with status <n>`).

    A Ctrl+C during the call kills dotnet's job and ends the run as an interrupt, as during git: nothing is pushed, and a read that failed while the interrupt is raised says no `cannot read` line. `--test-remote-interrupt-at sdks` raises the interrupt just before the read.

    **Workload parity** (GATE-18), planned, not built: the parity check compares SDK versions only, so a repo that needs a .NET workload the builder lacks (`wasm-tools`) fails on the builder with `NETSDK1147` as the gate's own exit 1 instead of a refusal.

    The builder's preflight reports its installed workloads beside the SDK line (`gate: remote workloads {ids}`), and the PC refuses with status 75 `parity` and falls back to a local run when the snapshot's projects need a workload the builder lacks.

    The source of what a repo needs is either this PC's own workload list or a scan of the snapshot's project files; the choice is made after measuring what `dotnet workload list` costs per preflight.

    **Toolchain manifest** (GATE-206, GATE-209): a repo's `.gate/toolchain.toml` declares what a routed run's builder must have beyond the .NET SDK, and `remote/toolchain.rs` reads it as data from text the caller hands over. [`gate check`](#gate-check) reads it from the worktree on disk and says what this PC lacks; a routed run with parity reads it from the snapshot's git tree and refuses the route when the builder lacks a tool it declares (below).

    `toolchain::meets` compares a found version with a minimum. A repo's sibling worktrees are not declared here but in `.gate/siblings.toml` ([Sibling worktrees](#routing)), whose problems keep a run local.

    The file has a `[tools]` table of `name = "version"` and a top-level `workloads` array of .NET workload ids. A version is a minimum, "this or later", written `major[.minor[.patch]]` with the missing parts 0 (`"24"` is 24.0.0), and `""` means the tool is present at any version. The known names are `rust`, `cargo-nextest`, `node`, `pnpm`, `pwsh`, `godot` and `ffmpeg`; an unknown name is kept and flagged unknown, for the consumer to skip with a line. A leading byte order mark is skipped.

    A file that is not UTF-8 or does not parse is unreadable whole, and the line for it is `gate: cannot read .gate/toolchain.toml: <why>; skipping the manifest's checks`, `<why>` being `line <l>, column <c>: <toml message>` or `not valid UTF-8 (...)`. A routed run words a manifest problem its own way, `gate: .gate/toolchain.toml: <problem>; skipping it`, for a whole file that cannot be read as for a bad entry, `<problem>` escaped.

    A bad entry is skipped alone and named in the reader's problems: `tools.<name> is not a version: "<text>"`, `tools.<name> is not a string (TOML <type>)`, `tools is not a table (TOML <type>)`, `workloads is not an array (TOML <type>)`, `workloads[<i>] is not a string (TOML <type>)`, `workloads[<i>] is empty` and `<key> is not a key of the manifest (tools, workloads)`.

    **On a routed run**, the client lists the snapshot's tree once before the preflight, with `git ls-tree -r -t -z <commit>` (a sibling's listing keeps `--name-only`): the file names parity needs and the `.gate` folder and `toolchain.toml` candidates for the manifest are read from that one listing. The `.gate` folder and `toolchain.toml` are found ignoring letter case, and the exact-case regular file wins.

    The file is used only when it is a regular blob, read by its object id; a submodule, a symbolic link, a folder, or several spellings of it with none in exact case gets a skip line saying why. A repo with no manifest says nothing.

    The preflight's `Needs` carry the tools the manifest declares whose names the box knows how to probe (the known names above), each with its minimum, or empty for any version. A name outside them is skipped with `gate: .gate/toolchain.toml names unknown tool <name>; skipping it` and not sent.

    The builder answers each tool with a `gate: remote tool <name> <version|present|missing|skipped: <why>>` line. A tool it found missing, or older than the minimum, is a need, and the route is refused as under Parity: one `gate: remote toolchain mismatch: <repo> needs <name> <min> or later; <host> has <version|none>` line per need (`<name>` alone when the manifest sets no minimum), then the parity fallback line, and the run goes local.

    A tool the builder did not probe, gave no answer for, or found at a version that cannot be read against a minimum gets `gate: <host> did not probe <name>: <why>; not checking it` and is not a need. The lines come in this order: the SDK step's own, then the manifest's problem lines, then the per-tool skip lines in the manifest's order, then the mismatch lines.

    A text from the manifest or the builder is escaped, and the builder's cut to 4096 bytes, so neither can forge a line. `toolchain::standing` judges a tool against what its probe found (met, too old, missing, not readable, skipped) in one place, which `gate check`'s report shares; `gate check`'s output is unchanged. `--test-remote-interrupt-at manifest` raises the interrupt just before the manifest is read.

    Two pins a repo already declares are read beside it, also as minimums: each tool's version in `.config/dotnet-tools.json` (the release part, so `1.0.0-beta.1` is 1.0.0; read as strict JSON, as dotnet reads it, so a comment or a trailing comma makes the file unreadable, and a tool id given twice counts once, with its last version) and the `Godot.NET.Sdk/<version>` a project file names, on its root `<Project Sdk=...>` or an `<Sdk Name="Godot.NET.Sdk" Version=...>` element directly inside it (an `<Import Sdk=...>`, a deeper `<Sdk>` element and a `min=` version are not read).

    `global.json` stays the SDK pin, read by `parity.rs` as above.
  - **Push.** `git push --no-verify --no-recurse-submodules --progress [--receive-pack=…] -- <host>:<builder root>/<repo>.git +<snapshot>:refs/gate/<key> [+<bridge>:refs/gate-bridge/<key>]`, with `GIT_SSH_COMMAND` carrying the ssh options. Success says `gate: pushed snapshot <8 hex> in <s> s (<size>|up to date)` and moves the local `refs/gate-remote/<key>`.

    The builder keeps one bare repo per repo name, `<builder root>/<repo>.git`, shared by every key of that repo; the key is only the ref's name.

    The size is read from git's last `Writing objects: 100%` line, ignoring `remote:` lines, and only a number with a unit (or `bytes`) counts as one, else the line says `up to date`. Failure says `gate: pushing the snapshot to <url> failed (status <n>)` and falls back `unreachable`.

    A pair pushes each repo in order, the main one first, under one transfer clock started before the first push, and says `gate: remote pushed snapshot <repo> <8 hex> in <s> s (<size>|up to date)` for each; a failure says `gate: pushing the snapshot of <repo> to <url> failed (status <n>)`. The run's push time is the pushes' sum.

    The push is given `GIT_ALLOW_PROTOCOL`, `ssh` (`file` for the test transport's local bare repositories), which git reads in place of every `protocol.*.allow`, so no `pushInsteadOf` or protocol setting a repository's config names can turn the push into a protocol helper's.
  - **Inputs.** In `remote/inputs.rs`, for a run whose repo declares `inputs`; a run without them builds no manifest, makes no inputs call and says nothing. The manifest is built before the preflight: the files the globs match, kept when `git check-ignore` reports them ignored, each hashed once (the hash cache below), so a refusal at the preflight or the parity check sends nothing.

    The calls are made after the push and before the run call, and the run call's request carries `InputsDigest`, the SHA-256 of the manifest line.

    The diff call (`gate box inputs`, `Step` `diff`) is fed the request and the manifest line and answers the paths whose copy is missing or differs. The put call (`Step` `put`) is made only when the diff named a file, fed the request, the manifest line and then an archive of exactly those files, in the outputs archive's format. A diff that names none says `gate: inputs: <n> file(s) unchanged` in the log.

    A glob that matches nothing, a match git does not report ignored (it is in the snapshot) and a file inside a folder holding a `.git` of its own (another repository, which git cannot be asked about) are each said in the log and skipped.

    Never sent, whatever the globs: any `.git` path, `.lfsconfig`, `.env`, `*.env`, `.env.<name>` and `.envrc` at any depth, matched without regard to case, the gate's own archives and stages under `.tmp` (named for the work folder's key) and the run's own log and `.err` file, found by canonical path. `.tmp` is otherwise reachable.

    Caps: 50,000 members and a 64 MiB manifest, counted on what the globs match, and 4 GiB per put, counted on the files the diff named (`--test-remote-inputs-budget-bytes` replaces the 4 GiB). A set past a cap is not sent. A file that changes between its hashing and its packing is not sent either, and its entry is dropped from the hash cache.

    The hash cache is `remote\<key>.inputs-hashes` in the state root, beside the outputs one and apart from it, since the outputs saves drop every file the outputs walk no longer finds. It holds the manifest line of the last built manifest's files last written well before that build, is saved when a build changes it, and is the same shape and rules as the outputs cache ([Manifest time](#routing)). There is no PC record of what the builder holds: the diff call is the truth.

    The worktree's `.tmp` is held through the step. The diff's answer (`<key>-<nonce>.inputs-diff`) and the archive (`<key>-<nonce>.inputs.tar.zst`) are made new there, opened by handle with delete-on-close, so a gate killed outright leaves none, and each put attempt reads the archive at a position of its own.

    The diff is watched as a fetch is, by the stall window on its output and its own transfer ceiling (1800 s) counted from its own start. The put is watched while the archive is being fed by the later of the last byte fed and the last byte of output, and once it is fully fed by the transfer ceiling alone, since the builder installs without a word. Either call is made once more when it failed before the banner.

    Any failure of either call refuses the route with reason `inputs`: console `gate: remote <host> inputs; running here`, log `gate: remote inputs not sent: <reason>` and the run goes on here.

    The reasons include `the box named <path> twice`, `the put stalled: no bytes sent or received for <n> s`, `the put passed its transfer ceiling of <n> s`, `the diff stalled: no output for <n> s`, `the diff passed its transfer ceiling of <n> s`, `git check-ignore failed: status <n>: <git's stderr>`, the builder's last error line, a count the builder installed other than the one sent, and a manifest or put past a cap.

    A cut call says no `STALLED` or `TRANSFER TIMED OUT` line, since the log scan would fail the run here after the fallback. A run the box refuses with the `inputs` tag (its work folder no longer holds the inputs sent) falls back the same way, saying `the builder's work folder does not hold the inputs sent`.

    A Ctrl+C during the step ends the run as an interrupt with no fallback. A call whose input could not be fed logs `gate: the inputs call's input was not sent: <error>`.

    A sibling pair whose repos declare inputs is refused with reason `inputs` and `a sibling pair does not send inputs yet`, before any inputs call. `--test-remote-watch-call inputs-diff` names the diff and `inputs` the put; `--test-remote-interrupt-at inputs` raises the interrupt after the put's or diff's first output byte.
  - **Run call.** The line goes over `ssh -o RequestTTY=no -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=4 -o ForwardAgent=no -o ClearAllForwardings=yes -o Compression=yes -- <host> <line>`, each argument separate.

    `-o RequestTTY=no` leads the options of every `gate box` call and `gate push`'s, so stdin never passes through a pseudo-terminal. The last option, `-o Compression=yes`, is the run call's alone: the preflight, stop, boot and outputs calls, git's ssh command and `gate push` ask for none. The run call's compression covers both directions of its connection: the command's output and protocol lines one way, the request and the outputs manifest the other.

    The box's output passes through to the log and the console, except the box's heartbeats and its machine lines, which the client hides from both and keeps out of the lines it holds: the start marker `gate-remote-started: <nonce>` and the refusal tag `gate-remote-refused: <nonce> busy|tools|layout`, each naming this run's `Nonce` and read on stderr only, as a whole line, and the kill tag and the adjusted line below.

    The kill tag `gate-remote-killed: <nonce> timed-out|backstop` is said by the box just after its own kill line, and the client counts a box kill only on this run's tag with a status 124 (a status 124 alone is also what a stopped job or the command itself may end with). The client takes the run's verdict from the tag's kind, never from the log's last kill line.

    The adjusted line `gate-remote-adjusted: <nonce> <s.sss>` carries the load-adjusted seconds the box counted, said once the command's output has drained unless the sampler failed, read like the tag on stderr only and the last one winning; another run's, a malformed one or one whose seconds are not a finite number is a line like any other.

    A status 75 is a refusal only when this run's refusal tag came on stderr before any start marker (`busy`, `tools` or `layout`, the fallback reason); a tag after the marker is ignored, so a command that prints the human busy, tools or layout line, or a tag of its own, and exits 75 ends with status 75 and no fallback.

    Safety rests on order, not on the nonce being secret (the request is on no command line, but the nonce still appears in the run id, the pipe names and the archive name): the box says the marker before it creates the command's process, and the client ignores every tag once it has heard the marker.
  - **Warm-up call.** A call whose `warmth` is a warm-up (`Warmth::WarmUp` in `remote/mod.rs`) runs the phases above with the run call's payload built by `payload::warm_word`: the run's keys, `Timeout` always among them, with `WarmCommands` and `WarmHash` in place of `Command` and no `Outputs` or `OutputsFull` key. Its limits are a run's, from the call's `timeout`. `gate warm` makes such a call; no route does yet.

    The run call's end is classed by `warm_end` in `remote/client.rs` and handed back as `RemoteOutcome.warm`. It is warm only on status 0, after the start marker, with the call's last stderr line that is neither a heartbeat nor a protocol line exactly `gate: remote warm {H}` for the `WarmHash` sent. A Ctrl+C is deleted: it leaves no record.

    Any other end is a failure: a refusal when it came before the start marker, as no command ran (the box's refusal tag with status 75, or a status, kill or lost connection before the marker, or an end of the client's own), and the commands' failure when it came after the marker (any non-zero status, the box's kill, a lost connection), and a status 0 the box did not confirm. A status 0 the box did not confirm is reported as status 1.

    The failure's reason is built from the end alone, never from a line the builder or the commands said: `command stopped with status {n}`, `the builder ended the warm-up before its commands with status {n}`, `the box did not confirm the warm hash`, `the builder refused: {reason}`, `the warm-up was killed: it ran out of its budget or went silent`, `the connection to the builder was lost`, `the client could not send the warm-up` or `the client failed`.

    A warm-up touches no outputs state (it reads, saves and marks none, whatever globs the call holds) and syncs nothing back; a lost connection is handled as a run's is. The key mutex a caller holds around it can be taken with a wait (`hold_key_waiting` in `remote/client.rs`): it waits in slices of 100 ms, looks at the console interrupt between them, and a Ctrl+C ends the wait.
  - **Outputs.** In `remote/outputs.rs`, for a run with output globs; a run without them sends `Outputs` `[]` and `OutputsFull` false, no manifest, and says nothing here, and writes no marker unless the route withheld its outputs (a dotnet `--no-build` command the builder built): that run writes the pending marker after the push, as below, and no end of it clears the marker. The client builds the manifest ([The outputs module](#the-box-side)) before the run call, hashing every matching file.

    **Manifest time** (GATE-23): SHA-256 streams, with no whole-file read into memory, and only the PC hashes every file (the box hashes changed files only; `obj` is not under the default globs). No timing line is added to routed runs.

    The ignored test `remote::outputs::tests::measure_the_manifest_time_of_a_real_tree` times the walk and `manifest_line`, cold (an empty hash cache) and warm (the cache the cold build saved, loaded back), over the tree `GATE_MEASURE_ROOT` names (globs from `GATE_MEASURE_GLOBS`, `;`-separated, else the defaults).

    Measured on a critical slot over the three largest built trees on the owner's PC: the walk takes at most 0.25 s, and the hash is nearly all the time, so the time follows the bytes under the globs. 2,230 files of 208 MiB take 0.4 to 1.6 s and 2,255 files of 632 MiB take 0.8 to 1.9 s, but 22,199 files of 29.5 GiB take 45 to 48 s, whether or not the files were read just before (the PowerShell manifest takes 36 to 41 s on the same tree).

    That is past a few seconds, so the client keeps a hash cache (`outputs/cache.rs`) in the state root at `remote\<key>.outputs-hashes`: a file whose size and modified time, as the open file's handle reads them (`GetFileInformationByHandle`), match its cached entry takes its hash from the cache instead of being read again. The file is one outputs manifest line, as the box is sent one, read back with the manifest's own parser.

    The cache is a hint checked against each file's stamp, not a record of a completed outputs step. It is saved right after each manifest the client builds, whatever the run's end; a build that fails or is interrupted saves nothing, and one that would save what the cache already holds skips the write. A file the install replaces gets a new stamp, the box's last write.

    An outputs install that completes (no member skipped but ones it cannot rewrite) therefore also saves the hashes of the files it put in place, so the next run does not rehash them. Each entry takes the stamp the installed file's handle reads after the move and the future-time clamp, with the SHA-256 the unpack verified against the box's manifest, under the 2 s rule below measured against the PC's clock when the moves began: a file clamped to the PC's clock is not kept.

    A member the install rewrites after the hash check (the files that record a box tree's path) is not kept, its bytes no longer being the hash's. The install loads the cache file again, adds its entries and saves once; a failed, refused, partial or interrupted install saves nothing.

    An entry is saved only when its modified time is at least 2 s before the moment the walk began, so a file written again within the same tick at the same size is never taken from the cache; a modified time in the future is never saved. A saved entry whose stamp matches is trusted. A file the walk no longer finds drops out at the next save.

    A hit still checks for Ctrl+C, opens the file and checks its final path against the worktree's, as a read does, and trusts the cached hash only when the stamp the open file's handle reads equals the cached entry's.

    That stamp is also the one the manifest line carries, read from the handle before the file is hashed, not from the folder listing, which can lag a file still being written; so a file whose listing lagged is hashed once and sent with its true stamp, and the install does not refuse it as changed during the run.

    The running length the size cap counts is corrected after each hash by the difference between the listing's and the handle's stamp digits, so the cap still stops hashing before the next file. On a mismatch the file is hashed. Only the hash is skipped. The box's own walk and listings keep listing stamps.

    A missing cache counts as empty. One that is unreadable, corrupt or over 64 MiB gives no hash, and the next save replaces it even when it keeps no entry. A failed save is not said: either costs one full hash and changes nothing else, and no line is printed about the cache.

    The save writes `<key>.outputs-hashes.<pid>.tmp` beside the cache and renames it over it. Two routed runs of one worktree are not locked against each other: the last writer wins.

    While the pending marker stands (`OutputsFull`), the manifest is built without reading the cache, and the hashes that build read are saved: the full send is the recovery path, so it reuses no hash that may be wrong. A moved worktree, whose key changes, starts with an empty cache, and deleting `remote\<key>.outputs-hashes` resets it.

    Accepted residuals: a copy that keeps a file's size and modified time over other bytes takes the old hash from the cache; a network share whose clock runs more than 2 s behind the PC can write a file whose modified time already reads as settled; and a gate killed mid-save leaves its `<key>.outputs-hashes.<pid>.tmp` beside the cache.

    Once the push succeeds, and before the run call, it writes the pending marker `<state root>\remote\<key>.outputs-pending` (an empty file; a marker whose existence cannot be read counts as present), so a run refused at the preflight or by the parity check leaves none.

    The payload carries the globs as `Outputs` and `OutputsFull` (true when the marker was there), and the manifest goes as the line after the request on the run call's stdin, which is then closed; a pre-banner retry feeds both again. A marker that cannot be written says `gate: the remote run failed: cannot write the outputs pending marker <path>: <error>`.

    A call whose input could not be sent logs `gate: the <verb> call's input was not sent: <error>`, only when the call reached the box's banner and ended neither refused nor with ssh's 255, since any other end says why on its own lines. An input still being sent 2 s after the call ended logs `gate: the <verb> call's input was still being sent 2 s after the call ended`, and a feeder thread that panicked `gate: the <verb> call's input feeder panicked`.

    The client accepts only the last stderr line starting `gate-remote-outputs: `, kept as it is heard so a stdout flood cannot push it out, and echoes it like any box line. It must be `gate-remote-outputs: 0` or `gate-remote-outputs: <n> <key>-<this run's nonce>.tar.zst` with n above 0.

    After an accepted `0` the marker is removed (`gate: cannot remove the outputs pending marker <path>: <error>` if it cannot, and the status stands); so does an install that ends as the Outputs install below says, and every other end leaves it, so the next run sends `OutputsFull`. An accepted line naming an archive leaves the marker; the fetch of the archive and its install (the Outputs fetch and Outputs install phases, below the sync-back) follow.

    A passing run says at most one of these lines and then exits 1: `gate: remote outputs not brought back: the worktree changed during the run` (wins when it applies), `gate: remote outputs not brought back: the manifest could not be built: <msg>` (`<path> resolves outside the worktree`, `it is larger than the box takes (64 MiB)`, checked as the manifest is built before the rest is hashed, or the read error), or a third.

    The third is `gate: remote outputs not brought back: the box printed no 'gate-remote-outputs: <n> <archive>' line`.

  - **Sync-back.** In `remote/syncback.rs`, after a run call that ended with the command's own status (a refusal, an interrupt or a failed call never syncs back).

    The sync-back runs no `git rev-parse --git-path objects`: the real object store's path comes from the snapshot, whose `rev-parse --git-path` call also asks for `objects`. It reads it even when there turns out to be no back commit, and sweeps the quarantines of earlier gates (below).

    Then `git ls-remote -- <url> refs/gate-back/<key>` then `git -c fetch.fsckObjects=true -c maintenance.auto=false -c gc.auto=0 fetch --progress --no-recurse-submodules --no-tags -- <url> <ref>` run with the push's `GIT_SSH_COMMAND` and `--upload-pack=pwsh -NoProfile -NonInteractive -Command git-upload-pack`; a bare repository on this machine names no pack command.

    The fetch writes into a quarantine, a new folder `<git common dir>\objects\incoming-gate-<pid>-<start ms>-<nonce>` (this gate's pid, its start in Unix epoch milliseconds and a nonce), with `GIT_OBJECT_DIRECTORY` the quarantine and `GIT_ALTERNATE_OBJECT_DIRECTORIES` the real store (C-quoted when its path holds `;` or starts with `"`), so nothing reaches the real store before the back commit is kept or applied.

    The fetch and the calls that read its objects (`rev-list --parents`, the `diff-tree` calls, `cat-file --batch-check`) run with that environment; `rev-parse FETCH_HEAD`, the snapshot calls, `update-ref` and `git apply` run without it. A folder that cannot be made says `gate: the remote run failed: cannot make <folder>: <error>`, and a start of this gate that cannot be read (`--test-own-start-fails` makes it fail) `gate: the remote run failed: cannot read this gate's start: <error>`.

    The fetch runs with the console's interrupt, so it starts no automatic `maintenance run --auto` or `gc --auto` whose lock a Ctrl+C could leave in the user's repository.

    The quarantine is deleted at the sync-back's end, whatever the ending (a Ctrl+C, a cut, an ignored commit, a failed git call, or a kept or applied commit whose objects were moved out), except after a failed move (below).

    Their output stays out of the run's log. No back ref on the builder means the command changed nothing, and the run ends with the command's status and no line. A Ctrl+C during either call ends the run as an interrupt.
    - A back commit whose only parent is not this run's snapshot, or a fetched commit that is not the one `ls-remote` advertised for exactly that ref, says `gate: ignored stale remote changes <back 8 hex>: not made from snapshot <snapshot 8 hex>`, applies nothing and leaves the status.
    - A back commit that touches a path with a component starting with `.git` (letter case aside: `.gitignore`, `.gitattributes`, `.gitmodules`, `.git`, `.github`) or named `.lfsconfig` (at any depth), or makes a symbolic link (mode 120000) or a submodule (160000), is never applied, since the builder is trusted less than this machine: such a change could widen what the next snapshot uploads or plant what runs here.

      The names are matched as Windows opens them: `\` counts as a separator, `name:stream` as `name`, trailing dots and spaces are dropped, and the 8.3-shaped aliases (`GIT~1`, `GITATT~1`, `GITHUB~1`, `LFSCON~1`) count as the names they shorten.

      It says `gate: remote changes not applied: they touch <path>, which a sync-back never writes; see git diff <snapshot 8 hex> <back 8 hex>`, keeps the back commit at the local `refs/gate-remote/<key>-back`, and a passing run becomes status 1. This check comes before the worktree comparison.
    - A back commit that writes or deletes a path the worktree's attributes put through a checkout filter or `ident` is never applied, since `git apply` runs the filter's clean side on a file it changes or deletes (a rename's or copy's source included) and the filter is the user's own program.

      The gate reads the attributes of every path the commit lists with `git check-attr -z --stdin filter ident working-tree-encoding text eol crlf` run in the worktree (so its `.git/info/attributes` counts), fed the paths in slices whose answers stay under the 16 MiB output cap, and the configuration with `git config -z --list` (neither is run when the commit lists no path).

      It says `gate: remote changes not applied: <path> is written through a checkout filter (<attr>), which a sync-back does not take; see git diff <snapshot 8 hex> <back 8 hex>`, keeps the back commit at the local `refs/gate-remote/<key>-back`, and a passing run becomes status 1.

      `<attr>` is `filter=<driver>`, `filter (no driver)`, `ident`, or `filter=unset` or `filter=unspecified` when the configuration defines a driver of that name. The first listed path that matches is named. This check comes after the `.git*` check and before the size check; an answer that does not give each attribute of each path fails the run like a malformed size answer below.
    - A back commit whose files would write more than 4 GiB into the worktree is never applied, so the builder cannot fill the PC's disk. The size is the sum, per path, of the stored blob sizes of the regular and executable files the commit adds or changes (a deletion counts zero, and a blob two paths share counts twice), each times its weight, read with one `git cat-file --batch-check=%(objectsize)` fed the blob ids on stdin and skipped when the commit adds or changes no file.

      The weight is what the checkout writes: 2 for a path converted to CRLF, 4 for a `working-tree-encoding` path, 8 for a path that is both, and 1 otherwise (an `eol=lf` path included). The legacy `crlf` attribute is read like `text` when `text` is not given.

      A path counts as converted to CRLF when its `eol` is `crlf`; else not when it is binary (`-text`), `text=input` or `eol=lf`; else, as `text` or `text=auto`, when `core.autocrlf` is `true` or (it is not `input` and) `core.eol` is `crlf`, `native` or unset (Git for Windows builds with a native CRLF); and, with no `text` attribute, when `core.autocrlf` is `true`.

      `core.autocrlf` is false only when it is `false`, `no`, `off`, empty or `0`, so under `core.autocrlf=true` a sync-back of more than about 2 GiB of text is refused. A path whose attributes are missing counts as much as any can. A single blob is read at its true size past 4 GiB.

      It says `gate: remote changes not applied: they would write <size> into the worktree, more than <cap>; see git diff <snapshot 8 hex> <back 8 hex>` (both sizes written as in the fetch cut below), keeps the back commit at the local `refs/gate-remote/<key>-back`, makes no patch file, and is not a fallback event. A passing run becomes status 1 and a failing run keeps its status. This check comes after the `.git*` check and before the worktree comparison.
    - A size answer that is not one size per blob id fails the run with `gate: the remote run failed: git cat-file --batch-check=%(objectsize) failed in <root>: it answered '<line>' for <id>` (or `it answered <n> line(s) for <m> object(s)`), keeps the back commit and ends with `; the changes are kept: see git diff <snapshot 8 hex> <back 8 hex>`.

      When git succeeded but its fed input did not get through, the failure reads `cannot feed git its input: <error>` (or `... it did not end within <n> s of git's exit`).
    - A worktree whose tree (what `git add -A` and `git write-tree` write through a copy of the index, `snapshot::worktree`) is no longer the snapshot's says `gate: remote changes not applied: the worktree changed during the run; see git diff <snapshot 8 hex> <back 8 hex>`. The back commit is kept at the local `refs/gate-remote/<key>-back`, a passing run becomes status 1, and a failing run keeps its status.

      **Narrower refusal** (GATE-29), planned, not built: the refusal turns a passing run into exit 1 whenever any tracked or untracked non-ignored file changed during the run, including files the command never reads (another agent's doc edit in the same worktree), and the outputs step is gated on the same check.

      The journal and gate logs are measured first (how many refusals, and what changed in each; the kept `-back` ref holds the run's changes); if most are files outside what the command could read, one rule is adopted.

      Either refuse only when a path the returned changes touch also changed locally, a true conflict, and apply the rest; or refuse only when a changed path is under the gate's subfolder or a project the command builds. Bazel's `--guard_against_concurrent_changes` defaults to `lite`, which checks only the main repository's source files, because full checking gave false positives.
    - Otherwise the diff from the snapshot to the back commit is written by `git diff-tree -p --binary --full-index -r` (plumbing, so no diff setting or `GIT_EXTERNAL_DIFF` shapes it) to `<git dir>/gate-remote-<pid>-<nonce>.patch`, a file the gate creates new, applied with `git -c core.protectNTFS=true apply --whitespace=nowarn -- <patch>`, and the patch is removed whether or not it applied.

      The run says `gate: synced back <n> file(s): <names joined by ", ">`, names from `git diff-tree -r -z --name-only` with control characters escaped; it reads `0 file(s): ` with nothing after the colon when no files changed.
    - A git failure here ends the run with status 1 and `gate: the remote run failed: ...`, with no fallback.
    - The `ls-remote` is killed after the git limit (60 s); the `fetch` after a stall window with no output. Either fails the run the same way, with the reason. A `fetch` still running 1800 s after its own start (its transfer ceiling) is killed with `gate: TRANSFER TIMED OUT after 1800 s fetching the sync-back` and the run ends 124 after a command that passed, or with the command's own status after one that failed, with no fallback.
    - The `fetch` is also cut when its job has written more than 4 GiB: the Windows job write-transfer I/O counter (`gate_win32::Job::written_bytes`), read on each watch tick and once after the call exits. It counts every process in the job, pipe writes included, so about 3 times the pack over ssh (roughly a 1 GiB pack).

      The cut says on stderr and in the log `gate: remote sync-back stopped: the fetch wrote more than <size>; the run's changes stay on the builder (refs/gate-back/<key>)`. `<size>` is in the largest of B, KiB, MiB and GiB whose value rounded to one decimal is at least 1.0 (`4.0 GiB`, `256.0 KiB`).

      Nothing is applied and the outputs step still runs. A passing run exits 1; a failing command keeps its status. This cap is the sync-back fetch's alone; the outputs fetch has its own, see Outputs fetch.
    - After any fetch end that is not a clean exit 0 (a cut, Ctrl+C or git's non-zero status), the call's job is terminated and the client waits up to 30 s for its processes to exit. The quarantine is then deleted with the rest of the sync-back's endings, so a half-written pack goes with it; `FETCH_HEAD` is left to git.
    - The delete of the quarantine retries a file another process holds (os error 32) for up to 30 s. A folder still left says `gate: remote could not remove <path>: <error>` on stderr, naming the quarantine, and changes no status.
    - The objects move into the real store only when the back commit is kept (every case above) or applied, before either `update-ref` or `git apply`. Each pack's files move first with the `.idx` last, then the loose objects; a `.keep` never moves, and a target that exists is skipped. Each is a no-replace rename (`gate_win32::move_no_replace`, both paths given the verbatim `\\?\` prefix so a path past MAX_PATH moves), a file another process holds open retried for up to 30 s.

      A move that fails part-way returns the files it moved to the quarantine, keeps the quarantine with a `gate-kept` mark, writes no back ref and applies nothing. It says `gate: remote changes not applied: the fetched objects could not be moved into the repository: <detail>; they are kept in <folder>`, and a passing run becomes 1.

      `<detail>` is `<path relative to the quarantine>: <error>` or `cannot list the quarantine: <error>`; when some moved files could not be returned it adds `; <n> moved files could not be returned: <first path>`. A quarantine that is gone says `...into the repository: the quarantine folder is gone` and keeps nothing. After a failed step or an interrupt, this line is said first and the run's ending says the changes could not be kept, see the line above.
    - At the start of each sync-back, the quarantines in the object store are swept, silently. A folder whose gate ended (its pid and start checked) is removed; one whose liveness is unknown, or whose name does not parse, is left.

      A folder with a `gate-kept` mark is removed 7 days after the mark's time (the folder's creation time when the mark's is unreadable or in the future), and at most 3 kept folders stay per object store, the oldest past 3 removed. A folder the sweep cannot remove says the `gate: remote could not remove <path>: <error>` line once per sync-back, and a store that cannot be listed says `gate: remote could not list <objects> for leftover sync-backs: <error>`.

      The log alone gets `gate: cannot read the bytes <program> wrote: <error>` (the first failed counter read of a call), `gate: the processes of <program> did not end within 30 s`, `gate: cannot read the processes of <program>: <error>` and `gate: cannot end <program>: <error>`.
    - A `fetch` that fails before the banner is not retried when its job wrote more than 1 MiB.
    - **The worktree lock.** A sync-back with a back commit takes the mutex of each worktree it will write ([Kernel object names](#kernel-object-names)), the main one's alone for a single repository, before its fetch and holds it until the sync-back ends, the quarantine and any patch removed, so two runs that write one folder, under one spelling of its path or two, never write it at once.

      The client takes the same locks for every routed run, a single repository's and a pair's alike, for its main root and each carried sibling, before its snapshots (`Attempt::hold_run`), and holds them until its last push is saved, so no other run's sync-back writes a folder while it is snapshotted and pushed.

      A folder that is not the one the route was decided for, by its id, is not routed. One another run holds past the lock wait keeps the run here with `gate: not routed: <who> is busy with another routed run` (the routed-run lines, [Output lines](#output-lines)), where the sync-back's wait fails the run as below.

      The locks are taken in the order of the folders' ids, one per folder. One another run holds is waited for, up to the attempt's lock wait (the 60 s git limit, counted from the first lock asked for), saying once `gate: remote waiting for the worktree <name>: another run holds it`; a wait past it fails the run with `another run held the worktree <name> for more than <n> s`, and a Ctrl+C ends the wait as an interrupt. A lock left by a holder that ended is taken.
    - **A pair's sync-back** (a run that carries siblings, built in `remote/syncback.rs` and the box) is all or nothing across its repos. It applies only the back commits of the box's last stderr line `gate: remote back commits: <name> <sha|none>, ...` (a standard output line never counts), which must name exactly the pair's repos, the main one first, in order.

      A line that is missing, malformed or names other repos says `gate: remote changes not applied: the builder did not say which commits it made`; a pair whose sibling snapshots do not match its siblings says `gate: remote changes not applied: the run's sibling snapshots do not match its siblings`. Either applies nothing and a passing run becomes 1; its outputs are then not brought back, `gate: remote outputs not brought back: the pair's remote changes were not applied`.

      The box says `gate: remote back commits: failed` instead of the commits on every pair exit past its refusal checks that did not push every back commit: a failed lock, stop event, back ref clear, layout step, checkout, subfolder check or tools restore, a command killed by the ceiling, a failed commit or push, and a failed wait for the command only once its job has ended and its output drained.

      A busy, layout or tools refusal says no back commits line. `failed` and a line of only `none` apply nothing and leave the run's status.
    - In a pair, each repo with a back commit is fetched into a quarantine of its own in its own object store and checked as above, with the repo's name right after the first colon of each refusal (`gate: remote changes not applied: <repo>: <why>`). A back commit the builder does not advertise as the line's says `gate: remote changes not applied: <repo>: the builder does not have the commit it named (<back 8 hex>)`.

      Any refusal, ignored commit or failed move applies nothing in any repo and keeps every back commit it took at its worktree's `refs/gate-remote/<key>-back`; a repo that could have been applied is named in `gate: remote changes not applied to <repos> either: a sync-back of a pair is all or nothing`, and a passing run becomes 1.
    - Otherwise the PC checks each worktree against its snapshot again (it does so after every fetch and just before it writes), writes every repo's patch and checks it with `git apply --check` (a failure says `git apply --check failed in <repo> (<why>); nothing was applied` and fails the run), looks at the interrupt once (`--test-remote-interrupt-at apply` raises it there), and applies the patches in pair order, saying `gate: synced back <n> file(s) in <repo>: <names>` for each.
    - An apply that fails after the first fails the run with `git apply failed in <repo> (<why>); applied: <repos or none>; not applied: <repos>`. Any step that fails or is interrupted keeps every taken back commit and ends `; the changes are kept: see git diff in each repo at refs/gate-remote/<key>-back` (an interrupt says it as the single repository's does).

  - **Outputs fetch.** In `remote/outputs.rs` and `remote/client.rs`, after a passing run whose box said an accepted outputs line naming an archive and a passing sync-back.

    The client calls `gate box outputs` over ssh with the payload's location word and `Name` `<key>-<nonce>.tar.zst`, built from this PC's own key and nonce, never from the box's line, and copies the call's stdout raw into `<worktree>\.tmp\<key>-<nonce>.tar.zst` (`.tmp` created when missing). The archive and the pending marker are kept for the install. The call's stderr goes to the log only.

    - A fetch the watchdog stops says its `gate: STALLED: ...` or `gate: TRANSFER TIMED OUT after 1800 s fetching the outputs` line (the ceiling counted from the fetch's own start), then `gate: remote outputs not brought back: the fetch was stopped by the watchdog`, and the run ends 124.
    - Any other failed fetch says `gate: remote outputs not brought back: <reason>`: the box's last stderr line (escaped, its first 4 KiB), `the outputs call failed (status <n>)`, `the outputs call sent a line longer than 64 KiB` or `cannot write <path>: <error>`. A passing run becomes 1. The partial archive is removed; a removal that fails says `gate: cannot remove the partial outputs archive <path>: <error>`.
    - A Ctrl+C during the fetch, or noticed by the end of an outputs call however it ended, ends the run as an interrupt with no `stop` call, and removes the partial archive.
    - A fetch that already sent a byte is never retried as a pre-banner drop; one dropped before any byte is retried like the other calls, up to three times.

    **Fetch cap** (GATE-137): the raw relay is bounded by the stall window, which any byte resets, and the transfer ceiling of 1800 s counted from the fetch's own start, and also by a size cap, so a broken or hostile builder cannot make the archive grow without bound. The cap bounds the archive only; what a builder's output may write to the gate's log has a cap of its own, the log cap (see The output cap below), which counts the outputs call's standard error but not the archive.

    The fetch is capped at zstd's worst case for the box's outputs budget, `outputs::archive::sent_limit(outputs_budget)` (`limit + limit/64 + 1 MiB`, saturating, about 50.8 GiB for the 50 GiB budget), the one formula `read_members` also uses, so every archive the box can legitimately send fits.

    A counting writer around the relay's sink (`remote/client.rs`) writes no byte past the cap and raises a flag, and the watch cuts the call, including one that exits between ticks.

    The partial archive is removed and the gate says `gate: remote outputs not brought back: the archive passed <cap>` (`the archive passed 50.8 GiB` at the real cap); nothing is installed and the outputs-pending marker stays.

    Each hit is counted in the worktree's outputs cap record, `remote\<key>.outputs-capped` in the state root. The first hit in a row keeps the run's status, so a pass stays 0. A second hit in a row fails the run (exit 1) and says, after the cut's line, `gate: the builder's outputs passed the cap again, <n> runs in a row; this run fails`; the record is kept, so every further hit in a row fails too.

    Every other end of an outputs step removes the record: an archive fetched whole, removed before its install, so outputs brought back, an install that refused and a Ctrl+C during the install all clear it; a box that packed nothing (`gate-remote-outputs: 0`); or a fetch that failed another way, a watchdog kill included. A run that made no outputs call (refused, fallen back, failed, lost, or a pass the client turned to 1 before the fetch) leaves it, and so does a Ctrl+C during the fetch.

    A record that is missing or cannot be read counts no hit. One that cannot be written says `gate: cannot write the outputs cap record <path>: <error>`, one that cannot be removed says `gate: cannot remove the outputs cap record <path>: <error>`, and the end the count read decides stands.

    The disk peak during the install is the archive plus the stage, about 50.8 GiB plus 50 GiB, and grows by the bytes of the archive's copies (see Outputs install). A run seam, `--test-remote-outputs-budget-bytes`, lowers the budget for both the fetch cap and the install's unpack limit.

    Hardening: the fetch holds `<worktree>\.tmp` open for the fetch and the install, opened once with no delete or rename sharing, so nothing can rename or remove it while the archive and the stage are in it.

    `Archive::create` fails the fetch, a passing run becomes 1 and no archive is left, when `.tmp` is a link or a junction (`cannot write <archive>: it is a link or a junction, not a folder`), so a junction cannot redirect it, or when it is not the worktree's own `.tmp` (`cannot write <archive>: it is not the worktree's .tmp folder, but <path>`).

    The archive is made with delete access, and a make it refuses removes the file through that same handle; a removal that fails keeps the file marked made and returns `the outputs archive <path> is not inside the worktree; and it could not be removed: <error>`, so the failed fetch then says `gate: cannot remove the partial outputs archive <path>: <error>`.

    The raw copy stops writing once the call has ended, and its reader thread is joined before the archive is removed. A copy that has not ended within the 2 s drain logs `gate: the outputs call's raw copy did not end within 2 s`, and one that panicked logs `gate: the outputs call's raw copy panicked`. The drain is waited once: when the call's own end already gave the readers their 2 s, it is not waited a second time.

  - **Outputs install.** In `remote/install.rs`, after a whole fetch. The archive is unpacked through `outputs::archive::unpack` into a stage `<worktree>\.tmp\<key>-<nonce>` with the box's 50 GiB `OUTPUTS_BUDGET` (one shared constant) as the decompressed limit; a stage folder that was there before the install is not removed.

    The unpack also makes each copy the archive lists in the stage, from the first earlier member with that content, else from the first path of the run's manifest whose file still hashes to it (the stamp is not checked; a source whose final path lies outside the PC worktree root, through a junction for one, is passed over), so the stage holds every listed file and the checks and moves below treat a copy as any other member.

    Each copy's bytes count against the 50 GiB limit and the 50,000-file limit and add to the disk peak.

    A copy that no member and no PC file can supply refuses the install (`no PC file still holds the content of <path>, SHA-256 <hex>: tried <p1> (<why>), … and <n> more`, at most 5 paths named, each why being missing, not a plain file, outside the root, `unreadable: <OS message>` or other content), which keeps the pending marker. Ctrl+C is honoured between 64 KiB buffers of a copy, and says `the unpack was stopped while its copies were made`.

    Before any move, in order, each member is checked against the run's output globs, against the manifest (the file must still have its size and last write; a file the manifest lacked that now exists counts as changed), for being writable, and against the worktree's git index (Tracked members, below).

    `MvcTestingAppManifest.json` and `.msCoverageSourceRootsMapping_*` are rewritten from the box's path of each tree of the run to that tree's root on the PC (a run of one repo: the work folder to the worktree; a pair: `{Work}\{leaf}` to the worktree and `{Work}\{name}` to that sibling's worktree), the JSON-escaped form first, then the raw one, case-insensitive, the longest path first.

    A path is matched only at a whole path component: the next character is not an ASCII letter or digit, `.`, `_` or `-` (the characters a folder name the builder lays out is made of), or the text ends, so `{Work}\yaat` never rewrites part of `{Work}\yaat-server` and `{Work}.lock` is left alone. Each box path is computed in one place (`boxside::payload::trees`), shared by the box and the client.

    Each file is decoded by its byte order mark, UTF-8 (with or without one) or UTF-16 in the byte order the mark names, and written back in the same encoding with the same mark and its last write kept; a file with no mark is read as UTF-8 only, and no other encoding is guessed.

    A rewritten file over 16 MiB, or whose body is not text in that encoding (not UTF-8, or UTF-16 of an odd length or with an unpaired surrogate), is skipped, not refused, as below. The client does not skip the back-commit paths again; the box does.

    Each file is then moved with a rename, after a link and junction check on the way, its last write clamped to the PC's clock in the stage before the move and `unchanged` checked once more just before the rename.

    A member git tracks, one whose target is in use or read-only, or one of the rewritten files above that cannot be rewritten, is skipped, not refused: `gate: remote outputs not fully brought back: <path> is tracked by git`, `... <path> is in use or read-only`, `... <path> cannot be rewritten (it is not UTF-8 or UTF-16 with a byte order mark)` or `... <path> cannot be rewritten (<n> bytes, over 16 MiB)`.

    Each is one line per file before the success line (a member both tracked and in use gets the tracked line only); the other files install and a passing status stays 0.

    A file that cannot be rewritten is not installed, so the PC keeps whatever file it had. A file starting `FF FE 00 00` (UTF-32 little-endian) is not text and is skipped the same way. An install whose only skips are such files is complete: it clears the marker, keeps the hashes of the files it moved in the hash cache and counts as brought back (the worktree's nothing-back record is cleared); only the tracked and in-use skips make an install incomplete.

    The tracked and in-use skips keep the pending marker, so the next routed run sends `OutputsFull`; a run whose only skips are files that cannot be rewritten clears it, since a refetch would skip them again.

    The success line is `gate: outputs: <n> file(s), <X.X> MB (<Y.Y> MB compressed) in <Z.Z> s`: `<n>` counts installed files only, the seconds run from the start of the fetch to the end of the install, and each decimal rounds a half away from zero. A fully installed run clears the pending marker.

    - A passing run becomes 1 with the archive and stage removed and nothing installed, and says `gate: remote outputs not brought back: ` then `the outputs archive <name> cannot be opened: <detail>`, `the archive member <path> is not under the output globs`, `<path> changed during the run`, `<path> is not a file` or `cannot rewrite <path>: <error>` (the file could not be read or written).

      A refusal that a refetch of the same archive would repeat, because the archive's own members decide it, clears the pending marker, so later routed runs send `Outputs`, not `OutputsFull`.

      Those are a member not under the output globs (which includes a `.lfsconfig` or a `.git*` path) and every refusal the unpack makes of a member for what it is: a link or folder entry, a `.git` form, a short name, a bidi character, a reserved device name, the member and folder count limits, a future last write, a path that is not relative, a member after the list, a long name, an archive with no list or a list that is not a list of paths, sizes and SHA-256s.

      Every other refusal keeps it, since the next fetch may succeed: a fault reading the stream (an I/O error, a hash mismatch, a stream cut short, a bad zstd frame), the archive cannot be opened, `<path> changed during the run`, `<path> is not a file` (the PC has a folder where the output goes), a failed move, a failed or over-cap `git ls-files`, a `--bring-tracked` glob that cannot be compiled, or an interrupt.

      The same holds for the git runner's line when the `git ls-files -z -s` of the tracked check failed or wrote past its 16 MiB cap, and for a `--bring-tracked` glob that cannot be compiled.
    - A move that fails after others were made says `gate: remote outputs not brought back: <n> of <m> files were installed before <path> failed: <detail>` (a file that changed between the checks and its move gives `<path> changed during the run`); the files already moved stay and the status is 1. `<m>` counts only the files to be moved.
    - A Ctrl+C during the moves says `gate: remote outputs not brought back: <n> of <m> files were installed before the interrupt`, then the interrupt ending, with the marker kept and no `stop` call; one before the first move says nothing extra.
    - The archive and the stage are removed on every end; a failure says `gate: cannot remove the outputs stage <path>: <error>` or, after a whole fetch, `gate: cannot remove the fetched outputs archive <path>: <error>` (a cut fetch still says "partial").

    **Tracked members** (GATE-148): the box skips the back commit's paths, but that is no control against a broken or hostile builder, which can send a file git tracks that sits under the globs (a checked-in file under `**/bin/**`), and the install would overwrite it. A member that counts as tracked is therefore not installed: `gate: remote outputs not fully brought back: {path} is tracked by git`, and the run keeps its status, as an output in use does.

    A member counts as tracked when the worktree's git index lists its path, when it is under a submodule (a gitlink, mode 160000, initialised or not: the superproject's index does not list a submodule's files), or when a folder above it inside the worktree holds a `.git` file or folder of its own (a nested repository the outer one does not track). A folder whose `.git` cannot be looked at counts as holding one.

    The index is read with one `git ls-files -z -s` over the whole of it through the client's git runner, intersected with the members in Rust, case-insensitively; there are no pathspecs, since a member list can pass the command-line limit and pathspecs are glob-read. A record of another shape than `<mode> <object> <stage>\t<path>` refuses the install.

    An index path that is not UTF-8 makes every member in its folder count as tracked (every member below the folder, when the part that is not UTF-8 is a folder's name): Git for Windows checks such a name out under a decoded name the install cannot match exactly, and a spurious skip costs a full send while a miss overwrites a tracked file.

    An archive with no members asks git nothing. A listing past the runner's 16 MiB cap, or a failed call, refuses the install with the runner's line; an interrupt is an interrupt.

    A skipped member carries its reason, so a line ends `is tracked by git`, `is in use or read-only` or `cannot be rewritten (…)`, and each counts as not fully brought back; the first two keep the pending marker, the third does not.

    A repo that checks build outputs into git opts in per call with `--bring-tracked <glob>` (repeatable, matched with the output-glob matcher), which installs tracked members matching it as before. The matcher's exclusions hold for it too, so it never brings back a path the output globs never do (`.git*`, `.lfsconfig`, `.tmp/`, `TestResults/`, `.env` files).

    A glob the matcher cannot compile refuses the install as `the --bring-tracked glob '<glob>' cannot be matched: <detail>`. Nothing in `remote.json` turns the refusal off for a repo.

    A repo whose build regenerates a file it tracks under the globs therefore keeps the pending marker on every routed run, and each next run sends `OutputsFull` without the hash cache, until its caller passes `--bring-tracked`.

    **Never `.git*` or `.lfsconfig`** (GATE-155): an output glob such as `*`, `*.*` or `**` would match a root `.lfsconfig` or a `.gitattributes` at any depth, since the `EXCLUDED` pattern leaves out only the `.git/`, `.tmp/` and `TestResults/` folders and `.env` files. That would give a builder a second way to reach the files a sync-back never writes.

    `Globs::matches` is therefore false for any `.git*` path component and a `.lfsconfig` name, matched as the sync-back matches them (`is_git_s`, `is_lfs_config` in `outputs/names.rs`, which `remote/syncback.rs` imports), with the same normalised spellings: `\` as a separator, a cut at the first `:`, trailing dots and spaces stripped, and 8.3 aliases. `Globs::enters` is false for a `.git*` folder, so the box's walk never enters one.

    The box never packs such a file, and the PC's install refuses one that still arrives through the existing `the archive member <path> is not under the output globs` line.

    **Never a name the PC's install refuses** (GATE-166): a builder output can have a name that is not plain (the member rule under **The outputs module** below): an 8.3-shaped `m~10.txt`, a trailing dot, a device name such as `COM1.txt`, a bidirectional mark, or more than 64 parts.

    So can a file named as the archive's own list, `.gate-outputs.json` at the root in any letter case, or a name that is not Unicode. Packing one would fail the pack, lose the whole archive and turn a passing run into 1.

    `Globs::matches` is therefore false for a path that is not plain (`is_plain_path` in `outputs/names.rs`, which `outputs/archive.rs` imports for its pack and unpack checks) or is the list name, and `Globs::enters` for a folder whose own name `is_plain_part` refuses (the 64-part limit is the file's, not the folder's). The walk passes over a name that is not Unicode. The PC's manifest walk leaves all of these out silently.

    The box's pack skips them and says so once, before its `gate-remote-outputs:` line, and the run keeps its status: `gate: remote outputs: skipped <n> file(s) with names this PC cannot take: <first path>`, with `, …` after the path when n is above 1.

    The line counts the files the pack would have taken but for the name (the back commit's paths and the PC's unchanged copies left out, as for any file), names the first by relative path, `/`-separated, a name that is not Unicode read lossily, cut to at most 1024 bytes at a character boundary and followed by `…` when cut, and escaped as other box lines escape paths. It is not said when the pack fails.

    The walk reads long names, so an alias-shaped name such as `l~10.txt` or `GITATT~1` is a real file and is counted, though the PC never takes it. Only a name spelled as a `.git*` component or a `.lfsconfig` (letter case, `\`, a `:` stream and trailing dots and spaces aside) stays silent, as does an excluded path.

    A folder the walk does not enter because its name is not plain or not Unicode (`bin/x~1/`, `bin/con/`, `bin/report./`) is read through a verbatim `\\?\` path only to count the files below it that the globs select, which are never packed (`Globs::counts_below`). That read enters no excluded or `.git*`-spelled folder and no folder of 64 parts or more. A folder or entry it cannot read leaves those files uncounted and the walk goes on, so the outputs are still brought back.

  A pre-banner failure, which is exit 255 (128 for git) with empty stdout, no start marker heard and a first stderr line starting `Connection timed out during banner exchange`, `kex_exchange_identification: ` or `banner exchange: `, is made again up to three times, each after a 2 s pause and the line `gate: <host> did not answer the ssh banner; trying again (<n> of 3)`, until a call gets past the banner; the call counts as unreachable only when every one of them failed before it. This rides out a builder whose ssh server starts no session for half a minute.

  The bounded `stop` and `boot` calls keep a single retry, said as `(1 of 1)`. A run call whose box said the start marker is never retried or read as a pre-banner failure, whatever its first stderr line: the command may have started.

  Two pre-banner failures of the run call fall back `unreachable`; the sync-back's `ls-remote` and `fetch` are retried the same way, and two failures of either end the run with status 1.

  **The watchdog** (`remote/watchdog.rs`, `remote/client.rs`) watches the preflight, the push, the run call, the sync-back's fetch and the outputs fetch by the run's stall window and two limits (the run call's backstop and the transfer ceiling) on three clocks.

  The setup (the preflight and the push) is cut at the transfer ceiling, 1800 s whatever `--timeout` says, counted from the attempt's start.

  The ceiling itself is counted by the builder ([The box side](#the-box-side), `run`), in load-adjusted time from the command's start, so a slow push does not use the command's time. The run call is cut only at its backstop, 5 x `--timeout` + 60 s of wall time counted from the run call's own start, for a box that cannot end its run. Each fetch after the run call, the sync-back's and the outputs', is cut at the transfer ceiling counted from its own start.

  A `--stall` under 15 s is raised to 15 for a remote run (the box's heartbeats come every 5 s) and says `gate: --stall <n> raised to 15 for a remote run (heartbeats come every 5 s)`. It looks every quarter of the window, at most once a second.

  A run call killed by the client says `gate: STALLED: no output or remote CPU for <n> s` or `gate: BACKSTOP on <host>: no end after <w> s of wall time (5 x the <n> s ceiling + 60 s); killed the run call and stopped the command`, stops the command on the builder and ends the run with status 124, with no fallback.

  A command the builder killed ends the run the same way as a client kill, with the verdict `timed-out` or `backstop` the kill tag names, but with no stop call (the box already ended the job), no sync-back fetch and no outputs fetch, the outputs pending marker kept; the kill line is the box's own, read from the log.

  A killed preflight or push never started the command, so an early stall says `gate: remote <host> unreachable: <the call> stalled` (or `outlived its transfer ceiling of 1800 s`) and falls back `unreachable`, with no kill line and no exit 124.

  A fetch after the run call cut at its transfer ceiling says `gate: TRANSFER TIMED OUT after 1800 s fetching the outputs` (or `fetching the sync-back`), ends the run with status 124 and journals `transfer-timed-out` after a command that passed; after a command that failed, the line is said and the run keeps the command's status and the verdict `failed`.

  **The stop** is a `stop` call to the builder's `gate box`, made after a kill and after a Ctrl+C that ended a call that may have started the command. It is bounded to 30 s, is retried once after a pre-banner failure while 5 s of the bound would be left, and runs on a hidden console of its own (`CREATE_NO_WINDOW`), so a second Ctrl+C cannot end it.

  After a Ctrl+C, and before the stop call, the client says once `gate: stopping the command on <host> (up to <n> s)` to stderr and the log, with `<n>` the stop's bound in whole seconds, rounded up (30 by default); a second Ctrl+C adds nothing. A stall or backstop kill, a kill by the builder, and a Ctrl+C after a lost connection, say no such line.

  The stop says `gate: stopping the command on <host> did not finish in <n> s` or `gate: stopping the command on <host> failed (status <n>)`; the run ends the same either way.

  **A lost connection** is any exit 255 of the run call that is not a pre-banner failure (`End::Lost`); a command that itself exits 255 is treated the same, as the PowerShell wrapper does; so is one that prints ssh's banner words and exits 255, since its call said the start marker (`gate: lost the connection to <host>`, not `unreachable`).

  Nothing is synced back. The client always sends the `stop` first, and a Ctrl+C by the end of the stop says the lost line below and ends with the Ctrl+C status, with no boot call.

  Then, only when the preflight gave a boot time (`gate: remote boot <time>`), it makes a bounded `boot` call (`gate box boot`): the same 30 s bound and pre-banner retry rule as the stop. The outcome:
  - Both boot values parse as timestamps and the new one is more than 30 s later: `gate: <host> restarted during the run (booted <time>); the run did not finish there`, and the run falls back `rebooted`.
  - The boot call gives no value, or either value is no timestamp: `gate: <host> unreachable after the connection was lost; the run did not finish there`, and the run falls back `unreachable`.
  - Otherwise (the same boot, an earlier boot, exactly 30 s later, or no boot time at the preflight): `gate: lost the connection to <host>` and the run ends with status 255.

  A boot call that fails says `gate: asking <host> for its boot time did not finish in <n> s`, `gate: asking <host> for its boot time failed (status <n>)` or `gate: asking <host> for its boot time failed: <why>`, and counts as giving no value.

  On a fallback after a loss, the run call's output lines are dropped from what the local rerun's marker scan reads. Unlike the PowerShell wrapper, the stop always comes first (it skips the stop on a restart), and a restart needs a later, parseable boot time (it compares unparsable values as strings and takes any difference over 30 s).

  **Ctrl+C** ends the call it interrupts and the run as an interrupt. A Ctrl+C whose kill of the call fails stays an interrupt and still sends the stop.

  **Jobs.** Every remote call (ssh or git) runs in an anonymous kill-on-close job, created suspended and assigned before it runs, so a kill ends its whole tree; the routing decision's local git calls start the same way (`remote/launch.rs`). Breakaway from the job is allowed only for the local test transport, whose stand-in box starts under the gate's own job.

  **The size cut.** The watch also cuts a call that wrote too much: the sync-back's fetch, when its job has written more than the cap (see Sync-back), with the reason `<call> wrote more than <size>`. The bytes check comes before the stall and ceiling checks, and a line over the 64 KiB line cap wins over an after-exit size cut.

  **The output cap.** Every call's line is capped at 64 KiB, and a call keeps only its last 1000 lines and at most 1 MiB of them, dropping the oldest first; the newest line is always kept. A `\r` resets the current line, as git's progress does (the Progress and Unlogged readers only). A progress update over 64 KiB, however the pipe splits it, ends a capped call with `... sent a line longer than 64 KiB`. The run call is not capped: its longer lines pass on in pieces.

  **The log cap.** One call may write at most 256 MiB of the builder's output to the gate's log, its standard output and standard error counted together, whole lines and the run call's pieces alike.

  The piece that would pass the cap is not written: the log gets one line in its place, `gate: the builder's output passed 256 MiB; the rest is not logged`, said once by whichever of the call's two readers passed it first, on a line of its own even when it cuts into a long line passed on in pieces, and nothing more of that call's output.

  A reader's charge and its write are not one step, so a piece the other reader was cleared to write just before can land after the cut line: at most one piece per stream, and the log still holds no more than the cap of the call's output. The console keeps echoing a run call's output past the cap, so the end of the run is still seen live, and the run keeps its status.

  Each call counts afresh, so a flooded preflight still leaves the run call's output logged. The client's own lines, the box's protocol lines and heartbeats (never logged) and the outputs archive (a raw copy, capped by the fetch cap) are not counted. The test seam `--test-remote-log-cap-bytes` lowers the cap, and the line names the lowered size as a whole number of its largest binary unit when it is one (`64 KiB` for 65536).

  **Escaping.** Every text a builder supplies that a `gate:` line quotes is escaped: control characters, U+061C, U+200E, U+200F, U+202A-U+202E, U+2066-U+2069, U+2028 and U+2029 are written `\u{..}`. That covers the preflight's last line, the builder's version in the version refusal, and every `git ... failed` line of the snapshot, the saved ref and the sync-back. A failed git line lists git's stdout first and its stderr last, keeping the last 10 lines and 4 KiB so git's `fatal:` line survives.

  The client's git calls also drop `GIT_CONFIG_COUNT`, `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM` and every `GIT_CONFIG_KEY_n` and `GIT_CONFIG_VALUE_n`, so the caller's git configuration never reaches the builder's repo, and `GIT_SSH_VARIANT` and `GIT_EXTERNAL_DIFF`, so neither the ssh option syntax nor the diff a sync-back applies can be changed from outside.
- **Sibling worktrees** (GATE-19, GATE-213), the declaration, the machine's consent, the decision, the rewrite, the box's layout, the client's send, the PC's sync-back and the outputs install and stale lines built: some repos' builds reach into a sibling repo by relative path, and a routed run that sends one worktree's snapshot would compile something different on the builder without any error.

  A repo declares its siblings in a tracked file of its own, `.gate/siblings.toml`, beside the [toolchain manifest](#routing) and apart from it, so a fault in one never changes what the other means.

  A declaration alone carries nothing: the machine consents per repo in `remote.json`'s `siblings`, `{"siblings": {"<repo>": ["yaat-server"]}}` ([remote.json](#remotejson)), so a tracked file cannot make this PC send a folder it was never told it may. The file has one key, `siblings`, an array of folder names, each a sibling worktree at `../<name>` of the worktree root: `siblings = ["yaat-server"]`. `siblings = []` declares none; a file without the key, or with any other key, is a problem. Only the routed repo's own file is read: a sibling's `.gate/siblings.toml` is never followed, so a pair is one level deep.

  A name is one or more ASCII letters, digits, `.`, `_` and `-`, not starting or ending with a dot, not a reserved device name (`CON`, `PRN`, `AUX`, `NUL`, `COM1` to `COM9`, `LPT1` to `LPT9`, whatever follows the first dot), not repeated in another letter case, and not the root's own folder name; at most 8 names. The same rule decides whether a root's own folder name can be laid out. A leading byte order mark is skipped.

  Unlike the toolchain manifest, any problem keeps a matched run here: a sibling left out would build something different with no error. [`gate check`](#gate-check) lists every problem of the file and each declared sibling's consent and folder beside the worktree root.

  The reader's problems are `<key> is not a key (siblings)`, `declares no siblings`, `siblings is not an array (TOML <type>)`, `siblings[<i>] is not a string (TOML <type>)`, `siblings[<i>] is empty`.

  Name faults: `siblings[<i>] is not a folder name of letters, digits, '.', '_' and '-' not starting or ending with a dot, not a device name: "<name>"`, `siblings[<i>] repeats an earlier name: "<name>"`, and `siblings names more than 8 folders` (at the ninth name, reading no further), `line <l>, column <c>: <toml message>` and `not valid UTF-8 (...)`.

  The file is read from disk as it stands, never from a snapshot, and only once a routed command matched. `.gate` is opened as itself and held, and `siblings.toml` is opened by its name inside that folder object (`gate_win32::open_beneath`, an `NtCreateFile` relative to the held handle), never following a link, so nothing swapped in for `.gate` after the open is read. Both are opened sharing reading, writing and deletion, so a checkout or an editor's save is never blocked.

  A `.gate` that is a link or a junction says `.gate is a link or a junction`, and one that cannot be opened `.gate: cannot read it: <error>`; a `.gate` that is a file, or no `.gate`, holds no siblings.

  A `siblings.toml` that is a link, a junction or a folder says `is a link, a junction or not a file`, one over 64 KiB `is larger than 64 KiB`, and one that cannot be read `cannot read it: <error>`, each after `gate: not routed: .gate/siblings.toml: `.

  A file that is another kind of reparse point (a deduplicated one) is read through its filter from a handle opened again (`gate_win32::reopen_through_filter`), which must be the same file (`gate_win32::same_file`), else it is the not-a-file problem.

  A declared sibling is checked in this order, the first fault keeping a matched run here with its line ([Routing](#routing), the decision): its name, the machine's consent, the root's folder name, then each folder, then the folder ids.

  A sibling that passes all is carried: the route records the root's folder name and each carried sibling, `RemoteRun.leaf` and `RemoteRun.siblings` (its name, its root and its folder's id), and the run says `gate: routed with siblings: <name> (<path>), ...`.

  Before the run takes its slot, the client re-checks each carried sibling's name and root as text and reads no folder (`remote/carried.rs`); one that fails falls the run back here with the client fallback reason, so no sibling is dropped from a run that is sent.

  How a word naming a sibling is rewritten is under Routing (The in-worktree path rewrite).

  The box side of a sibling pair is built ([The box side](#the-box-side)): the payload's `Pair` and `SiblingSnapshots`, a bare repository per repo, the work folder laid out as `Work\{leaf}` and `Work\{sibling}` under the layout record, and back commits all or nothing across the repos, so relative paths between the trees resolve.

  The PC's sync-back of a pair is built too, all or nothing across the repos (Sync-back, under [Routing](#routing)).

  The client sends the pair: the payload's `Pair` and `SiblingSnapshots` carry the main repository's snapshot and one for each sibling, in declared order, and the pushes, the parity check and the sync-back cover every repository.

  A pair's outputs come back for the main repository alone. The install rewrites each tree's box path in the files that record it to that tree's own root, the main's to the worktree and each sibling's to the sibling's worktree (Outputs fetch, under [Routing](#routing)), and the stale line also names each sibling's output folders (Output lines).

  The client's side of a pair, in order: it holds the folders (`remote/held.rs`) and takes the worktree locks; asks git where each repository is (its top folder, git folder and common git folder, in one `git rev-parse` bounded at the root's parent) and holds the git folders too; snapshots each repository at the place git found, reading the folders again after each snapshot.

  It then reads the payload back with the box's own reader, so a pair the box would refuse stays here; checks toolchain parity for each repository; pushes each and saves each local ref; and lets the folders and locks go before the run call.

  Parity is checked for each repository in order: a sibling's SDK need is read from its own `global.json` and its lines name it, and the tools are the main repository's manifest alone. A builder that lacks anything says one `gate: remote toolchain mismatch: <repo> needs ...` line per need and falls back `parity` once.

  `remote/held.rs` holds a pair's folders from the check until the client lets them go: the root's parent is opened once, following links, on a local volume (a network volume is refused), and the root and each sibling are opened beneath it as themselves, never through a link, each with its `.git` entry (a folder or a linked worktree's file, no reparse point of any kind). Each is checked against the exact path the route was decided for and, the root and a sibling, its folder id.

  Every handle shares reading and writing but not deletion, so none of these can be renamed, removed or swapped for a link while held. Once git has said where each repository's git folder and common folder are, those are held too, so a linked worktree's git folder, which lies outside the pair's parent, is covered; one reached through a link or a junction is refused. A recheck reads every path again and opens each as git does, to see that it still leads to the held folder.

  Two repositories whose common git folders have one folder id share a repository and keep the run here. The refusals are the `gate: not routed: <who> <why>` lines of [Output lines](#output-lines), the main repository said by the root's folder name.
- **Inputs** (GATE-22): a routed command can need ignored files only the PC has, and the snapshot sends only tracked and untracked-not-ignored files. A repo's optional `inputs` list of ignored-file globs, beside `outputs` in the routing file, names the ignored files a routed run sends to the builder's work folder before the run, compressed in the outputs archive's format and hash-diffed against what the work folder holds, so an unchanged input costs nothing.

  The design is [plans/remote-inputs.md](plans/remote-inputs.md).

  Built: the `inputs` key and the route's `RemoteRun.inputs`, the box's `inputs` verb with the run's `InputsDigest` check ([The box side](#the-box-side)), and the client's part (the manifest, the diff and put calls, the hash cache, the lines and the `inputs` fallback reason), described under Inputs in [The remote client's run](#routing).

  Inputs have their own exclusion set, not the outputs': `.tmp/` is reachable, but a client never sends a path with a `.git` component, `.lfsconfig`, `.env`, `*.env`, `.env.<name>` or `.envrc` (at any depth, matched without regard to case), the gate's own archives and stages, or the run's log and `.err` file. The box refuses a manifest that names a `.git*` name, `.lfsconfig` or a `.env` file, or a path that is not plain and relative.
- **The per-host failure record** (GATE-27), planned, not built: a record in the state root holds a builder's failure reason and an expiry. A routed gate that finds an unexpired record says `gate: not routed: <host> <reason> <n> s ago`, runs here and journals `fallback` with the reason, rather than paying the snapshot, preflight and push again for the same reason.

  The windows per reason: `unreachable` a short one (60 s, distcc's default backoff); `parity` and `tools` until the snapshot's `global.json` or tool manifests change; `busy` and `full` are load, not faults, and are never recorded.

  The record also counts box-side losses (exit 255, `rebooted`, `unreachable` after the start, a `stop` that leaves processes behind) apart from command failures, which never count; past a threshold nothing routes to that builder until a preflight succeeds.
- **The host** is the one `gate.exe` validated against the allowlist (the routing file's text, above). `gate-router` carries it in the routed run as a `Host` newtype that only the routing file's reader builds, so no host string the reader did not check reaches a process. The Rust remote client starts ssh with each argument separate and `--` before the destination, and keeps the allowlist too, since a git `host:path` URL cannot carry `--`.

## The box side

A builder runs the same `gate.exe`, under the hidden `gate box` subcommand, from its own `%LOCALAPPDATA%\gate\bin`. The exe is pushed to every builder the local `remote.json` names, over ssh, by `gate push` ([`gate push`](#gate-push)), not by the local install script.

A routed run first learns the builder's gate version; when its whole version text, build id included, differs from its own, the run falls back to local, saying `gate: not routed: <host> runs gate <version>, this PC <version>; run gate push`, and journals `fallback` with reason `version`. A builder running another build of the same package version falls back too.

Builders are configured only in the local `%LOCALAPPDATA%\gate\remote.json`. The repo, its docs and its tests name no builder; the suite's remote cases run against a stub.

The box keeps the PowerShell wrapper's lines for the verbs and the refusals, and adds tagged machine lines the wrapper never emits: the start marker, the refusal tags, the kill tag and the adjusted line (Run call, in [Routing](#routing)). `gate box` is what the Rust remote client calls on a builder, so it runs whenever a routed run goes through the remote client.

Each verb is one ssh call today, and the call's cost is its connection, not its work. Measured warm, setup takes about 2.1 s (TCP about 0.2 s, key exchange and auth 1.5 s, the session 0.4 s) against 0.2 to 1.0 s of verb work, so a routed run's three verb calls spend about 6.4 s, 30 to 37% of a 17 to 26 s routed build, before its git transfers pay the same again.

A routed run therefore moves to one long-lived connection carrying every verb, `gate box serve`, and the per-call client stays until that lands (GATE-70 holds the measurement, and GATE-109 the work).

The payload is one base64 word (standard alphabet, padded) of UTF-8 JSON, sent as the first line of the call's stdin (never on a command line), holding the wrapper's keys: `Key`, `Bare`, `Work`, `Lock`, `Index`, `OutputFolder` and `Job`, and for `run` also `Command` (or, for a warm-up, `WarmCommands` and `WarmHash` in its place), `Subfolder`, `Identity`, the optional `Env`, `Outputs`, `Nonce`, `OutputsFull`, `Timeout`, `Host` and the optional `InputsDigest`, and for `outputs` also `Name`.

`inflight` reads only `Root`.

`inputs` reads `Key`, `Bare`, `Work`, `Lock`, `Nonce` and `Step`.

A warm-up's `run` payload sets `WarmCommands` (1 to 8 commands, each a non-empty list of non-empty texts) and `WarmHash` (64 lower-case hex digits, the hash of the warm-up file the commands come from), sets no `Command`, and asks for no outputs. The PC's client sends it (The remote client's run, Warm-up call) for `gate warm`; no route does yet.

`preflight` also reads an optional `Needs`, an object of tool name to minimum version text (`""` for any version), which the PC builds with `remote::payload::preflight_word` from the tools it wants probed; it is omitted when there are none, so a payload without needs is the same bytes as before.

Unknown keys are ignored, and `Outputs`, `OutputsFull` and `OutputFolder`, which the outputs transport owns (GATE-44), are read only by `run` (the outputs) and `outputs` (`Name`, `OutputFolder`); `preflight`, `stop` and `boot` ignore them.

`Timeout` is the run's ceiling in whole seconds, 1 to 2147483647, which the box counts in load-adjusted time; absent or null means no box ceiling and no adjusted line, and anything else is refused before the lock with `gate: box payload Timeout is not a whole number of seconds` and exit 2. `Host` is the PC's `remote.json` name for the builder, which the box's kill lines name, control characters escaped; absent leaves it out of them.

For `run` the payload also needs a `Snapshot` key: exactly 40 lowercase hex characters, the commit the run checks out. Anything else is refused before the lock and the checkout with `gate: remote run payload's Snapshot '<value>' is not a 40-hex commit sha` and exit 2.

The value is empty for a missing key or JSON null, the string as given with control characters escaped, and compact JSON for any other non-string. Two differences from the wrapper are deliberate: the echoed value has its control characters escaped, and the check is stricter than the wrapper's .NET `$`, which accepts a trailing newline.

Every path comes from the payload and nothing is written under the gate's state root; `gate box` takes no `--state-dir`. Test seams are `test-seams`-gated flags, never environment variables, and sit before the verb (`gate box --test-... run --stdin`) in a `BoxSeams` struct on `BoxRequest`, apart from the `gate run` seams, so `--test-print-request` is unchanged.

The box seams go in any order before the verb, each also as `--<seam>=<value>`. `--test-heartbeat-ms <n>` sets the period of the command heartbeat, and `--test-box-console fail-before` makes the command's UTF-8 console fail before the command starts (the only accepted value, else `gate: --test-box-console must be fail-before, got '<value>'`).

`--test-box-token refused` makes the start with the command's restricted token fail as refused, so the box falls back to its own token (the only accepted value, else `gate: --test-box-token must be refused, got '<value>'`).

`--test-box-park-launch <path>` parks the launch after the donor is in the job and before the output lock, writes the empty file `<path>.parked`, and goes on once `<path>` exists (at most 600 s).

The box's ceiling has three seams: `--test-box-sample-ms <n>` replaces the 5 s sample period, in milliseconds, `--test-box-sampler-fail true|false` makes every sample fail (`true`), so only the backstop watches and no adjusted line is said, and `--test-box-free-percent <1-100>` makes the samples find that share of the machine free, so the load-adjusted clock runs at that share of wall time.

`--test-box-kill-leaves <n>` stands in for the number of processes the last ending of a ceiling or backstop kill leaves (a whole number above 0), so the box says its note for a job that is still not ended.

The other four box seams are `--test-outputs-budget-bytes <n>`, which replaces the 50 GiB outputs budget of the box's trim, in bytes, `--test-inputs-budget-bytes <n>`, which replaces the 4 GiB a `gate box inputs` put unpacks, decompressed, in bytes, `--test-dotnet-limit-ms <n>`, which replaces the 30 s bound of the preflight's `dotnet --list-sdks` read, in milliseconds, and `--test-probe-limit-ms <n>`, which replaces the 10 s shared bound of the preflight's tool probes, in milliseconds.

A seam is refused as the `gate run` seams are: no value, given twice, and a value outside its rule (`gate: --test-outputs-budget-bytes must be a whole number above 0, got '<v>'`, `... must be at most 2147483647 ...`, `... needs a value`, `... is given twice`).

The rules: a whole number above 0 and at most 2147483647 for the heartbeat, the sample period, the outputs and inputs budgets, the dotnet limit, the probe limit and the kill's leftover count, 1 to 100 for `--test-box-free-percent`, `true` or `false` for `--test-box-sampler-fail`. Without the feature each word gets the box usage line.

The box gives each run's command its own build servers: the command's environment is the box's own with `SharedCompilationId` and `MSBUILDNODEHANDSHAKESALT` set to `gate-box-<Nonce>` (the payload's `Nonce`), `MSBUILDUSESERVER=0`, `DOTNET_CLI_USE_MSBUILD_SERVER=0` and `MSBUILDDISABLENODEREUSE=1`, each replacing any inherited value (names compared case-insensitively).

So no other run's command, on the box or on the PC, uses the compiler server or the MSBuild nodes this one starts, and no node idles in the job after its build (an idle node would keep alive anything in the command that waits on its whole tree).

The box's own git calls, tool restore and pnpm install do not get these settings.

The servers a run starts live in its job and are ended at its end, so every routed build starts its own nodes and compiler server; none is kept warm between runs.

`git` and `dotnet` are found with the gate's own program resolution, which never searches the current folder.

The box reads the request, before the verb and before any lock, as the first line of stdin, ended by its newline or the end of input (one newline, and a carriage return before it, is not part of it), reading at most 16 MiB and never waiting past the newline; it has no deadline, as a client that stops sending is ended by its own watchdog. All of these print one line on stderr and exit 2: no request, `gate: the box request is missing on standard input`; 16 MiB without a newline, `gate: the box request on standard input reaches 16 MiB without a newline`; stdin failing, `gate: the box request on standard input could not be read: <error>`; a line that is not UTF-8 or a payload that is not base64 JSON, `gate: box payload is not base64 JSON: <reason>`.

Built verbs:

- **`boot`** prints `gate: remote boot <UTC time>` on stdout (ISO 8601, 7 fractional digits, `Z`; the system time less the tick count since boot) and exits 0.

  When the time cannot be read it prints `gate: remote could not read the boot time: <message>` on stderr and exits 1, so the client's boot query after a lost connection says the call failed rather than taking a status 0 with no time. The preflight says the same line and goes on: an unreadable boot time fails no preflight, and the client then makes no boot query.
- **`preflight`** creates the bare repo at `Bare` when missing and the work folder at `Work` with `.git`, `origin` set to `Bare`, `core.autocrlf false` and `core.eol lf`, and writes `.git\objects\info\alternates` as `<Bare with />/objects` and a line feed, without a byte order mark, unless the file already holds exactly that.

  On a tree it has just made with `git init`, the preflight writes the config itself: it takes `.git\config.lock` (as git does), reads `config` beneath the held `.git`, and when that config is `git init`'s own (a single `[core]` section, no `autocrlf` or `eol`, a plain UTF-8 file with no U+FEFF, last line not ending in `\`) it writes the `origin` remote (`url`, `fetch = +refs/heads/*:refs/remotes/origin/*`) with `core.autocrlf=false` and `core.eol=lf` into the lock in one write, quoted as git quotes values, and renames the lock over `config`.

  Any other config, or one it cannot take, gets its lock deleted and the git steps run as before (`config --get remote.origin.url`, `remote add`, two `config` calls).

  Apart from that config write, the preflight takes no lock, so a link, a junction or a file at the tree's `.git`, `.git\objects` or `.git\objects\info`, or anything but a file at `.git\config`, is never followed: it readies nothing there (no git step, nothing written) and the run repairs it under the lock. It readies the work folder's repository only when the folder's layout record says a run of one repo or is missing; a record that says a pair, `changing`, or cannot be read leaves the folder to the run.

  With a `Pair` in the payload the preflight makes the folder above `Work` and one bare repo per repo, `Bare` for the main checkout and `{root}\{stem}.git` for each sibling (`{root}` is `Bare`'s folder, `{stem}` the sibling's name with every character but an ASCII letter, digit, `-` and `_` made `-`), and touches nothing inside `Work`.

  It prints on stdout, in order: `gate: remote created <Bare>` (only when it created the bare repo), `gate: remote version <gate version>` (the same text as `gate --version` after `gate `, build id included), `gate: remote warm <64-hex>` or `gate: remote warm none` (the hash in the work folder's warm record `{Work}.warm`, read before the folders are readied; `none` when the main tree's `.git` or the record is missing, or the record is not a plain file holding exactly 64 lower-case hex digits, a line end included), the boot line, `gate: remote sdks <v1,v2|none>`, and one `gate: remote tool <tool> ...` line for each tool of the payload's `Needs` (Tool probes, below); git's own output passes through. A payload without `Bare` or `Work` prints `gate: box payload lacks <key>` and exits 2.
- A failing git step prints `gate: remote <step> failed (status <n>)` and exits with that status; a git that cannot start prints `gate: remote <step> failed: <OS message>` and exits 1.

  The same `failed: <message>` form covers a config read or write the preflight made itself: `gate: remote git remote add failed: cannot write <config>: <why>`, `gate: remote git remote add failed: cannot open|read <path>: <why>` and `gate: remote git remote add failed: cannot write <config>: config.lock could not be removed: <why>`. The steps are `git init --bare`, `git init`, `git remote add`, `git config core.autocrlf` and `git config core.eol`.

  On a pair run a step's line names its repo after the step: `gate: remote git init --bare failed for <leaf|sibling>` (status `n` and message forms as above), `gate: remote git init failed for <name>`, `gate: remote git remote add failed for <name>`, `gate: remote git config core.autocrlf failed for <name> (status <n>)`; a run of one repo says them as above.

  A folder or file that cannot be made prints `gate: remote could not create the folder <path>: <error>` or `gate: remote could not write <path>: <error>` and exits 1. An SDK list that cannot be read prints `gate: remote could not read the SDK list: <message>`, omits the sdks line and still exits 0; the client then skips the parity check.

  The SDK list is read as this PC's is ([Bounded SDK read](#routing)): `dotnet` found on the box's `PATH`, run in the system folder with `NoDefaultCurrentDirectoryInExePath=1`, in a kill-on-close job of its own, on the box's console, its standard output read up to 16 MiB and the end of its standard error kept for a failed read's reason, which then ends in its last lines (it reaches neither the box's stderr nor the client's log otherwise).

  The box has no interrupt; the job is killed at a 30 s bound (`--test-dotnet-limit-ms` lowers it).

  Past the bound the line reads `gate: remote could not read the SDK list: dotnet --list-sdks did not answer in 30 s`. The other `<message>`s are `dotnet --list-sdks wrote more than 16 MiB`, `dotnet --list-sdks exited with status <n>` and `cannot run dotnet: <why>`.

  Tool probes (`boxside/probe.rs`): for each tool of the payload's `Needs`, in payload order, the box runs a probe and says one line on stdout after the sdks line, all once every probe has ended: `gate: remote tool <tool> <version>`, `gate: remote tool <tool> present` or `gate: remote tool <tool> missing`, or `gate: remote tool <tool> skipped: <why>`. The box only reports; the PC compares the minimums.

  The probes are `rustc --version` for `rust`, `cargo-nextest nextest --version` for `cargo-nextest` (the plugin directly, never through `cargo`, so no `.cargo\config.toml` above the folder can alias it), `<name> --version` for `node`, `pnpm` and `pwsh`, and `ffmpeg -version`. `godot` is probed as the next paragraph says.

  The `godot` probe reads `GATE_GODOT_ROOT`, an environment variable of the builder naming a folder of side-by-side Godot installs, one per subfolder, each holding `Godot_console.exe`. It lists the direct subfolders of that folder (the first 64 entries, in path-name order), runs each install's `Godot_console.exe --version` under the batch's deadline and says the versions found as one line, `gate: remote tool godot <v1>,<v2>,...`, ordered by number and without repeats.

  A version is the leading dot-separated digits of the first line of stdout, three parts as printed or two with `.0` added (`4.3.stable.mono.official.<hash>` is `4.3.0`); an install that prints a version and exits non-zero is still listed. With no `GATE_GODOT_ROOT` the probe runs the `Godot_console.exe` found on `PATH`. A root that names no install is `missing`. Subfolders whose name is not valid Unicode, that hold no plain `Godot_console.exe`, past the first 64, or that cannot be read are left out.

  When no install gives a version the line is `gate: remote tool godot skipped: <why>`, the first reason met: `cannot run <install>: <install> lies inside the worktree` (an install inside the run's own folders, the folder above `Work`, `Work`, `Bare` and the outputs folder, is refused unrun; a `gate check` refuses the repo root), `cannot run <exe>: <path> is a link or a junction` (a subfolder or the exe), `GATE_GODOT_ROOT <path> is not an absolute path on a drive`, `cannot read GATE_GODOT_ROOT <path>: <error>`, `no answer in <limit>` (the listing was cut by the deadline with nothing read) or `Godot_console.exe --version gave no version: <line>` (the first line of stdout, cut to 200 characters with `…`, escaped, when a version part has over ten digits or the line leads with no version).

  Each program is found through `PATH` and `PATHEXT` as `dotnet` is (a `.cmd` shim runs through the system folder's cmd.exe; a `.ps1` is not run) and runs in the system folder on a hidden console, in the bounded runner of `remote/git.rs` (`program_output`).

  The probes run in parallel, started at the same time as `dotnet --list-sdks`, under one shared 10 s deadline (`--test-probe-limit-ms` lowers it); one still running at the deadline is killed with its process tree, and one with no time left is skipped without being launched.

  The version is read from the first non-blank line of stdout, else of stderr: the first run of two or more dot-separated numbers after an optional `v`, its first three kept. A line with no such run, a number longer than 10 digits or a non-zero exit gives `present`; a program that cannot be found gives `missing`.

  The `<why>` of a skip is `unknown tool`, the `godot` reasons above, `no answer in <n> s` (or `<n> ms` when the limit is not whole seconds), `no answer in <limit>, and could not be stopped: <error>`, `the probe failed` (followed by `: <panic text>` when the probe panicked), or the runner's failure text: `cannot run <program>: <why>`, `<program> <args> wrote more than 16 MiB` or `cannot run <program>: cannot read the system folder: <error>`.

  A payload whose `Needs` is not an object of texts prints `gate: box payload Needs is not an object of texts; probing nothing` on stderr, probes nothing and still exits 0.
- **`run`** runs the payload's `Command` (a list of texts), or, for a warm-up, each of its `WarmCommands` in order (Warm-up, below), in `Work` (in `Work\{Leaf}` for a sibling pair), and exits with the command's own whole 32-bit status after a successful run. Its stdout is the command's stdout, LF-terminated; its stderr is the command's stderr plus the box's own lines.

  The heartbeat: `gate box run` writes `gate-remote-heartbeat: <cpu ms>` on stderr every 5 s, on `gate-watchdog`'s `BeatClock` and `Pump`. Before the command starts (from just before the checkout) and after the command's wait ends it is not CPU-gated, so every period writes a beat, the first one period after it begins.

  While the command runs it writes only when the CPU of the command's job since the last beat reaches 1% of the time since it. The command's build servers are its run's own and run in the job, so a build server outside the job counts for nothing.

  The ceiling: with a `Timeout` in the payload, the box watches the command from its start with the local watchdog's arithmetic (`boxside/ceiling.rs`; sampling every 5 s, demand every core at normal priority, the stall left to the client through the heartbeat).

  At the load-adjusted ceiling it ends the job and says `gate: TIMED OUT on <host> after <n> s of load-adjusted time (wall <w> s, machine free <p>% on average); killed <command> and its children`; at 5 x the ceiling of wall time it says `gate: BACKSTOP on <host>: <w> s of wall time (5 x <n> s ceiling, load-adjusted <a> s, machine free <p>% on average); killed <command> and its children`, the clause reading `machine load unknown: the sampler failed` after a sampler failure.

  At a kill the box ends the job and drains the command's output first, then says, in order, the kill line, `gate-remote-killed: <nonce> timed-out|backstop` and the adjusted line `gate-remote-adjusted: <nonce> <s.sss>`, and exits 124. It makes no back commit (`refs/gate-back/<key>`) and packs no outputs.

  When ending the job fails or leaves processes, the box tries once more to end it. If the job is still not ended, it says first `gate: note: the box's ceiling|backstop kill left <n> process(es) running in job <job>` (or `gate: note: the box's ceiling|backstop kill could not end job <job>: <error>`), then the kill line, the tag and the adjusted line as on any kill, and exits 124: the run is still a kill.

  A drain that ends with the output still held open says `gate: remote command's output is still held open by a leftover process; stopped reading`. A client gone during the kill drain gets the box's client-gone status, not a kill.

  The PC journals a tagged kill with the tag's verdict whatever its log holds, applies no sync-back and fetches no outputs. A command's own exit 124 (no tag) is an ordinary status with a sync-back.

  `gate-remote-adjusted: <nonce> <s.sss>` is also said as any watched command exits, once its output has drained, unless the sampler failed. A failed sample says `gate: sampler failed: <message>; watching by wall time only` and only the backstop is read from then on.

  The start marker: just before it launches the command, before the donor console and the launch, the box says `gate-remote-started: <nonce>` on stderr once, with the payload's `Nonce`; a launch then refused or stopped has said it too. A warm-up says it once, after every tool and pnpm restore and before its first command, and not again before the later ones.

  Warm-up: a `run` with `WarmCommands` lays out and readies the work folder, restores the tools and the pnpm packages of every command, as a run does, then removes `{Work}.warm`, says the start marker and runs each command in order as a run's command is run (the stop signal looked at before each). It makes no back commit and packs no outputs, and on a pair run never says `gate: remote back commits: ...`. The payload's `Timeout` is one budget for all the commands: each command's ceiling is what the earlier ones left of it, in whole seconds rounded up, counting each one's load-adjusted seconds (its wall time once the sampler failed).

  The first command that ends non-zero or is killed ends the warm-up with `gate: remote warm-up stopped at command <k> of <n>` on stderr and that command's status (124 for a kill). A budget spent before command `k` ends it with `gate: remote warm-up ran out of its <s> s budget before command <k> of <n>` and exit 124. Once every command passed, the box writes `{Work}.warm` holding exactly `WarmHash` and says `gate: remote warm <hash>` last, exit 0. A record that cannot be removed or written says `gate: remote could not remove <Work>.warm: <error>` or `gate: remote could not write <Work>.warm: <error>` and exits 1.

  The `CpuSource` is the job's own CPU, which only rises: the job's accounting keeps what its ended processes used. Another run's busy build on the box does not keep a silent idle command alive.

  A heartbeat line never splits a forwarded line: the pump writes under the output lock the forwarders take. The run's git children (fetch, checkout, clean, the back ref reset, the back commit's add, write-tree, rev-parse, commit-tree and push, and diff-tree) have their output forwarded the same way, line by line; the box's own lines are written whole under std's stream lock, which the pump's writes take too. A line still waiting for the lock when the readers stop is dropped.

  After git exits, the box waits at most 2 s for its pipes to close. A pipe a leftover process still holds says `gate: remote git's output is still held open by a leftover process; stopped reading` and the run goes on.

  A client that closed the connection while git's output was forwarded ends the run with 255 and `gate: remote the client closed the connection during git <step>`, the step's name saying git once (`during git init`, not `during git git init`). A pipe that cannot be read says `gate: remote could not read git's stderr: <error>` (or `stdout`).

  A failed CPU read writes `gate: remote heartbeat stopped: cannot read the CPU of job <name>: <error>` (or `... cannot list the processes of job <name>: <error>`) and a thread that cannot start writes `gate: remote heartbeat stopped: cannot start the heartbeat: <error>`; the heartbeats end and the run does not.

  Ending the heartbeat waits at most 2 s (`Pump::stop_within`), so a pump stuck on a client that stopped reading is left behind and the work folder's lock is released.

  Payload refusals exit 2, each on stderr. The command's own: `gate: box payload Command is not a list of texts`, `gate: box payload lacks Command`, `gate: box payload Subfolder is not a text`, `gate: box payload Identity is not an object of texts`, `gate: box payload Identity may set only GIT_AUTHOR_* and GIT_COMMITTER_* names: <name>` and `gate: box payload Identity <name> holds a NUL`.

  `Env`'s: `gate: box payload Env is not an object of texts`, `gate: box payload Env has a malformed name '<name>'` (empty, or holding `=` or a NUL), `gate: box payload Env gives <name> a value holding NUL`, `gate: box payload Env sets <name>, which the gate sets itself` and `gate: box payload Env sets <name>, which git would read over the gate's own git settings`.

  Also `gate: box payload Env sets <name>: GATE_ names are the gate's`, `gate: box payload Env sets <name>, a machine path the builder keeps`, `gate: box payload Env names <name> twice` and `gate: box payload Env is over its cap of 64 names or 32 KiB`. A null `Env` is not set.

  A warm-up payload's: `gate: box payload sets both Command and WarmCommands`, `gate: box payload lacks WarmHash`, `gate: box payload asks a warm-up for outputs` (an `Outputs` glob or `OutputsFull` true), `gate: box payload asks a warm-up for Env` (an `Env` that is not null, even `{}`), `gate: box payload WarmCommands is not a list of 1 to 8 commands, each a non-empty list of non-empty texts` and `gate: box payload WarmHash is not 64 lower-case hex digits: <value>`.

  A `WarmHash` beside a `Command` is ignored, and a null `WarmCommands` is not set.

  The work folder's: `gate: box payload Lock must be <Work>.lock`, `gate: box payload Index must be <Work>.index`, `gate: box payload Key is not a work-folder key: <key>` and `gate: box payload Job is not this work folder's job name: <job>`.

  The sibling pair's, when the payload has a `Pair` (`{"Leaf": "<name>", "Siblings": ["<name>", ...]}`, the main checkout's folder name and one to 8 sibling folder names).

  Its names: `gate: box payload Pair is not an object of a Leaf text and a Siblings list of texts`, `gate: box payload Pair name '<name>' is not a folder name of letters, digits, '.', '_' and '-' not starting or ending with a dot, not a device name`, `gate: box payload Pair names no sibling`, `gate: box payload Pair names more than 8 siblings` and `gate: box payload Pair names <a> and <b>, equal ignoring case` (the leaf counts as a name).

  Its work folder and bare repositories: `gate: box payload Key <key> is not the work-folder key of the Pair's Leaf <leaf>` (the key is `<repo name of Leaf>-<8 lower-case hex digits>`), `gate: box payload Work must be <root>\work\<key> with a Pair: <work>`, `gate: box payload Bare must be <root>\<name>.git with a Pair: <bare>` and `gate: box payload Pair sibling <name> would share the bare repository <stem>.git` (two repos whose bare repositories are equal ignoring case).

  `SiblingSnapshots`, an object of exactly the pair's sibling names, each a 40 lower-case hex digit commit, is read by `run` alone: `gate: box payload SiblingSnapshots is not an object`, `gate: box payload SiblingSnapshots must name exactly the Pair's siblings: <names>` and `gate: remote run payload's SiblingSnapshots <name> '<value>' is not a 40-hex commit sha`.

  `Identity` may hold only `GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL`, `GIT_AUTHOR_DATE`, `GIT_COMMITTER_NAME`, `GIT_COMMITTER_EMAIL` and `GIT_COMMITTER_DATE`.

  `Env` is the caller's variables for the command: an object of texts of at most 64 names and 32 KiB of names and values together, the cap read from the raw object before any pair is checked.

  It refuses a name the gate sets (`GATE_SLOT_HELD`, `GATE_STATE_DIR`, `SharedCompilationId`, `MSBUILDNODEHANDSHAKESALT`, `MSBUILDUSESERVER`, `DOTNET_CLI_USE_MSBUILD_SERVER`, `MSBUILDDISABLENODEREUSE`, `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_<n>`, `GIT_CONFIG_VALUE_<n>`, and .NET's Event Log logging settings in any spelling: the `__` and `:` forms, a `DOTNET_` or `ASPNETCORE_` prefix, the provider's full type name).

  It also refuses `GIT_CONFIG_PARAMETERS` and `GIT_CONFIG` (git reads them over the gate's own `safe.directory` entry), any `GATE_` name, and the machine paths `PATH`, `PATHEXT`, `TEMP`, `TMP`, `SystemRoot`, `windir`, `ComSpec`, `USERPROFILE`, `HOME`, `APPDATA`, `LOCALAPPDATA` and `ProgramData`. Names are compared without regard to ASCII case.

  A name given twice is refused, first as an ASCII-case scan and then as Windows compares names, which folds more than ASCII. A refusal line shows the name shortened to 200 characters and escaped.

  The pairs apply to the command only, never to the box's own steps (the git steps, the tool restore, the pnpm install).

  `Key` is `[A-Za-z0-9_-]+` and is the last name of `Work`; `Job` is `Local\gate-remote-<Key>-<12 hex digits>`. `run`, `stop` and `preflight` check the key; `run` and `stop` check the job.

  The lock `<Work>.lock` is opened with no sharing and without following links, must be a plain disk file, and is held for the run's life; a held lock prints `gate-remote-refused: <nonce> busy` then `gate: remote work folder <key> is busy` and exits 75. A lock that cannot be opened prints `gate: remote could not open the work folder lock <path>: <error>` and exits 1.

  `Work` must not end with a separator (`gate: box payload Work must not end with a separator: <work>`, exit 2). Every git call in a tree passes `-c core.hooksPath=<Lock>\hooks -c core.fsmonitor=false`, a hooks path that cannot exist while the box holds the lock, so a repository's own hooks (one a build installed) never run in the box's git; fetch and push name the repo's bare repository by its path, never `origin`.

  Every git call in a tree also passes `--git-dir=<tree>\.git --work-tree=<tree>`, so neither a `core.worktree` nor a repository above the tree is followed.

  The payload is data, refused unless well formed; the command, and whatever it writes into `Work`, is the user's own code running as the user, and is trusted. It runs non-elevated, with a restricted copy of the box's token (the command's token, below).

  The layout of the work folder: a run of one repo's tree is `Work` itself; a pair's are `Work\{Leaf}` for the main repo and `Work\{sibling}` for each sibling, each with its own bare repository and its own commit (`Snapshot` for the main repo, `SiblingSnapshots` for the others).

  The layout record `<Work>.layout`, beside the lock, says which layout `Work` holds: `single`, `pair {Leaf} {siblings}` (the siblings sorted without regard to ASCII case, one space before each, so the same siblings in another order are one layout) or `changing` while the folder is being cleared. A missing record says `single`. It is read and written as the tools record is, through a handle that refuses a link, a folder or a file with more than one name.

  The records beside the lock, one file each, named `Work` plus: `.lock` (the run's lock), `.index` (the sync-back's copy of the index), `.layout` (above), `.inputs` (the copies the `inputs` verb left), `.tools` (the dotnet tool restore), `.packages` (the pnpm package restore) and `.warm` (the hash of the warm-up file whose commands last passed in the folder, 64 lower-case hex digits, no line end, written only by a warm-up). Each is described where its feature is.

  The warm record is removed before a warm-up's first command, when a run finds the layout changed (before `changing` is written) and when the work folder is missing and the run makes it, so it never outlives the folder state it describes. The preflight reads it without taking the lock, sharing reading, writing and deletion.

  Under the lock, before any checkout, the run makes `Work` when missing, opens it as itself and holds it (a link, a junction or a file there is refused, never followed), and, when the record names another layout than the run's, writes `changing`, removes every child of `Work` (each as itself, a link never followed), readies each tree and writes the run's layout.

  A `changing` record left by a clear that did not finish clears again. A run of one repo in a folder already of that layout whose `.git`, `.git\objects` and `.git\objects\info` are each a folder of its own, and whose `.git\config` is a file when there is one, readies nothing here: the preflight readied the tree. A pair run readies its trees every time.

  Readying a tree makes the tree and its `.git` folders of their own (a link, a junction or a file at either is removed as itself and the folder made again), then does what the preflight does for `Work`: `git init`, `origin` set to the repo's bare repository, `core.autocrlf false`, `core.eol lf`, and the alternates file naming that repo's own bare repository's objects, rewritten when it holds anything else. A folder found at the alternates file's name is never walked, deleted or followed: it is renamed beneath its held parent to `alternates.gate-moved-<n>` (the first free `n` from 1 to 100, never replacing an entry) and the box says `gate: remote moved aside the folder <old> to <new>`; every other thing there is deleted as itself.

  Before its first git step, a run holding the lock replaces a link at `.git\objects` or `.git\objects\info` with a folder and removes a `.git\config` that is not a file, as itself; git then writes a fresh config. A config that cannot be removed refuses the run with `layout`.

  A folder the box cannot clear or lay out refuses the run: `gate-remote-refused: <nonce> layout` then `gate: not routed: the builder could not clear <repo>'s work folder: <why>`, said together under the output lock, with the command never started, and exit 75. `<repo>` is the main repo's bare repository's name without `.git`.

  The PC reads the 75 plus the tag as fallback reason `layout`, says `gate: remote <host> layout; running here`, runs the command here and passes the box's line on as it came. A git step that fails while a tree is readied keeps its own line and status instead.

  A payload with an `InputsDigest` (64 lower-case hex digits, else `gate: box payload InputsDigest is not 64 lower-case hex digits: <value>` and exit 2) is checked once the work folder is laid out and before the checkout: the `<Work>.inputs` record must exist, hold that digest, and name only copies still in `Work` with the size, last-write time and file id the box left them with, and the tree's `.git\info\exclude` is rewritten from the record (the `inputs` verb below).

  Otherwise the run is refused with `gate-remote-refused: <nonce> inputs` then `gate: remote inputs of work folder <key> are not the ones sent: <why>`, said together under the output lock, the command never started, and exit 75. A folder moved aside from the exclude file's name says its `moved aside` line, under the lock.

  A payload without `InputsDigest` skips the check. The PC reads the `inputs` tag as fallback reason `inputs` and says `gate: remote <host> inputs; running here`, with `gate: remote inputs not sent: the builder's work folder does not hold the inputs sent` in the log.

  The steps are the layout, fetch, the commit probe, checkout, clean and the back ref reset (each of the three git steps once per repo on a pair run, the main repo first), the dotnet tool restore, the pnpm package restore, then the command, then the sync-back. A failing step prints `gate: remote <fetch|checkout|clean|back ref reset> failed (status <n>)` and exits `n`.

  On a pair run the step is followed by the repo's name (`gate: remote fetch <name> failed (status <n>)`), while the steps that ready a tree say `failed for <name>` as the preflight's do.

  After the fetch of `refs/gate/<key>` from the repo's bare repository, the probe runs `git cat-file -e <sha>^{commit}` in its tree. A non-zero exit prints `gate: remote snapshot <sha> is not in the work folder's repository after the fetch` (on a pair run `gate: remote snapshot <sha> is not in the repository of <name> after the fetch`) and exits 1, and a git that cannot start prints `gate: remote cat-file failed: <message>` (`cat-file <name> failed` on a pair run) and exits 1.

  On a pair run every repo's back ref `refs/gate-back/<key>` is deleted before any checkout, so no repo keeps an earlier run's back commit whatever step of this run fails; a run of one repo deletes it in its checkout step.

  The checkout is `checkout -q --force --detach <sha>`, the payload's commit and not `FETCH_HEAD`, so a later push of the key changes nothing a running run runs. The box's git calls for preflight and the probe share one start path (`preflight::Git::code`).

  The tool restore (`boxside/tools.rs`) runs `dotnet tool restore` in `{Work}\{Subfolder}` (`{Work}\{Leaf}\{Subfolder}` on a pair run) on a hidden console, in a job of its own named `{job}-tools`, created kill-on-close and never handed to dotnet, so the box's own handle keeps it: a box that dies or is killed takes the restore and everything it started with it, and a process the restore leaves running is terminated before the command starts.

  Its stdout and stderr are forwarded into the run's output line by line under the box's output lock, as the command's are, so they never split a heartbeat line. It has no time limit: a hung restore holds the work-folder lock, so every routed run of that worktree falls back `busy` meanwhile, and the run's stall watch does not see it, since the box heartbeat goes on outside the command (whether to cap it is GATE-159, which decides the package restore's cap too).

  A stop already set when the restore would start runs none: status 1, `gate: the gate failed while running the command: the job {job} was stopped before the command started`. A stop during the restore ends its job (dotnet and anything it started) and gives the same line naming `{job}-tools`.

  A client that closes the connection during it ends the job and the run exits 255 with `gate: remote the client closed the connection; stopping job {job}-tools`. None of these writes the record or starts the command.

  The manifests are found by walking from the place folder up to the work root, never above the main repo's tree (the work folder on a run of one repo, `Work\{Leaf}` on a pair run), `.config\dotnet-tools.json` before `dotnet-tools.json` in each folder; a walk that finds none looks for nothing and writes no record.

  The record `<Work>.tools`, beside the lock, is 64 lower-case hex digits (SHA-256 over, per manifest in walk order, the UTF-8 path relative to the work folder, a NUL, the file bytes, a NUL), UTF-8, no BOM, no newline; the restore is skipped while the record equals the hash of the manifests.

  A restore that exits, passing or failing, first prints `gate: dotnet tool restore took <s> s` (one decimal, timed from its start to its exit; nothing caps it yet, GATE-159), after its own output.

  A restore that cannot start prints `gate: remote could not start the dotnet tool restore: <message>`, and a non-zero one prints `gate: remote tool restore failed (status <n>)` then `gate-remote-refused: <nonce> tools` and `gate: remote work folder <key> could not restore its dotnet tools` (the tag and the line together, under the heartbeat's lock).

  Both exit 75 with the command never started, the record unchanged and the back ref left cleared, and the PC side reads the 75 plus the tag as fallback reason `tools`.

  A manifest that cannot be read prints `gate: remote could not read the tool manifest <path>: <error>`, one that does not lie under the work folder prints `gate: remote could not place the tool manifest <path> under the work folder <work>`, and a record that cannot be written prints `gate: remote could not write <record>: <error>`; the first and last exit 1.

  The record is read and written through a handle that refuses a link, a folder or a file with more than one name, as the lock's is; a read that fails for any reason but "not found" says `gate: remote could not read <record>: <error>; restoring the tools` and restores.

  Known limit, as in PowerShell: after the builder's NuGet cache is cleared, the `*.tools` records must be deleted by hand.

  The package restore (`boxside/packages.rs`) runs for a routed command whose first word's file stem is `pnpm`, in any case, after the tool restore and before the command. A run whose commands include more than one pnpm command (a warm-up's) installs once per distinct lockfile folder, in the commands' order, before the first command, and stops at the first install that fails. It looks for the folder to install in, for each command, from `--dir` or `-C`, read only among pnpm's own options before the first non-option word, else the place folder, and walks up from it to the work root (the main tree on a pair run) for the first `pnpm-lock.yaml`; a walk that finds none does nothing.

  It runs `pnpm install --frozen-lockfile` in the lockfile's folder in a job of its own named `{job}-packages`, created kill-on-close and never handed to pnpm, with the command's restricted token (the box's own is never the fallback) and the box's environment plus `CI=true` and git's `safe.directory=*`. Its output is forwarded as the tool restore's is, and it has no time limit (GATE-159).

  The record `{Work}.packages`, beside `{Work}.tools`, is UTF-8 lines `{folder}\t{64 lower-case hex digits}\n`, sorted, `.` for the root and `\` between folder names. The hash is SHA-256 over each of `pnpm-lock.yaml`, `package.json`, `pnpm-workspace.yaml` and `.npmrc` that exists in the folder, in that order: its name, a NUL, its bytes, a NUL. The install is skipped while the folder's line matches and `node_modules` there is a real folder.

  Before it installs, the box asks git, with its own token, `git -c core.hooksPath=<lock>\hooks -c core.fsmonitor=false --git-dir=<work>\.git --work-tree=<work> check-ignore -q <folder>/node_modules/`, and the install runs only when git ignores `node_modules` there.

  An install that exits, passing or failing, first prints `gate: pnpm install took <s> s` (` in <folder>` after it when the folder is not the root of the work folder), after its own output. One that cannot start prints `gate: remote could not start the pnpm install: <message>` (`cannot make its restricted token: <error>` when the token cannot be made); a non-zero one prints `gate: remote package restore failed (status <n>)` then `gate-remote-refused: <nonce> tools` and `gate: remote work folder <key> could not restore its pnpm packages`, and exits 75 with the command never started, as the tool restore's does.

  A stop already set, a stop during the install and a client that closes the connection give the tool restore's lines naming `{job}-packages`: `the job {job}-packages was stopped before the command started`, `gate: remote stopped job {job}-packages` and `gate: remote the client closed the connection; stopping job {job}-packages`. A process that keeps the install's output open after the job ended prints `gate: remote the package restore's output is still held open by a leftover process; stopped reading`.

  The install is skipped, with the command still run, when the folder pnpm starts in lies outside the work root (`gate: remote pnpm install skipped: <folder> is outside the work root`), when a folder from the work root down to the lockfile's is a link or a junction (`... <folder> is reached through a link`) or when git does not ignore `node_modules` there (`gate: remote pnpm install skipped: node_modules is not ignored in <folder>`); git that cannot say prints `gate: remote could not ask git whether node_modules is ignored in <folder>: <error | status n>`.

  A lockfile or package file that cannot be read prints `gate: remote could not read the lockfile <path>: <error>` or `gate: remote could not read the package file <path>: <error>`, and a record that cannot be written prints `gate: remote could not write <Work>.packages: <error>`; these exit 1. A record that cannot be read prints `gate: remote could not read <Work>.packages: <error>; installing the packages` and installs. Folders and paths in every line are escaped.

  A `Subfolder` is a folder of the main repo's tree, compared against its snapshot with per-character simple uppercase (the wrapper's OrdinalIgnoreCase): one that is not a folder inside `Work` prints `gate: remote subfolder <sub> is not a folder inside the work folder`, deletes the back ref and exits 2, and one missing from the snapshot prints `gate: remote subfolder <sub> does not exist in the snapshot` and exits 2, `<sub>` with its control characters escaped in both.

  The shared batch refusal (a `.cmd` or `.bat` word, [Command launching](#command-launching)) exits 2.

  The command runs outside the box's own job, created with `CREATE_BREAKAWAY_FROM_JOB` so it leaves the ssh session's job (the box's job refusing breakaway prints `gate: the box cannot start a command outside its own job: <message>` and exits 1). Its console is a donor's UTF-8 console (the console, below), else a hidden console of its own (`CREATE_NO_WINDOW`).

  It is assigned to the payload's named job, which has no limits and no kill-on-close, and inherits exactly NUL, the two pipe write ends and a query-only handle to its job, as copies made under the spawn lock ([Command launching](#command-launching)). The box's git and dotnet calls start through `gate_win32::spawn`, under the same lock.

  The command's token: it starts through `CreateProcessAsUserW` with a restricted copy of the box's token (`CreateRestrictedToken` with `LUA_TOKEN`), so it is not elevated, is lowered to Medium integrity when the box's is above it, has the user as default owner, and has a default DACL granting `GENERIC_ALL` to the user and SYSTEM.

  The ssh logon's NETWORK group (S-1-5-2) is made deny-only in it, so WMI does not treat the command's calls as remote and `taskkill`, `tasklist` and `Get-CimInstance` work from the command (with NETWORK enabled they fail with `ERROR: Access denied`). `gate_win32::NetworkGroup` (`Absent`, `Enabled`, `DenyOnly`, `Disabled`) and `TokenFacts::network` report the group's state.

  The console donor and keeper `cmd.exe` run with the same restricted token, so no elevated process shares the command's console. The box itself, its git calls and its tool restore keep the box's own token; the pnpm install runs with the command's restricted token, with no fallback to the box's own.

  The restriction makes the command's token non-elevated with NETWORK deny-only, but it is still a network logon's token and lacks INTERACTIVE, so it is not the token a local run's command has; it is not a boundary against the command, whose code is trusted, since the box's elevated git and tool restore later read what the command wrote.

  When the token cannot be made, or a start with it is refused and a start with the box's own token then works, the box prints `gate: the command runs with the builder session's own token: <error>` on stderr, after the start marker, and the command (with the donor and keeper) runs with the box's own token. A start refused with both tokens says only `could not start`.

  The command's environment gains `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_<n>=safe.directory` and `GIT_CONFIG_VALUE_<n>=*`, where n is the inherited count (0 when there is none) and the count grows by one.

  The inherited count is read as git reads it (leading whitespace, an optional `+`, decimal digits to the end, at most `i32::MAX`; an empty value is 0); a count git would refuse, or `i32::MAX` itself, leaves the environment unchanged. The work folders are owned by Administrators, which git refuses from a non-elevated token.

  The command's environment is the box's own with the payload's `Env` pairs set over it, each replacing any inherited value; the settings below are applied after the pairs, so none of them can be undone by one.

  The command's environment also sets `Logging__EventLog__LogLevel__Default=None`, replacing any inherited value, so a .NET host with default logging writes nothing to the Windows Event Log; its console output is unchanged. Of the groups the Application log's default access lets write, a restricted copy of the box's token could hold only Administrators, INTERACTIVE, SERVICE, BATCH and WRITE_RESTRICTED, and the command's token holds none of them.

  On a builder without the setup grant below (a fresh or reimaged one), the host's default EventLog provider would throw `Access is denied` on its first Warning; the variable keeps such a host working wherever the grant is missing.

  Known cost: deny entries naming NETWORK still apply to the command, but allow entries naming NETWORK no longer grant, so a resource whose DACL grants NETWORK alone is open to a command run directly over ssh and closed to the same command routed.

  Known cost: the first non-elevated build in each work folder starts fresh MSBuild nodes and compiler servers, since their handshake includes elevation. A command that creates symlinks needs Developer Mode on the builder, as on the PC; builder setup turns it on (`AllowDevelopmentWithoutDevLicense` = 1).

  Known cost: the variable does not cover a command that writes to the Event Log by another route (`EventLog.WriteEntry`, a logging library's own sink). Nor does it cover a host whose own configuration sets the EventLog provider's level: a category rule (`Logging:EventLog:LogLevel:Microsoft`), which wins over the default, or a configuration source added after environment variables, or a bare `HostBuilder` that never reads them.

  Builder setup therefore grants the builder user write access to the Application log: one allow entry for the user's SID, write access 0x2, added to the channel's access SDDL with `wevtutil sl Application /ca:` from an elevated session. That covers every writer to that log under a registered source.

  Any routed command, and anything else run as the builder user, can then write entries under any registered source, so the builder's Application log is not evidence of what the system did. Registering a new event source and writing to any other log still need Administrators, so they fail on a routed run.

  The stop event `<job>-stopped` ([Kernel object names](#kernel-object-names)) is a manual-reset event the run creates, unset, right after it takes the work-folder lock, before the fetch and checkout; one that cannot be created prints `gate: the gate failed while running the command: cannot create the event <job>-stopped: <error>` and exits 1.

  `gate box stop` sets it whether or not the job exists yet (a missing event is silent), then terminates the job when there is one. The run looks at it once just before it creates the job (set: the stop line and exit 1, nothing started), and again after the command is assigned to the job and before it is resumed, on every launch.

  Set means a `stop` came first: the command is ended unstarted, the job is terminated, and the run prints `gate: the gate failed while running the command: the job <name> was stopped before the command started` and exits 1. An event that cannot be read prints `gate: remote cannot read the stop signal of job <name>: <error>; starting the command`, and the command starts.

  A job name already held prints `gate: the job <name> already exists; a run's job name must be new` and exits 1. A command that cannot start prints `gate: could not start <word>[ (<resolved>)]: <message>` and exits 127, the word and the resolved path with their control characters escaped; a failure of the gate after the start prints `gate: the gate failed while running the command: <problem>` and exits 1. If the box panics after the resume, the job is terminated with 124.

  Output is forwarded from the pipes until both end. What the command left in its job is ended first (below), the console's donor excepted, so a leftover there holds no pipe open and costs no drain.

  A process the run's end spares, such as a live nested gate, can still hold a pipe open after the command ended: the box then stops reading after a 2 s drain and says `gate: remote command's output is still held open by a leftover process; stopped reading`. A pipe that cannot be read prints `gate: remote could not read the command's <stdout|stderr>: <error>`.

  Once the command has exited, before the readers drain and before the outputs are listed, the box ends what the command left in its job, sparing live gates and console hosts as the local run end does, and the console's donor (a spared gate may still be using it; the box ends it itself once the wait for the command is over), with the passes the local run end uses (`end_leftovers`), under the state root a `gate run` with no `--state-dir` resolves.

  A failure says `gate: cannot end the run's job: <problem>` on stderr, the problem worded as the local run end words it (the pass cap's `<n> of its processes still ran after 5 passes; first: <first problem>` included); a state root that cannot be resolved says the fixed line `gate: cannot end the run's job: the box's state root cannot be resolved; ending the whole job` (no path) and the whole job is terminated. The run's status stands either way.

  A failed write by a forwarder to the box's stdout or stderr means the PC's ssh connection closed, and a closed connection always stops the job, whether it is noticed while the command runs or during the final drain. The box terminates the job with 124 and polls its processes every 100 ms for up to 10 s. It then drains the readers, skips the sync-back, releases the work-folder lock and exits 255.

  On stderr, best effort, it says `gate: remote the client closed the connection; stopped job <name>` once the job is empty. Otherwise it says `gate: remote the client closed the connection; stopping job <name>` followed by the could-not-stop line or the `still has <n> process(es)` line of `stop`. The stdin closing is not the signal. A command that itself exits 255 is indistinguishable from a lost connection, as with the PowerShell wrapper.

  After the command, the sync-back commits the tree when it changed: a copy of the index at `<Work>.index` is staged, and the commit (message `gate-remote back`, parent HEAD) is written to `refs/gate-back/<key>` in `Bare`. A sync-back that fails prints `gate: remote sync-back failed: <message>` and turns a 0 status into 1; an index copy that cannot be removed prints `gate: remote could not remove <index>: <error>` and changes no status.

  On a pair run the back refs are all or nothing across the repos: every repo's tree is committed first, the main repo's first (the one `Index` file serves each in turn), then every changed repo's commit is pushed to `refs/gate-back/<key>` in its own bare repository. A push that fails deletes the back ref of every repo pushed so far and the failing repo's own, each deletion tried and a failed one said, and the run fails as a sync-back failure.

  A client that closed the connection during that undo ends the run with 255 instead. After all pushes, a pair run says under the output lock `gate: remote back commits: <name> <sha|none>, ...`, the main repo first and `none` for a repo whose tree is unchanged. The outputs step packs the main repo's tree only.

  A pair run that does not push every back commit says `gate: remote back commits: failed` instead, under the output lock so it is the last such line and a line the command printed itself never stands for it: after a failed lock, stop event, back ref clear, layout step, checkout, subfolder check or tools restore, a command that cannot be waited for or that the ceiling killed, and a failed commit or push (after the undo above).

  A failed wait ends the job and drains its output first, so no line of the command follows. A refusal (`busy`, `layout`, `tools`) says no such line, and a run of one repo never does. The PC applies nothing of a `failed` pair.

  A payload with `Outputs` globs adds these steps (GATE-44):

  - Once the heartbeat beats and before the checkout, `run` reads the PC's manifest from stdin, the line after the request, up to its newline (at most 64 MiB; a manifest that cannot be read is said only once the command has passed), then removes the old archives from `OutputFolder`: each file named `<key>-<32 lower-case hex digits>.tar.zst`, whatever the key, last written more than a day ago, and no other file.

    An archive already gone counts as removed; one that cannot be removed, or a folder that cannot be listed, is said (`gate: remote could not remove the old outputs archive <path>`, `gate: remote could not list the old outputs archives in <folder>`) and fails nothing.
  - Just before the command, unless `OutputsFull`, it lists the files under the globs (size and last write); a listing that fails is said (`gate: remote could not list the outputs before the command: <error>`) and leaves every file counting as changed.
  - After a command that exited 0 and its back commit, with the commit pushed, it reads the commit's paths (`git diff-tree`) and packs the files to bring back into `<OutputFolder>\<key>-<nonce>.tar.zst`, created new.

    It says `gate-remote-outputs: <n> <archive name>` on stderr (`<n>` counts every path the archive lists, copies included), or `gate-remote-outputs: 0` with no archive when nothing is to come back. The paths are read only after the push, so failing to read them fails the outputs and never the sync-back.
  - A file the pack would take but whose name does not fit the archive (not plain, the list name or not Unicode), or that sits below a folder whose name is not plain, is skipped, and the box says `gate: remote outputs: skipped <n> file(s) with names this PC cannot take: <first path>` (`, …` after the path when n is above 1) on stderr once, just before its `gate-remote-outputs:` line; the run keeps its status, and the client echoes the line like any box line.
  - Right after a pack that wrote an archive, the box trims `OutputFolder` to at most 50 GiB of outputs archives (files named as in the first step; other files are neither counted nor removed). It removes the archives last written strictly before the run's own, oldest first (by path among equals), until the total fits; an archive written with the run's own or after it is never removed, so the folder may stay over the budget until the newer run's own trim.
  - The run's own archive is opened sharing reads only, so another run's trim cannot remove it mid-write; such a removal fails, is said as above (`gate: remote could not remove the old outputs archive <path>: <error>`) and that trim goes on to the next oldest. An entry whose metadata cannot be read is said (`gate: remote could not read the outputs archive <path>: <error>`, control characters escaped), left out of the total, and the trim goes on.
  - A run whose own archive alone is larger than the budget removes only its own archive, leaves the others, and says `gate: remote outputs not brought back: the archive is <X.X> GiB, more than the box keeps (50 GiB)`. A run whose own archive is gone before its trim says `gate: remote outputs not brought back: the archive was removed before it could be sent`. Either turns a passed 0 into 1.
  - A pack that fails removes its half-written archive, says `gate: remote outputs not brought back: <reason>` and turns the passed 0 into 1. A command that did not exit 0, or a failed sync-back, packs nothing.
- **`stop`** sets the job's stop event, then terminates the payload's job, taking no lock. It terminates the job again on every 100 ms poll of the job's process ids, until the job is empty or 10 s pass, so a process that joins late is ended; its processes end with 124.

  A terminate after the first that fails is not a failure of the stop: it is said once, `gate: remote could not terminate job <name> again: <error>`, after the other lines. An event that cannot be set prints `gate: remote could not signal the stop of job <name>: <error>` on stderr, and the job is terminated all the same. A stopped job prints `gate: remote stopped job <name>` on stdout and exits 0; no such job exits 0 with no output.

  The stop also ends the run's tool restore job `<name>-tools` and its pnpm install job `<name>-packages` when they exist: it opens each before it sets the event and terminates it after the payload's job, with the same lines and rules for `<name>-tools` and `<name>-packages`. It exits 1 when any job is left with processes or cannot be stopped, else 0.

  Processes still alive 10 s after the termination print `gate: remote job <name> still has <n> process(es) 10 s after it was terminated` and exit 1, and a job that cannot be terminated prints `gate: remote could not stop job <name>: <error>` and exits 1.

- **`outputs`** sends the archive the payload's `Name` names in its `OutputFolder` raw on stdout, then removes it. It takes no work-folder lock, so it never waits on a run.

  A payload without `Name`, `Work` or `OutputFolder`, or with a `Work` or `OutputFolder` that is not the work folder's own, exits 2.

  A `Name` that is not `<Key>-<32 lower-case hex digits>.tar.zst` for the payload's own key (`gate: remote outputs archive name '<name>' is not an outputs archive of this work folder`), an archive that does not exist (`gate: remote outputs archive <name> does not exist`) or one that cannot be read or sent exits 1. The file is opened without following a link, and only a plain file is sent.

  A sent archive that cannot be removed prints `gate: remote could not remove the outputs archive <name>: <error>` and still exits 0; a later run's prune takes it. An archive already gone at its removal (another run's trim took it after it was sent) says nothing and exits 0.

- **`inflight`** counts the routed runs holding a lock in the payload's `Root`, for the cutover's drain. It reads only `Root`; a trailing separator is folded off.

  For each `{Root}\work\*.lock` (any case) it opens the file for reading with read, write and delete sharing. A sharing violation means a routed run holds it, since the run and the PowerShell wrapper open the lock share-none. The probe's handle closes at once, so a run that starts after the probe can take the lock, while a run whose open lands during the probe is refused busy (exit 75) and its PC falls back locally.

  It prints `gate: remote in flight <n>`, then `gate: remote in flight <key>` for each held lock, the keys escaped and sorted, and exits 0. A missing `Root` or work folder counts 0. A lock it cannot open for another reason counts as held and reads `gate: remote in flight <key> (cannot read: <error>)`.

  A work folder it cannot list (anything but not found) prints no count line, `gate: remote cannot list <work>: <error>` on stderr, and exits 1. A refused payload exits 2.

- **`inputs`** (GATE-22, `boxside/inputs.rs`) installs a routed run's ignored inputs into the work folder in two calls, the payload's `Step` being `diff` or `put`. The payload holds `Key`, `Bare`, `Work`, `Lock`, `Nonce` (32 lower-case hex digits) and `Step`; a payload with a `Pair` is refused with `gate box: inputs for a sibling pair are not taken yet` and exit 2, and any other refused payload exits 2.

  Stdin is the request line, then the PC's inputs manifest as one line (the outputs manifest's format, at most 64 MiB, its paths plain, relative and not `.git*`, `.lfsconfig` or `.env`/`*.env`), then for `put` the archive of exactly the files the diff named, in the outputs archive's format, to the end of stdin.

  A manifest that cannot be read fails the step: `gate: remote inputs <diff|put> failed: <why>` and exit 1, before any lock.

  Both steps take the work folder's lock as `run` does (a held lock prints `gate-remote-refused: <nonce> busy` and the busy line, exit 75) and lay the work folder out as `run` does, so a later run finds the layout unchanged and clears nothing.

  The diff prints `gate-remote-inputs: need <n>` then `gate-remote-inputs: path <path>` per file (escaped, manifest order) whose copy is missing or differs; a diff that needs nothing finishes the install as a put of nothing would. The put prints `gate-remote-inputs: installed <n>`.

  The record `<Work>.inputs`, beside `.lock`, `.tools` and `.layout`, is one JSON line: the digest of the manifest line last installed from (empty while an install is under way or after one that did not finish), and per installed file its path, size, SHA-256, and the last-write time and file id the box left it with. A diff trusts a copy whose size, last-write time and file id match its entry and hashes any other.

  A copy a put left in place because it changed on the box, or could not be opened, hashed or deleted, is a kept entry, `{"kept": true}`: listed in the exclude file, never trusted as installed, never swept, and dropped when its file is gone or a manifest names its path again.

  A record that is not a record is taken as empty (`gate: remote inputs record <path> <why>; every copy is hashed`); one that cannot be read (a link, a folder, more than one name) is removed as itself (`gate: remote removed the inputs record <path>, which cannot be read: <error>; every copy is hashed`).

  The record is written without its digest before anything below `Work` changes and with it last, only once `Work` holds every file of the manifest, so no run passes the digest check halfway through an install.

  The put deletes a recorded copy that left the manifest only while it is a plain file that still hashes to the record's value (`gate: remote kept the stale input <path>: <why>` or `gate: remote could not remove the stale input <path>: <error>` when it cannot); it deletes nothing else.

  It then rewrites `<Work>\.git\info\exclude` from the record, one anchored literal line per installed or kept path, so `clean -fdq` keeps the inputs and the box's back commit never picks one up, whichever ignore rule made them ignored on the PC; a user edit of that file is overwritten at the next install.

  A folder found at the exclude file's name is moved aside to `exclude.gate-moved-<n>` (`n` from 1 to 100, never replacing an entry) and never walked, with the line `gate: remote moved aside the folder <old> to <new>`.

  Every file and folder the box makes, moves or deletes below `Work` is reached through handles opened one name at a time beneath the held work folder (`gate_win32::open_beneath_as`), never through a link, and acted on through the handle it was checked by.

  A member's target must be a plain file of one name, or missing; a folder on its way that is a link, a junction or a file refuses the put (`cannot install '<path>': ...`). Each file is moved in by `rename_beneath` and given the DACL and label its folder hands down (`inherit_security`), in place of the stage's.

  The put unpacks the archive before it takes any lock, into a stage `<root>\inputs-stage\<key>-<nonce>` of an elevated box, `<root>\inputs-stage-medium\<key>-<nonce>` of an unelevated one (`<root>` being the folder above `work`); neither box touches the other's folder.

  The stages folder and the stage are made with a protected owner (Administrators when elevated, the box's user when not), a protected DACL granting full control to SYSTEM, Administrators and the box's user alone, and a mandatory label of the box's integrity level with no write up, all in the creation call, so they are never seen with any other security (`gate_win32::protected_descriptor`).

  A stages folder that already exists is adopted only when it is a folder of its own whose owner, DACL and label are exactly those (`is_protected_folder`), read through the handle held; anything else at its name, one the box cannot open included, refuses the put with `cannot use <path>: another account made it; remove it, or give it to Administrators` and is never walked, emptied or replaced. A stage of the put's own name already there is refused.

  The root, the stages folder and the stage are held without delete sharing for the whole put, and the stage is removed beneath its handle on every way out; a stage older than a day (any work folder's) is removed by the next put, a stage that cannot be opened or removed says so and stays (`gate: remote kept the stage <path>: <why>`, `gate: remote could not remove the old stage <path>: <error>`).

  Caps: an archive of more than 50,000 members, or that decompresses to more than 4 GiB (`--test-inputs-budget-bytes` replaces it), is refused as the outputs archive's are, and the put fails (`gate: remote inputs put failed: <why>`, exit 1). The step is not watched on the box; the PC watches both calls ([The remote client's run](#routing), Inputs).

  Deployment prerequisites: the folders above `<root>` are writable by Administrators only; no account outside SYSTEM, Administrators and the builder's user may write in `<root>`; and `<root>\work` is not writable by other accounts. The stage's protection and the beneath-handle walk assume it; the box checks the stages folder it adopts, not the folders above it.

**The outputs module** (GATE-44) is shared by both ends, this exe, in `crates/gate/src/outputs/`. The box's `run` packs with it and the box's `outputs` verb sends the archive. The PC's manifest, the pending marker, the reading of the box's `gate-remote-outputs: ` line, the fetch of the archive and its install are built in the Rust remote client ([The remote client's run](#routing)).

- Globs: each is relative to the work folder's root and `/`-separated, matched without regard to case: `**/` is any number of folders (none included), `**` anything, `*` any run of characters within one name, `?` one character within one name.

  Never brought back, whatever the globs: anything under a `.git`, `.tmp` or `TestResults` folder at any depth, a `.env` file (`.env`, `.env.<name>`, `<name>.env`), any path component starting `.git` (`.git`, `.github`, `.gitattributes`) and a `.lfsconfig` at any depth, spelled any way the sync-back's names check reads (`\` as a separator, a cut at the first `:`, trailing dots and spaces stripped, 8.3 aliases such as `GITATT~1` and `LFSCON~1`).

  Nor is a path that is not plain (the member rule below), the archive's own list `.gate-outputs.json` at the root in any letter case, or a name that is not Unicode, each of which the box's pack skips and says.
- Walk: lists the files below a folder that the globs match, ordered by relative path, and does not enter an excluded folder, a `.git*` folder included, or a folder whose name is not plain or not Unicode, however deep.

  The box's pack also takes from it the files the globs would select but for their names, those below such a folder included, to say them.

  A link or a junction, to a file or to a folder, is neither followed nor listed. A folder that cannot be read, or an entry whose metadata cannot be, fails the walk; a name that is not Unicode does not.
- Manifest: the PC's list of its matching files, sent as one line of JSON and a newline, `{"files":{"<path>":{"size":<bytes>,"modified":<file time>,"sha256":"<hex>"}}}`, paths relative to the worktree root and found without regard to case.

  The box's pack takes each file the manifest lacks, and each it holds that the run created or changed (the box's stat listing from just before the command lacks it or holds another size or last write) whose SHA-256 differs from the manifest's, skipping the paths of the run's back commit. Under `OutputsFull` there is no listing, so every file counts as changed and each that differs from the PC's comes back.
- Archive: a tar stream compressed with zstd (level 3, long-distance matching, a window of log2 = clamp(ceil(log2(total bytes of the chosen files)), 20, 27), so a small archive uses a small window; the reader accepts a window up to 2^27), named `<key>-<nonce>.tar.zst`, each file read once and hashed as it is packed.

  The last member, `.gate-outputs.json`, is a JSON array with one item per chosen path in pack order, `{"path","size","sha256"}` for a member and `{"path","size","sha256","copy":true,"modified":<seconds since 1970>}` for a copy, and the reader checks the members against it after extraction. Both ends stream; memory does not grow with a file's size.

  A **copy** is an item carrying `"copy":true` and having no tar member: the tar holds only the first path of each distinct content that the PC does not already hold, and every later path with that content is listed as a copy.

  The PC holds a content when a file of the manifest sent with the run has its SHA-256 and is outside the paths of the run's back commit; the box never uses a back-commit path as a copy source. A copy carries the box file's own last write as `modified`, not its source's, and the stage clamps it as it does a member's.

  Before the pack the box hashes only the chosen files whose size matches another chosen file's or a manifest-held file's, since only those can be copies. Copy or member is decided at the file's turn: a file hashed before the pack becomes a copy only when its size and last write still match the walk and its hash matches an earlier member's, or a manifest hash outside the back commit; otherwise it is packed as a member, with the hash from that read.
- Payload: `Nonce` is 32 lower-case hex digits, `OutputFolder` is `<root>\outputs` for the work folder `<root>\work\<key>`, `Outputs` a list of non-empty globs holding no `[` or `]`, `OutputsFull` true or false, and `Name` an archive name of the payload's own key and nonce, so that the outputs verb reaches no file in `OutputFolder` but an archive of this work folder. A refusal is a `gate: box payload ...` line.

`unpack` creates its destination, which must not exist, and refuses an archive for any of these, a member before anything of it is written:

- A member name that is not a plain relative path inside the destination: `.git` in any case, an 8.3 short name (`~` before a digit), a part ending in `.` or a space, any of `<>"|?*:` or `\`, a control or bidirectional character, a device name (`CON`, `PRN`, `AUX`, `NUL`, `CONIN$`, `CONOUT$`, `COM0` to `COM9`, `LPT0` to `LPT9`, also with an extension), or more than 64 parts.

  The rule is `is_plain_path` in `outputs/names.rs`. The globs and the walk apply it too, so the box never packs such a name; the pack still refuses one (`the output '<path>' has a name the outputs archive's reader refuses`), a backstop the walk leaves unreachable.
- A GNU long name over 4096 bytes, a listed path of more than 4096 bytes (`the outputs archive lists a path of more than 4096 bytes: '<first 200 chars>…'`), and any pax, `g`, `K` or `S` entry.
- More than 50,000 files or folders.
- A member over the bytes left of the caller's limit (and over 16 GiB). Each copy counts against that limit by its listed size, and in the 50,000-file limit above.
- Input as sent past the limit plus limit/64 plus 1 MiB, zstd's worst case for the limit, or a zstd window over 2^27.
- A last write in base-256 form, before 1970 or more than a day ahead of the machine's clock; a copy's `modified` is held to the same day (`the outputs archive copy '<path>' has a last write <reason>`).
- A member that would be written through a link or a junction, or a member that is itself not a plain file.
- A member that differs from the list in size or SHA-256 (a copy's size refusal says `copy '<path>'`, not `member`), one the list lacks, or a listed file the archive lacks.
- A list that is not a list of paths, sizes and SHA-256s (`the outputs archive's list is not a list of paths, sizes and SHA-256s`): a copy without `modified`, or a member with `copy` or `modified`.
- A path listed twice, the same path ignoring case across members and copies (`the outputs archive lists '<path>' twice`).
- A copy the list cannot support: one whose content no member before it has and no file of the caller's manifest holds (`the outputs archive lists '<path>' as a copy of <n> bytes with SHA-256 <hex>, which no member before it and no file the PC holds has`), or one named as the list itself (`the outputs archive lists a copy named as its own list, <path>`).

After the members are extracted and checked, `unpack` makes each copy in the destination, from the first earlier member with that content, else from the first manifest path whose file below the caller's root still hashes to it (its stamp is not checked).

A copy whose bytes differ from its item is refused (`the outputs archive copy '<path>' does not match its list: ...`), and so is one no PC file still holds (`no PC file still holds the content of <path>, SHA-256 <hex>: tried …`, as above); these two are faults of the transfer or the PC, not of the members, so a refetch may clear them. The caller's `go_on` is asked before each copy, so a Ctrl+C is honoured between a copy's 64 KiB buffers.

`pack` fails on a link or a junction, a file last written before 1970, and a file whose size or last write changes while it is packed. On any refusal `unpack` removes the folder it made and all it extracted there; when that removal fails too, the refusal says so.

The console: `gate box run` gives the command a UTF-8 console through a donor console, as the PowerShell wrapper does. Right after it creates the run's job, the box starts a hidden donor, the system's `cmd.exe /d /q /k` (never one found on PATH), in the job. The donor holds a stdin pipe nobody writes so `cmd /k` does not exit, and lives for the whole run, until the wait for the command is over.

To create the command the box starts a keeper `cmd.exe` on its own console, so that console stays alive, then takes the output lock, waiting at most 10 s. It leaves its console, attaches to the donor's and sets the input and output code pages to 65001.

It creates the command suspended in the job, without `CREATE_NO_WINDOW`, so the command inherits the donor's console, then checks the stop event and resumes it. It then returns to its own console, the keeper's (`AttachConsole(ATTACH_PARENT_PROCESS)` as the last resort), ends the keeper, and puts back its standard handles that were not console handles.

Before the command starts, a donor that cannot start, an attach or code page that fails, or an output lock not had in 10 s (reason `the box's output is blocked`) does not fail the run: the command runs on the plain hidden console and the box says `gate: remote could not give the command a UTF-8 console (<reason>); running it on the box's default code page`. The line comes after the launch, and only when the launch was not stopped.

There is no fallback after the command has started and no relaunch: a command that never shows on the console (a GUI program, one that calls `FreeConsole`, a slow starter) runs once.

The rest of the protocol, the transport of snapshots and outputs, is the remote port's design (GATE-10).

## Install

- `pwsh tools/install.ps1` builds a Release `gate.exe` with `cargo build --release -p gate` (default features, so no `test-seams`) through the repo's `tools\gate.ps1` on a heavy slot, and installs it to `%LOCALAPPDATA%\gate\bin\gate.exe`. Installed gates never run from `target/`, which build cleaners empty and a rebuild overwrites while gates hold it open. It refuses an unset or empty `LOCALAPPDATA`.
- When the built exe has the same SHA-256 as the installed one, the copy is skipped and the script prints `install: gate <version> already installed at <path>`.
- Otherwise the new exe is first copied to a temporary name in the folder; then a copy already there, running or not, is moved aside to `gate.exe.<stamp>.<pid>.old` (which works while gates run it), and the temporary file is renamed into place, so `gate.exe` is missing only between two renames.

  A failed copy leaves the installed exe untouched; a failed final rename moves the old exe back, and the failure line says whether that worked. The script prints the version it replaced and the version it installed.
- The script holds `push.lock` in the install folder, open and unshared, from the confirmed Install step to its end, the same lock `gate push` takes on a builder and on this PC ([`gate push`](#gate-push)). While a push holds it the script stops with `install: another gate push is installing; nothing was changed` and exit 1.

  The script needs pwsh 7.2 or later (`#Requires -Version 7.2`); `install: could not prepare the install: <message>` says its small C# type for the file checks could not be added.

  A `push.lock` that is a link, a folder, a junction or a second name of another file gives `install: <dir>\push.lock is a link; nothing was changed`, a folder it cannot make `install: could not create <dir>: <message>`, and any other refusal `install: could not lock <dir>: <message>`. Declined (`-Confirm` "No") and `-WhatIf` create no folder and no lock.
- The installed copy is verified when it answers `--version` with `gate <version>` (the script's version reads are bounded at 30 s).

  A copy that does not answer is taken out and the previous gate.exe put back: the script fails with `install: the installed gate.exe does not run; the previous gate.exe is back in place` (`; the previous gate.exe is at <aside>` when the move back fails, `; the copy was removed and no earlier gate.exe existed` on a first install, `; the copy at <path> could not be removed: <message>` when that fails).
- The move into place and the move back run without force over a missing gate.exe and act only while gate.exe is missing or the install's own file (the temp copy's file id, read from the handle that wrote it; a file gone after the last id read counts as missing); one another process placed is left as it is, with `; another gate.exe was placed meanwhile and was left as it is` and, on a replacing install, `, the previous gate.exe is at <aside>`.
- Aside copies no process holds are deleted only once the installed gate is verified (also when the exe is already installed and answers), but this install's own, which stays as the last good gate; one a running gate still holds stays until a later install. Every aside copy is kept, silently, when the install ends unverified. One that cannot be deleted prints `install: could not delete <path>: <message>` on stderr.
- It adds `%LOCALAPPDATA%\gate\bin` to the user's PATH when missing, so callers run `gate`: the raw registry value of the user environment is read and written, keeping its kind and its unexpanded `%NAME%` entries; the change is broadcast (`WM_SETTINGCHANGE`, as `setx` does) so programs started from then on see it, and the script says to open a new terminal. It never touches the state root's files.
- `-WhatIf` lists every step a real run would take without taking it, the aside deletes included.

  For the exe already in place it prints `install: gate <version> already installed at <path>` and a Delete line for each aside copy a real run would delete, when the installed gate answers `--version`. `-Exe` (install a prebuilt exe, skipping the build) and `-EnvironmentKey` (the registry key the PATH entry lives in) exist for the suite, which runs the script against a temporary `LOCALAPPDATA` and registry key.
- Every install deletes a leftover `remote-bridge.ps1` file in the install folder, which no gate starts. When it cannot, it prints `install: cannot remove the old remote-bridge.ps1 (<reason>); it is unused` on stderr and carries on with exit 0; a folder at that name is left alone.
- The script installs locally only. `gate push` copies the installed exe to each builder in `remote.json` (`host`, then `hosts`), with the same rename-aside on the builder, so a machine with builders runs `pwsh tools/install.ps1` and then `gate push` ([`gate push`](#gate-push)). It pushes the installed exe, never one from `target/`, and never installs here.
- Install is a script, not a `gate install` subcommand.

### Build id

`gate --version` prints `gate <package version>+<build id>`, and the box preflight's `gate: remote version` line says the same text. The id is the 12-hex head commit, or `<12-hex>-dirty.<8-hex>` for a build of changed tracked files, the 8 hex being the first 8 of the SHA-256 of `git diff HEAD --binary`. Untracked files do not count.

`tools/install.ps1` works the id out and sets `GATE_BUILD_ID` for its release build only, running git with `--no-optional-locks`. An empty id removes any `GATE_BUILD_ID` the caller had set, so the build never takes an outer one. The build runs with the launcher's `-Local`, so it is never routed to a builder.

A build with no id (not made by the install script, or git failing) says `+dev`, and compares equal only to another `+dev`. When git cannot say, the script prints `install: git could not say which commit this is or what changed in it; building a gate whose version ends +dev`.

The remote client compares the builder's whole version text with its own ([Routing](#routing)), so another build of one package version falls back with reason `version`; `gate push` still decides "already installed" by the exe's SHA-256, and its lines name the whole version text.

## Cutover

The PowerShell gate and `gate.exe` share slot mutexes and never share files.

The default root's mutex names are the PowerShell gate's, so a PowerShell gate still running and a new `gate.exe` gate take turns on one set of slots and the machine is never over-subscribed; each generation writes its claims, tickets and journal in its own folder and format, and once the drain is over ops-tower reads `gate.exe`'s, plus the PowerShell claims of a slot whose mutex is open. Moving a machine to `gate.exe` is a drained cutover:

1. Before it runs, ops-tower's Gates tab already writes the default root's `slot-counts.json` (`%LOCALAPPDATA%\gate\v1\`) on every edit, through `gate-state`'s writer, since `gate.exe` never reads the PowerShell gate's `%LOCALAPPDATA%\gate\slot-counts.json` ([Counts file](#counts-file)).

   `gate cutover` copies the PowerShell gate's slot counts into the default root's `slot-counts.json`, so both generations count the same slots, then writes `%LOCALAPPDATA%\gate\cutover.flag`, holding its write time in RFC 3339 UTC (the launcher never reads it). The PowerShell launcher in each repo reads the flag, and from then on every new gate runs `gate.exe` through the compatibility layer.
2. It then waits until every PowerShell gate's claims and tickets in `%LOCALAPPDATA%\gate\` are gone or stale, naming on stderr what it still waits on, and exits 0 once none is left. The drain of the PowerShell gates (`powershell_gates`) reads claims and tickets only and probes no mutex; a pid whose start cannot be read counts as live.

   Under the default root a slot a live PowerShell claim holds counts once.

   It probes slots `0..remote_count` and every index a v1 claim names, reading tickets, then claims, then mutexes, and a process whose start cannot be read counts as live.

   What it cannot read (a remote-slot claim, a ticket of any kind, an out-of-range index or arrival, a `slots` or `waiting` folder that exists but cannot be listed) it reports by the root's real path in `unreadable`; a v1 ticket whose open is refused is going away, not unreadable.

   It ends only when every scan across a 5 s confirmation window finds no gate, because the PowerShell gate deletes its ticket when it takes a slot and writes its claim up to about 2 s later, and a single empty scan could fall in that gap. The lines are in [`gate cutover`](#gate-cutover).

   It ends by printing the flag's path and saying that deleting it goes back to the PowerShell gate: that is the take-back, by hand, so it works even when `gate.exe` is what is broken. A `gate cutover` stopped part way is run again: the rerun rewrites the flag and goes back to waiting, and it copies the slot counts only when the default root's `slot-counts.json` is absent, so a Gates-tab edit made since is kept.

   The copy never replaces: the file is written beside the target and hard-linked into place, so one that appears meanwhile wins, and a target that exists but cannot be read is kept too.

   Callers that start the PowerShell gate without the launcher keep starting PowerShell gates after the flag is set. The drain names on stderr each PowerShell gate whose earliest claim or ticket `since` is later than the flag's write time (a late arrival), with its command line, so a bypassing caller is visible rather than holding the drain open silently; each pid is named once. The cutover is not run until claude-config lists the callers it moved.

### The launcher's hand-over

The launcher is claude-config's `tools/gate/gate-launcher.ps1`, which `sync-launchers.ps1` copies into each repo as `tools/gate.ps1`; the hand-over is built there, and this repo's black-box cases run the synced copy.

- The flag is `%LOCALAPPDATA%\gate\cutover.flag`. Its existence is the switch; its content is not read. `--state-dir` and `GATE_STATE_DIR` do not move it.
- With the flag present, the launcher runs `%LOCALAPPDATA%\gate\bin\gate.exe` with the caller's words, waits for it, and exits with its whole status. It says nothing of its own: gate.exe's lines are the whole output. A Ctrl+C reaches both; the launcher waits for gate.exe to end and exits with its status.
- The words go through as the caller gave them, `-StopTree` included (the compatibility layer parses it). An in-session call whose `--` PowerShell's parser ate gets it put back before the first command word, which the launcher already finds (`Read-Argument`).
- With the flag present and `gate.exe` missing, nothing runs: the launcher says `gate: the cutover flag is set but {exe} is missing; reinstall with tools/install.ps1` on stderr and exits 127.
- With the flag absent, the launcher runs the PowerShell gate as before.

Callers that start the PowerShell gate without the launcher (inventory section 4) are moved by the caller sweep.

During the drain a PowerShell gate neither reads `gate.exe`'s tickets nor signals its wake events: a PowerShell critical request can be overtaken by `gate.exe` gates that arrive after it (it still waits for every slot), and a `gate.exe` waiter learns of a PowerShell gate's release from its fallback poll.

ops-tower follows the flag: it reads the PowerShell gate's files until the flag exists and then `v1\`, plus the PowerShell claims of a slot whose mutex is open, and its waiters come from `gate.exe`'s tickets ([Common rules](#common-rules)). The hooks and scripts that read gate command lines or output (inventory section 4) change in the same drain.

The caller sweep deletes each repo's `tools/gate.ps1` in the same change that switches that repo's docs and scripts to `gate run`; no hand-over shim is kept. Callers write `gate run --log <path> --timeout <s> --slot <kind> -- <command>` (`--stall`, `--tail`, `--no-markers`, `--local`; `gate stop-tree <pid>`), never `gate.exe` or a path. `tools/install.ps1` builds with a plain `cargo build`, not through a gate, so a broken installed gate can always be replaced.

After the caller sweep there is no fallback: callers run `gate ...` on PATH, a machine without it gets "command not found", and each repo's docs name the install script. Before the sweep, the PowerShell launcher's own fallback for a machine without the gate (H2) is the launcher's, not `gate.exe`'s.

The compatibility layer is deleted once no caller uses `-Log`, `-TimeoutSeconds` or `-Slot`. The guard launcher stays: `sync-launchers.ps1` publishes both launchers.

## Crate layout

A virtual Cargo workspace (`resolver = "3"`) with every crate under `crates/`, listed by path. The root copies ops-tower's `[workspace.lints.clippy]` table, its `[profile.test-release]` and its `rustfmt.toml`, so moved code lints and formats as it did.

| Crate | Owns | Builds on Linux |
|---|---|---|
| `gate-state` | Every file format in the state root (claim, ticket, counts, journal, archive, `remote.json`), their readers and writers, path and object-name rules, journal history and `normalize_command`, the dashboard state ops-tower shows. Moved from ops-tower with its history (GATE-4). | yes; a pure crate |
| `proc-table` | The process snapshot model (`ProcessInfo`, `ProcessTable`, `ProcessTree`) and its Windows reader. Moved with `gate-state` (GATE-4). | yes; its `win32` module is Windows-only |
| `gate-win32` | The Win32 layer: job objects, process creation, named mutexes and events, priority, console, the WSL VM's CPU reader, the samples the watchdog consumes. | no |
| `gate-watchdog` | Free-share, load-adjusted clock, stall, ceiling and backstop arithmetic over samples with the kill lines, and the box heartbeat, on an injected clock; the cores-hint verdict. | yes; a pure crate |
| `gate-args` | The gate's own argv: the subcommand CLI, the compatibility layer, the C-runtime and cmd.exe quoting, the `%VAR%` refusals and the PATHEXT resolution rules over an injected environment and file lookup. | yes; a pure crate |
| `gate-router` | The routing rules (T2-T4) over strings: what routes, the path rewrite, the `--no-build` drop (`test` and `run` only), the wrapped dotnet form, the builder warm-up's file, hash and judge (`warm.rs`), the caller's variables a routed run does not send and the line it says for them (`watch.rs`). | yes; a pure crate |
| `gate` | The `gate` binary: the subcommands, the wait loop, the run, the journal writes, the remote client and `gate box`. | no |

Rules:

- No pure crate's API exposes `windows` or `windows-sys` types; Windows-only code sits in `cfg(windows)` modules. `proc-table` is not a pure crate: its `win32` module's API carries `windows-sys` handle and access-right types.
- `gate-state` and `proc-table` are also ops-tower's dependencies, by one pinned git rev, so their types unify there.
- The pure crates build with `cargo clippy` and `cargo test` on Linux in CI.

## Black-box suite harness

A Rust integration suite drives the built `gate.exe` through its CLI and reads only what it writes: exit codes, stdout, stderr, the log, the journal, claims and tickets.

- **Own state root per case.** Every case creates a fresh folder under the target directory's temporary folder. The folder is the case's working folder, holding its logs and its `LOCALAPPDATA`, and its `state` subfolder is the `--state-dir`, so cases never touch the machine's real slots, journal or counts, or each other's. Cases run side by side.
- **Load.** Watchdog-timing cases depend on the scheduler. The gate under test runs at Normal priority, and its command runs at BelowNormal. Under an all-core load, the gate was measured runnable but not running for up to 13.7 s, and a BelowNormal command for up to 31.8 s with no CPU.

  So a case never races a fixed sleep against the watcher: its command sleeps past the 600 s hang guard, and only the behaviour under test ends it. A stall window a case relies on is far past those gaps, or the case is ignored and run with `--ignored` on a critical slot (the repo's nextup profile lists them).

  The load rule: a case never asserts that something did or did not happen within a wall-clock window a loaded PC can cross. A case that expects a kill runs a command that sleeps until killed (`support::UNTIL_KILLED`), so a starved gate is late, never wrong.

  A case that cannot be written without a timer is a machine-alone case, `#[ignore = "needs the machine alone: run with -Slot critical and --ignored"]`, listed in the nextup profile and run on a critical gate with `--run-ignored only`. Widening a window until it passes on one PC is not a fix: it fails again on a busier one.
- **Git home.** Every git the suite starts, in a case or a harness helper, reads the suite's own git home, `<workspace>\target\tmp\git-home`, not the developer's `~/.gitconfig` and `~/.config/git`: `HOME` and `XDG_CONFIG_HOME` point at it (`support::git_home_for`), the inherited `GIT_CONFIG_*` variables are removed as the gate removes them, and its `.gitconfig` and `git\attributes` are written once per test process. The config pins line endings, longpaths, `untrackedCache`, and auto-maintenance and gc off, so a case's git behaves the same on every PC. A case that tests the gate's own git environment (`routing`, `syncback_conversions`' `isolated`) sets its own values after `prepare`. Auto-maintenance stays on in the product; only the suite turns it off.
- **Unit-test scratch folders.** The unit tests of `gate`, `gate-state` and `gate-win32` write under `target\tmp\unit-<crate>`, through each crate's `#[cfg(test)]` `test_support::temp_dir()`, not `%TEMP%`. One unit test keeps `%TEMP%`: the one in `remote/mod.rs` that needs a path outside any checkout, since everything under `target` is inside the worktree. Two suite cases in `tests/suite/durations.rs` also keep it: one runs from a folder outside any git repository, and the other only builds a path under the system temp root.
- **Why the files stay under `target`.** `%TEMP%` lies outside the excluded tree and is scanned, which slows every file create and git start there; the suite keeps its files under `target`, which is excluded.
- **Timing the suite.** `cargo nextest run --profile timing` (the `timing` profile in `.config/nextest.toml`; the default profile is unchanged) writes a junit report of every case's time to `target\nextest\timing\junit.xml`. To count git processes, set `GIT_TRACE2_EVENT=<folder>`: each git writes a trace file there, at near-zero cost. A process tracer built on WMI is too heavy to time with, since it slows the very starts it counts.
- **What a routed case costs.** A routed case's time is about its git process count divided by the PC's git start rate, so 16 test threads queue on git starts, not on CPU. A change that makes the suite faster removes git processes or makes each start cheaper; adding threads does not.
- **Test seams.** Hidden `--test-*` flags, compiled only with the `test-seams` cargo feature, set the timing knobs: `--test-sample-ms` (sample period), `--test-heartbeat-ms` (wait heartbeat; on `gate box` the command heartbeat's period), `--test-poll-ms` (fallback poll) and `--test-stall-ms` (stall window below a second). Each takes whole milliseconds above 0 on `gate run`. The sample and stall knobs drive the watchdog.

  Two seams of `gate box` shape the command's console: `--test-box-console fail-before` makes the UTF-8 console fail before the command starts, and `--test-box-park-launch <path>` parks the launch until `<path>` exists ([The box side](#the-box-side)). `--test-box-token refused` refuses the start with the restricted token, for the fallback to the box's own token. `--test-outputs-budget-bytes <n>` replaces the 50 GiB outputs budget of the box's trim.

  Two more seams make a run end by Ctrl+C without a console: `--test-interrupt-after-samples <n>` raises the interrupt after the n-th sample, and `--test-interrupt-queued` raises it when the waiting line is printed, so once no slot is free for the gate. `--test-print-request` shows them as `testSeams.interruptAfterSamples` and `testSeams.interruptQueued`.

  `--test-interrupt-queued-after-fallback` (a switch) raises it once the gate has said it waits for its own slot after a `full` fallback from the builder, the wait that counts from the run's queue line, so a case can pin how a Ctrl+C there counts that wait; `--test-print-request` shows it as `testSeams.interruptAfterFallback`, after `interruptQueued`.

  On `cutover` the seams are `--test-poll-ms`, `--test-heartbeat-ms`, `--test-confirm-ms` (the confirmation window) and the switch `--test-cutover-interrupt-after-list`; `--test-print-request` shows them as `testSeams.pollMs`, `heartbeatMs`, `confirmMs` and `interruptAfterList` ([`gate cutover`](#gate-cutover)).

  On `push` the seams are `--test-push-ssh <path>` (the ssh program to run instead of the one found along `PATH`, the suite's `fake-ssh`) and `--test-push-bound-ms <n>` (each builder's bound); `push` has no `--test-print-request` ([`gate push`](#gate-push)).

  On `check` the seams are `--test-probe-limit-ms <n>` (refused with `--builder`) and, each needing `--builder`, `--test-remote-transport`, `--test-remote-ssh` (which also needs `--test-remote-transport`) and `--test-remote-timeout-ms` ([`gate check`](#gate-check)).

  `--test-print-request`, on `run`, `stop-tree`, `cutover` and the compatibility form, prints the parsed request as one JSON line on stdout and exits 0, so the suite sees what a command line asked for; it is the one hidden switch the compatibility layer accepts. On `run`, `"timeout"` is the number given, or the floor of `auto:<n>`, and `"timeoutAuto": true|false` follows it, always present.

  `--test-sampler-fail` (a switch) makes every sample fail with `--test-sampler-fail makes every sample fail (test seam only)`, so the run says `gate: sampler failed: ...; watching by wall time only` and only the backstop watches; `--test-print-request` shows it as `testSeams.samplerFail`, after `interruptAfterFallback`.

  `--test-diagnostics-budget-ms <n>` (whole milliseconds above 0) replaces the 2000 ms diagnostics gather budget, so a case can make every block it gets cut; `--test-print-request` does not show it.

  `--test-diagnostics-fail` (a switch) makes the diagnostics gathering of a kill fail, so the log carries `gate: diagnostics failed: <message>` in place of the block; `--test-print-request` shows it as `testSeams.diagnosticsFail`, after `samplerFail`.

  `--test-remote-client <path>` replaces the remote client with a scripted one that runs in the gate's own process: the file's steps, one a line, are `out`, `err`, `record`, `wait`, `interrupt`, `killed`, `box-killed timed-out|backstop` (sets the box kill's kind, as the real client reads it from the tag), `status`, `adjusted <seconds>` (the load-adjusted seconds the builder counted), `refuse` (any fallback reason, `cold` included) and `fail-start`, and the steps that set how a warm-up ended: `warmed`, `warm-failed commands|refused <reason>` and `warm-deleted`.

  The `record` step's JSON carries the call's `warmth`: `null` for a call that asks nothing of the warm-up, `{"needs": <hash>}` for one that needs the builder warm with that hash, `{"warmUp": {"commands": …, "hash": …}}` for a warm-up.

  `--test-remote-wait-ms <n>` (whole milliseconds above 0) replaces the 60 s limit of the wait for a remote slot. `--test-print-request` shows them as `testSeams.remoteClient` and `testSeams.remoteWaitMs`.

  `--test-git-limit-ms <n>` (whole milliseconds above 0, on `gate run` only) replaces the 60 s limit of each git call of the routing decision; `--test-print-request` shows it as `testSeams.gitLimitMs`, after `remoteWaitMs`.

  `--test-git-drain-ms <n>` (whole milliseconds above 0, on `gate run` only) replaces the 2 s the readers of each git call of the routing decision get to end once git has exited; `--test-print-request` shows it as `testSeams.gitDrainMs`, after `gitLimitMs`. The suite's routing cases pass it with a very long drain, so a starved gate does not fail a call that a loaded machine only delays.

  `--test-beside-claim-wait <file>` (on `gate run` only) makes a local light gate that took its slot holding the beside mutex wait, after it journals its `take` and before it writes its claim, until `<file>` exists, so a case can hold a gate between its take and its claim; `--test-print-request` does not show it.

  `--test-remote-snapshot` (a switch, on `gate run` only) sends the run to the Rust remote client's snapshot alone ([The remote client](#routing)); `--test-print-request` shows it as `testSeams.remoteSnapshot`, after `gitDrainMs`.

  `--test-remote-transport local:<folder>` (on `gate run` only) sends the run to the Rust remote client with a local folder standing in for the builder; `local-ssh:<folder>` does the same but pushes and fetches to `<host>:<folder>/<repo>.git` with the `--test-remote-ssh` program as git's ssh (which it needs), so git's own ssh calls reach the stand-in.

  `--test-remote-ssh <exe>` replaces ssh with a program (the test bin `fake-ssh`), found as the gate finds its own programs ([Command launching](#command-launching)): a name with a folder part against the working folder, a bare name along PATH only. One it cannot find ends the run before any call with `gate: cannot use the remote transport '<transport>': cannot find --test-remote-ssh <word>: <why>` and status 1, after the transport's own form is checked.

  `--test-remote-retry-pause-ms <n>` (whole milliseconds above 0) replaces the 2 s pause before the pre-banner retry; it and `--test-remote-ssh` need the transport, and `--test-remote-snapshot` cannot be given with it. `--test-print-request` shows them as `testSeams.remoteTransport`, `testSeams.remoteSsh` and `testSeams.remoteRetryPauseMs`, after `remoteSnapshot`.

  `--test-local-sdks <comma list|none>` (needs `--test-remote-transport`) replaces the `dotnet --list-sdks` read of the parity check with a list of versions separated by commas, or `none` for no SDK.

  `--test-dotnet-limit-ms <n>` (whole milliseconds above 0; refused given twice) replaces the 30 s bound of the parity check's `dotnet --list-sdks` read. `gate box` takes the same seam before the verb for the preflight's read ([The box side](#the-box-side)).

  `--test-remote-bound-ms <n>` (whole milliseconds above 0) replaces the 30 s bound of the client's `stop` and `boot` calls.

  `--test-remote-interrupt-at <preflight|inputs|run|outputs|install|sdks|stop|manifest|apply>` makes the client raise the console interrupt itself after that call's first output byte (for `inputs`, the first of either inputs call); `install` raises it before the second file's move, so an install of one file is never interrupted, and `sdks` just before the parity check reads this PC's SDK list with `dotnet --list-sdks`; `manifest` just before it reads the snapshot's `.gate/toolchain.toml`; `apply` once a pair's sync-back has checked every patch and before it applies the first (no output byte is waited for); `stop` raises it at the run call's first byte and again while the stop call runs.

  `--test-remote-backstop-ms <n>` (whole milliseconds above 0) replaces the run call's backstop of 5 x `--timeout` + 60 s for the client, counted from the run call's start.

  `--test-remote-tamper <point>:<step>` makes the client change its own folders at one point of its run, so a case can change a folder after the route checked it. It exists only in a `test-seams` build and needs `--test-remote-transport`.

  The values are `before-hold:case` (the first sibling's folder renamed to upper case), `before-hold:replace` (the first sibling replaced by a new folder holding an empty `.git`), `before-hold:replace-root` (the worktree root replaced the same way) and `after-first-snapshot:worktree` (a `core.worktree` written into the first sibling's git config, naming the folder beside it with `-canary` after its name).

  The first three are made before the client holds the run's folders, the last once the main repository's snapshot is taken (`remote/tamper.rs`).

  Refused without the transport with `gate: --test-remote-tamper needs --test-remote-transport`, and a value outside those four with `gate: --test-remote-tamper must be before-hold:case, before-hold:replace, before-hold:replace-root or after-first-snapshot:worktree, got '<v>'`. `--test-print-request` does not show it.

  `--test-remote-timeout-ms <n>` (whole milliseconds above 0) applies only under `--test-remote-watch-call` naming a call other than `run`, as that call's transfer ceiling; given with `--test-remote-transport` without such a call it is refused with `gate: --test-remote-timeout-ms needs --test-remote-watch-call naming a call other than run`.

  `--test-remote-watch-call <preflight|push|inputs-diff|inputs|run|ls-remote|fetch|outputs>` makes `--test-stall-ms` and `--test-remote-timeout-ms` apply to that one call alone, its limit counted from the call's own start, every other call keeping the run's own window and limits. For `run` only the stall seam applies (the backstop has its own seam); for any other call `--test-remote-timeout-ms` is that call's transfer ceiling.

  With `ls-remote`, `--test-remote-timeout-ms` is that call's deadline and `--test-stall-ms` alone leaves it on the git limit. With `fetch`, the stall and ceiling seams apply to the fetch alone; with `outputs`, to the outputs fetch alone; with `inputs-diff`, to the inputs' diff call alone, and with `inputs`, to the inputs' put call alone, whose stall window counts the later of the last byte fed and the last byte of output while the archive is being fed.

  A value outside a seam's calls says `gate: --test-remote-watch-call must be preflight, push, inputs-diff, inputs, run, ls-remote, fetch or outputs, got '<v>'`, and `--test-remote-interrupt-at` says `must be preflight, inputs, run, outputs, install, sdks, stop, manifest or apply`.

  The five (with `--test-remote-backstop-ms`) need the transport; `--test-print-request` shows them as `testSeams.remoteBoundMs`, `remoteInterruptAt`, `remoteWatchCall`, `remoteTimeoutMs` and `remoteBackstopMs`, after `remoteRetryPauseMs`.

  `--test-remote-fetch-cap-bytes <n>` (a whole number of bytes above 0, default 4 GiB) replaces the cap on what the sync-back fetch's job may write. Refused without the transport with `gate: --test-remote-fetch-cap-bytes needs --test-remote-transport`, and a value that is not a whole number above 0 with `gate: --test-remote-fetch-cap-bytes must be a whole number above 0, got '<v>'`. `--test-print-request` does not show it.

  `--test-remote-apply-cap-bytes <n>` (a whole number of bytes above 0, default 4 GiB) replaces the cap on what a back commit's files may write into the worktree. It needs `--test-remote-transport` and is given once, and `--test-print-request` does not show it.

  `--test-remote-outputs-budget-bytes <n>` (a whole number of bytes above 0, default the 50 GiB outputs budget) lowers the budget behind both the outputs fetch cap and the install's unpack limit.

  Refused without the transport with `gate: --test-remote-outputs-budget-bytes needs --test-remote-transport`, and a value that is not a whole number above 0 with `gate: --test-remote-outputs-budget-bytes must be a whole number above 0, got '<v>'`. `--test-print-request` shows it as `testSeams.remoteOutputsBudgetBytes` (null unless given), after `remoteTimeoutMs`; the fetch-cap and apply-cap seams are not shown.

  `--test-remote-inputs-budget-bytes <n>` (a whole number of bytes above 0, default the 4 GiB one put of inputs may total) lowers that budget: a set the diff names past it is not sent, and the run falls back `inputs`. It is refused without the transport with `gate: --test-remote-inputs-budget-bytes needs --test-remote-transport`, and `--test-print-request` shows it as `testSeams.remoteInputsBudgetBytes` (null unless given), after `remoteOutputsBudgetBytes`.

  `--test-remote-log-cap-bytes <n>` (a whole number of bytes above 0, default 256 MiB) lowers the log cap, the most bytes of one call's output the client writes to the gate's log.

  Refused without the transport with `gate: --test-remote-log-cap-bytes needs --test-remote-transport`, a value that is not a whole number above 0 with `gate: --test-remote-log-cap-bytes must be a whole number above 0, got '<v>'`, and given twice with `gate: --test-remote-log-cap-bytes is given twice`. `--test-print-request` does not show it.

  `--test-remote-script` and `--test-bridge-pwsh` are unknown options in every build (`gate: unknown option --test-remote-script`, and likewise for `--test-bridge-pwsh`), and `--test-print-request` shows no `testSeams.remoteScript` or `testSeams.bridgePwsh`.

  `--test-own-start-fails` (a switch, on `gate run` only) makes both reads of the gate's own start fail, so the run ends with the refusal line of [Output lines](#output-lines), and the quarantine's start read in a sync-back too; `--test-print-request` shows it as `testSeams.ownStartFails`, after `bridgePwsh`.

  `--test-parent-job-trusted` (a switch, on `gate run` and in the compatibility layer) skips the binding of the slot marker to the parent's job, so a case can run under a marker that names the test process; `--test-print-request` shows it as `testSeams.parentJobTrusted`, after `ownStartFails`.

  `--test-job-breakaway` (a switch, on `gate run` only) makes the command's job allow a process in it to create one outside it with `CREATE_BREAKAWAY_FROM_JOB`, the job staying not kill-on-close, so a case stands a process that left the job in for a packaged app; `--test-print-request` shows it as `testSeams.jobBreakaway`, after `parentJobTrusted`, the last key.

  An installed gate has none of them, and without the feature each is an unknown option.
- **Spawn and wait helpers.** The suite's `start` launches the gate and returns a `Running` that can wait for a stderr line, say whether the gate is still running, and finish; dropping a `Running` whose program still runs first runs `gate stop-tree` on it, with its `--state-dir` and environment, under a 60 s guard, saying any failure on stderr and never panicking, so a case that panics also ends its gate's command and what that started; then it kills the pid.

  `Running::kill` kills the pid alone and leaves the command running, for the abandoned-claim cases. Output readers are joined with a deadline, so a grandchild holding the pipes cannot hang the suite. A run's `Outcome` carries `raw_stdout`, the bytes as the gate wrote them, beside the decoded text.
- **Holding and killing a gate.** A case holds a run with a real command, `fake-tool wait <go>`, and lets it go by creating `<go>`. A case that kills a gate lets its command go and waits until the command's log is closed (`support::wait_until_closed`), since the job has no kill-on-close. `support::run_words(log, extra, command)` builds the words of a light run.
- **Ordering, never elapsed time.** A case asserts the order of events in the journal and the log (a waiter's `take` after the holder's `release`, a heartbeat line before the inner gate's take, a kill line before the exit), never a wall-clock bound a busy machine could cross. A case's own deadline only guards against a hang and is generous.
- **Injected clock.** The watchdog arithmetic is unit-tested in `gate-watchdog` with an injected clock and scripted samples, not through the binary.
- **Fake process trees.** A small helper executable, `fake-tool`, is built only with the `test-seams` feature.

  It runs its steps in order:

  - `argv`, `pid` (prints `pid <its own pid>`), `env <name>` (prints the variable's value) and `exit <n>` (any 32-bit status).
  - `out <text>`, `err <text>`, `bytes <hex>` (writes those bytes to stdout as they are) and `lines <n> <text>` (prints `<text>1` to `<text><n>`).
  - `noise <path> <bytes>` (writes a file of that many incompressible bytes, the same on every run, its folders made when missing).
  - `sleep <ms>` and `wait <path>` (until the path exists, giving up after 600 s with exit 1).
  - `burn <stop-path>` (prints `burning`, then spins on the CPU until the file exists), `burn-threads <n> <ms>` (spins on `n` threads for `ms` of wall time, then goes on), `spin <n> <stop-path>` (spins on `n` threads, `n` at least 1, until the file exists).

    There are also `spawn-away <pid-path> <n> <stop-path>` (starts `spin <n> <stop-path>`, or, when `n` is 0, `sleep 600000`, which never looks at the stop path and uses no CPU, outside its job with `CREATE_BREAKAWAY_FROM_JOB`, which the job must allow, writes `<pid> <start>` and a newline to `<pid-path>`, prints `spawned <pid>` and goes on) and `chatter <path>` (prints numbered lines `chatter 1`, `chatter 2`, ... about every 50 ms until the file exists).
  - `spawn-wait <path>` starts `fake-tool wait <path>` with the caller's handles, prints `spawned <pid>` and does not wait.
  - `spawn-orphan <path>` runs `fake-tool spawn-wait <path>` as a child, waits for it to end, then continues with its next step, so the waiting helper is left without a parent and its `spawned <pid>` line is the up signal.
  - `spawn-pid <pidfile> <release-path> <program> [words]` starts the program with those words and the caller's handles, writes its pid to the pidfile, prints `spawned <pid>` and holds the program's handle until the release path exists, so the pid cannot be reused while a case opens it as the parent of a `gate burn`. It ends the steps: nothing after it runs.

  It is copied under the names the gate classifies (`VBCSCompiler.exe`, `dotnet.exe`, `MSBuild.exe`).

  A second helper, `fake-ssh`, stands in for ssh in the remote client's cases (`--test-remote-ssh`). Its `ok` step puts the words of the variable `GATE_FAKE_SSH_BOX_SEAMS` before the verb of a `gate box run` call, so a case gives the box its seams; the variable `GATE_FAKE_SSH_PREFLIGHT_SEAMS` does the same for a `gate box preflight` call.

  It reads a box call from the last three words of the line, `box <verb> --stdin`, and the box it runs inherits its stdin, so the request the client feeds reaches the box; a box call whose last word is not `--stdin` is refused by name.

  Its steps are `ok`, `hold`, `version`, `boot <time>` (as `version`, for the box's boot line), `no-boot` (drops the box's boot line), `drop` (exits 255 at once, writing nothing), `exit`, `fail-banner`, `fail-other`, `out-then-drop`.

  They also include `ok-then-out <line>` (as `ok`, then writes the line on stdout), `say-exit <n> <line>` (writes the line on stderr and exits `<n>`, as a call that fails with a line of its own does).

  They also include `fed-hold <path>` (reads its whole stdin and keeps it as `<steps file>.input`, then waits for `<path>`, then runs the call as `ok` does fed what it read, so the call's input has all been sent before the call does anything), `flood <n>`, `say-hold <word> <path>` `progress-ok <n>`, `truncate <n>` (passes `<n>` stdout bytes, writes `Connection timed out during banner exchange` on stderr and exits 255), `garbage <n>` and `bytes-hold <n> <path>`.

  A line that starts `pwsh ` and holds `-EncodedCommand` is a `gate push` call (`push` in its call log): `ok` runs it with the local `pwsh`, `LOCALAPPDATA` set to `<GATE_FAKE_SSH_BUILDERS>\<host>`, so each builder is a folder the case owns. Started with `--version` alone, `fake-ssh` answers `gate 0.0.0-old`, the stand-in for an older gate on a builder.

  Beside the `.log` it writes `<steps file>.words`, one line a call (the verb, then the words before the host, ssh's options), and `<steps file>.input`, the stdin of the last push call.

  It spawns children, burns CPU, prints its argv, writes output or sleeps on command, so build-server, orphan and kill-tree cases need no real toolchain.
- **No fixture from the checkout path.** No routing pattern, expected path or assertion is derived from where the repo is checked out; a case builds its own repository and routing file in its root. `checkout::repo_file` is the one route to a checked-in file (the install script), never for a path or pattern a case asserts on.
- **Launcher cases** (GATE-41): `crates/gate/tests/suite/launcher.rs` has 8 cases. They run the synced `tools/gate.ps1`. A case's `LOCALAPPDATA` is its `appdata` folder, whose `gate\v1` is the default root with the real slot names, so every such case also sets `GATE_STATE_DIR` to a non-default folder. The flag-unset case points the launcher at a stub canonical gate through `CLAUDE_CONFIG_DIR`. A new suite module that calls `repo_file` updates the callers pin in `checkout.rs`.
- **Cutover cases** (GATE-9): `crates/gate/tests/suite/cutover.rs` has 15 cases: the counts copy, the flag, the refused words and the printed request, and six over the drain. A live PowerShell gate is a `fake-tool wait` child whose claim or ticket, written by `support::write_powershell_claim` or `write_powershell_ticket` under `support::powershell_folder`, carries the child's real pid and start time; a stale one names a pid that has ended or a start that differs from the running process's.

  The six cases wait on a live claim and name it, wait on a live ticket and name its command line, ignore stale claims and tickets, name a late arrival, resume after an interrupt, and run the launcher's hand-over after a finished cutover.

  They use the launcher cases' helpers (`stub_claude`, `install_gate`, `run_launcher`, `run_direct`). Their waits are ordering-only: `Running::wait_for_stderr_while_running` returns the stderr once the line is there and the gate has not exited, and no case asserts how long anything took.
- **A packaged app the command starts.** `stall::a_stall_kill_ends_a_packaged_app_the_command_started` runs only where the first `pwsh` on PATH is the Store's (WindowsApps) alias, and skips elsewhere. To tell whether a process is in the gate's job, `IsProcessInJob(h, NULL)` and the job's `TotalProcesses` answer in one run; `winget --version` under `cmd` is the packaged reproducer on Windows 11 and Server 2025.
- **Running it.** The suite is a test target with `required-features = ["test-seams"]`, run through the gate with `cargo test --workspace --profile test-release --all-features`. Without the feature (no `--all-features`) cargo skips the suite silently.

## Cores hint

When a run that passed did not fit its slot, the gate says how many cores the command kept busy, so the caller learns whether heavy or light was right (GATE-6). For example: `gate: used 7.4 cores on average (peak 15, 0.6 free) on a light slot; work that keeps more than 2 busy belongs on heavy`.

- The line is said on stdout, just before the passed line, only for a passed run that took its own light or heavy slot or ran on no slot, ran 10 s or more of unpaused wall time, and had no gate under it take a slot of its own. A critical run is never judged, and a covered or lent run takes no slot.
- `avg` is the job's CPU (user plus kernel time of every process ever in the job, exited ones included, with a final read at exit) plus the CPU of the escaped tree under the job's members ([Watchdog](#watchdog); read per sample, the final stretch counted with the last sample's processes that still run), over the run's unpaused wall time.

  The CPU of paused samples is left out.

  `peak` is the highest own CPU over one sampler interval, paused samples skipped. `free` is the mean over the unpaused samples of the cores others left free. `avg` and `free` show one decimal; `peak` is rounded to a whole number, ties to even.
- A light run averaging over 2 cores is told heavy would fit. A heavy run averaging under 1.5 cores with at least 4.0 free on average is told light would fit. The peak is reported but does not change the verdict.
- A run on no slot averaging 1.5 cores or more is told `gate: used <avg> cores on average (peak <peak>) on no slot; work that keeps 1.5 or more busy belongs on light or heavy`, whatever was free; the line has no free cores. The conditions above are otherwise unchanged.
- Whether a gate under the run took a slot of its own is read from the journal (`nested_took_slot`): a gate under `Nesting::Under` or `Nesting::Beside` writes `parentPid` on its `take`, and the parent drops its hint and cores only when that `take` lists a slot or took the machine (a critical `take`).
- A nested no-slot gate takes nothing, so it leaves its parent's hint and cores. When the CPU at the end or the journal cannot be read, the hint is left out with a log-only note ([Output lines](#output-lines)).
- The journal's `release` line records `avgCores`, `peakCores` and `freeCores` ([Journal](#journal)), the history `--slot auto` will read (GATE-8). `--slot auto` runs a command with too little history on heavy and says it guessed; it never chooses critical or none.
- The gate learns durations and says the expected-time line ([Output lines](#output-lines)); `--slot auto` is built after the local cutover.

## Open questions

What no ruling settles yet, with the issue that owns it:

- **The box protocol and `remote.json`'s next format** (GATE-10): verbs, payload and transport between PC and builder, whether ssh keeps cmd.exe as the builder's shell, and the routing file's version and matcher.
- **A prediction a run has passed** (GATE-14): what a holder past its p90 reports in wait lines, and whether the critical gate re-prices its shadow end and closes or shrinks the lent window when a blocker overruns.
- **Which members the run's end spares** (GATE-144): the run's end spares every job member whose image is `gate.exe`, whatever gate it is; GATE-144 tracks hardening that judgement.
- **The surfaces of `gate status` and `gate drain`** (no issue yet). Install stays a script (`tools/install.ps1`) until a reason for a `gate install` subcommand appears.
- **The Windows facilities to spike before their slice** (GATE-5): other users' command lines (B1), the desktop-app breakaway policy (X1) and nested jobs, each of which changes kill behaviour if it silently fails.
