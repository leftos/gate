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
- Running on Linux or macOS. The PC stays Windows, and only the pure crates run elsewhere; a builder may run macOS (GATE-591).
- Configurable failure markers. The marker regex is built in ([Log and tail](#log-and-tail)).

## CLI

The CLI is subcommands with GNU-style flags: `--name value` or `--name=value`, and `--` ends the gate's options. The first word picks the parser. `--version`, `--help`, `-h` and `help` are matched first; then a subcommand name selects the CLI below, and any other first word starting with `-` selects the [compatibility layer](#compatibility-layer). No arguments at all print the usage on stderr and exit 2.

A `--help` or `-h` before the `--` of a `run`, `stop-tree`, `cutover`, `push`, `drain`, `prune`, `check`, `warm`, `burn` or `expect` line also prints the usage on stdout and exits 0, whatever else the line holds; after the `--` it belongs to the command (`gate run --slot light --timeout 5 --log x.log -- tool --help` runs `tool --help`).

| Subcommand | Purpose |
|---|---|
| `gate run` | Run one command under a slot and the watchdog. |
| `gate stop-tree` | Stop a process tree, reaching the gate jobs inside it. |
| `gate cutover` | Switch this machine from the PowerShell gate to `gate.exe`, draining it first. |
| `gate push` | Copy the installed `gate.exe` and desktop helper to every builder `remote.json` names and register the desktop task there. |
| `gate drain` | Raise a barrier on a builder and wait until no routed run is going there, so it can be redeployed; `--lift` lifts it, `--status` reports ([`gate drain`](#gate-drain)). |
| `gate prune --builders` | Mark the worktree records whose worktree is gone, and prune the work folder of each one that has been gone an hour on every builder `remote.json` lists, then remove what this PC keeps of the key ([`gate prune`](#gate-prune)). |
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

`gate run --slot heavy|light|critical|none [--alone] [--reason "<why>"] --timeout <seconds>|auto:<seconds> --log <path> [options] -- <command> [args...]`

`--slot`, `--timeout`, `--log` and the command are required and have no default, because a gate with no ceiling holds its caller for as long as the command hangs (kept, C1). Each missing or unreadable input is named on its own line before the usage.

| Flag | PowerShell option | Meaning | Source |
|---|---|---|---|
| `--slot heavy\|light\|critical\|none` | `-Slot` | The slot kind ([Slots](#slots)); `none` takes no slot and is for a command that only dispatches or watches remote work, except for a `git commit`, a commit run, which takes the slot asked (light for `none`; [Commit run](#critical-drain-and-nesting)). | ruling |
| `--alone` | `-Alone` | With `--slot critical` only: hold the whole machine, the light slots a plain critical gate leaves beside it included ([Critical drain and nesting](#critical-drain-and-nesting), Alone). Any other slot, or a gate's own command, is a usage error (exit 2). Ungated processes keep running: the gate cannot stop them, and the load snapshot it logs is the evidence, not a guarantee. | ruling (GATE-482) |
| `--timeout <seconds>` | `-TimeoutSeconds` | The ceiling in load-adjusted seconds, a whole number above 0 (W3). `auto:<n>` makes `n` a floor: each take resolves the ceiling to `max(n, ceil(3 x the command's adjusted p99))`, capped at 2147483647, and to `n` with no adjusted p99 ([Output lines](#output-lines), [design/gate-durations.md](design/gate-durations.md), part 4). The new CLI only: `-TimeoutSeconds` refuses it. | ruling |
| `--log <path>` | `-Log` | The command's log; a relative path is read once, at start, against the `--cd` folder when one is given, else against the caller's working directory ([Run folder](#run-folder)). | ruling |
| `--stall <seconds>` | `-StallSeconds` | The stall window, default 120 (W2); the gate passes it as given, and the remote client raises a value under 15 to 15 on a routed run (F2). | kept (C1, F2) |
| `--tail <lines>` | `-Tail` | Lines of the log printed at the end of a local run, default 20 (V3); a routed run shows its output live instead. | kept (C1, V3) |
| `--no-markers` | `-NoMarkers` | The command's status is the verdict; the marker scan is off (V2). | ruling |
| `--local` | `-Local` | Never route this run to a builder (F1). | kept (F1) |
| `--builder <host>` (or `--builder=<host>`) | none | Route this run to the builder `<host>` alone, never another and never here; heavy runs only ([Routing](#routing), Pinned run). | ruling (GATE-677) |
| `--reason "<why>"` | `-Reason "<why>"`, `-Reason:<why>` | The free-text reason a run states for itself. Required, and non-blank, when the run is `--local`, `--slot critical` or `--alone`; accepted and recorded on any run. Recorded on its `queue` event, in its claim and in its ticket. | ruling |
| `--state-dir <path>` | none | Use another [state root](#state-root); a relative path is read against the caller's working directory, before `--cd`. | ruling |
| `--cd <folder>` | `-Cd` | Run the command in `<folder>` and route from it ([Run folder](#run-folder)). | ruling (GATE-323) |
| `--bring-tracked <glob>` | none | A routed run's fetched output that git tracks and `<glob>` matches is installed rather than skipped (Outputs install, "Tracked members"); repeatable, and the one option that is not refused when given twice. The launcher's PowerShell form (`-Log …`) does not take it: use the `gate run` form. | ruling |
| `--env NAME[=value]` | `-Env` | Sets `NAME` for the command, local or routed ([Command environment](#command-environment)); without `=value`, forwards `NAME`'s value from the gate's own environment, the form for secrets. Repeatable. | ruling (GATE-362) |

A local or critical run without `--reason`, and one whose reason is blank, is a usage error (exit 2) before anything is queued: `gate: --local needs --reason "<why>"`, `gate: --slot critical needs --reason "<why>"`, `gate: --local and --slot critical need --reason "<why>"`, or `gate: --reason must not be empty` (the compatibility layer says `-Local` and `-Slot critical`, and `-Reason`).

`--alone` needs no separate line: it already requires a critical slot. The reason is required of every caller; the gate cannot tell an agent from a person, and an exemption by environment would be bypassable.

The words after `--` reach the command exactly as the caller passed them ([Command launching](#command-launching)).

#### Run folder

`--cd <folder>` (a relative folder is read against the caller's working directory) makes the gate enter `<folder>` once, after `--state-dir` is made absolute against the caller's folder, before `--log` is made absolute, and before any slot is taken. A `--cd` it cannot enter is said before a `--log` that cannot be made absolute.

From then on `<folder>` is the folder the gate was started in: [routing](#routing) reads its worktree, the command's working folder and program lookup, a fallback's local run, and the journal's and claim's `cwd` all take it, and so does a relative `--log`. A relative `--state-dir` stays the caller's. `--test-print-request`, which enters no folder, shows a relative log joined to the `--cd` folder.

It is how a caller in a main checkout gates a command that names another worktree's files: `--cd <worktree>` routes the run as if the gate had been started there. Given twice it is refused (`gate: --cd is given twice`). `gate expect` and `gate check` do not take it (unknown option). The compatibility layer reads it as `-Cd`, one of the two new options it takes (the launcher's callers need it); the frozen PowerShell gate refuses `-Cd`. Nothing is said on success.

#### Command environment

`--env NAME=value` sets `NAME` in the command's environment over the gate's own; the written value is on the gate's command line. `--env NAME` is the bare form: the value is `NAME`'s in the gate's own environment, so a secret stays off the command line. It is repeatable and means the same on a local run and a routed one. The compatibility layer reads it as `-Env`, in the same two forms, and `-Env` is the other new option it takes.

A name is one or more printable ASCII characters, U+0021 to U+007E, `=` excepted. ASCII only keeps the ASCII-case check of the refused names below equal to Windows' own comparison of names, so a name that is not ASCII can never pass that check and then be read as one the gate protects. A value may hold anything but NUL (CR and LF included) and may be empty.

The gate refuses a name it sets itself (the build-server settings, `UseSharedCompilation`, `GATE_SLOT_HELD`, `GATE_COMMIT_SLOT`, `GATE_STATE_DIR`, the box's `GATE_BOX_JOB` and `GATE_BOX_EXE` and the rest listed under [Command launching](#command-launching) and [The box side](#the-box-side)), any `GATE_` name, `GIT_CONFIG_PARAMETERS`, `GIT_CONFIG` and the machine paths the box keeps (`GODOT_PATH` and `UE_ROOT` among them, which a routed run sets itself).

The refusal is the same on both sides so one `--env` means the same on the PC and on a builder.

It also refuses a name given twice in any ASCII case, and more than 64 names or 32 KiB of names and values together. The gate's own settings are applied after the caller's pairs, so no pair undoes one.

Refusals are exit 2 on stderr before any slot. `{n}` is the word's 1-based place among every `--env` word given, an empty one included; a word whose name is malformed, that is not Unicode, or that names a variable to forward the gate's environment lacks, is never echoed, since it may be a secret pasted without its name.

- `gate: --env word {n} has a malformed name`, `gate: --env word {n} is not valid Unicode`, `gate: --env word {n} names a variable to forward, and it is not set` and `gate: --env word {n} names a variable whose value is not Unicode`.
- `gate: --env sets {name}, which the gate sets itself`, `gate: --env sets {name}, which git would read over the gate's own git settings`, `gate: --env sets {name}: GATE_ names are the gate's` and `gate: --env sets {name}, a machine path the gate keeps for the command`.
- `gate: --env gives {name} a value holding NUL`, `gate: --env names {name} twice` and `gate: --env is over its cap of 64 names or 32 KiB`.

A routed run sends the pairs as the payload's `Env` ([The box side](#the-box-side)), in the payload on the ssh call's stdin and nowhere else: no output line, the journal or ops-tower state shows a value. A warm-up sends none. A routed run also sends the gate's own `SourceRevisionId` pair after them ([Routing](#routing)).

A command tells that it runs routed by `GATE_BOX_JOB`: both boxes set it on every routed command, the Windows box as `Local\` or `Global\gate-remote-<key>-<12 hex>` and the Unix box as `gate-remote-<key>-<12 hex>` ([The box side](#the-box-side)), and a local run's command never has it, since a gate outside a box job removes it from its command's environment ([Critical drain and nesting](#critical-drain-and-nesting)).

A test that needs what only the PC gives skips when it is set. One example is a named pipe whose ACL admits the interactive user and denies NETWORK: the Windows builder runs a routed command with a network logon's token. Only whether the variable is set is the contract; its value is the box's own.

Files a routed command writes in the worktree come back after the run, whether it passed or failed. That covers tracked files and untracked files git does not ignore, which travel by the sync-back ([The box side](#the-box-side): the back commit and its apply). Ignored files come back only through the outputs globs.

A switch that makes a test write files (a golden regeneration, say) reaches the routed command only through `--env`, and a repo that declares its prefix in `.gate/env.toml` gets the dropped-variable line above when the switch is set in the caller's shell without `--env`.

A routed run does not send the rest of the gate's environment: when it holds a variable a repo's builds read (`NuGetAudit`, `RUSTFLAGS`, `RUSTDOCFLAGS`, and the `NUGET_`, `MSBUILD`, `CARGO_BUILD_` and `CARGO_PROFILE_` families) that no `--env` names and the gate does not set itself, the route says so after its other lines: `gate: a routed run does not send the caller's <names>; --env NAME sends it`.

A repo names more of its own variables in its `.gate/env.toml`: `prefixes = ["COLLIE_", ...]`, at most 32 entries, each an ASCII letter, digit or `_` name prefix (an exact name is its own prefix), matched in any letter case, none starting `GATE_`.

The file is read only for a routed run, after the decision, from the worktree's `.gate` or else the repo's declarations folder ([remote.json](#remotejson)), through the same guarded read as the other declaration files, and a declarations-folder copy logs `gate: .gate/env.toml read from <path>`.

The same line then also names the caller's variables starting a declared prefix, under the same conditions, and never one `--env` refuses (a machine path such as `PATH`, `TEMP`, `GODOT_PATH` or `UE_ROOT`, a name the gate sets, `GIT_CONFIG_PARAMETERS`, `GIT_CONFIG`, a `GATE_` name or a malformed name). A routed run never sends a declared variable without `--env`, since a variable a repo reads may hold a secret.

A file with a problem does not keep the run here: it says `gate: .gate/env.toml: <problem>; only the build variables are watched` (a declarations-folder copy named by its full path) on stderr after the route lines and before the dropped-variable line, and the run routes watching the build variables alone. `gate check` does not report this file.

### `gate stop-tree`

`gate stop-tree <pid>` takes one process snapshot (pids, parents and start times, no command lines) and builds the tree: the pid plus its descendants, a child counting only when its creation time is readable and at or after its parent's, so a recycled pid adopts no one (a child whose creation time is unreadable is left out with its subtree).

It terminates the job of every gate in the tree first, then the tree's processes deepest first, skipping itself and any process whose creation time changed since the snapshot (the pid was reused) (kept, X6). The members of a terminated gate job share a 2 s grace to exit on their own, and only the processes stop-tree had to terminate itself are counted.

A job or process it stops ends with status 124. Gate jobs are found under the resolved [state root](#state-root), named as the job of each gate pid in the tree, so `--state-dir` and `GATE_STATE_DIR` are honoured.

A process found already ending (the system answers `STATUS_PROCESS_IS_TERMINATING`) counts as gone, not as stopped or failed, so a tree holding one exits 0 with only the summary line. A terminate the system refuses is a failure at once (`Access is denied` is Win32 error 5).

It prints `gate: stopped <n> gate job(s) and <m> process(es) under <pid>` on stdout and each failure as `gate: could not stop: <reason>` on stderr, and exits 0 when nothing failed and 1 when anything did. A gate whose routed run holds an ssh call prints `gate: stopped 0 gate job(s) and 2 process(es) under <pid>`: the ssh call's job is anonymous, so stop-tree ends ssh by pid.

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

The drain ends only when every scan across a 5 s confirmation window finds no gate; a scan that finds one resets the window. Ctrl+C prints `gate: cutover: interrupted; the flag stays set, and running gate cutover again resumes the wait` and exits 0xC000013A. A rerun resumes; the take-back is reinstalling the previous commit ([Cutover](#cutover)).

Test-seams builds accept hidden options on `cutover`: `--test-poll-ms`, `--test-heartbeat-ms`, `--test-confirm-ms` and the switch `--test-cutover-interrupt-after-list` (raises the interrupt once the first list is printed). `--test-print-request` prints `{"mode":"cutover","localAppData":...,"testSeams":{"pollMs","heartbeatMs","confirmMs","interruptAfterList"}}`.

### `gate push`

`gate push` takes no words; its usage line is `       gate push`, after `gate cutover`. A word prints `gate: unexpected argument <word>` and any option, `--state-dir` included, prints `gate: unknown option <opt>`, each followed by the usage, exit 2. It reads `%LOCALAPPDATA%` only; `GATE_STATE_DIR` is ignored. What it does is [Install](#install); this is what it says.

It copies the installed `%LOCALAPPDATA%\gate\bin\gate.exe` and the desktop helper `gate-desktop-helper.exe` beside it to each builder, one after another, in the order `host` then `hosts` of `remote.json` ([remote.json](#remotejson)), into the builder's admin-owned `C:\Program Files\gate\bin` (so the builder's ssh session runs elevated), and registers the desktop task there by running the builder's installed `gate box register-desktop`.

A macOS `builders` entry is pushed to after them, by building its `gate-box` from the installed gate's commit (macOS builders, below).

The routing file must be one routing accepts, so `repos` and the other keys are checked as routing checks them. Every builder is tried, whatever happened to an earlier one. The exit is 1 if any builder failed or nothing could be pushed, else 0; a registration that failed is said on that builder's line and does not change the exit.

After the last builder, whatever each one's result, it runs the PC's prune sweep as `gate prune --builders --quiet` does, through the ssh it called the builders with, its lines on stderr as [`gate prune`](#gate-prune) says them. A push with no worktree records says nothing of it, and the sweep's status never changes the push's exit.

With nothing to push or nowhere to push it, `gate push` tries no builder, prints one line on stderr and exits 1:

- `push: no remote.json at <path>`;
- `push: ignored <path>: <reason>`, the router's reason text for an unusable file, or for a bad `hosts` ([remote.json](#remotejson));
- `push: no installed gate at <path>; run tools/install.ps1`;
- `push: no installed desktop helper at <path>; run tools/install.ps1`, for a `gate-desktop-helper.exe` missing beside the installed gate.exe, or not a file;
- `push: no remote.json: LOCALAPPDATA is not an absolute folder`, for an unset, empty or relative `LOCALAPPDATA`;
- `push: could not read the version of <path>`, when the installed exe's `--version` gives no `gate <version>` line or takes over 30 s;
- `push: cannot read <path>: <error>`, when the installed exe or the desktop helper cannot be read;
- `push: an install is in progress on this PC; nothing was sent`, when `tools/install.ps1` holds this PC's `push.lock`;
- `push: <bin>\push.lock is a link; nothing was sent`, and `push: could not lock <bin>: <error>; nothing was sent` for any other refusal of the lock. The link text is also said for a `push.lock` that is a folder, a junction or a second name of another file (a hard link); the open never follows a link.

`gate push` holds `%LOCALAPPDATA%\gate\bin\push.lock` on this PC, open and unshared, while it reads the installed exe's and helper's bytes and runs the exe's `--version`, and lets go before the first builder, so it never reads a gate.exe that `tools/install.ps1` is replacing.

`<v>`, `<old>` and `<new>` are the whole text after `gate ` in `gate.exe --version`, build id included, as `tools/install.ps1` reads it; the local one is read once, before the first builder. Per builder, on stdout:

1. `push: <host>: sending gate <v>`, before the transfer.
2. Then one of `push: <host>: gate <v> already installed` (an identical SHA-256: nothing was replaced), or `push: <host>: replaced gate <old>` followed by `push: <host>: installed gate <new>` (`replaced` only when a gate was there before), or on stderr `push: <host> failed: <reason>`.
3. After either install line, in this order: `push: <host>: installed the desktop helper`, only when the helper's bytes changed (a builder whose helper has the same SHA-256 says nothing); then `push: <host>: registered the desktop task` on every push that registers, the identical-gate path included, or `push: <host>: desktop task not registered: <why>`. A failure of the install (the line on stderr above) tries no registration.
4. Then, with gate.exe in place, `push: <host>: slot count <N>` on stdout: the builder's `slots.json` was written with `<N>`, the builder's `slots` of `remote.json` ([remote.json](#remotejson)), `host` and `hosts` taking the top-level one. The file is `{root}\slots.json`, `{root}` the builder's default remote root (`root` of `remote.json` when set), holding `{"v":1,"slots":<N>}` ([Builder-side slots](#builder-side-slots)); it is written to a temporary file and moved over the old one. A builder whose root is not there yet, a Windows builder's first push, says on stderr `push: <host>: no slot count written: <root> does not exist yet; the next push writes it`.

The order on a builder is gate.exe, then the helper, then the registration; `tools/install.ps1` places the helper first, and the push places gate.exe first, so a helper that cannot be placed still leaves the builder's gate updated. The helper is installed only with a gate.exe in place, installed or identical.

A registration runs the installed gate's `box register-desktop` (killed with its process tree after 60 s) while the script holds gate.exe open, shared for reading only, after checking its SHA-256 through that handle.

The `<why>` of `desktop task not registered` is one of: `it did not finish in 60 s`; `gate.exe changed before registration`; `gate.exe could not be held: <message>`; `could not run <path>: <message>`; the verb's own text after `gate-register: failed: `; else the last non-blank line the verb printed; else `exit <n>`.

The verb's own lines are in [The box side](#the-box-side) (`register-desktop`). A registration succeeded when the verb printed `gate-register: registered ...` and exited 0. Whatever it did, the script then exits 0.

A note on stderr never fails the push: `push: <host>: no slot count written: <why>` says the count was not written, `<why>` being `<root> does not exist yet; a routed run makes it, and the next push writes the count` (a root on a drive the builder lacks included), `<root> is a link`, `<root> is not a folder`, `<root>\slots.json is not a plain file` (a folder or a link there is never replaced) or the write error; the builder writes the count to a fresh `slots.json.<pid>.tmp`, after deleting any older one, and moves it over `slots.json`; a push that fails after the count was written still says its `slot count` line; `push: <host>: could not delete <path>: <message>` names an aside or stale temporary copy the builder could not delete, `push: <host>: could not delete the aside copies in <dir>: <message>` says the folder could not be listed after a verified install, and `push: <host>: could not list <dir>: <message>` says the same of the count of kept aside copies.

`push: <host>: could not stop the version read of <path>: <message>` says a version read past its bound could not be killed (the read still gives no version).

After the builder's other stdout lines, a push that ends with `gate.exe` missing or one it could not verify left in place, and with aside copies in the folder, says `push: <host>: kept <n> aside copies: gate.exe is not verified` on stdout, `<n>` counting every `gate.exe.*.old` present, this push's own included. It is never a failure by itself: an exe identical to the installed one that does not answer `--version` still says `already installed` and exits 0, with this line after it.

A failure reason is one of: `timed out after <n> s`, `no answer`, ssh's last stderr line, `cannot feed the call: <error>`, or the builder script's own.

The builder script's own reasons are `the copy could not be read: <exception message>`, for bytes that do not decompress; `the copy arrived damaged`;

The helper's two are `the desktop helper's copy could not be read: <exception message>` and `the desktop helper's copy arrived damaged`, the same two for the helper's bytes, each before anything is touched; `<dir> is a link`; `<dir>\push.lock is a link` (also for a folder, a junction or a hard link there); `another gate push is installing on this builder`; `could not lock <dir>: <message>`; `the builder needs pwsh 7.2 or later (it has <version>)`.

The ownership and access checks add these, each before anything is installed over what they judge: `the builder's Program Files folder is <folder>, not C:\Program Files`; `could not create <folder>: <message>`; `<folder> is a link` (for `C:\Program Files\gate` as for `<dir>`); `<object> is owned by <account>, not SYSTEM, Administrators or TrustedInstaller`; `<object> has a conditional access entry, which this push does not judge`; `could not read who may change <object>: <message>`; and `<object> lets <account> change what is in it` for a folder or `<object> lets <account> change it` for a file.

`could not prepare the install: <message>` says the script's small C# type for its file checks could not be added (constrained language, no compiler), before anything is touched. Any other failure says the failing step's message.

`the installed copy does not match`, `the installed gate.exe does not run` (the installed copy hashes right but gives no `gate <version>` line within 30 s), or `could not install <path>: <message>` when the installed copy cannot be read after the move or the move into place fails, ends with `; the previous gate.exe is back in place` or `; the previous gate.exe is at <aside>`.

On a first install it ends instead with `; the copy was removed and no earlier gate.exe existed` or `; the copy at <path> could not be removed: <message>`. A first install whose move of the temporary copy into place fails with no gate.exe there says `could not install <path>: <message>; nothing was installed and no earlier gate.exe existed`.

The move back acts only while `gate.exe` is missing or still this push's own file, the one with the temp copy's NTFS file id (`FILE_ID_INFO`: the volume serial number and the 128-bit id), which the script reads from the handle that wrote the temp copy and which the rename into place keeps; the hash does not decide it.

An id that cannot be read is tried three times, 300 ms apart, and a file gone at any try or after the last counts as missing, never as another's. The script reads the id through a small C# type it adds with `Add-Type`, which costs about 0.3-0.5 s on a push that installs; the same-exe path never adds it. The move back never overwrites a `gate.exe` that is missing.

A `gate.exe` another process placed meanwhile, a by-hand install say, is left as it is and the reason ends with `; another gate.exe was placed meanwhile and was left as it is`, followed on a push that moved an old gate aside by `, the previous gate.exe is at <aside>`.

The desktop helper's install fails the builder (exit 1) with `could not install <helper path>: <message>`, where the helper path is `gate-desktop-helper.exe` in the builder's bin folder; `the installed copy does not match` (the helper in place lacks the temp copy's file id or the pushed SHA-256) takes the same `could not install <helper path>:` lead.

The reason ends with one of `; no earlier desktop helper existed`, `; the previous desktop helper is at <aside>`, `; the previous desktop helper is back in place`, `; another desktop helper was placed meanwhile and was left as it is` (followed by `, the previous desktop helper is at <aside>` when one was moved aside) or `; the copy at <helper path> could not be removed: <message>`.

Then comes what happened to gate.exe, which the push had already settled: `; gate <v> is installed and its previous copy is at <aside>`, `; gate <v> is installed` (a first install), `; gate <v> was already installed`, or, for an identical gate.exe that gave no version, `; the same gate.exe was already installed`. A move back acts as the gate.exe's does: only while the helper is missing or still this push's own file (its NTFS file id), over a missing one never by force.

Builder text is escaped as the remote client escapes it and cut to 4096 bytes. The script says every line with its line breaks made spaces, so builder text with line breaks arrives as one line and cannot say a second fact.

Each builder is one ssh call, run in a kill-on-close job with stdin piped and bounded at 300 s.

A timeout stops only the local ssh, so the builder's install may still finish.

The call is `pwsh -NoProfile -NonInteractive -OutputFormat Text -EncodedCommand <bootstrap>` through the transport's ssh words ([Routing](#routing)), with `-o RequestTTY=no` ahead of them so stdin never passes through a pseudo-terminal.

The bootstrap is a short fixed PowerShell line that reads stdin's first line and runs it as the install script, so the call stays within the 8127 characters cmd takes (8191, less 64 for sshd's own `cmd.exe /c`, `%LOCALAPPDATA%` counted as 100) however long the script grows, which a unit test checks.

Stdin is three lines: line 1, the install script as base64 of its UTF-8 with this PC's SHA-256 of the exe and of the helper put in (and the registration command); line 2, the exe as base64 text of its gzip (flate2's best level); line 3, the helper the same way. The push call is not asked for ssh compression: the bytes are already compressed.

The builder needs pwsh 7.2 or later, whose .NET the file checks need; an older one fails the push with `the builder needs pwsh 7.2 or later (it has <version>)` before anything is touched. The script decompresses each copy with .NET `GZipStream`, carries this PC's SHA-256 of the exe and of the helper and refuses decompressed bytes that differ, reports on `gate-push: ` lines that this side maps to the `push:` lines, and exits 1 after a failure.

Two pushes to one builder at once are refused: the script holds `push.lock` in the builder's `C:\Program Files\gate\bin` open, unshared, for the whole install, so the second push fails at once with `another gate push is installing on this builder`, touching nothing, the cleanup of aside copies included.

The open handle is the lock: `push.lock` stays as a plain file after a push and never blocks a later one, and a `push.lock` that is a link, a folder, a junction or a second name of another file is refused with `<dir>\push.lock is a link`: the lock is opened without following a link and the opened handle is checked.

`tools/install.ps1` and the PC side of `gate push` take the same `push.lock` in their own bin folder with the same open, so an install, a push's read of the exe and a push to a builder never change or read one folder's gate.exe at once ([Install](#install)).

On the builder the script installs as `tools/install.ps1` does ([Install](#install)), in `C:\Program Files\gate\bin`.

Before it reads stdin it refuses a builder whose Program Files known folder is not `C:\Program Files`. It then creates the `gate` folder when missing, refuses it when it is a link and judges it, and only then does the same for `bin` beneath it, so `bin` is never made in an unjudged or linked `gate`.

Judging a folder or file refuses it when it is owned by anyone but SYSTEM, Administrators or TrustedInstaller, when its access list has a conditional or object allow entry, or when it lets any other account change it: for a folder, add files or folders to it, delete from it, delete, re-permission or take it; for a file, write, append to, change the attributes of, delete, re-permission or take it. Each refusal is a `failed` line and touches nothing.

The new copy of gate.exe is written to its temp file first and judged there, with the access list it inherited from `bin`, before the old one is moved aside; a refusal deletes the temp copy and fails with nothing moved. An identical gate.exe already in place is judged too before `same` is said, and the helper's temp copy, or the identical helper already in place, is judged the same way, a helper refusal ending with what happened to gate.exe as a helper install failure does.

It first deletes the stale `gate.exe.<pid>.tmp` and `gate-desktop-helper.exe.<pid>.tmp` copies of other pids that no process holds, and the aside copies (`gate.exe.*.old` and `gate-desktop-helper.exe.*.old`) only after the installed gate is verified.

It says the same version when `gate.exe` already has the same SHA-256; else it writes the exe to `gate.exe.<pid>.tmp` (created new), moves the old one aside to `gate.exe.<stamp>.<pid>.old`, moves the temporary file into place without force, checks the SHA-256 of the installed copy and that it answers `--version` with `gate <version>`, and puts the aside copy back when anything failed, the check's own read included; with no old exe to move back, it removes the installed copy.

With gate.exe in place the script then installs the helper the same way, unless `gate-desktop-helper.exe` already has the pushed SHA-256: a temp copy `gate-desktop-helper.exe.<pid>.tmp`, the old helper moved aside to `gate-desktop-helper.exe.<stamp>.<pid>.old`, the temp moved into place without force, and a check that the helper in place has the temp copy's file id and the pushed SHA-256, with the old helper put back (or the push's own copy removed, when there was none) on a failure, unless another process placed a helper meanwhile.

An installed gate is verified when its SHA-256 matches and `--version` answers (read through a handle held open, shared for reading only, with the SHA-256 checked again through it), and only then does the cleanup run: it deletes every aside copy, the helper's included, no process holds but this push's own, which stays as the last good gate.

The helper's aside copies thus go with a later verified push. The `kept <n> aside copies` line counts `gate.exe.*.old` only. A push that ends with `gate.exe` missing or unverified, a failed one and an identical exe that does not answer included, deletes no aside copy.

Each version read on the builder is bounded at 30 s.

The ssh a push runs is `ssh.exe` found along `PATH`, never `ssh.cmd` or `ssh.bat`.

**macOS builders.** After the Windows builders, each `builders` entry with `"os": "macos"` is pushed to in file order, one after another; an entry whose alias is also `host` or a `hosts` entry is left out. A Mac gets no copy of `gate.exe`: it builds its own `gate-box` ([The Unix box](#the-unix-box)) from the commit the installed gate was built from, so the box on the Mac carries the installed gate's version text.

The commit comes from `%LOCALAPPDATA%\gate\bin\source.json` ([Install](#install)), read once under `push.lock` with the exe. The Mac is refused, with no ssh call to it, when `source.json` is:

- missing: `no source.json beside the installed gate; reinstall it with tools/install.ps1`;
- unreadable: `source.json is unreadable: <why>`, for a file that is not an object with a string `repoRoot`, `commit` and `buildId` and a boolean `dirty`, a `commit` that is not 40 lower-case hex digits, a `buildId` that does not start it, or a `repoRoot` that is not absolute;
- dirty: `the installed gate was built from uncommitted changes; commit them and reinstall`;
- without a build id, or with one that is not the installed gate's: `source.json does not match the installed gate (<id> vs <version>); reinstall it with tools/install.ps1`;
- naming a commit its repo lacks: `the repo <repoRoot> does not hold commit <sha>`, asked with `git cat-file -e`.

Each refusal is a `push: <alias> failed: <reason>` line on stderr and fails the push.

Per Mac, on stdout, after the sending line and any other lines:

1. `push: <alias>: sending gate-box <v>`, `<v>` the installed gate's version text.
2. One ssh call, `/bin/sh -s -- prepare <root> <count>`, runs the embedded POSIX script (`MAC_SCRIPT`), writes `<root>/slots.json` (`{"v":1,"slots":<count>}` and a line feed, mode 0600, through a temporary file moved over the old one) and says the installed `gate-box`'s version, `<count>` being the entry's own `slots`. A count outside 1-64 makes no call and no line on the PC, and the script refuses it with `failed the slot count <c> is not a whole number from 1 to 64`. When the version is `<v>`, the push ends with `push: <alias>: gate-box <v> already installed`, with no git push and no build.
3. Otherwise this PC runs `git push --no-verify -- <alias>:<root>/gate-push/src.git +<sha>:refs/gate-push/src` in the repo `source.json` names, through the ssh it found, then says `push: <alias>: building gate-box <v>` and makes a second call, `/bin/sh -s -- build <root> <sha> <id> <v>`.
4. The build ends with `push: <alias>: replaced gate-box <old>` (only when an old box answered a version) and `push: <alias>: installed gate-box <new>`, or with `push: <alias> failed: <reason>` on stderr.
5. After the gate-box line, `push: <alias>: slot count <count>` when the prepare call wrote the file; else the note `push: <alias>: no slot count written: <why>` on stderr, `<why>` being `<root>/slots.json is not a plain file` (a folder or a link there is never replaced) or `could not write <root>/slots.json`, which does not fail the push. A failed build after the prepare still says the `slot count` line.

The script's notes are said on stderr as `push: <alias>: <note>`: `could not read the login shell's PATH; using the ssh PATH`, `<root>/gate-push/src.git is not a repository; making it again` (the prepare call, for a half-made bare repo an interrupted `git init --bare` left), and `<why>; making <root>/gate-push/src again` (the checkout failed once and is remade).

A `src.git` that is not a bare repository is made again only by the prepare call, after the folder checks below have passed; the build call fails on one with `<root>/gate-push/src.git is not a repository` and leaves it as it is. A `src.git` that is a link or a plain file is refused by those checks in both calls and never removed.

Both calls check their words again and refuse a builder without `/usr/bin/perl`, git, cc or cargo (git and cc in `/usr/bin` count only when `xcode-select -p` finds the developer tools) and without `/usr/bin/lockf` (`the builder needs macOS 15 or later (/usr/bin/lockf)`).

They run with every `GIT_*` variable removed, no global or system git config and `PATH` read from the login shell as the box reads it, and judge `<root>`, `<root>/bin` and `<root>/gate-push` as the Unix box judges a root, making each missing one at mode 700; a root that is not the user's own or has a group or other bit says `<root> is not owned by <user> or is open to others`.

They hold `<root>/gate-push/push.lock` with `/usr/bin/lockf`, not waiting, for the whole call: a second push to the Mac says `another gate push is installing on this builder`.

`push.lock`, `build.log` and `building` are refused unless each is a plain file with one name.

`gate-push/src.git` is a bare repository whose files are mode 0600 and whose automatic gc is off (`receive.autogc` and `gc.auto`, each written only when it differs), so no gc a git push starts prunes objects the checkout borrows. Ref locks under it and its `packed-refs.lock`, `HEAD.lock` and `config.lock` older than 30 minutes, which a cut push or config write leaves, are deleted.

Every git call on the Mac runs with no hooks, no automatic gc or maintenance and no file-system monitor, and closes the lock's descriptor.

The build step refuses a `bin/gate-box` that is a link or a folder and sweeps `bin/gate-box.*.tmp`. It makes `gate-push/src/.git` afresh every build, a repo borrowing `src.git`'s objects, then fetches the pushed ref, checks out its commit detached with `core.autocrlf` false and cleans the tree; when any of that fails it says a note, removes `gate-push/src` and tries once more. It then requires the checkout to be `<sha>` and to differ from it in no file.

The build is `cargo build --release --locked --manifest-path <root>/gate-push/src/Cargo.toml -p gate-box-unix --target-dir <root>/gate-push/target`, run from `/` so no cargo config or toolchain file above the checkout is read, with `GATE_BUILD_ID=<id>`, empty `RUSTC_WRAPPER`, `RUSTC_WORKSPACE_WRAPPER` and `RUSTFLAGS` (which override cargo's config), and no `CARGO_BUILD_*` or `CARGO_ENCODED_RUSTFLAGS`.

It runs in a process group of its own, killed whole after 1800 s (`cargo build did not finish in 1800 s; see <root>/gate-push/build.log`); the PC's call is bounded at that plus 600 s.

The build's output is `gate-push/build.log`, named in each failure reason.

A `building` marker is written before the build and removed when cargo ends by itself or at its bound. One left by a build something else killed makes the next build wipe `gate-push/target` first, so a cold build follows an interrupted one and never a build the bound killed.

The built binary is copied to `<root>/bin/gate-box.<pid>.tmp`, made mode 700, and installed only when its `--version` is `gate <v>` (`the built gate-box says gate <new>, not gate <v>`; each version read is bounded at 30 s). It is renamed over `<root>/bin/gate-box` without following a link there, so a push never leaves a half-written box.

`--test-push-bound-ms` replaces the 300 s bound and the Mac build's 2400 s bound alike.

Test-seams builds accept hidden options on `push`: `--test-push-ssh <path>` (the program each builder's call runs in place of the ssh found along `PATH`), `--test-push-bound-ms <n>` (the 300 s bound, in milliseconds above 0) and `--test-push-register ok|fail` (each builder's registration runs the pushed gate's `box --test-box-register <ok|fail> register-desktop`, the box's seam, so no Task Scheduler is reached; any other value is `gate: --test-push-register must be ok or fail, got '<value>'`).

A push through `--test-push-ssh` installs into each fake builder's own `%LOCALAPPDATA%\gate\bin` with no Program Files pin and no ownership or access check.

`gate push` has no `--test-print-request`.

### `gate check`

`gate check` takes no words but `--builder <host>`; its usage line is `       gate check [--builder <host>]`, after `gate push`. With `--builder` it checks one builder instead of this PC ([With `--builder`](#with---builder)).

A word prints `gate: unexpected argument <word>` and any option, `--state-dir` included, prints `gate: unknown option <opt>`, each followed by the usage, exit 2. It says what this PC lacks for the repo the working folder is in, and how to install it; it installs nothing and runs only version commands (each tool's own, and `dotnet --list-sdks`).

The repo is the git worktree root of the working folder, as routing finds it.

Outside a git worktree it prints `check: not in a git repository` on stderr and exits 1; a working folder or git that cannot be read prints `check: <why>` on stderr and exits 1. The repo's `.gate/toolchain.toml`, `.gate/siblings.toml`, `.gate/warmup.toml` and root `global.json` are read from the worktree on disk, so an uncommitted edit counts.

Any of those three `.gate` files that the worktree lacks is read from the repo's declarations folder when `remote.json` names one ([remote.json](#remotejson), Declarations folder).

The `.gate` files are read as routing reads `.gate/siblings.toml`: a `.gate` folder or file that is a link or junction, or a file over 64 KiB, is refused. `global.json` is read the same way, a link refused and 64 KiB the most.

A file that cannot be read whole (not one that reads with problems) fails the check: the summary says it and the exit is 1, however the items came out.

On stdout, each line prefixed `check: here: `, `here` being this PC, in this order:

1. The header `check: here: <repo> at <root>`, `<repo>` being the name routing gives the repo and `<root>` the worktree root as git spells it. With no `.gate/toolchain.toml` the line `check: here: <repo> declares no toolchain` follows; the SDK pin and the siblings are still checked.

   A line `check: here: .gate/<name>: read from <full path>` follows for each file taken from the declarations folder, and `check: here: .gate/<name>: the worktree's copy wins over <full path>` for each file of the worktree's own that shadows a copy there, readable or not.

   The same lines, with the builder's name for `here`, come from `gate check --builder`.
2. One line per item, every item printed, the ones that are fine included: the manifest's tools in manifest order, then the .NET SDK, then the workloads, then the `remote.json` item when it applies, then `declarations for <repo>: <why>` when the repo's declarations folder cannot be used, judged on every check whether or not a read reaches it (counted missing, so the exit is 1; `<why>` is the reason the log line gives), then one `remote.json: builders[N].crossBuild…` line per `builders` entry whose `crossBuild` cannot be used (whatever the repo, control characters escaped, counted missing; a routed run says nothing of it), then the siblings, then the warm-up's items ([The warm-up items](#the-warm-up-items)).
3. One `install: here: <subject>: <command>` hint line per missing or too-old item, in item order.
4. The summary: `check: here: nothing missing` or `check: here: <n> missing, <n> too old`, with `, <n> skipped` added when any item was skipped; with nothing missing and some skipped it is `check: here: nothing missing (<n> skipped)`. When a file could not be read it is `check: here: could not read <files>; <n> missing, <n> too old`, `, <n> skipped` added as above, the files comma separated in the order `.gate/toolchain.toml`, `.gate/siblings.toml`, `.gate/warmup.toml`, `global.json`.

The problems of the files go to stderr, one per line, before the item lines: `check: .gate/<file>: <problem>`, `<file>` being `toolchain.toml`, `siblings.toml` or `warmup.toml` and `<problem>` the text the file's reader gives ([Toolchain manifest](#routing), [Sibling worktrees](#routing), [remote.json](#remotejson)), or why the file could not be read at all (`.gate is not a folder`, for the manifest, when `.gate` is a file).

A file with problems is still used for what reads, and the rest is still checked. For a file taken from the declarations folder the problem line and the `could not read` summary name its full path in place of `.gate/<file>`; lines for the repo's own files are unchanged.

A `remote.json` that cannot be used is said on stderr too, as `check: remote.json: ignored <path>: <reason>`, with the exit code unchanged and no declarations folder read.

Every problem of `.gate/warmup.toml` is said, whether `remote.json` consents to warm-ups or not. A warm-up file that cannot be read whole (a link or junction, over 64 KiB, not UTF-8 or not TOML) fails the check whatever the consent, and the summary names it. One that reads with problems of its keys or commands is counted missing only with consent, and without consent only informs.

The `global.json` ones are `check: global.json: <reason>`, `<reason>` being `is larger than 64 KiB`, `cannot read it: <error>`, `is a link, a junction or not a file`, `is not UTF-8`, `does not parse as JSON`, `its sdk is not an object`, `its sdk.version is not a version dotnet reads`, `its sdk.rollForward is not a policy gate knows` or `its sdk.allowPrerelease is not true or false`.

The tool lines, for each tool of the manifest, probed as the box probes the tools of a preflight ([The box side](#the-box-side), Tool probes), all together within one 10 s bound, on this PC's own `PATH` with the worktree's own folders dropped, a program found that still lies inside the worktree refused:

- `godot <found> ok (needs <pin> exactly)` when the manifest's `godot` pin is met, else `godot missing (needs <pin> exactly: <found>)`, `<found>` being the versions the probe found, `none`, or `not probed (<why>)` for a probe that gave no answer or a version it could not read.

  The pin is judged as a routed run judges it ([Toolchain manifest](#routing)): exact on the parts it gives, so `4.5` reads `needs 4.5 exactly` and matches any `4.5.x`. A pin not met counts as missing, never as skipped, and has no install hint. Only the manifest entry is read, not the project files' pins;
- `unreal <found> ok (needs <pin> exactly)` and `unreal missing (needs <pin> exactly: <found>)` for the manifest's `unreal` pin, worded and judged as `godot`'s are (`<found>` is the versions under `GATE_UNREAL_ROOT`, `none`, or `not probed (<why>)`), with no install hint.

  With `GATE_UNREAL_ROOT` unset on this PC it is `unreal not checked here: GATE_UNREAL_ROOT is not set`, which counts as neither missing nor skipped; a root that overlaps the worktree gives `unreal skipped: GATE_UNREAL_ROOT <root> overlaps <worktree root>, which a run writes`;
- `netfx-sdk <found> ok (needs 4.6.0 or later)`, `netfx-sdk <found> too old (needs 4.6.0 or later)` and `netfx-sdk missing (needs 4.6.0 or later)`, for the .NET Framework SDK that `UnrealBuildTool` looks for, which a manifest naming `unreal` (at any version) needs besides ([Toolchain manifest](#routing)); `<found>` is every SDK this PC's registry lists, and the highest is judged.

  With `GATE_UNREAL_ROOT` unset on this PC it is `netfx-sdk not checked here: GATE_UNREAL_ROOT is not set, so the Unreal builds that need it are not checked here`, which counts as neither missing nor skipped. Test-seams builds accept the hidden `--test-netfx-key <path>`, a key under `HKEY_CURRENT_USER` read in place of the registry's own list;
- `<tool> <found> ok (needs <min> or later)`, `<min>` written as `major.minor.patch`, when the found version is the minimum or later, and `<tool> <found> too old (needs <min> or later)` when it is below it;
- `<tool> missing (needs <min> or later)`, or `(needs it present)` for a tool declared with `""`;
- `<tool> <found> ok` for a tool declared present at any version, and `<tool> ok (version not readable)` for a present one whose version the probe could not read;
- `<tool> skipped: found, version not readable (needs <min> or later)` when a minimum is declared and the tool answered with no version;
- `<tool> skipped: <why>` when the probe gave no answer, `<why>` being the probe's own reason (`no answer in 10 s`, `cannot run <program>: <error>`, and for `godot` and `unreal` the reasons of their probes, such as `cannot run <install>: <install> lies inside the worktree` for a `GATE_GODOT_ROOT` inside the repo, [Tool probes](#the-box-side));
- `tools.<name> skipped: not a tool gate knows` for a name the manifest declares that gate has no probe for, which runs nothing.

The SDK line comes only when the root has a `global.json` with an `sdk.version`; one with no `sdk` key, or an `sdk` with no `version`, pins nothing and prints none.

With a pin that names a version it is `.NET SDK <version> (rollForward <policy>) ok` when one of this PC's SDKs serves the pin by the rules of the parity check ([Parity](#routing)), else `.NET SDK <version> (rollForward <policy>) missing (this PC has <list>)`, the list being the SDKs `dotnet --list-sdks` gives or `none`; the words are the parity check's, so the PC's and a builder's lines agree.

The SDK list is read with the folders inside the worktree dropped from `PATH`. A `global.json` whose pin cannot be read (a reason above) says `.NET SDK skipped: global.json has no readable sdk pin`, which counts as skipped, and the file counts as unreadable; a list that cannot be read `.NET SDK skipped: cannot read this PC's SDK list: <why>`.

A manifest that lists workloads gets one line, `workloads <ids, comma separated> not checked yet`, which counts as neither skipped nor missing.

The tools in `.config/dotnet-tools.json` and a `Godot.NET.Sdk` pin are not checked by `gate check` and say nothing; a routed run does read a Godot pin (Toolchain manifest, under Routing), and `gate check` judges a manifest `godot` or `unreal` pin as a routed run does (the first two tool lines above), and a `godot` of `""` and every other tool as a minimum.

Each sibling the repo declares is checked as routing checks it, and gets one line:

- `sibling <name> not allowed in remote.json`, counted missing, when this PC's `remote.json` does not allow the repo that sibling (`siblings`, [remote.json](#remotejson)); its folder is not looked at. Consent is asked only when `remote.json` is usable and lists the repo; with no usable file, or one that does not list the repo, routing would never carry a sibling and every sibling is checked for its folder alone.
- `sibling <name> ok (<path>)` when its folder, `../<name>` of the worktree root as routing spells it, is a real folder with a `.git` (a folder, or the file of a linked worktree) that is no link or junction.
- `sibling <name> missing (no git worktree at <path>)` when nothing is there or it holds no `.git`.
- `sibling <name> <reason>`, counted missing, for a refused folder, worded as routing words it: `is a link or a junction`, `is not a folder`, `has a .git that is a link or a junction`, or `is the worktree's own folder` for a name that is the root's folder name in any ASCII letter case.
- `sibling <name> skipped: <why>` when its folder cannot be read.

When the repo declares siblings and `remote.json` lists the repo but its `siblings` value cannot be used, the line `check: here: remote.json: <problem>` (control characters escaped, counted missing) comes before the sibling lines, and every sibling is not allowed. A repo that declares none says nothing of it.

#### The warm-up items

Each is one line `check: here: warm-up: <text>`. The state root, where the work folder's warm-up record and lock are, is found as the other commands find it, `GATE_STATE_DIR` and then `%LOCALAPPDATA%\gate\v1`; `gate check` takes no `--state-dir`.

Consent is asked only of a repo `remote.json` lists in `repos`, whatever its `warmup` value: an unlisted repo has none, and so has one with no usable `remote.json`. A repo with neither `.gate/warmup.toml` nor consent, or whose `.gate` is not a folder, says nothing of warm-ups.

- With the file and no consent: `.gate/warmup.toml is there, but remote.json does not consent to warm-ups for <repo>`. With consent and no file: `remote.json consents, but the repo has no .gate/warmup.toml`. Both only inform.
- `remote.json: <problem>`, counted missing, when `remote.json` lists the repo, its `warmup` value cannot be used and the file is there.
- `.gate/warmup.toml has problems, so gate warm cannot prewarm a new worktree`, counted missing, with consent and a file that has problems.
- With consent and a file that parsed, the work folder's record and lock are judged for where its warm-up stands ([remote.json](#remotejson)), the lock probed and never taken. `the state root cannot be found: <why>` is skipped, and `warm on <host>` is ok.
- `<host> is warming the work folder` (the lock is held), `the warm-up on <host> failed <n> min ago: <reason>; run gate warm to try again` (a failure inside its backoff; an age under a minute, zero included, says 1 min), `not warm on <host>; gate warm warms it` and `not warm on <host>; the last warm-up ended without a verdict; gate warm warms it` (a `warming` record whose lock is free) only inform.
- `the warm-up record <path> cannot be read: <error>; gate warm overwrites it, and only a layout fallback's delete reads it` and `the warm-up lock <path> cannot be read: <error>; gate warm and a routed run waiting on the work folder read it` only inform: a routed run does not read the record or the lock for its route.

The lines that only inform are never counted and never fail the check.

The install hints, for the items that have one:

| Item | Hint |
|---|---|
| `rust` | `winget install Rustlang.Rustup`; with a minimum, `rustup default <min>` when rust is too old, and `winget install Rustlang.Rustup, then rustup default <min>` when it is missing |
| `cargo-nextest` | `cargo install cargo-nextest --locked` |
| `node` | `winget install OpenJS.NodeJS` |
| `netfx-sdk` | `install the .NET Framework 4.8 Developer Pack (https://dotnet.microsoft.com/download/dotnet-framework/net48)`; on a builder, the `install-toolchains.ps1` step below |
| `pnpm` | `corepack enable pnpm` |
| `pwsh` | `winget install Microsoft.PowerShell` |
| `ffmpeg` | `winget install Gyan.FFmpeg` |
| the .NET SDK | `.NET SDK: winget install Microsoft.DotNet.SDK.<major>`, the pin's major |
| a missing sibling | `sibling <name>: clone it to <path>` |

A tool not in the table (`godot`, `unreal`) gets no hint line, nor does a sibling that is not allowed or refused, nor the `remote.json` item.

The exit is 0 when nothing is missing or too old and every file read, whatever was skipped and whatever warm-up line only informs; 1 when anything is missing or too old, or the check could not run (not in a git repository, an unreadable `.gate/toolchain.toml`, `.gate/siblings.toml`, `.gate/warmup.toml` or `global.json`, `.gate` not a folder); 2 for a usage error.

Test-seams builds accept the hidden `--test-probe-limit-ms <n>` (the 10 s probe bound, in milliseconds above 0), `--test-probe-drain-ms <n>` (how long the output readers of each tool probe, of this PC's `dotnet --list-sdks` and, with `--builder`, of the builder check's call get to finish after the program exits, 2 s by default) and `--test-sdk-limit-ms <n>` (this PC's SDK list limit in milliseconds above 0, in place of 30 s).
`--builder` refuses `--test-probe-limit-ms` and `--test-sdk-limit-ms` ("gate: --test-sdk-limit-ms is not used with --builder"), and accepts `--test-probe-drain-ms`.

#### With `--builder`

`gate check --builder <host>` says what one builder lacks for the repo in the current folder. It needs one host (`gate: --builder needs a value`; given twice, `gate: --builder is given twice`), which must be a builder `remote.json` lists (`host`, `hosts` or a `builders` entry), matched ignoring ASCII case and printed in `remote.json`'s spelling. Any other host prints `gate check: <host> is not a builder remote.json lists` and exits 2.

The repo is found as for the local check (outside a git worktree is `check: not in a git repository`, exit 1). A `remote.json` that is missing, ignored or cannot be found is a stderr line with exit 1: `check: <host>: remote.json not found at <path>`, `check: <host>: remote.json is ignored: <reason>` or `check: <host>: remote.json cannot be found: LOCALAPPDATA is not an absolute folder`.

It runs the box's preflight once over ssh with this repo's real work-folder key and the manifest's tools, bounded at 120 s, with no retry and no PC-side lock. A routed run of the same worktree running at the same time may collide with it (GATE-270). The preflight creates the same folders a routed run's preflight would.

On stdout, each line prefixed `check: <host>: `, in this order:

1. The header `check: <host>: <repo> (work folder <key>)`.
2. `check: <host>: created <folder>` for each folder the preflight made, at most 20, then `check: <host>: and <n> more created folders`.
3. The items, worded as on this PC with `<host>` for `here`: the SDK reads `.NET SDK <version> (rollForward <policy>) missing (<host> has <list>)`, the list cut at 20 entries and then `, and <n> more`; `.NET SDK skipped: <host> sent no SDK list`, with `: <last stderr line>` when there is one; a tool that did not answer is `check: <host>: <tool> skipped: the builder gave no answer`; the workloads are `not checked yet`.

   A builder whose box refused its work root (Work root in [The box side](#the-box-side)) adds one more item after the `remote.json` consent and hooks items, `check: <host>: the work root is not locked down: <why>; run the installer's "Gate work root" step (install-toolchains.ps1 -Only "Gate work root")` or `check: <host>: the work root is busy: <why>`, counted missing, so the check exits 1.

   That builder answered no probe and read no warm-up, so every tool is then said `<tool> skipped: the builder gave no answer` and no builder warm-up item is said.

   `git-lfs` at 3.1.3 or later (or the manifest's higher minimum) is an item like any tool when a `.gitattributes` git tracks in the worktree, at any depth, names the LFS filter, judged as routing judges it.
4. The warm-up's items, as on this PC ([The warm-up items](#the-warm-up-items)) and this PC's own: the lock is probed before the record is read, as the route does, and the record is said only when it names `<host>` or cannot be read (`check: <host>: warm-up: the warm-up record <path> cannot be read: <error>`, with the suffix above, only informing), and with consent and a warm-up file that parsed, `check: <host>: warm-up: the work folder is warm` (ok) when the preflight's `gate: remote warm` line holds the hash of the worktree's file, else `check: <host>: warm-up: the work folder is not warm` (only informs).
5. Tailscale's path to the builder, one item `check: <host>: tailscale path: <text>` after the warm-up's items, never failing the check: `direct` (ok), `relayed via DERP (<region>)`, `relayed via a peer relay`, `no path yet`, or `not checked: <why>` for `tailscale is not installed`, `tailscale is <state>` (a tailnet that is not running), `no tailnet peer matches <host>`, `several tailnet peers match <host word>` (the alias without `user@`, or the `ssh -G` hostname; of several matches the one online wins) and `ssh -G <alias> <why>`. `tailscale status --json` is read once, bounded at 3 s, and the peer is the one whose `HostName`, `DNSName` (its first label or the whole name) or `TailscaleIPs` equals the alias with any `user@` cut, in any case; when none does, the hostname `ssh -G -- <alias>` resolves the alias to is matched the same way. Text from `tailscale` and `ssh` is escaped. This PC's check says no such item.
6. The install hints `install: <host>: <tool>: <command>`: `cargo install cargo-nextest --locked`, `npm install -g pnpm@<min>` (`npm install -g pnpm` with no minimum), `rustup toolchain install <min> && rustup default <min>`, `dotnet-install.ps1 -Version <v>`, and for node, pwsh, ffmpeg and a rust with no minimum a hint that names `tools/builder/install-toolchains.ps1` and points to [routing-a-repo.md, Builder setup](routing-a-repo.md#builder-setup).

   `netfx-sdk` gets the same hint, with the script's step `'.NET Framework 4.8 Developer Pack'`.
7. The summary.

When the repo's `.gate/siblings.toml` names a sibling or cannot be read, one line `check: <host>: siblings skipped: a routed run lays them out` comes instead of the sibling lines and counts as skipped. The sibling folders themselves are not checked on the builder.

The problems of `.gate/siblings.toml` are said on stderr as on this PC (`check: .gate/siblings.toml: <problem>`, for example `check: .gate/siblings.toml: siblings[0] is empty`), and a problem alone does not change the exit status. A `.gate/siblings.toml` that cannot be read is named in the summary, `check: <host>: could not read .gate/siblings.toml; <n> missing, <n> too old`, with `, <n> skipped` as on this PC, and the exit is 1.

A failure is one stderr line, exit 1, and nothing else on stdout:

- `check: <host>: unreachable: <ssh's last error line>` for ssh status 255 (`ssh exited 255 with no message` when ssh printed nothing on stderr), and on the timeout `check: <host>: unreachable: no answer in <limit>`;
- `check: <host>: no answer in <limit>, and ssh could not be stopped: <error>`;
- `check: <host>: runs gate <theirs>, this PC runs <ours>; run gate push`, a version mismatch printing only this line;
- `check: <host>: has no gate box that takes this call (status <n>); run gate push`;
- `check: <host>: its gate install is not trusted (<why>); reinstall with gate push`, when the call exited 1 and said the box's install line (`gate: the builder's gate install is not trusted: <why>`, Install tripwire in [The box side](#the-box-side)) on either stream; the why is escaped;
- `check: <host>: preparing <root> failed (status <n>): <line>`, `<root>` being the builder's work root;
- `gate: check: the builder is drained (raised <hh:mm> UTC, lapses <hh:mm> UTC); nothing was checked; check again after gate drain --lift`, when the preflight is refused with status 75 and the box's `drain` place and barrier lines (after the install and work-root cases above); the parenthesis is left out when the box said no barrier stands. The check sends `NoTicket` true and no `Session`, so it takes no place in the drain's line, and exits 1 because nothing was checked.

  The same lines with another status are a failed preparation.
- `check: <host>: cannot run ssh: <error>`, and `check: <host>: <why>` for any other failure of the call.

Text from the builder is cut to 4 KiB and escaped. The exit is 0 when nothing is missing, 1 when something is missing or the check could not run (not in a git repository, unreachable, a different build, no box, `remote.json` missing or unusable), 2 for a usage error.

Test-seams builds accept, with `--builder` only, `--test-remote-transport`, `--test-remote-ssh` (which also needs `--test-remote-transport`) and `--test-remote-timeout-ms`, as the routed client reads them ([Black-box suite harness](#black-box-suite-harness)), and `--test-tailscale-status <file>`, the file the path is read from in place of running `tailscale status --json` (a file that does not exist stands for a PC without Tailscale); a test-seams build also reads `GATE_TEST_TAILSCALE_STATUS` when the flag is absent.

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

It takes no command, `--log`, `--timeout` or `--local`: the commands come from the file, the log is `remote\warm\<key>.log` and the ceiling is the file's `timeout`. A warm-up takes no slot and writes no journal event. It runs the warm-up even when the work folder is already warm, since a warm-up of a warm folder is incremental.

A warm-up takes no remote slot because queuing for the single default slot would make every other worktree's routed run fall back `full`. It takes the key mutex (with a bounded wait) and a PC-wide cap of one running warm-up.

- `--cd <folder>` enters the folder as `gate run --cd` does ([Run folder](#run-folder)), with the same refusals, which exit 2. Whatever folder of the worktree the gate starts in, the warm-up is worded and laid out from the worktree root (the gate makes the root its working folder), so its commands, their path rewrite and its work folder are those of a routed run started at the root, and the call's subfolder is empty.

  The layout and wording must match a routed run's: a different layout makes the box's `clear_work` wipe the warmth, and a command without the `-p:SourceRevisionId=<HEAD>` stamp makes the next real run rebuild everything.

  The warm-up's log (`remote\warm\<key>.log`) starts with the decision's notes, such as the `gate: no PathMap word` line, which a warm-up does not say on stderr.
- `--background` keeps a warm-up that does not start quiet: no consent, no warm-up file and a warm-up already running say nothing and exit 0. It detaches nothing; whatever starts a warm-up in the background does that, and no gate command does.
- `--state-dir` and `GATE_STATE_DIR` resolve the [state root](#state-root) as for `gate stop-tree`.
- A test-seams build also takes `--test-warm-wait-ms <n>`, which replaces the file's ceiling as the length of the key mutex wait and of the cap wait, and the remote client's seams a routed run takes (`--test-remote-client`, `--test-remote-transport`, and the others that need the transport), but not `--test-print-request`.
- Exit codes: 0 warm (also, with `--background`, a warm-up that did not start for no consent, no file or a warm-up already running); 2 a usage or precondition problem; 1 a warm-up that failed or was refused, a failed record write, and, without `--background`, a warm-up already running; `0xC000013A` on Ctrl+C. A word other than the options above is a usage error, exit 2.

It works in this order, each step ending the warm-up where it fails.

1. The state root, then `--cd`, then the worktree root, then `remote.json`, then the repo's consent (`warmup` in `remote.json`; one that cannot be read is no consent), then the routing target (a repo `remote.json` does not list, or siblings that cannot be used, end here), then the warm-up file's first read: through the `.gate` reader (the repo's declarations folder's copy when the worktree has none), parsed and hashed, each command worded for the builder as a routed run words it (`builder_words`).

   This read gives the hash of the first `warming` record only.

   The commands need not match the repo's routing patterns: consent is per repo, so a restore-only script that is never routed may still be a warm-up.
2. The warm-up lock `remote\warm\<key>.lock`, taken for the warm-up's whole life: created when missing, opened without sharing and without following a link, required to be a plain file, its open tried again every 20 ms for 200 ms while another handle holds it, which rides out a route's probe. A lock still held ends the warm-up without touching the record. In the foreground it says `gate: a warm-up of <host> is already running` and exits 1; with `--background` it says nothing and exits 0.

   Liveness is a lock file because `Local\` objects are per logon session and pids are reused, so neither can show that a warm-up is alive; the pid in the record is for the lines only.
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
- No file, exit 2 (quiet with `--background`, exit 0): `gate: warm: <root>\.gate\warmup.toml does not exist`, followed by `; declarations for <repo> not read: <why>` when the repo's declarations entry cannot be used (the reason the log line gives).
- A file problem, exit 2: `gate: warm: .gate\warmup.toml <problem>` (the first problem, then ` (and <n> more)`), `gate: warm: .gate\warmup.toml command <n> starts with <word>, which a routed run never sends` (a command a routed run keeps on this PC) and `gate: warm: .gate\warmup.toml command <n> names <word>, outside the worktree`; for a file taken from the declarations folder, each of the three names its full path in place of `.gate\warmup.toml`.
- Withdrawn at the recheck, exit 2 (quiet with `--background`, exit 0): `gate: warm: remote.json's builder changed from <host> to <host>`, `gate: warm: remote.json's entry for <repo> changed` (also for a changed work folder key or inputs), `gate: warm: the siblings of <repo> changed while the warm-up waited`, and the consent and file lines above.
- Ends that write no record, exit 1: `gate: warm: cannot make <folder>: <error>`, `gate: warm: cannot take <lock>: <why>`, `gate: warm: cannot remove the earlier warm-up log <path>: <error>`, `gate: warm: cannot create the warm-up log <path>: <error>`, `gate: warm: cannot use the warm-up log <path>: <error>` and `gate: warm: cannot write the warm-up record <path>: <error>`.
- `gate: warm: cannot delete the warm-up record <path>: <error>` joins the line of a final write that failed or a record a withdrawal could not delete.
- Ends at the builder's drain, exit 1 (also with `--background`): `gate: warm: the builder is drained; this warm-up ends without warming`. The call sends `NoTicket` true and takes no place in the drain's line; the `warming` record is deleted, so no `failed` record and no backoff follow, and the key is given back.
- `gate: cannot install the Ctrl+C handler, so Ctrl+C ends the gate at once: <error>`, and the verdicts: `gate: warm-up of <host> passed` (exit 0) and `gate: warm-up of <host> failed: <reason>` (exit 1). The client's own lines and the box's go to the console and the warm-up's log as a run's do.

### `gate drain`

`gate drain [--lift | --status] [--builder <host>]` drains one builder before a redeploy: it raises the builder's drain barrier, waits until nothing of a routed run is going there, and says so. Its usage line is `       gate drain [--lift | --status] [--builder <host>]`, between `gate push` and `gate check`, and `gate drain --help` or `-h` prints it.

The barrier, the sessions and the gated verbs are the box's ([The box side](#the-box-side), `drain`); the routed runs' wait is [Routing](#routing), Builder drain; this is what the PC's command does and says.

- With no option it raises the barrier, or raises it again, and waits. `--lift` lifts the barrier, whoever raised it; the waiting tickets stay and are admitted in arrival order. `--status` says how the drain stands and changes nothing.
- The builder is the one `remote.json`'s `host` names, else the one `--builder <host>` names, which must be a builder `remote.json` lists. No line names the builder.
- It reads `%LOCALAPPDATA%` only, as `gate push` does; `GATE_STATE_DIR` and `--state-dir` do not apply. It takes no slot and writes no claim, journal line or log.

It calls the builder's `gate box drain --stdin` over ssh, found along the gate's own `PATH`, with the payload `{"Root","Action"}` (`payload::drain_word`), `Action` being `raise`, `status` or `lift`. Each call is bounded at 120 s and ended by Ctrl+C. Every line goes to stderr.

**The wait** (no option) works in this order.

1. The first call raises the barrier. `gate: drain: barrier raised on the builder; it lapses at {hh:mm} UTC unless gate drain runs again`, the time in UTC from the box's Unix seconds. An answer that says no barrier stands is the lifted-mid-wait line below.
2. It then asks the box every 2 s. A `raise` goes in place of a `status` once 5 minutes have passed since the last raise the box answered with a standing barrier, so a long drain never lapses its own barrier; a re-raise that failed is tried again on the next poll, and each answer takes the lapse time of the barrier it says.
3. While the box says `waiting`, the list is said at the start and again only when an item comes, goes or changes in anything but its age: `gate: drain: waiting for {n} run(s) on the builder:`, then one line per thing the box lists, or, when it lists nothing, `gate: drain: waiting for the builder to finish what it has started`.
   - `gate:   {pc}  {repo}  {age} s  {command}` for a live run (a field the box sent as `null` shows `?`);
   - `gate:   process {pid} (prune|git|job)` for a process the box counts;
   - `gate:   a run whose record cannot be read ({key})`;
   - `gate:   the builder cannot list its processes: {why}`, from a Unix box that cannot list them.
4. `gate: drain: still waiting for {n} run(s) ({s} s)` every 30 s from the start of the wait, where `n` is the count of the last list; when the last answer listed nothing, `gate: drain: still waiting for the builder to finish what it has started ({s} s)` instead. The cadence is `heartbeat.rs`'s, which `gate cutover` shares.
5. When the box says `drained`: `gate: drain: the builder is drained; gate drain --lift lifts it (it lapses at {hh:mm} UTC; gate drain re-raises it)`, and exit 0. The barrier stands. Nothing re-raises it after the exit, so it lapses 30 minutes after the last raise unless `gate drain` runs again.

Every text from the builder (a PC, a repo, a command, a key, a reason) is escaped to one line.

**When the builder does not answer**, a call that fails after the first is not the end of the wait: `gate: drain: the builder did not answer: {why}; still trying`, said once per distinct reason, and the wait goes on while the last barrier it was told has not lapsed. Once that barrier has lapsed with no answer: `gate: drain: the builder has not answered and the barrier lapsed at {hh:mm} UTC; gate drain raises it again`, exit 1.

The lapse is judged on the PC's monotonic clock: each answer sets a deadline as long after it as the barrier's lapse is after its raise, and an answer whose lapse comes before its raise (malformed) gives a deadline of now.

Every reason from ssh or the box (the could-not-be-reached and did-not-answer lines) has the builder's alias and its host part replaced with `the builder`.

**The first call** failing ends the drain at once: `gate: drain: the builder could not be reached: {why}`, exit 1; the same line for `--lift` and `--status`. A builder whose gate box does not take the verb (its usage answer, never an answer of "drained") is `gate: drain: the builder's gate box does not know drain (status {n}); run gate push`, exit 1, whatever call met it.

**Lifted mid-wait.** An answer that says no barrier stands, or that the state is `open`, while the wait goes on (a lift from another shell, a lapse): `gate: drain: the barrier was lifted or lapsed while gate drain waited; gate drain raises it again`, exit 1. The first call answering the same way, with no barrier standing after a raise, says it too.

**Ctrl+C** is installed for the raise form only (`--lift` and `--status` are single calls). It ends a call in flight and the wait, leaves the barrier standing, says `gate: drain: interrupted; the barrier stands; gate drain --lift lifts it, gate drain resumes the wait` and exits 0xC000013A.

A Ctrl+C during the first raise call says `gate: drain: interrupted before the barrier was raised; nothing changed on the builder` instead, with the same exit. A call that ends in anything but an answer while the interrupt is raised counts as interrupted.

A rerun raises the barrier again, keeps the waiting tickets, and resumes the wait. A handler that cannot be installed says `gate: cannot install the Ctrl+C handler, so Ctrl+C ends the gate at once: {error}` and the wait goes on.

**`--status`** is one call and exits 0 when the box answered. The barrier line is `gate: drain: a barrier stands on the builder; it lapses at {hh:mm} UTC`, `gate: drain: no barrier stands on the builder`, or, when one lapsed since the last lift, `gate: drain: the last barrier lapsed at {hh:mm} UTC without a lift`.

The list above follows it only when a barrier stands and the state is `waiting` or the box lists something. Then `gate: drain: {n} waiting ticket(s)` and `gate: drain: state {drained|waiting|open}`.

**`--lift`** asks `status` first: with no barrier standing it says `gate: drain: no barrier stands on the builder; nothing to lift` and exits 0, lifting nothing; else it lifts and says `gate: drain: barrier lifted; {n} waiting run(s) start in arrival order`, `n` being the tickets the box still holds, and exits 0.

**Usage and setup errors**, each before any call to the builder:

- Exit 2, each on its own `gate: ` line before the usage: `gate: --lift cannot be given with --status`, `gate: unexpected argument {word}` (any word that is not an option, `--` included), `gate: unknown option {opt}`, and the other problems the shared parser finds; and, from the routing file, `gate drain: {host} is not a builder remote.json lists` (no usage after it).
- Exit 1, on one line each: `drain: no remote.json at {path}`, `drain: no remote.json: LOCALAPPDATA is not an absolute folder`, `drain: ignored {path}: {why}` (a `remote.json` that cannot be used, or a builder entry it cannot use), and `gate: drain: {why}` when ssh cannot be found.

The exit is 0 for a drained builder, a lift, a lift of nothing and a status; 1 for a failure; 2 for usage; 0xC000013A on Ctrl+C.

A drain that returns does not stop a `git push`: the push to the builder's bare repositories is not gated, only counted by the box's drained test (a `git-receive-pack` or `git-upload-pack` process of the root), so a PC whose session lapsed between its preflight and its push can still push after "drained". `gate push` and `install-toolchains.ps1` neither check nor raise a barrier.

Test-seams builds accept hidden options on `drain` and no `--test-print-request`: `--test-poll-ms` (the 2 s ask), `--test-heartbeat-ms` (the 30 s line), `--test-drain-raise-ms` (the 5 minute re-raise), the switch `--test-drain-interrupt-after-list` (raises the interrupt once the first list is said), the switch `--test-drain-interrupt-before-raise` (raises it before the first raise call, which is then not made), `--test-remote-transport` (a local folder in place of ssh, as `gate check --builder` takes it) and `--test-remote-ssh` (which also needs `--test-remote-transport`).

`--test-drain-poll-ms` stays `gate run`'s.

### `gate prune`

`gate prune --builders` sweeps the PC's worktree records ([State root files](#state-root), `remote\<key>.worktree`) and prunes, on every builder, the work folder of each key whose worktree has been gone an hour. Its usage line is `       gate prune --builders`, after `gate drain`'s, and `gate prune --help` or `-h` prints it. It takes no other word but `--quiet`, which every build accepts and the usage line leaves out: a missing `--builders` is `gate: missing --builders` before the usage (exit 2), and any other word, `--quiet` without `--builders` included, is a usage error.

The exit is 0 when nothing failed, 1 on any failure and 2 for usage. Every line goes to stderr. It reads the state root as `gate run` resolves it, takes no slot and writes no claim, journal line or log. On the PC it writes a record's `goneSince`, and removes what the PC keeps of a key once the builders pruned it (The removal, below).

**The scan.** Every regular file `remote\*.worktree` is read in name order. A file that cannot be read as a worktree record, or whose name is no key, says `gate: prune: skipped {path}: it cannot be read as a worktree record` and is left. With no such file the sweep says `gate: prune: no worktree records` (nothing under `--quiet`) and exits 0; a `remote` folder that cannot be listed says `gate: prune: cannot list {folder}: {error}` and exits 1.

**The gone test.** A worktree is live when anything stands at its `root`. It is gone when nothing does, the drive of `root` as the record spells it is there, and either the git common folder `commonDir` is missing or `git --git-dir={commonDir} worktree list --porcelain` (bounded at 30 s) does not list `root` or lists it `prunable`; roots compare lower-cased, with `/` as `\` and no trailing separator. Anything else is unknown and the record is kept: `gate: prune: kept {key}: cannot tell whether {root} is gone: {why}`, the cause being a root off a drive, a missing drive, a root or common folder that cannot be looked at, git not found or failing, or git still listing the worktree.

**The mark.** The first sweep that finds a worktree gone writes `goneSince` in its record (`gate_state::v1::mark_gone`) and says `gate: prune: {key}: {root} is gone; its builders are pruned on a sweep an hour from now`. A later sweep that finds it gone less than an hour after `goneSince` says `gate: prune: {key}: {root} gone since {goneSince}; its builders are pruned once it has been gone an hour`. A sweep that finds a marked worktree live again writes the record back without `goneSince`, saying nothing. A write that fails says `gate: cannot write {path}: {error}` and the sweep exits 1.

**The builders.** Once a record has been gone an hour, its key goes to every builder `remote.json` lists, `host`, each `hosts` entry, then each `builders` entry no `hosts` alias names, routed or not, one at a time. A call is `gate box prune --root {root} --keys {key}...` over ssh, found along the gate's own `PATH`, with no `--stdin`, at most 50 keys a call and bounded at 300 s; a builder whose root has a space is not called. Each line the box says is passed on, its stamp taken off and its text escaped: a line of a swept key becomes `gate: remote {host} {line}` and any other `gate: remote {host}: {line}`, `{host}` being the builder's alias.

- The box's lines for a key are `pruned {key} (worktree gone)`, `kept {key}: in use`, `kept {key}: the builder is drained` and `could not prune {key}: {why}`. A kept key is no failure; a `could not prune` line is, whatever else is said of the key.
- A key the box said nothing of says `gate: remote {host} kept {key}: its box said nothing of it` and fails.
- A builder that cannot be reached says `gate: remote {host} could not be reached: {why}` (`no answer in 300 s` when the bound passed), fails each of its keys, and its later calls are not made. A box that does not take the keyed prune says `gate: remote {host} kept {key}: its box cannot prune by key` for each key, which is no failure.
- A builder whose root has a space says `gate: remote {host} cannot prune: its root {root} has a space` and fails its keys.
- Without `remote.json`: `gate: prune: no remote.json at {path}`; one that cannot be used: `gate: prune: ignored {path}: {reason}`; both exit 1, as does ssh not found (`gate: prune: {why}`), and neither is said when no record is a candidate.

**The removal.** After the builders, the sweep removes what the PC keeps of each key it called them for, in this order, as far as the builders pruned it.

- Once at least one builder pruned the key, the sweep takes the key's routed-run mutex without waiting before it removes anything of the key, and reads the record again: a record rewritten since the sweep judged it, or one that cannot be read, is left with nothing removed, silently. Another gate holding the mutex says `gate: prune: kept {key}: in use`, not a failure.
- Under the mutex, each builder that pruned the key loses its own files under the state root: `{key}.outputs-hashes`, `.inputs-hashes`, `.inputs-carried`, `.sent-build-hashes` and `.outputs-capped` (`remote\` for `host` and a `hosts` entry, `remote\{id}\` for a `builders` entry). A file that is not there counts as removed.
- Once every builder `remote.json` lists pruned the key, with none that held it or failed, it removes `warm\{key}.lock` first: a lock a warm-up holds open says `gate: prune: kept {key}: a warm-up holds it`, not a failure, and nothing more is removed for the key until a later sweep. Then it removes the shared files `{key}.outputs-pending`, `{key}.nothing-back`, `warm\{key}.json`, `warm\{key}.log`, `{key}.outputs-from` and `{key}.built-here`.
- Then the refs `refs/gate-remote/{key}`, `{key}-back` and `{id}/{key}` go, in the record's repo (`-C {commonDir} --git-dir={commonDir}`, when the common folder is there) and in each sibling repo `remote.json` allows that repo: a sibling is the folder beside the worktree root that routing lays out (`sibling_root`), skipped when it is not there, is a link or junction, or has no `.git` entry, and git runs there with `GIT_CEILING_DIRECTORIES` set to its parent.

  One listing and one `update-ref --no-deref --stdin` a repo. A `-back` ref whose commit is less than 7 days old is kept, with `gate: prune: kept {key}'s sync-back ref {ref}: it is less than 7 days old`, not a failure, and it keeps the record.

  Every git call of the sweep runs with none of the caller's `GIT_*` location or config variables, `core.fsmonitor=false` and no hooks; git that cannot be found fails the sweep with `gate: prune: cannot run git: {why}` before any record is judged.
- Last it removes `{key}.worktree` and says `gate: prune: removed {key} ({root})`.
- A key a builder did not prune keeps its shared files, refs and record, so the next sweep tries again.
- Failures exit 1 and keep the record: `gate: prune: could not remove {path}: {error}`; `gate: prune: kept {key}'s refs in {repo folder}: {git's last stderr line}`; `gate: prune: kept {key}: cannot take the mutex {name}: {error}`; `gate: prune: kept {key}'s refs in its siblings: {problem}`, the problem being the routing file's refusal to name the siblings. A builder's own file that cannot be removed also keeps the shared files and the record.

The sweep never runs `git worktree prune`.

**The triggers.** Besides the command, two things run the sweep with `--quiet`, so an empty scan says nothing.

- `gate push`, once it got past its early refusals, runs it after its builders through the ssh the push itself used, its lines on stderr unchanged ([`gate push`](#gate-push)). Its status never changes push's.
- A routed run whose builder call ended, passing or failing its command (a `--builder` pin included), starts `gate prune --builders --quiet` in the background after its log is closed. Not after Ctrl+C, not inside a box run, and not after a fallback before any builder call. The child is the gate's own exe with `GATE_STATE_DIR` set to the run's state root and that root as its working folder. It starts outside the caller's job (`CREATE_BREAKAWAY_FROM_JOB`), on a hidden console of its own (`CREATE_NO_WINDOW`), in a process group of its own and at below-normal priority, its stdin `NUL` and its stdout and stderr the file `remote\prune-sweep.log`, replaced at each start. When the caller's job refuses breakaway, nothing is started and no stamp is written.
  The run starts it at most every 6 hours. It reads `remote\prune-sweep.stamp` ([State root files](#state-root)) and starts the sweep when the stamp is missing, unreadable, 6 hours old or more, or more than 5 minutes in the future; it writes the stamp before the start and removes it if the start fails. Two runs ending together may both start a sweep. The run's status and output are unchanged and nothing is said, whatever fails.

A `test-seams` build starts the background sweep only for a routed run given the hidden switch `--test-prune-sweep`, which also hands the child the run's `--test-remote-transport` and `--test-remote-ssh`.

Test-seams builds accept hidden options on `prune` and no `--test-print-request`: `--test-remote-transport` (a local folder in place of ssh, as `gate check --builder` takes it), `--test-remote-ssh` (which also needs `--test-remote-transport`) and `--test-prune-gone-age-ms <n>` (the least age of a gone worktree, in milliseconds, 3600000 by default; a whole number, 0 allowed).

### `gate box`

Hidden from the usage and from `--help`. It is the builder side of a routed run, run over ssh on the builder ([The box side](#the-box-side)).

`gate box preflight|run|stop|boot|outputs|inflight|inputs|lfs|drain --stdin` takes exactly one verb and the word `--stdin`, and no `--state-dir`: the request is the first line of stdin, and every path comes from it. Three exceptions take no request and no `--stdin`: `gate box register-desktop`, `gate box prune --root <folder> [--keys <key>...]`, which takes the root and, optionally, the keys to remove, and `gate box hold --root <root> --holder <text> --ceiling <seconds> [--wait <seconds>] -- <command> [args...]` ([The box side](#the-box-side), `hold`).

Any other shape (no verb, no `--stdin`, any other word in its place, an unknown verb, an extra word) prints the box usage alone on stderr and exits 2. The usage is three lines: `usage: gate box preflight|run|stop|boot|outputs|inflight|inputs|lfs|drain --stdin (the base64 JSON request as stdin's first line)`, then the `       gate box prune --root <folder> [--keys <key>...]` line, then the `       gate box hold --root <root> --holder <text> --ceiling <seconds> [--wait <seconds>] -- <command> [args...]` line.

### Later subcommands

Further modes arrive as subcommands of the same binary, each designed with its issue: `gate status`. New options, such as `--timeout auto:<n>` (GATE-16) and `--slot auto` (GATE-8), exist only in the new CLI, never in the compatibility layer. The one exception is `--cd` (GATE-323), which the compatibility layer reads as `-Cd` because the launcher's callers need it.

### Compatibility layer

`gate.exe` also accepts the PowerShell gate's argument form, so the cutover breaks no caller:

`gate -Log <path> -TimeoutSeconds <n> -Slot heavy|light|critical|none [-StallSeconds <n>] [-Tail <n>] [-NoMarkers] [-Local] [-Alone] [-Reason "<why>"] [-Cd <folder>] [-Env NAME[=value]] -- <command>` and `gate -StopTree <pid>`.

- It accepts exactly the options `-Log`, `-TimeoutSeconds`, `-Slot`, `-StallSeconds`, `-Tail`, `-NoMarkers`, `-Local`, `-Alone`, `-Reason`, `-Cd`, `-Env` and `-StopTree`, compared case-insensitively, and maps each to the flag in the table above (`-StopTree <pid>` to `gate stop-tree <pid>`). `-Reason` is required with `-Local`, `-Slot critical` and `-Alone`, and its refusals use the compatibility spelling.

  Any other option, or a new-CLI flag in the old form (`-StateDir`, `--state-dir`, `--slot`), is a usage error (exit 2). `-TimeoutSeconds` refuses `auto:<n>`: `gate: -TimeoutSeconds must be a whole number above 0, got 'auto:900'`. The one exception is the hidden `--test-print-request` of a `test-seams` build ([Black-box suite harness](#black-box-suite-harness)).
- `--` is required before the command; the command never starts at the first word that is not an option.
- `-StopTree <pid>` (or `-StopTree:<pid>`) must be the only option.
- A value option never takes one of the twelve option names as its value: `-Log -NoMarkers` is `-Log` with no value, then `-NoMarkers`.
- The colon form `-Name:value` in one word is read as that option with that value (C2). A switch's colon form is read by its value, compared case-insensitively: `-NoMarkers:true`, `-Local:$true` set it, `-Local:false`, `-Local:$false` clear it, and any other value is a usage error. This fixes the PowerShell gate's reading of every `-Switch:<value>` as true (C2, CC-194).
- Values are validated as in the new CLI; the messages may name either spelling. The missing-slot line reads `gate: missing --slot heavy|light|critical|none` or `gate: missing -Slot heavy|light|critical|none`, and a bad slot says `gate: <option> must be heavy, light, critical or none, got '<value>'`.
- The layer is removed once no caller uses it. Until then, every reader of command-line text (ops-tower's activity join, the hooks that inspect a gated command) matches both spellings.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | The command exited 0 and its log holds no failure marker. `gate check` found nothing missing or too old (with `--builder`, nothing missing). |
| `N` | The command's own non-zero status, passed through whole: the gate exits with the command's 32-bit exit code (300 stays 300; 0xFFFFFFFF reads as -1 where a caller reads it signed), and the FAILED line and the journal show it signed (a routed run whose connection was lost reports 255, RC7). |
| 1 | The command exited 0 but its log holds a failure marker (never with `--no-markers`, V2); or the gate itself failed after the command started (X4); or it failed before queueing (the working directory, the command's environment, the log's folder or an earlier log that cannot be removed); or a routed run passed but its changes or outputs could not be brought back for a reason other than a lost connection (RC9, D1); or `gate check` found something missing or too old, or could not run (outside a git worktree; with `--builder`, also an unreachable builder, a different build, no box, or a `remote.json` missing or unusable). |
| 2 | Usage: a missing, unknown or invalid input; a `.cmd` or `.bat` word the gate refuses (C5); a `--log` inside the state root; a critical gate under a heavy or light slot marker (K7); `--alone` with a slot other than critical, or inside a gate's command; a `--local` or `--slot critical` run without `--reason`, or with a blank one; a gate whose script lost its caller (below). Nothing is queued. `gate box` also uses 2 for a refused payload and for a refused subfolder or batch word ([The box side](#the-box-side)). |
| 124 | The watchdog killed the command: STALLED, TIMED OUT or BACKSTOP (X2), for a routed run by the builder (TIMED OUT, BACKSTOP) or the client (STALLED, BACKSTOP); or a routed run's fetch outlived its transfer ceiling: TRANSFER TIMED OUT. |
| 127 | The command could not be started (C7); the slot is released at once. `gate box run` uses it for the same failure. |
| 75 | A builder refused a routed run (`gate box`): busy (`gate box run` finding its work folder's lock held), unreachable, parity, rebooted, tools, layout (the box could not clear or lay out its work folder), inputs, lfs, desktop (the box could not give the run the builder's desktop), install (the builder's gate or its desktop helper is not installed where only administrators can change it), version. The PC side reads the refusal and falls back to a local run, so a caller sees 75 only as a command's own status, or as a refusal that carried no reason (T5). |
| 74 | A routed command exited 0, but a transfer after it (the sync-back's `ls-remote` or `fetch`, or the outputs pull) failed as a connection fails on every attempt, so its changes or outputs were not brought back: TRANSFER FAILED, journal verdict `transfer-failed`. A command that itself exits 74 reads the same by status alone; the last line and the verdict tell them apart. |

A gate with no live parent gate (no marker, a malformed one, one whose parent ended, or one that does not bind) walks its ancestors at start, right after admission and before any slot, log or plan. When the walk ends at a shell that was running a script or a command whose parent is gone or reused, it refuses with exit 2 and `gate: refused: the script that started this gate lost its caller ({image} {pid}, whose parent {ppid} is gone); stop the sequence with gate.ps1 stop-tree {pid}`.

The reason is a background script whose shell was killed: it keeps starting gate steps with nobody to stop it.

A gate whose own token is elevated skips the walk: an agent's killed sequence never runs elevated, and Windows `sudo` starts its elevated command and exits, which would read as a lost caller. A gate whose token cannot be read walks as an unelevated one does.

Git Bash's fork emulation leaves a script's `bash.exe` with a Windows parent that has already exited while its launching shell lives, so where the walk meets a process whose Windows parent is gone it asks the MSYS runtime for the parent it keeps (the MSYS parent; `msys_parent` in gate-win32 `msys.rs`).

The runtime's records are named kernel objects: the gate lists `\BaseNamedObjects` for `msys-*` and `cygwin1S*` directories, reads the `winpid.{pid}` link of the process, then the `cygpid.{msys pid}` sections (pid, state, ppid, Windows pid) of it and of its parent, and trusts an object only when its owner is the current user (or, for an elevated token, the token's default owner); a directory that will not open is skipped.

With no record the process is judged on its own, by the shell rule below. With an MSYS ppid of 1 or less (started from outside MSYS) its Windows parent is its real one, judged the same way. A live MSYS parent (not marked ended, started no later than the process) continues the walk from that parent's Windows pid, at most 16 times, after which nothing is refused.

A gone or ended MSYS parent makes the process the orphan candidate, refused only when it is a shell running a script or a command. Anything unreadable refuses nothing. So a gate started from a Git Bash script, directly or through `pwsh tools/gate.ps1`, runs while the script's launching shell lives and refuses once that shell was killed.

A Git Bash script that passes `/c` to `cmd` writes `//c` or sets `MSYS_NO_PATHCONV=1`, since MSYS path conversion turns `/c` into `C:/`.

The shells that count are `bash`, `sh`, `zsh` and `dash` with `-c` or a non-option argument, `pwsh` and `powershell` with `-File`, `-f`, `-Command`, `-c`, `-EncodedCommand`, `-e`, `-ec` or a first non-option argument, and `cmd` with `/c` or `/r`. Switches match whole words in any case.

A `pwsh` or `powershell` with `-NoExit` or `-noe` among its arguments is interactive whatever else it carries (Explorer's "Open PowerShell window here", the Developer PowerShell shortcut and a terminal multiplexer's pane shell start as `-NoExit -Command`), and any other abbreviation of a switch is no match. An interactive shell, a non-shell or anything unreadable refuses nothing, and a step already running is not watched.

The codes are kept (V4), with 75 added for refusals. 124 is the status coreutils `timeout` uses, so no caller mistakes a kill for the command's own status; a command that itself exits 124 is told apart in the journal's `verdict`.

## Output lines

Every line the gate says is written and flushed as it is said, never held until exit, so a running or killed gate's lines are already visible.

Every line the gate writes for itself starts `gate: ` and goes to stderr and to the log, stamped and tagged `gat` there and written as it is said ([Log and tail](#log-and-tail); V1), except the passed line, the stop-tree summary and the `gate burn` start line, which go to stdout (V3, X6). Three forms are the published interface that callers may parse; they keep their wording, and a change to them is a breaking change:

| Line | Where | Meaning |
|---|---|---|
| `gate: STALLED: ...`, `gate: TIMED OUT ...`, `gate: BACKSTOP: ...` | at line start on stderr, and in the log at the start of the text after the line's stamp and `gat` tag | The watchdog killed the command; the exit is 124. Match `^gate: (STALLED\|TIMED OUT\|BACKSTOP)` against the text ([Log and tail](#log-and-tail)). A stalled `ssh` command's STALLED line is followed by the `gate: a command run over ssh shows no CPU ...` line (W6a). A routed run's builder says `gate: TIMED OUT on <host> after ...` and `gate: BACKSTOP on <host>: ...` ([Routing](#routing)). A gate wrapping a gate reports the inner kill through its own marker scan (V2). |
| `gate: TRANSFER TIMED OUT after <n> s fetching the outputs` (or `fetching the sync-back`) | at line start on stderr, and in the log at the start of the text after the stamp and tag | A routed run's fetch after the command outlived its transfer ceiling of 1800 s, counted from the fetch's own start; the exit is 124 and the release verdict `transfer-timed-out`. It is a failure marker of the log scan; it does not match the `TIMED OUT` of the command's ceiling. |
| `gate: TRANSFER FAILED: the command exited <n> on <host>; <reason>` | at line start on stderr, and in the log at the start of the text after the stamp and tag | A routed run's transfer after the command (the sync-back's `ls-remote` or `fetch`, or the outputs pull) failed as a connection fails on every attempt ([Routing](#routing), retry). After a command that exited 0 the exit is 74 and the release verdict `transfer-failed`; after one that exited `<n>` other than 0 the run keeps `<n>` and the verdict `failed`. It is a failure marker of the log scan; the line before each retry, `gate: bringing the results back from <host> failed (<reason>); trying again (<k> of 2)`, is not. |
| `gate: waiting for the machine (critical)` | stderr | A critical gate is queued and waiting for the machine (K5); the stable part is the prefix, and the status after it is prose. A critical gate that takes the machine at once says nothing. |
| `Full output: <path>` | the end of the verdict line: `gate: passed in ... Full output: <path>` on stdout, `gate: FAILED (status <n>). Full output: <path>` on stderr | Where the whole log is (V3). |

Every other `gate: ` line is prose: it informs people and agents and may change in any release. That includes the passed and FAILED lines apart from their `Full output:` tail, the wait lines, the heartbeat lines, the diagnostics block and the stop-tree summary.

The lines of a run that starts its command:

- Passed, on stdout: `gate: passed in <w> s (load-adjusted <a> s, ceiling <n> s). Full output: <log>`, `<w>` and `<a>` in whole seconds rounded half to even. Once the sampler failed there is no load-adjusted time, and the line reads `gate: passed in <w> s (load-adjusted unknown: the sampler failed, ceiling <n> s). Full output: <log>`.

  A command that ends with status 0 after its clock reached the ceiling (a gate starved between samples can miss the kill) still passes, and the line names the ceiling it ran past: `ceiling <n> s` becomes `past its ceiling of <n> s`. The clock is the one the ceiling kill reads, the load-adjusted time, or the wall time once the sampler failed, compared as the kill compares it (reached or passed).
- The cores hint, on stdout just before the passed line, when its conditions hold ([Cores hint](#cores-hint)): `gate: used <avg> cores on average (peak <peak>, <free> free) on a light slot; work that keeps more than 2 busy belongs on heavy` or `... on a heavy slot; work that keeps under 1.5 busy fits on light`.

  Two notes go to the log only, and the hint is left out: `gate: cannot read the command's CPU at its end: <error>; its cores are unknown` and `gate: cannot tell whether a gate under this one took a slot of its own: <error>; its cores are unknown`.
- The cores hint of a run on no slot, in the same place and under the same conditions, has no free cores: `gate: used <avg> cores on average (peak <peak>) on no slot; work that keeps 1.5 or more busy belongs on light or heavy`.
- The cores hint of a commit run ([Commit run](#critical-drain-and-nesting)), in the same place, names the commit's hooks: `gate: the commit's hooks used <avg> cores on average (peak <peak>, <free> free) on a light slot; hooks that keep more than 2 busy belong on --slot heavy` or `... on a heavy slot; hooks that keep under 1.5 busy fit on --slot light`. A commit run gets no route hint.
- The route hint, on stdout after any cores line and just before the passed line, when its conditions hold ([Cores hint](#cores-hint)): `gate: ran <n> s on a light slot; on heavy, remote.json would send this command to a builder`, `<n>` the whole unpaused seconds, rounded down.
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

  A word outside the worktree whose path lies in another git worktree of a repo `remote.json` lists says instead `gate: not routed: the command works in <tree>; run it with --cd <tree>` ([Routing](#routing), the decision). `<tree>` is the worktree's absolute path written with `\`, escaped like the sibling lines, and in double quotes when it holds a space or any of `; & ' ( ) , = $` or the backtick.

  A matched command with a word that names an existing project input git ignores routes, and the inputs step carries the ignored files as exact paths marked `[carried]` ([Routing](#routing), the decision).

  One whose files cannot go stays here, saying `gate: not routed: <path> is gitignored and a command's inputs carry at most 64 MiB in 2000 files; its files hold <n> MiB in <k> files` (`<n>` rounded up to whole MiB), `gate: not routed: <path> is gitignored, and a sibling pair does not send inputs yet` or `gate: not routed: <path> is gitignored and its files cannot be sent: <why>`, `<path>` as the word wrote it and escaped. A light run naming a gitignored project gets no route hint.

  A failed `git check-ignore` that leaves a word unjudged says `gate: cannot tell which words are gitignored: <why>` in the log, and the run routes.

  It is also said for a word of the command that is not valid Unicode (`gate: not routed: a word of the command is not valid Unicode`), or a fact that could not be read (`gate: not routed: cannot run git: <error>`, or the working directory or git's output).

  These lines keep a matched command here for siblings: a problem with the repo's `.gate/siblings.toml` (`gate: not routed: .gate/siblings.toml: <problem>`, control characters and reordering marks escaped), a declared sibling with no worktree beside the root (`gate: not routed: sibling <name> not found beside the worktree`), a root folder name the builder cannot lay out (`gate: not routed: the worktree folder name <leaf> cannot be laid out on the builder`).

  Also a declared sibling `remote.json` does not allow: `gate: not routed: sibling <name> is not allowed in remote.json`, or `siblings <a>, <b> are not allowed in remote.json` for more, with `: <problem>` added when the `siblings` value cannot be used.

  A command the repo's `.gate/local.toml` keeps here says `gate: not routed: .gate/local.toml keeps it here (<pattern>)`, the first pattern its words match, escaped; a file the gate cannot use says `gate: not routed: .gate/local.toml: <problem>`, escaped, and keeps every routed command of the repo here ([Routing](#routing), the decision).

  For the builder's desktop: a problem with the repo's `.gate/desktop.toml` (`gate: not routed: .gate/desktop.toml: <problem>`, escaped), and the first declared pattern the command matches when `remote.json` does not allow it (`gate: not routed: desktop pattern '<pattern>' not allowed in remote.json`, with `: <problem>` added when the `desktop` value cannot be used).

  A sibling folder that cannot be carried says `gate: not routed: sibling <name> <reason>`, `<reason>` being `is a link or a junction`, `is not a folder`, `has a .git that is a link or a junction`, `is the worktree's own folder` or `is the same folder as sibling <other>`.

  A repo's warm-up (warm-up consent and a `.gate/warmup.toml`) keeps no run here: its record, lock and file problems are not part of the route ([remote.json](#remotejson)).

  The remote client keeps a routed run here, after the route, with `gate: not routed: <who> <why>`, status 75 and the fallback reason `client`, then `gate: remote <host> client could not start: <who> <why>; running here`. `<who>` is the worktree root's folder name for the main repository and `sibling <name>` for a sibling.

  A run of one repository can say `<leaf> is not the folder the route was decided for` (the root's folder id is not the one the route read) and `<leaf> cannot be checked: <error>`.

  A run that carries siblings holds every folder first and can say, for the root or a sibling, `is a link or a junction`, `is not a folder`, `has a .git that is a link or a junction`, `is not the folder the route was decided for`, `differs in case from its declared name`, `cannot be held: <error>` and `cannot be held: its path is not valid Unicode`.

  For the root alone: `is under a folder that is not the one the route was decided for`, `cannot be held: it is on a network volume` and `has no parent folder`.

  Once git is asked where each repository is: `<who> is not the top folder of its git repository: git names <path>`, `<who> shares its git repository with <other>` (<other> the earlier repository sharing its common git folder, by folder id), `<who> has a git folder reached through a link or a junction`, `<who> cannot be checked: <error>` (a git or common folder that cannot be opened or read).

  Then `<who> cannot be laid out on the builder: <why>` (the box's reader refused the pair; <why> is its usage line without `gate: `, the main repository's name when no one sibling is at fault), `sibling <name> cannot be sent: <why>` (a sibling's git call or push failed) and, after a save or the end of the run, `<who> moved while it was held` and `<who> changed its git location while it was held`.

  A lock another routed run holds past the lock wait says `gate: not routed: <who> is busy with another routed run`. A pair's root spelled other than it was at its check has no git location recorded, and fails the run with `gate: the remote run failed: the git location of <root> was not recorded at its check`, status 1 and no fallback.

  A gate that is not heavy, has a covering parent, matches no pattern or has no `remote.json` says nothing.
- A commit run ([Commit run](#critical-drain-and-nesting)), on stderr and in the log: asked for on no slot, `gate: a commit's hooks take a slot: pass --slot heavy or --slot light` once, before its slot wait (not said under a covering parent or inside a box run). A critical or `--alone` gate inside a commit's hooks is refused, exit 2, on stderr only, with the earlier log left in place: `gate: inside a git commit's hooks; a critical or --alone gate cannot run there`.
- A local run that drops its dotnet `--no-build`, on stderr and in the log, before it takes a slot: `gate: dropped --no-build: <reason>; building on a heavy slot`, `gate: dropped --no-build: <reason>; building on its critical slot` for a critical run, or `gate: dropped --no-build: <reason>; building on the parent gate's <heavy|critical|light> slot` for a gate covered or lent a slot ([Routing](#routing)).
- An `--alone` run whose dotnet `--no-build` would be dropped, refused with exit 2 before it takes a slot, on stderr only, nothing journaled and an earlier `<log>` and `<log>.err` left untouched: `gate: refused: --no-build cannot stand: <reason>; an --alone run measures the command as given, so build first (a passing local build of the project clears the record) and run it again` ([Routing](#routing)).
- A passing local build that would add a project to the nothing-back record when the record would pass 4096 bytes, on stderr: `gate: cannot add to <path>: the record is full`, the record left as it was ([Routing](#routing)).
- A passing local build that writes the built-here record, in the log: `gate: built-here record trimmed to <n> entries`, `gate: built-here record not written: entry over <cap> bytes`, `gate: built-here record not written: work folder <key> is held`, `gate: waiting for the work folder's key to remove <path>` and `gate: removed <path> without the work folder's key: <why>`, with `gate: cannot write <path>: <error>` on stderr when the record or the lock fails ([Routing](#routing)).
- A routed run whose built-here record cannot be removed before an attempt, on stderr and in the log: `gate: remote build not sent: cannot remove the built-here record: <why>`, then the `inputs` fallback line ([Routing](#routing)).
- A passing local build that would add a project to the nothing-back record and cannot take the worktree's work-folder key, on stderr: `gate: cannot add to <path>: another run of the worktree held the work folder's key for <n> ms` (`held for <n> ms` when the build's built-here write already waited for the key), `gate: cannot add to <path>: Ctrl+C ended the wait for the work folder's key`, or `gate: cannot add to <path>: cannot open the mutex <name>: <error>` / `cannot take the mutex <name>: <error>`; the record is left as it was ([Routing](#routing)).
- A routed run, on stderr and in the log, says first `gate: routed <given> as <name>` when it sent the command's first word, which named a program by an absolute path outside the worktree, as the program's name (`<given>` the word as given, `<name>` the name sent, each escaped; [Routing](#routing), the in-worktree path rewrite).

  After that line and any `gate: dropped --no-build: the last build may have run on the other machine` (the routed dropped line, which a run with a send leaves to its client, as [Routing](#routing) says; the local one is the line above) and `gate: outputs: none brought back for a --no-build command` lines, it says `gate: routed with siblings: <name> (<path>), ...` when the repo declares siblings.

  The line names each sibling in declared order with its root as routing spells it. While it waits for a remote slot: `gate: waiting for a remote slot on <host> (<n> busy)` and `gate: still waiting for a remote slot on <host> (<s> s)`, as a local wait says them, ETA included. While the builder itself refuses the run `full`, its own slots all held: `gate: waiting for a slot on the builder <alias> (its slots are all held)` once, then `gate: still waiting for a slot on the builder <alias> (<s> s)` each heartbeat ([Routing](#routing)).

  A run the builder does not take says why and goes on to the next builder or here: `gate: remote <host> <reason>; running here`, with `<reason>` one of `full` (no remote slot, or no slot on the builder itself, within what is left of the run's 60 s remote wait), `busy`, `unreachable`, `parity`, `rebooted`, `tools`, `layout`, `version`, `inputs` (the run's ignored inputs could not be sent, or the builder's work folder no longer held them at the run), `lfs` (the run's Git LFS content could not be sent or listed, or the builder could not hold it or make the run's LFS scratch folder), `desktop` (the builder could not give the run its desktop), `install` (the builder's gate is not installed where only administrators can change it) or `root` (the builder's work root is not locked down, or another process holds it open), and `gate: remote <host> client could not start: <detail>; running here` when the remote client cannot start.

  When the reason is one the run retries and a later builder is left to try ([Routing](#routing), The next builder), the line ends `; trying <next alias>` in place of `; running here`: `gate: remote mac-1 version; trying host`. The last builder says `; running here`. The next builder is built before the line is said, so the log holds the refusing attempt's lines, then the next builder's own read notes, then this line, then the next builder's route lines.

  A candidate skipped for a reason other than a steer-off says it in the log only: `gate: not trying <alias>: <reason>` (`gate: not trying builder: ignored <remote.json path>: repos is missing`). A steer-off says nothing.

  A `layout` fallback removes the worktree's warm-up record, but only while it is the `warm` record of the builder the run went to, for the warm-up file on disk, under the remote key; another builder's record and the other fallbacks keep it.

  The record is not dropped on `version`, `tools` or `inputs`: that would re-warm every worktree after each `gate push` for nothing, since the preflight's `.warm` check catches a folder that really went cold. A removal that fails says `gate: cannot remove the warm-up record <path>: <error>` before that hint.

  A routed run whose repo declares `inputs` says `gate: inputs: <n> file(s), <X.X> MB (<Y.Y> MB compressed) in <Z.Z> s` once a put has sent files.

  A routed run whose snapshot holds Git LFS pointers says `gate: lfs: <n> object(s), <X.X> MB in <Z.Z> s` once a put has sent objects, and `gate: lfs: <n> object(s) present` in the log only when the builder's store held them all. The reason of an `lfs` fallback is the log line `gate: remote lfs not sent: <reason>` ([Routing](#routing), Git LFS).

  Four lines go to the log only: `gate: inputs: <n> file(s) unchanged` (the manifest's entries, said when the builder needed none), `gate: inputs: <glob> matched nothing`, `gate: inputs: <path> is in the snapshot; not sent as an input` and `gate: inputs: <folder> is another repository; not sent as an input`. The reason of an `inputs` fallback is the log line `gate: remote inputs not sent: <reason>` ([Routing](#routing), Inputs).

  A remote part the gate cannot start says the second line before it takes a remote slot when ssh is missing, else after it ([Routing](#routing), The remote client's run).

  A Ctrl+C while waiting for the remote slot says `gate: interrupted while waiting for a remote slot`. One during the client's call, or raised before it, ends the gate with `gate: interrupted before the command started` and no fallback, even when the client refused the run; the remote slot is released.

  A Ctrl+C during the walk over the later builders (before one is built, while it is built or once it is built, before it is tried) ends the gate the same way after a refusal, and no further builder is tried: it says only `gate: remote <host> <reason>`, the fallback line without its `; trying ...` or `; running here` ending, then `gate: interrupted before the command started`. The refusing builder's slot was already let go, so the release carries `"heldSeconds":0.0`.

  A Ctrl+C while the client waits to start ends the gate the same way with no slot to release, and journals an `abandon` after the `queue` line, not a `release`.

  A routed run that finds its work folder's key held by another run on this PC says `gate: waiting for this worktree's work folder on <host>, which another of its runs holds`, with ` (about <figure> left)` after it when the key's latest claim (by `started`) has an expected time not yet reached, `<figure>` by the gate's figure rule (`<n> s` below 90 s, else whole `<n> min`, never below 1). Each heartbeat it says `gate: still waiting for this worktree's work folder on <host> (<s> s)`.

  A routed run that cannot write its worktree record ([State root](#state-root), Files) says `gate: cannot write <path>: <error>` on stderr, at most once per routed run however many builders it tries, before the first builder is called; the run's status, verdict and journal are as they would be.

  A routed run that cannot write the outputs-from record ([State root](#state-root), Files) says `gate: cannot write <path>: <error>` for it, as it does for the worktree record, and changes nothing else of the run.

  `gate warm` writes the record only when it can take the key's mutex at once: when another gate holds it, warm writes nothing and says nothing, and when the mutex cannot be opened or taken it says `gate: cannot take the mutex <name>: <error>` and warms up without writing the record.

  A warm-up lock that cannot be probed says `gate: cannot probe the warm-up lock <path>: <error>` once (path and error escaped) and the wait goes on. A Ctrl+C during that wait, or raised at the key take, says `gate: interrupted while waiting for this worktree's work folder on <host>` and ends the gate with 0xC000013A, never a `busy` fallback.

  A routed run that passed says `gate: passed in <w> s on <host> (load-adjusted <a> s, ceiling <n> s; command <c> s, push <p> s[, inputs <i> s][, lfs <l> s][, sent build <b> s][, sync-back <s> s][, outputs <o> s]). Full output: <log>`, with `past its ceiling of <n> s` in place of `ceiling <n> s` as the local line has it.

  In it `<w>` is the attempt's whole wall time; `<a>` is the load-adjusted time the builder counted for the command, the clock `--timeout` bounds; `<c>` is the run call's own wall time; the push is always named, the inputs calls and the LFS calls (diff and put together), the sync-back and the outputs fetch only when they ran, in the order they ran, each in whole seconds rounded half to even.

  With no figure from the builder (its sampler failed) the clause reads `load-adjusted unknown: the sampler failed` and the past-ceiling check uses `<c>`. The client's status is the gate's own, passed through as `N`.
- The stale line, on stderr and in the log, before the passed line, when a routed dotnet build (verb `build`, `test`, `publish`, `pack`, `run` or `msbuild`, with no `--no-build` of its own) passed and did not bring everything back: `gate: routed build left stale: <folders>`.

  The folders are every PC-side folder (no cap) that a judging glob matches and no brought-back glob covers, judged per file: a folder whose every matching file came back is not named.

  The judging globs are the run's `outputs` (the repo's own list, kept as it is when it is `[]`; the default globs when its entry is absent, none on a macOS builder), plus the defaults `**/bin/**` and `**/.godot/mono/temp/bin/**` always. The folders are sorted, relative to the worktree root, `/`-separated, escaped and joined with `, `.

  A pair (a run with siblings) names each sibling's output folders after the main's, written `../<sibling>/<folder>` (`../lib/src/App/bin`), judged by the same globs with nothing brought back, since no sibling's output ever comes back; the siblings follow in declared order.

  A glob names the folder where its last plain folder part ends, at the first place from the left where its plain parts match whole path components: `**/bin/**` and `**/bin/**/*.dll` name each `src/App/bin`, `out/**/App.dll` names `out`, `publish/App.exe` names `publish`; a glob with no plain folder part (`**`, `*.dll`, `**/*.dll`) names nothing. The line may name a folder this build did not touch.

  It is not said after a failed build (an exit 0 whose log reports a failure included), a fallback or a command that is not a build, and it neither writes nor clears the nothing-back record. A walk of the worktree that fails says `gate: routed build may have left stale output; could not list it: <reason>` instead, after the folders line.

  A pair's sibling whose walk fails says `gate: routed build may have left stale output in ../<sibling>; could not list it: <reason>` on a line of its own, after the folders line and after the main's failure line; the folders of the trees that could be listed are still named. A run of one repo says the same lines as without siblings.
- The restore lines, on stderr and in the log, after a routed dotnet build that passed and brought everything back restores its target here ([Routing](#routing), The restore here), after the command's tail and before the stale, cores, route and passed lines, which stay the run's last. Each line that is not a success ends `; a --no-build run here builds first`, and the target and the side file's path are escaped.
  - `gate: restored <target> here in <n.n> s, so a --no-build run here can use the routed build's outputs`.
  - `gate: dotnet restore of <target> here failed with exit <code> (<log>.restore.log); a --no-build run here builds first`.
  - `gate: dotnet restore of <target> here ran past <limit> and was stopped (<log>.restore.log); a --no-build run here builds first`, and `gate: dotnet restore of <target> here was interrupted (<log>.restore.log); a --no-build run here builds first` when Ctrl+C was pressed during it, whatever its exit code.
  - `gate: cannot run dotnet restore here: <why>; a --no-build run here builds first`, when the restore could not be started or its working folder could not be read.
  - `gate: cannot write the end of <log>.restore.log: <why>`, after the line of a restore whose output did not all reach the side file.
  - The skip lines, `gate: not restoring <target> here: <reason>; a --no-build run here builds first`, where the reason is `it is not a project or solution file`, `Ctrl+C was pressed` or `under 10 s of the routed command's ceiling is left`.

  The run's status and its passed line are the routed command's whatever the restore says.
- A critical gate, on stderr and in the log: the `waiting for the machine (critical)`, `still waiting for the machine (critical)`, `holds the machine (critical)` and `interrupted while waiting for the machine` lines, in [Critical drain and nesting](#critical-drain-and-nesting). A gate asked for `--alone` says `the machine alone (critical)` in the first three, and adds the `load at` lines of its load snapshot to the log only.
- Nesting, on stderr and in the log, from the slot marker in the gate's environment: the `running under` (including the lend's `while a critical request drains`), `ignored GATE_SLOT_HELD`, `could not judge GATE_SLOT_HELD` and `does not cover` lines, the barrier's `waiting for a heavy slot behind the critical request`, `waiting for a light slot beside the critical request` and `waiting for a light slot behind the request of gate <pid> that holds the machine alone` lines, all in [Critical drain and nesting](#critical-drain-and-nesting).

  The refusal of a critical gate, and of an `--alone` gate under a gate's command, is there too.

  The late take adds `the parent gate <pid> ended; ...` and `cannot watch the parent gate ...` lines, in the same section.
- A light gate the barrier holds back with no place beside the request, whose command would route, says `gate: held back by the critical request of gate <pid>; sending this light run to <alias>` on stderr and in the log, then the route lines, in place of its waiting line ([Routing](#routing), "What routes"). It is prose.
- The gate prints no `queued for` or `took ... slot` line, for a local, a routed or a no-slot run: the queue and the take are in the journal, and a wait longer than 30 s says `gate: still waiting for a <kind> slot (<s> s)`, with the busy count and an ETA when every slot is busy ([Slots](#slots), step 6). A no-slot run never waits.
- The expected-time line, on stderr and in the log, once a run that takes a slot has written its claim and woken the waiters and before the command's output: `gate: expected about <n> s (usual <n> s, p90 <n> s, <runs> runs)`, or in whole minutes from 90 s: `gate: expected about <n> min (usual <n> min, p90 <n> min, <runs> runs)`. A local light or heavy take says it, a critical gate after its holds line, a routed run at its remote take, and a fallback a second one at its local take.

  `about` is the mean wall time over the estimate's window (the last 30 passing runs or those within 14 days, whichever holds fewer), `usual` the median of the 5 newest, and every figure is rounded and floored at 1 s (or 1 min). An estimate from the family key ends `<runs> runs of similar commands)`. A key with fewer than 5 passing runs, a `--slot none` run, and a run a parent's slot covers or lends say nothing ([design/gate-durations.md](design/gate-durations.md), Estimate).
- The ceiling lines, on stderr and in the log, right after the expected-time line (at each take, a lent take included): `gate: ceiling 42 min (3x this command's p99 of 14 min, above the given 15 min)` only when history raised an auto ceiling above its floor, then `gate: the ceiling 10 min is below this command's load-adjusted p90 of 14 min` when the resolved ceiling, given or auto, is below the estimate's adjusted p90.

  The ceiling is in load-adjusted seconds, so the hint compares it with the adjusted p90 alone: an estimate without one says no hint, however long its wall p90.

  Their figures are the expected-time line's: `<n> s` below 90 s, whole minutes from 90 s. An estimate from the family key reads `similar commands'` in place of `this command's`, and a family key whose p90 is above 2x its p50 does not raise the ceiling.
- The hang hint, once a run, on stderr and in the log: `gate: still running after 42 min, 3x its usual p90 of 14 min; running here for 42 min` (`similar commands' usual p90` for a family key), `k` rounded down, from the run's passing 3x the p90 of its take's estimate.

  The last clause names the phase and how long it has run, in the same spans: `running here` for a local run, `running on <builder>` for a routed run call, and `pulling outputs from <builder>` for a routed outputs pull.

  A local run counts wall time less paused time, checked at each watch sample; a routed run counts the wall time since the builder's start marker reached the client, said live, and its hint can fire during the outputs pull (still at most once per run), the pull's phase time (`for <d>`) counted from the pull's start, a retry of the outputs call included. It is never said for a usual p90 under 1 s, nor for a cold routed run ([Cold run](GLOSSARY.md#cold-run)), and kills nothing.
- The durations store's notes, on stderr and in the log. Said once a run: `gate: cannot read <path> (<reason>); expected times are unavailable` for a store that is torn or cannot be read, said before the run queues, so it can come before a waiting line, and `gate: could not write <path> (<reason>)` for a failed store write, which leaves the run's exit code as it was.

  `gate: cannot rebuild <store path> (<reason>); expected times are unavailable` when the run's claim on a rebuild fails other than by another gate holding it.

  A rebuild says `gate: <note>` for each of `skipped <n> release line(s) of <file> that do not parse`, `could not read <path>: <error>`, `could not list the archive folder <dir>: <error>` and the swap's own notes, and `gate: could not remove <marker path>: <error>` when an interrupted rebuild cannot remove its marker.
- Notes that never fail the gate: `gate: cannot remove the log <path> of the attempt: <error>` (a routed run that fell back could not delete the log the client wrote) and `gate: cannot read this gate's priority class, so the command runs below normal: <error>`.

## Slots

### Pools and names

A slot is a named mutex in the logon session's `Local\` namespace, held by the gate for the command's whole run (kept, S1, S3). A gate killed while holding a slot is the crash recovery: the next gate takes the slot over, where a semaphore's count would be lost for good. It sees the takeover in either of two ways, the mutex coming back abandoned or the slot's claim naming a gate that is no longer alive ([Abandoned slots](#abandoned-slots)).

| Pool | Mutex names (default root) | Who takes it |
|---|---|---|
| heavy | `Local\gate-slot-<i>` | `--slot heavy`; a critical gate takes every heavy slot. |
| light | `Local\gate-light-slot-<i>` | `--slot light`; a critical gate takes every light slot but the beside allowance of its count (K2), and an `--alone` one every light slot. |
| remote | `Local\gate-remote-slot-<i>` | a heavy run routed to `host`, for the length of its remote attempt (T5). |
| builder's remote | `Local\gate-remote-<pool id>-slot-<i>` | a heavy run routed to a `builders` entry, or pinned to a `hosts` entry, one pool per entry, so a run to one builder never waits behind runs to another. |
| critical | `Local\gate-critical` | a critical gate, for its whole run, so two critical requests run one at a time in arrival order (K1). |

A pool id is the first 8 lower-case hex digits of the SHA-256 of the entry's lower-cased alias (`mac-1` is `1f0efc62`); `host` has none and keeps the plain names. A `hosts` entry has one too, and only a pinned run uses its pool. `gate_router::builder_id` computes it, for the gate and for `gate-state`'s dashboard read alike.

The pools never wait on each other, so a serial test run does not queue behind builds (S1). A gate takes and releases its mutexes on one OS thread, since Windows mutex ownership is per thread.

Except for a commit run ([Commit run](#critical-drain-and-nesting)), a `none` run is in no pool: it takes no slot, mutex, ticket, claim or covered marker, never waits, wakes no waiters and starts at once, even while a critical gate holds the machine. It is for a command that only dispatches or watches remote work (`gh run watch`, a deploy script polling a server, a remote image build).

It still has its job, so `gate stop-tree` reaches it, and it runs at BelowNormal. A critical drain does not wait for it, it is not in the take's `otherClaims`, and the barrier never holds it.

**Builder-side slots.** A builder, Windows or macOS, may count slots of its own, whatever PC the runs come from: the PC's pool of a builder counts only that PC's runs, and a GitHub Actions runner job on the builder takes none of them. A builder-side slot is no mutex. It is a claim file in `{root}\slots\`, counted against the count in `{root}\slots.json` ([Formats](#builder-side-slots)), and held by a routed run's drain session or by a [hold](#the-box-side) (`gate box hold`). A builder with no `slots.json` counts none: every routed run and every hold behaves as without this section, and a hold exits 2.

### Counts

The default count of a pool is `max(1, floor((logical processors - 1) / threads))`, with threads 4 for heavy and 2 for light (kept, S2). `slot-counts.json` in the state root overrides either count with a whole number from 1 to 64. It is re-read on every pass of the wait loop, so a count changed while gates wait applies to them, and a gate holding a slot the new count leaves out keeps it.

A missing file or key means the default; an unreadable or invalid value is ignored for that pool with one line, `gate: ignored <path>: <reason>; using <n> <kind> slots`, said once per wait and only for the waiting gate's own kind (S2).

A processor count the system will not give is taken as 1, with `gate: cannot read the processor count: <error>; counting 1`, said once. The remote pool's size comes from `remote.json` (T1): the top-level `slots` for `host`'s pool, the entry's own `slots` for a `builders` entry's.

### Taking a slot

1. A gate that needs a slot creates its wake event, then writes its arrival [ticket](#ticket), both before its first try, so no wake is lost. A gate a parent's slot covers takes no slot and writes no ticket (N1).

   A gate its light parent lends the slot while a critical request drains (N2) writes no ticket when it is lent before it queues, only a lend marker ([Claim](#claim)); one lent as it waits has queued with a ticket, which it deletes when lent, writing its lend marker and waking the waiters.
2. Waiters of one kind take slots in ticket `arrived` order, ties broken by pid; a remote waiter counts only the tickets of its own pool (a ticket without `pool` is `host`'s).

   A gate tries a slot only when fewer live tickets of its kind are ahead of it than the pool has free slots (indices whose mutex no handle holds, a probe that fails counting as not free), not counting a ticket whose gate already holds a live claim (it holds a slot and only failed to delete its ticket), so a try never takes the slot an earlier waiter would get.

   It then tries indices 0 up to the count in order and takes the first free one. A live critical ticket that arrived before the gate (by `arrived`, then pid; the barrier, Q3) narrows that: a heavy gate tries no slot, and a light gate tries any light slot, and none once as many light gates run beside the critical request as the beside allowance (K3), which is 0 while a critical ticket says `alone`. See [Critical drain and nesting](#critical-drain-and-nesting).

   A local light gate left with no place beside the request may route instead of waiting on.

   A remote wait of such a rerouted light gate takes a slot only when it is free at once and another slot of the pool stays free after it: it takes the slot first, then probes the pool, and lets the slot go untaken when none is spare. A pool of one slot never takes such a run, and a slot that cannot be probed counts as no spare. A slot a dead gate left abandoned that the check lets go loses that gate's claim, as a takeover deletes it, and journals no `take`. Two such gates may both let go.
3. A take deletes the gate's own ticket, writes the claim, journals `take` and wakes the waiters, so the next in line tries at once. A release deletes the claim, journals `release`, lets the mutex go and wakes the waiters. A critical gate takes the machine instead ([Critical drain and nesting](#critical-drain-and-nesting)): its `take` and `release` list every slot it holds, and it writes one claim, `gate-critical.json`, and none for the slots.
4. A waiter sleeps on its own wake event, `Local\gate-wake-<pid>-<start>` (auto-reset). A gate that takes or releases a slot, or deletes a dead gate's ticket, signals the wake event of every live ticket in its state root. An auto-reset event stays signalled until its waiter wakes, so no wakeup is lost between a waiter's check and its wait. A wake that cannot be opened or set is not said: the fallback poll covers it.
5. Each wait has a fallback poll as its timeout (2 s, kept from S4's interval). The poll re-reads the counts and the tickets, which deletes a dead waiter's ticket so it cannot hold the queue (Q1), and picks up a count changed by hand. A pass that found the [beside mutex](GLOSSARY.md) held by another gate (a light gate's, a lend's, a critical gate's) sleeps 100 ms at most, since that gate wakes the waiters only once it took or lent something.
6. A waiter prints `gate: waiting for a <kind> slot (<n> busy)` once, on the first pass whose own [probe by open](#probe-by-open) finds none of the pool's slots free, where `<n>` is how many slots that same probe reports held, followed by `, the first frees in about <eta>` when the ETA below is known.

   A gate that waits only behind earlier tickets while a slot is free for it does not print it. A gate held back by a critical request prints the barrier line instead, on its first pass ([Critical drain and nesting](#critical-drain-and-nesting)); a light gate held back with no room that routes prints the line saying so, and never its local waiting line.

   It prints `gate: still waiting for a <kind> slot (<s> s)` every 30 s from its queueing, on stderr and in the log, so a gate around it does not read the wait as a stall (kept, S4). When a probe of the last pass's slots finds every one busy, the line reads `(<s> s; <n> busy)`, with the ETA before the `)`. Its watchdog starts only once it holds a slot.

   The ETA, on the heavy, light and remote queue lines and these heartbeats, names the soonest expected end among the busy slots, `started + expected.p50` of each holder's live claim. It reads `, the first frees in about <eta>`, or once that end has passed `, the first is <over> past its usual <p50>, ceiling <ceiling>`, the ceiling being the claim's `timeoutSeconds`.

   It is left out, and the line is as without it, when any busy slot has no live claim carrying `expected` or any slot of the probe could not be probed. A gate held back by a critical request, and a gate with a free slot behind earlier tickets, prints no ETA. Each figure is `<n> s` below 90 s and otherwise whole `<n> min`, never below 1, its unit picked by itself.

   `waitedSeconds` in its `take` counts from its `queue` line; the local `take` of a routed run that fell back counts from the `fallback` line, as the PowerShell gate does, except when the last fallback was `full`, whose local `take` counts from the run's `queue` line (as the PowerShell gate did for it).

   A routed run's remote wait is one budget of 60 s (`--test-remote-wait-ms` in a test build), shared by every builder it tries ([Routing](#routing), The next builder): each builder's pool waits only for what the earlier attempts left of it, so the run never waits longer for a remote slot than for one builder. The wait for a slot on a builder that refused the run `full` ([Routing](#routing)) counts into the same budget. An attempt reached with nothing left takes a free slot and otherwise finds the pool full at once, saying no waiting line.

   A rerouted light gate does not use this budget: its remote wait is zero, and its `take` counts its `waitedSeconds` from its first `queue` line.

   After a `full` fallback the wait's schedule, its still-waiting lines and its `take` or `abandon` `waitedSeconds` all count from that one instant, the run's `queue` line, so the `(<s> s)` includes the remote wait. The first still-waiting line of a wait entering from a fallback comes at the next whole heartbeat period past what it has already waited, never at once: for a 60 s remote wait and a 30 s heartbeat, the first line comes at 90 s.
7. A problem a pass meets is said as a prose line, once per name or path per wait: `gate: could not take <name>: <error>`, `gate: cannot probe <name>: <error>`, `gate: cannot create the wake event <name>: <error>` (the gate then waits on the poll alone), `gate: cannot wait on the wake event <name>: <error>`, `gate: cannot write the ticket <path>: <error>`, `gate: cannot write the claim <path>: <error>`.

   More: `gate: cannot read the tickets in <dir>: <error>`, `gate: cannot read the claims in <dir>: <error>`, `gate: ignored the ticket <path>: <reason>`, `gate: cannot check the gate of <path>: <error>` and `gate: cannot delete the ticket|the claim|the dead gate's ticket <path>: <error>`.

   A lend marker adds `gate: cannot write the lend marker <path>: <error>` (the gate is not lent and queues), `gate: cannot delete the lend marker <path>: <error>`, `gate: ignored the lend marker <path>: its name names no gate` and `gate: cannot delete the dead gate's lend marker <path>: <error>`, the last two said once.

   A dead gate's ticket or covered marker whose delete is refused with access denied is passed over unsaid and never counts as a delete, so it wakes no waiter. The usual cause is another gate deleting the same file (a [delete pending](GLOSSARY.md), which refuses every open and delete), and that gate sends the wake. A dead gate's file that can never be deleted stays in place unsaid and is tried again each pass; it never blocks the queue, since a dead ticket is never ahead.

   A gate under a parent's slot says `gate: cannot write the covered marker <path>: <error>`, `gate: cannot rewrite the covered marker <path>: <error>` and `gate: cannot delete the covered marker <path>: <error>`, and the survey says `gate: cannot read the covered markers in <dir>: <error>` and `gate: cannot delete the dead gate's covered marker <path>: <error>`, each of the last two once; the run goes on after each.

   A gate that looks for a stand-in says the read line too ([Critical drain and nesting](#critical-drain-and-nesting)).

A builder-side slot is taken by the box, not by a waiter's queue, always under the drain's lock ([`drain`](#the-box-side)), so two takers never get one slot:

1. The slots as judged, every claim or lock file in `slots\` whatever its index. A session claim is live while its session is live for a call of its own; a hold claim is live while its `{n}.lock` is held open by its hold. A dead claim is removed by the taker that finds it.
2. A session that already holds a live slot keeps it, at any index. Otherwise, while fewer claims are live than the count, the taker gets the lowest index below the count with no live claim. Every live claim counts, one past a count lowered since it was taken too, so a lowered count frees no slot early. With none, the take is refused `full`.
3. A hold waits for a slot: `--wait <seconds>` (0 when not given) is the time it keeps looking, every 5 s, saying `gate: waiting for a builder slot: {held} of {slots} held` on stderr on its first look and every sixth after (each 30 s). A routed run's preflight and run never wait in the box; they are refused `full` at once, and the PC's run then waits for a slot there within its remote budget ([Routing](#routing)) before it goes to its next builder.

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
| `remote.json` | people | the router, ops-tower (`slots`) | Builders and routing (T1); read from `%LOCALAPPDATA%\gate\remote.json`, beside the `v1\` folder, whatever the state root is. Its optional `declarations` names, per repo, a PC-owned folder holding the `.gate` files the repo cannot commit ([remote.json](#remotejson), Declarations folder). |
| `slots\<name>.json` | the gate holding the slot, written before its `take` line | other gates, ops-tower | [Claim](#claim); `<name>` is the mutex name without `Local\` and the root hash, plus `gate-critical.json` for a critical gate, its one claim for the whole machine (kind `critical`, index 0), written before its `take` line; the heavy and light slots it holds get none. |
| `slots\gate-lent-<parent pid>-<pid>-<start>.json` | a gate lent its light parent's slot, from before its command starts to after it ends | a critical gate's drain; ops-tower does not read it | [Claim](#claim) (the lend marker). Claim readers skip it. |
| `waiting\<pid>-<start>.json` | the waiting gate | other gates, ops-tower | [Ticket](#ticket). |
| `covered\<pid>-<start>.json` | a gate running under a parent gate's slot | other gates (the survey), ops-tower | [Covered marker](#covered-marker). |
| `paused\<parent pid>-<parent start>-<pid>-<start>.pause` | a gate waiting behind a critical request under a parent gate's slot | its parent gate's watchdog | Empty file; [Critical drain and nesting](#critical-drain-and-nesting) (kept, N3). |
| `events.jsonl`, `events.1.jsonl` | every gate, under the journal mutex | ops-tower, the durations store's rebuild, people | [Journal](#journal). |
| `archive\events-<yyyyMMddTHHmmssZ>.jsonl.gz` | the gate whose append rolled the journal | the durations store's rebuild | [Archive](#archive). |
| `durations.json` | every slot-taking gate that reaches a verdict, inside the journal mutex right after its `release` line; a rebuilding gate swaps one in | gates, `gate expect` (no lock); ops-tower does not read it | The learned durations store, `{"v":1,"keyVersion":1,"keys":{…}}`, at most 30 samples per key ([design/gate-durations.md](design/gate-durations.md), Store). Written to a temporary sibling and moved over. A gate rebuilds a missing, stale or torn file from the journal and the archive once it has released, outside the journal hold, leaving out `--slot none` runs; a run whose release was filed into a current store does not rebuild, and the store is read again just before the rebuild claim. A store whose `keyVersion` is above this build's is read as no history, and neither filed into nor rebuilt. |
| `durations.rebuilding`, `durations.rebuilding.<pid>.tmp` | the gate rebuilding the store | other gates | JSON `{"pid":<pid>,"start":<ms>}`; one gate rebuilds at a time, and a marker whose holder is dead or that cannot be read is taken over. Both exist only while a rebuild runs; a Ctrl+C during it swaps nothing and removes the marker. |
| `backfill\<pid>.json` and the backfill bar record | gates | gates | Reserved for backfill guests (GATE-7; [design/gate-durations.md](design/gate-durations.md)). |
| `remote\<key>.outputs-pending` | the Rust remote client | the same | Empty file. The Rust client writes it after the push and before a routed run's run call when the run has output globs, or when the route withheld its outputs (a dotnet `--no-build` command the builder built; that marker no end clears, and a routed `dotnet test` or `run` whose send of the PC's build landed, which builds nothing there, writes none), so a run refused at the preflight or by the parity check leaves none, removes it after an accepted `gate-remote-outputs: 0`, an install that skipped no file or only files it cannot rewrite, or a refusal of the archive's members for what they are, and sends `OutputsFull` while it stands, whichever builder ran the run that left it and whichever the next goes to: the marker, like the nothing-back record, is PC-wide in `remote\`, never in a builder's folder ([The remote client's run](#routing)). `gate prune --builders` removes it once every builder pruned the key ([`gate prune`](#gate-prune)). |
| `remote\<key>.outputs-hashes` | the Rust remote client | the same | The hash cache: one outputs manifest line, `{"files":{"<path>":{"size":…,"modified":…,"sha256":"…"}}}`, of the files of the worktree's last built manifest last written at least 2 s before that build. Saved after each manifest the client builds, and by a complete outputs install (one that skipped no file but ones it cannot rewrite), for the files it put in place, through `<key>.outputs-hashes.<pid>.tmp` and a rename; one that cannot be read, is not a manifest line or is over 64 MiB gives no hash and is replaced by the next save, and deleting the file resets the cache ([The remote client's run](#routing), Manifest time). A `builders` entry's cache sits in its own folder, `remote\<id>\<key>.outputs-hashes`, `<id>` being the entry's 8 lower-case hex digits; `host` and a `hosts` entry keep `remote\<key>.outputs-hashes`. `gate prune --builders` removes a builder's once that builder pruned the key ([`gate prune`](#gate-prune)). |
| `remote\<key>.outputs-from` | the Rust remote client, when a run brings the worktree's outputs back whole | the same | The outputs-from record: the alias of the builder (`host`'s or a `builders` entry's, as the routing file names it) whose run last brought the worktree's outputs back whole, overwritten by each such run. Written on a passing run with output globs whose box packed nothing and whose outputs the route did not withhold, and on a complete outputs install; never on a withheld run, a failed run's report pull or a failure-only run that brings nothing. A run on another builder sends `OutputsFull`; a record that is missing, unreadable, empty or over 512 bytes counts as another builder. PC-wide in `remote\`, never in a builder's folder ([The remote client's run](#routing), Outputs). |
| `remote\<key>.inputs-hashes` | the Rust remote client | the same | The inputs' hash cache, in the outputs cache's shape and saved as it is, kept apart from it since the outputs saves drop every file the outputs walk no longer finds. Saved after an inputs manifest the client builds changes it; a file whose hash the pack finds wrong is dropped from it, and deleting the file resets the cache ([The remote client's run](#routing), Inputs). A `builders` entry's is `remote\<id>\<key>.inputs-hashes`, as the outputs cache's. `gate prune --builders` removes a builder's once that builder pruned the key ([`gate prune`](#gate-prune)). |
| `remote\<key>.inputs-carried` | the Rust remote client | the same | An empty marker, written once an inputs step that carried a path a command named has landed. A later routed run of a repo with no `inputs` entry and no siblings, finding it, sends the empty inputs manifest so the box deletes the carried copies, then removes the marker and the inputs hash cache ([The remote client's run](#routing), Inputs). A `builders` entry's is `remote\<id>\<key>.inputs-carried`. `gate prune --builders` removes a builder's once that builder pruned the key ([`gate prune`](#gate-prune)). |
| `remote\<key>.sent-build-hashes` | the Rust remote client | the same | The sent build's hash cache, in the inputs cache's shape and saved as it is, kept apart from it for the same reason: the manifest line of the PC's build last sent to that work folder. Deleting the file resets the cache ([Routing](#routing), the send of the PC's build). A `builders` entry's is `remote\<id>\<key>.sent-build-hashes`, as the outputs cache's. `gate prune --builders` removes a builder's once that builder pruned the key ([`gate prune`](#gate-prune)). |
| `remote\<key>.nothing-back` | the gate, after a routed dotnet build ends on the builder without its outputs known to be brought back, or, when none stands, a routed dotnet `--no-build` command whose word the builder dropped (a `dotnet test` or `run` whose send of the PC's build landed kept it, and writes none) | every gate (the `--no-build` drop and the clear) | JSON `{"v":1,"host":"…","configuration":"…","framework":"…","runtime":"…","built":["…"]}`; the last four are optional and the first three of them come from the routed build's own `-c`, `-f` and `-r` words. `built` lists the project keys (the project's folder relative to the worktree root, lexical, lower-cased, `/`-separated, `.` for the root) of the projects a passing local build built since the record was written, and is left out when empty; one that is not an array of strings makes the record unreadable, which still counts as standing. A passing local build of the whole worktree with the same three removes it, as does a passing routed build of that kind that brought all its outputs back; a passing local build of one project with the same three adds the project's key to `built` ([Routing](#routing)). Written with create-new and the reparse-point flag, and read the same way, 4096 bytes at most; one that cannot be read counts as present. `gate prune --builders` removes it once every builder pruned the key ([`gate prune`](#gate-prune)). |
| `remote\<key>.built-here` | every gate, after a passing local dotnet `build`, `test` or `run` that builds in place | every gate (the `--no-build` keep and the send), the routed run (the clear) | The built-here record: JSON `{"v":1,"projects":{"<project key>":{"configuration":"…","framework":"…","runtime":"…","seq":<n>}}}`, one entry for each project built here under the build's own `-c`, `-f` and `-r` (each left out when the words set none), `<project key>` as in the nothing-back record's `built` list; a routed `dotnet test` or `run --no-build` whose projects all have an entry under its own settings is sent the PC's build, or runs here, and a run here keeps its word. Written whole or not at all under the work folder's key, 64 KiB at most, trimmed oldest `seq` first; one that is missing, cannot be read or is not of its format reads as empty. Every routed run that builds removes it ([Routing](#routing), the built-here record). `gate prune --builders` removes it once every builder pruned the key ([`gate prune`](#gate-prune)). |
| `remote\<key>.outputs-capped` | the Rust remote client, at an outputs fetch that passed the fetch cap | the same | The outputs cap record: JSON `{"v":1,"hits":<n>}`, the fetches in a row of that worktree that passed the cap. A second hit in a row fails the run; any other end of its outputs fetch but a Ctrl+C during it, and a box that packed nothing, removes it; one that cannot be read counts no hit ([The remote client's run](#routing), Outputs fetch). A `builders` entry's is `remote\<id>\<key>.outputs-capped`, as the outputs cache's. `gate prune --builders` removes a builder's once that builder pruned the key ([`gate prune`](#gate-prune)). |
| `remote\<key>.worktree` | a routed run, once, before its first builder call, and `gate warm` once it holds the warm-up lock (`gate-state` has the writer, the parser and the write rule; `crates/gate/src/worktree_record.rs` reads and writes the file for both) | `gate prune --builders` (reads it, writes `goneSince`, and removes it last, once every builder pruned the key) | The worktree record, which maps a work-folder key (a one-way hash of the worktree root) back to its worktree: JSON `{"v":1,"root":"…","commonDir":"…","repo":"…","lastRouted":"<ISO>"}` with an optional `"goneSince":"<ISO>"` after it, `root` the worktree root as routing spells it, `commonDir` the absolute path of the worktree's git common folder and `repo` the routing file's repo name. `goneSince`, a time in the form of `lastRouted`, is left out while the worktree is live and marks when `gate prune --builders` first found it gone (`gate_state::v1::mark_gone` sets it only when absent), and that sweep clears it when it finds the worktree again ([`gate prune`](#gate-prune)); older gates ignore it. Read strictly, unknown keys ignored. The existing file is opened as itself like the nothing-back record, never through a link or junction, at most 4096 bytes; a link, junction, folder or longer file at the name counts as unreadable. A routed run or `gate warm` writes it when none stands or it cannot be read (the write replaces whatever stands at the name), when `root`, `commonDir` or `repo` differ, when its `lastRouted` is an hour or more old or later than now, or when it has `goneSince` (the rewrite has no `goneSince`, even inside the hour); a worktree whose git common folder cannot be read, or whose path is not valid Unicode, writes none and says nothing, and its key is left to age pruning. The sweep writes it the same way, a temporary file and one move, and says `gate: cannot write <path>: <error>` when that fails. Written whole or not at all like the nothing-back record, but in a single try: a temporary file, then one move, with no retry and no wait (the other state files keep their retries). ops-tower does not read it. A write that fails says `gate: cannot write <path>: <error>` once on stderr, from a routed run or `gate warm` alike, and changes nothing else of the run ([Output lines](#output-lines)). |
| `remote\warm\<key>.json` | `gate warm` for the work folder `<key>` (`gate-state` has the writer and reader) | `gate check`'s warm-up item, a `layout` fallback's delete | The warm-up record: JSON `{"v":1,"host":"…","state":"…","at":"<ISO>","hash":"…","gate":"…","pid":<n>,"started":"<ISO>"}`, then `failure` and `reason` when, and only when, `state` is `failed`. `state` is `warming`, `warm` or `failed`; `failure` is `commands` (the commands failed or ran out of time) or `refused` (refused before any command ran); `hash` is the 64 lower-case hex digits of the SHA-256 of the raw bytes of the worktree's `.gate/warmup.toml` or, when the worktree has none, of its copy in the repo's declarations folder, a byte order mark included; `gate` is the build id of the PC gate that wrote it; `pid` and `started` are the warming process's, for the lines only. Written whole or not at all like the nothing-back record; a record that breaks a rule of its shape is refused on read and on write. ops-tower does not read it. A routed run reads it only after a `layout` fallback, without following the leaf link, and removes it while it is the `warm` record of the builder the run went to, for the warm-up file on disk. `gate prune --builders` removes it once every builder pruned the key ([`gate prune`](#gate-prune)). |
| `remote\warm\<key>.log`, `remote\warm\<key>.lock` | `gate warm` | `gate check` and a routed run waiting for its key (the lock) | The warm-up's log, made new for each warm-up, opened without following links and held open without delete sharing, and the lock it holds for its whole life, opened without sharing: a `warming` record counts as live only while the lock is held. A routed run probes the lock opened with full sharing and never takes or creates it. `gate prune --builders` removes it once every builder pruned the key ([`gate prune`](#gate-prune)). |
| `remote\drain\<builder>\<pid>-<session>` | a routed run waiting behind that builder's drain (`gate-state` has the paths, `drain_dir` and `drain_marker_dir`; `crates/gate/src/remote/drain.rs` holds the marker) | other routed runs' remote-slot and key waits | The drain marker: a file held open sharing nothing for the whole wait, so a file another open finds held is a live wait and one that opens is dead and is removed. `<builder>` is a `builders` entry's 8-hex id, or `host` for `remote.json`'s `host`; the waiter makes the folders. While one stands, the waits it suspends end in no fallback ([Routing](#routing), Builder drain). |
| `remote\warm.lock` | `gate warm` | other warming processes | The lock that lets one warm-up at a time run on this PC, taken after the key's mutex and waited for up to the warm-up file's ceiling. |
| `remote\prune-sweep.stamp` | a routed run whose builder call ended, before it starts the background prune sweep, and removed again if the start fails (`gate-state` has the path, `PruneSweepStamp` and the writer) | routed runs alone | The prune sweep stamp: JSON `{"v":1,"at":"<ISO UTC>"}`, `at` when a run started the last background sweep. Written whole or not at all in a single try, like the worktree record. A routed run that finds it missing, unreadable, 6 hours old or more, or more than 5 minutes in the future starts a sweep; it is not written when the caller's job refuses breakaway. Nothing else reads it, and ops-tower does not ([`gate prune`](#gate-prune), The triggers). |
| `remote\prune-sweep.log` | the background prune sweep's process, as its stdout and stderr | nobody | The output of the last background sweep, replaced at each start. |
`<start>` everywhere in a name is the process start time in Unix epoch milliseconds. Claims, tickets, counts, the nothing-back record and the outputs cap record are written to a temporary sibling (`<file>.<pid>.<n>.tmp`, where `<n>` is a per-process counter so two threads of one process never share one) and moved over the target, so a reader never sees a torn file (kept, L1, Q1).

A `remote-cutover.flag` or `remote-cutover.draining` left in a state root is ignored: nothing reads or writes either file.

The temporary sibling is made new, a link or junction at its name opened as itself, and an entry that already stands at a name is left and the next name tried. A write never removes an entry it did not create: after 16 names it fails with `something already stands at each of 16 temp names beside <path>`.

### Kernel object names

| Object | Default root | Non-default root |
|---|---|---|
| heavy slot | `Local\gate-slot-<i>` | `Local\gate-<h>-slot-<i>` |
| light slot | `Local\gate-light-slot-<i>` | `Local\gate-<h>-light-slot-<i>` |
| remote slot | `Local\gate-remote-slot-<i>` | `Local\gate-<h>-remote-slot-<i>` |
| builder's remote slot | `Local\gate-remote-<p>-slot-<i>` | `Local\gate-<h>-remote-<p>-slot-<i>` |
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

The critical-beside mutex (the beside mutex) is tried once, never waited on, and held briefly. A local light gate holds it from before it counts the light gates beside a critical request and tries its slot to after it writes its claim. A gate lent its light parent's slot holds it from its lend decision to after it writes its lend marker.

A critical gate holds it around its count of the light gates and its light takes, and around its count of the lend markers and the rewrites of its ticket. So no light or lent gate between its take and its claim or marker is missed by a count. A pass that finds it held by another gate has no room and looks again within 100 ms. One a killed gate left held is taken with the `took <name>, which a killed gate had left held` line.

A mutex that cannot be opened (access denied, or a name another kind of object holds) is said once as `gate: could not take <name>: <error>`. A local light gate then takes its slot without it when no critical ticket is live, and has no room when one is; a critical gate holds nothing on that pass and tries again, and a gate lent a slot is not lent, so it queues.

The box command's job name comes from the payload's `Job` (`<key>` is the payload's `Key`, `<nonce>` is 12 hex digits); `gate box` only checks its form and that it ends in the payload's key, and never derives it.

`<l>` is the log hash: 16 lower-case hex digits, the first 8 bytes of the SHA-256 of the lower-cased, normalised absolute log path. Each run takes its log's mutex without waiting, before it readies the log, and holds it until after the slot release and the final append to the log, so two gates never write one log; a second gate fails at once ([Output lines](#output-lines)).

`<k>` is the key hash: 16 lower-case hex digits, the first 8 bytes of the SHA-256 of the lower-cased work-folder key. The Rust remote client takes the routed lock before the remote slot, first at once, then by the run's wait of up to 90 s while another run holds it, and holds it for the whole remote attempt ([Routing](#routing)).

`<v>` and `<f>` are the worktree lock's folder id: the volume serial number as 16 lower-case hex digits and the 128-bit file id as 32, read from the worktree root opened following links, so two spellings of one folder share one lock.

Every routed run takes it for its main root and each carried sibling before its snapshots and holds it until its last push is saved, and a sync-back takes it again for each worktree it has a back commit for, each waiting for another run's hold ([Routing](#routing), The worktree lock).

The environment carries two more names: `GATE_SLOT_HELD`, the slot marker, and `GATE_COMMIT_SLOT`, a commit run's own marker ([Critical drain and nesting](#critical-drain-and-nesting)).

## Formats

### Common rules

- The claim, ticket, counts and journal files are JSON with the PowerShell gate's field names (kept, L1, Q1, S2, J1), each object carrying `v`, the format version, as a whole number. Every format starts at `v: 1`.
- Machine times are Unix epoch milliseconds as JSON numbers. Times people read (`time`, `started`, `processStart`) stay ISO 8601 UTC strings ending in `Z`.
- Files are read as strict JSON (RFC 8259: no comments, no trailing commas, no unquoted keys) with case-sensitive keys. A file with a syntax error, a byte order mark, bytes that are not UTF-8, a top level that is not an object, a missing required key, a wrong type, a duplicate key at any depth or a `v` that is missing or not the number 1 is unreadable.

  The reader takes the path each format's section names for an unreadable file. The journal is the exception: its rule is per line ([Journal](#journal)). Unknown keys are ignored, so an optional field is added without a `v` change; a rename, removal, retyping or change of meaning raises `v`.
- The reason a file is unreadable is one of `not JSON: <message>`, `not JSON: not UTF-8`, `duplicate key "<key>"`, `missing key "v"`, `unknown format v <value>`, `not a JSON object`, the message of the missing key or wrong type, or the io error. It is the `<reason>` of the gate's ignore line.
- `gate-state` is the one implementation of every format; `gate.exe` and ops-tower both use it, and its fixtures (`crates/gate-state/tests/state/fixtures/`) are the reference examples. The strict rules above are the `gate_state::v1` module's, the formats `gate.exe` writes.

  The crate's other readers (`claims.rs`, `state.rs` and the counts reader beside `v1`) read the PowerShell gate's files leniently, and ops-tower uses them while the cutover flag does not exist. ops-tower follows the flag: its `StateInputs.generation` (`Generation::PowerShell` or `Generation::V1`) is looked up on every `live_inputs()` call from `%LOCALAPPDATA%\gate\cutover.flag` ([Cutover](#cutover)).

  In the `PowerShell` generation the state and the history read `%LOCALAPPDATA%\gate\`; in `V1` they read `%LOCALAPPDATA%\gate\v1\`, and a missing `v1\` folder is an empty state.

  In `V1` a slot's row also reads the claims in the PowerShell root's `slots\` folder (`%LOCALAPPDATA%\gate\slots\`) while the slot's mutex is open and no live `v1\` claim holds it: a PowerShell gate holding the slot shows `busy` with that claim, its processes and last line.

  A mutex that cannot be probed counts as open, so a slot whose mutex an elevated gate holds shows as Held.

  A claim whose pid is absent from the process snapshot but whose `processStart` is later than every process's creation time in the snapshot counts as live, only while the slot's mutex is open. A claim file whose read fails with access denied (delete-pending) or not found (deleted after the listing) is skipped, never a stale claim; the cutover drain reads claims the same way.

  In `V1` the readers take `gate.exe`'s formats: the claims (their `SlotRow.claim` carries `"v":1`), the tickets and the counts file are read as the strict `v1` formats, and the journal is the one `gate.exe` writes.

  The waiters in `queue[]`, `pools[].waiting` and `critical.waiting` come from the tickets, not from process command lines: `queue[]` is ordered by (`arrived`, `pid`) with `arrived` as ISO text of 7 fractional digits and a lower-case `kind`; a waiter's `since` is its gate's start and its `commandLine` is the gate's command line from the process table; `pools[].problem` is worded for `gate.exe`'s files.

  `DashboardState.builders`, after `remote`, lists each builder's own remote pool: `[{id, name, host, os, count, problem, routed, slots}]`, `[]` (not skipped) when `remote.json` names none, with `name`, `host` and `os` always serialised and null when unknown.

  The order is `remote.json`'s `builders` entries, then each `hosts` entry that holds a claim (`routed: false`), then each pooled claim no entry names, sorted by pool id (`name` and `host` null, `problem` `no builders or hosts entry has pool id <pool id>`).

  The entries come from `gate-router`'s own reader of `remote.json` ([remote.json](#remotejson)), so the dashboard lists the pools the gate routes to; `id` is `gate_router::builder_id` of the alias and `routed` is `Config::builder_routed`. `slots` rows are the ordinary slot rows, a claim past `count` an extra row, and a busy builder slot's pid is a dashboard holder as `host`'s remote slots are. `remote` stays `host`'s pool, read as before.

  A file `gate-router` refuses still lists each `builders` entry that has a string `host`, and each `hosts` string that holds a claim, once per pool id, with `count` 0, its claims as extra rows and the refusal text as `problem`; a pooled claim no entry names then carries the refusal text too.

  A file `gate-router` accepts with a bad `hosts` value lists each `hosts` string that holds a claim with the top-level `slots` and the `hosts` problem's text. A `remote.json` that exists but cannot be read gives every builder pool the problem `remote.json cannot be read: <error>`.

  The `QueueTicket` of a waiter gains `pool` (after `remote`, only when present) and `pinned` (after `pool`, only when true), from the [ticket](#ticket)'s keys.

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
| `reason` | string | optional: the free text the run gave `--reason`; absent when it gave none. A local, critical or alone run always has one, since it is required |
| `expected` | object | optional, last: what the durations store expects of the run, `{"p50": <s>, "p90": <s>, "n": <runs>, "key": "<key>"}` with the seconds to 3 decimals, rounded half to even; absent for a run without history. It is the builder's estimate for a remote take and `crit`'s for a critical gate. The wait lines read `p50` ([Slots](#slots), step 6); `null` reads as absent, any other non-object is unreadable |

The file name `<name>` is `gate-slot-<i>`, `gate-light-slot-<i>`, `gate-remote-slot-<i>`, `gate-remote-<pool id>-slot-<i>` (a `builders` entry's slot, or a pinned `hosts` entry's; the pool id is exactly 8 lower-case hex digits, any other spelling is not a claim) or `gate-critical`, the same whatever the root: a non-default root's claims sit in that root's own `slots\` folder, so the file name carries no root hash.

A claim is live while its `pid` runs with that `processStart` (L2), the start compared in whole milliseconds. A killed gate leaves its file; the next holder overwrites it and readers treat it as stale. A gate that cannot write its claim says so once and runs on (L1). The claim is written before the gate's `take` line ([Journal](#journal)), so a gate paused or killed before that line shows a claim with no `take` line: a reader, ops-tower included, must not expect a `take` line for every claim.

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
| `alone` | bool | optional, written only when true: a critical gate asked for `--alone`, from its first ticket on, kept by the holding rewrite. While a live ticket says it, the beside allowance is 0 for every gate. A ticket without it reads as false, and a build that does not know the key ignores it, so it treats the request as a plain critical one |
| `remote` | bool | present and true only while queued for the remote pool |
| `pool` | string | optional, after `remote`: the pool id of the `builders` entry's pool the ticket queues for; absent for `host`'s pool and for a local ticket, so their tickets are unchanged. A build that does not know the key ignores it and counts the ticket as `host`'s, so every gate on a machine is reinstalled together |
| `pinned` | bool | optional, after `pool`: written only when true, by a run pinned to one builder with `--builder`; a ticket without it reads as not pinned, and a build that does not know the key ignores it. ops-tower shows it as `QueueTicket.pinned` |
| `reason` | string | optional: the free text the run gave `--reason`; absent when it gave none. A critical (and therefore alone) run always has one, since it is required; a critical gate's rewritten holding ticket keeps it |
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

### Builder-side slots

The builder's own slots ([Slots](#slots)) are three kinds of file beneath the builder's work root `<root>`, which the box alone reads and writes, except `slots.json`, which `gate push` writes (`gate_wire::slots` holds the formats; `gate-state` and ops-tower do not read them). The paths below are the Windows box's; the Unix box keeps the same files under `<root>/` with `/`, `slots/` at mode 0700 and each `{n}.lock` an `flock` ([The Unix box](#the-unix-box), `hold`). The rules of the common formats apply: strict JSON, `v` 1, UTF-8, a file that does not read as below is unreadable.

- `<root>\slots.json`: `{"v":1,"slots":<n>}`, `slots` a whole number from 1 to 64. A builder with no such file takes no slot. `gate push` writes it from the builder's `slots` of `remote.json` ([`gate push`](#gate-push)); on a macOS builder the file is `{"v":1,"slots":<n>}` and a line feed, mode 0600. A file that cannot be read or does not parse counts as none, and the box says `gate box: the builder's slot count cannot be read, so it takes no slot: <why>` on stderr (a hold then also says its no-count line and exits 2).
- `<root>\slots\<n>.json`, made as `drain\` is (a protected folder), one claim per held slot `n` from 0. A session claim is `{"v":1,"kind":"session","session":<id>,"pc":<text>,"repo":<text>,"command":<text>,"taken":<UTC text>}`, the session a drain session id, `pc` and `repo` capped at 100 characters and `command` at 300 as the drain's session record caps them. A holder claim is `{"v":1,"kind":"hold","holder":<text>,"ceiling":<seconds>,"pid":<pid>,"started":<UTC text>}`, `holder` the text of `--holder` (not empty, no control character, at most 300 characters), `ceiling` above 0 and `pid` the hold's process id above 0. A claim that does not read names no session and is live only while its lock is held.
- `<root>\slots\<n>.lock`: the lock of a hold's slot, opened by the hold and shared with no one for its whole life, so a claim of kind `hold` is live exactly while the lock is held; a dead one is removed by the next taker.

`events.jsonl` holds one JSON object per line, appended under the journal mutex (`Local\gate-journal`, [Kernel object names](#kernel-object-names)) with a 2 s wait (kept, J1). The events are `queue`, `take`, `release`, `abandon`, `covered`, `parent` and `fallback`. `kind` is `heavy`, `light`, `critical` or `none`.

`gate.exe` writes `covered` for a gate running under its parent's slot; `gate.exe` writes `parent` for a gate lent its light parent's slot while a critical request drains. A gate nested in a lent gate is covered by it and writes `covered` with the lent gate as `parentPid`.

Every line starts with `v`, `time`, `event`, `pid`, `processStart`, `kind`, `cwd` (the `--cd` folder when given) and `command` (the same text as the claim's [`command`](#claim)), in that order, then the event's own fields, as the PowerShell gate writes them (J1); a routed run adds `remote` (true), `host` (the ssh destination) and `key` (the remote work folder's key) after the event's own fields and before `abandoned`, on its `queue`, `take`, `release`, `abandon` and `fallback` lines, and on none after the `fallback`.

The journal keeps ISO 8601 UTC strings for `time` and `processStart` (7 fractional digits, ending in `Z`); it never held .NET ticks, so the move of claims, tickets and markers to epoch milliseconds does not touch it. `gate.exe` takes `processStart` from `GetProcessTimes`, in whole microseconds, so its 7th fractional digit is 0. Seconds fields (`heldSeconds`, `waitedSeconds`, `wallSeconds`, `adjustedSeconds`) are rounded to 3 decimals.

`v` is a line-level rule, unlike the other formats: the writer always writes `"v":1` first; a reader skips a line whose `v` is present and not 1, and reads a line with no `v` as the PowerShell gate's format until the cutover's drain (GATE-9) makes `v` required.

A reader also skips a line that does not parse. A crash of the PC can leave a run of NUL bytes where the last lines before it were. That run reads as one line, so the crash costs only the lines it lost.

The event's own fields:

| Event | Fields after the common ones |
|---|---|
| `queue` | `log` (the log's full path), `notRouted` (written right after `log`, only when a heavy gate was kept local with `--local`, with the value `--local`, or when a matched command's outside path lies in another worktree of a listed repo and the run says to `--cd` it, with the value `another worktree`, or when a matched command's repo `.gate/local.toml` keeps it here, by a pattern or by a problem, with the value `declared local`, or when a matched command's word names a gitignored project input, with the value `gitignored input`; every other reason for a local run, the plain outside line included, writes none), `reason` (the free text the run gave `--reason`, written after `notRouted`, when it gave one; absent otherwise), `pinned` (`true`, written after `reason`, only when the run was given `--builder`; absent otherwise) |
| `take` | `slots` (the slot names taken; a critical gate lists every heavy and light slot it holds; a `none` run lists none, `[]`, with `waitedSeconds` 0.0), `waitedSeconds`, optionally `parentPid` (also on the late take of a covered gate), and for a critical gate `counts`, `otherHeld`, then `otherClaims` or `snapshotError` |
| `release` | `slots` (the same list), `heldSeconds`, `status` (null when the gate failed before its command had one), `wallSeconds`, `adjustedSeconds`, optionally `avgCores`, `peakCores`, `freeCores`, `verdict`, the [run trace](GLOSSARY.md#run-trace) (`cpuSeconds`, `processes`, `peakProcesses`, `priority`, `loadFreeShare`, `children`, `phases`), and `cold` for a cold routed run, and `path` for a routed run whose Tailscale path was read |
| `abandon` | `waitedSeconds`: the gate ended while still queued (Ctrl+C while waiting for a slot, or, for a routed run, while its client waited to start or while it waited for its work folder's key, remote-tagged) |
| `parent`, `covered` | `parentPid`; a lent gate writes `parent` after its `queue` when it was lent the slot as it waited, and alone, with no `queue`, when it was lent it before it queued |
| `fallback` | `reason`, then `remote`, `host`, `key` (and `routedFor`, `criticalPid` for a light run a critical request sent to a builder): it ends the routed attempt, so a remote `take` that falls back gets no `release`; a `full` fallback, and a `client` fallback with no client built, follow no remote `take` |

A release with no run facts writes `wallSeconds: null`, `adjustedSeconds: null` and `verdict: "unknown"`.

Fields beyond J1's:

- any event: `abandoned`, the last field, a list of names present only when there were any ([Abandoned slots](#abandoned-slots); GATE-30). On a `take` it is the slots taken from a gate that died holding them, followed by the journal mutex's name when that came back abandoned; on any other event it is just the journal mutex's name.
- `take`: a critical gate's `take` carries, after `waitedSeconds` and in this order, `counts` (`{"heavy": <n>, "light": <n>}`, the counts of the pass that took the machine), `otherHeld` (the same shape: the slots within the counts that other gates hold), then `otherClaims` or `snapshotError`; `abandoned` stays last (K5, GATE-38).

  `otherClaims` is a list of `{pid, kind, index, command, file}`, one per live claim of another gate in this root's `slots\` folder, sorted by file name: only the `gate-*.json` files, the remote pool's claims and this gate's own left out, `file` the claim's file name. `snapshotError` replaces it only when the claims folder cannot be listed, and holds the reason.
- `take`: `parentPid`, an integer after `waitedSeconds` (and a critical take's snapshot fields) and before the remote fields and `abandoned`, written by a gate under a parent whose slot did not cover it ([Cores hint](#cores-hint)), or under a no-slot parent, and by a covered gate that takes a slot late once its parent ended ([Critical drain and nesting](#critical-drain-and-nesting)), where it names the parent that ended, or the stand-in that ended last.

  `gate-state`'s reader accepts it and does not treat the run as on the parent's slot, except for a covered run, which it reads as a run with a slot of its own.
- `take` of a `none` run: `"kind":"none"` on its `queue`, `take` and `release`, and a `take` with `"slots":[]` and `"waitedSeconds":0.0`, no `abandoned`, no machine fields, and `parentPid` only under a live no-slot parent. Its `release` has `"slots":[]` and is written like any run's, unlike a covered or lent run's. `v` stays 1. The history reader gives the run `kind` `none`, no `slot`, and `started` from the `take`.
- `queue`, `take` and `release` of a commit run ([Commit run](#critical-drain-and-nesting)): the slot kind the run took as `kind` (light for a commit asked for on `none`) and `"commit": true` after the event's own fields, before `remote`, `host` and `key` of a routed line. No other event and no other run has `commit`.
- `release`: `avgCores`, `peakCores`, `freeCores`, optional numbers of one decimal after `adjustedSeconds` and before `verdict` ([Cores hint](#cores-hint)). They are omitted when there is no unpaused sample, the sampler failed, the read of the command's CPU at its end failed, a gate under the run took a local slot of its own (or the machine; a routed gate's remote slot does not count), the journal could not be read to tell, or the run was routed.

  Covered and lent runs write no `release`, except a covered run that took a slot late, which writes one when it lets that slot go: its `slots` the one it took, its `heldSeconds` counted from that take.
- `release`: `wallSeconds` (the command's wall time less the seconds paused behind a critical request; a routed run's whole remote wall time), `adjustedSeconds` (load-adjusted seconds, 3 decimals, a number for every run that started; for a routed run the builder's figure, null when it said none; null for a run whose sampler failed) and `verdict`, one of `passed`, `failed`, `log-failure`, `stalled`, `timed-out`, `backstop`, `transfer-timed-out`, `transfer-failed`, `unknown` (kept, J5).
- `release`: the run trace, after `verdict` and before `cold`, in this order, each field left out when the run measured nothing for it (a run that measured none writes none), so a reader treats every one as optional:
  - `cpuSeconds`: the user and kernel CPU every process of the command's job used, in seconds of one decimal. `processes`: the processes the job ever held, those that exited included; `peakProcesses`: the most it held at one sample. `cpuSeconds` and `processes` are left out when the job's accounting cannot be read.
  - `priority`: the priority class the command started in, one of `idle`, `belowNormal`, `normal`, `aboveNormal`, `high`, `realTime`. `loadFreeShare`: the share of the machine free on average over the run as the load-adjusted clock counted it, from 0 to 1 with two decimals; the paused stretches are left out, and so is the time after a failed sampler.
  - `children`: a list of the job's processes worth naming, those that used at least 1 CPU-second or lived at least 10 s, at most 8, most CPU first.

    Each is `{cmd, image, start, end, cpu}`: `cmd` the process's own command line, redacted, with control and direction characters made spaces and cut to 200 characters, or its program's file name when the line could not be read; `image` the program's full path cut to its last 1024 characters, left out when it could not be read; `start` and `end` seconds of one decimal after the command started, `end` null for a process still running at the release; `cpu` its CPU seconds of one decimal.

    The list is read when the command ends.
  - `phases`: for a routed run, an object of seconds to three decimals, in the order the parts ran, named `command`, `push`, then each of `inputs`, `lfs`, `sentBuild`, `syncBack`, `outputs` the run made: the same parts the routed passed line gives. Left out for a client that timed none.

  The redaction of `cmd` is best-effort and hides what looks like a secret with `***`: credentials in a URL, the argument after a secret flag such as `--token` or `--password`, `Authorization` and `Bearer` values, the value of a `key=value` pair whose key names a secret, and words that begin like a known kind of token, also inside a quoted argument or a `cmd /c` line.

  A secret in a shape no rule knows is written as it is, and the process reports its command line itself and can rewrite it, so `cmd` says what the process claims to run, never proof of it.
- `release`: `cold`, `true` after `verdict` and before the remote tag's `remote`, `host` and `key`, on the release of a cold routed run ([Cold run](GLOSSARY.md#cold-run)) only. No other release has the field, and a run that fell back never writes it.
- `release`: `path`, after `cold` and the last of the event's own fields, Tailscale's path from this PC to the builder of a routed run's call: `direct`, `derp:<region>`, `peer-relay` or `none` ([Tailscale path](GLOSSARY.md#tailscale-path)). A reader treats it as optional: it is left out when the path is unknown (no Tailscale, no matching peer, a read that failed) and on a run that made no builder call. It is read on a thread of its own once the builder call returned, a refused call that falls back to a run here included (its local `release` line journals `path`); a call a Ctrl+C ended starts no read. The release line takes it only if the read has finished, never waiting for it; one not finished says `gate: tailscale path not read in time for the release line` in the log and leaves the field out. The log alone also says `gate: tailscale path not read: <why>` for a read that failed and `gate: tailscale path read failed` for one whose thread ended without an answer.

  `transfer-timed-out` is a routed run's fetch after the command cut at its transfer ceiling. `transfer-failed` is a routed command that exited 0 whose transfer after it failed every attempt as a connection fails; the verdict comes from the client's outcome, never from the log line.

  A routed run is a kill only when the gate killed it; a command that exits 124 on its own is `failed`.
- `fallback`: `reason`, one of `full` (no remote slot within the run's 60 s remote wait, or the box's `full` refusal tag, from the builder's own slot count, still given once the run has waited out the rest of that wait for a slot there), `busy`, `unreachable`, `parity`, `rebooted`, `tools` (T5), `layout` (the box could not clear or lay out its work folder), `version` ([The box side](#the-box-side)), `client` (the remote client could not start, as when it finds no usable ssh), `inputs` (the run's ignored inputs could not be sent, a sibling pair declared inputs, or the box refused the run because its work folder no longer held the inputs sent), `lfs` (the run's Git LFS content could not be listed or sent, or the box refused the run because its store does not hold the content the snapshot's pointers name or it could not make the run's LFS scratch folder), `desktop` (the builder could not give the run its desktop; the client reads the box's `desktop` refusal tag as this reason) `install` (the builder's gate is not installed where only administrators can change it; the client reads the box's install refusal on the preflight, inputs and run calls, and its `install` refusal tag, as this reason) and `root` (the builder's work root is not locked down or is busy; the client reads the box's root lines on the preflight, before the version check, as it reads `install`).

  A journal line with the reason `cold` is still read, but no run writes it.

  The `take` after it is the run's local one, untagged, its `waitedSeconds` counted from the `fallback`, except after `full`, where it counts from the run's `queue` line; a Ctrl+C that ends that local wait writes the same figure on its `abandon`.

  A run that tries a second builder writes one `take` and one `fallback` per attempt, each tagged with that attempt's builder, and one `queue` line, tagged with the first candidate. The refusing attempt's `fallback` is written while its slot is held; the slot is then let go, its claim deleted and the waiters woken, before the next attempt waits for its own.

  The local `take` after the last attempt counts from the `queue` line when that attempt's fallback was `full`, else from the last `fallback`.

  A `release` writes `"heldSeconds":0.0` when the run holds no slot at that point: a Ctrl+C ended the walk over the later builders after a builder's refusal let its remote slot go.
- `take`, `release`, `abandon` and `fallback` of a light run the critical barrier sent to a builder ([Routing](#routing), "What routes"): `"routedFor": "critical"` and `"criticalPid": <pid>` right after `key`, the pid of the critical request's gate. The run's `queue` line is the local, untagged one, and the remote `take`'s `waitedSeconds` counts from that `queue` line. No other run has either field; a run that fell back writes its later local lines untagged.
- a routed run's `take`, `release` and `abandon` carry the tag; its `release` has the builder's `adjustedSeconds` (null when it said none) and a `wallSeconds` of the whole remote call, and the verdict `timed-out` or `backstop` after a kill by the builder or by the client's backstop.

The verdict comes from the run's end: a kill line matching `^gate: (STALLED|TIMED OUT|BACKSTOP|TRANSFER TIMED OUT)\b` gives `stalled`, `timed-out`, `backstop` or `transfer-timed-out` (`gate: TRANSFER FAILED` is no kill line, and a routed run's `transfer-failed` is set by the client); otherwise a command status of 0 with a non-zero gate status is `log-failure`, a status of 0 is `passed`, anything else `failed`; `unknown` when there are no run facts.

The reader starts a run from a lone `parent` line that names a parent: state `parent` and `parentPid`, no queued or started time, and the status and held time of the parent's run. A `parent` line naming no parent is skipped.

A lone `covered` line is its own state, `covered` (`RunState::Covered`), distinct from a run that lent its slot: the run has `parentPid`, no queued or started time, the kind and first time of the `covered` line, and the status and verdict of the parent's released run.

The parent's run is the run of that pid whose first line is the latest at or before the `covered` line, whether or not it had taken its slot by then, so a gate covered by a stand-in reads its end from the stand-in's `release`. A gate covered by a lent gate finds no released run, since a lent gate journals no `release`, so its row has no status, held time or verdict. A covered run is never counted in the summary.

A covered run that also journals a `take` of its own (it took a slot late) is no longer a lone `covered` line: it reads as a run with its own take, `running` while its gate runs, `unrecorded` once the gate is gone without a `release`, and released with its own `release`, its status, held time and verdict its own. It counts toward the 50 newest runs, outside the covered quota, and the per-command held-time summary leaves it out, its held time covering only the stretch after the parent ended.

The history that ops-tower serves (`HistoryRun`, `GET /api/gate/history`) carries `verdict`, `wallSeconds` and `adjustedSeconds`, null when a line has none, and `abandoned`: every name any line of the run lists (`parent` and `covered` lines included), in line order without repeats, always present and empty when there are none.

It also carries `reason`, the free text the run's `queue` line holds from `--reason`, omitted when the run gave none; the first `queue` line that names one wins. A covered run, and one lent its parent's slot before queueing, writes no `queue` line, so it carries no reason.

It carries `cold` (`true`) when the run's `release` line says `"cold": true` ([Cold run](GLOSSARY.md#cold-run)), and omits it otherwise: a run with no release, and a release without the field, have none.

It carries `criticalPid` when the run's latest remote line names one ([Routing](#routing), "What routes"): the gate of the critical request that sent the light run to a builder. A fallback clears it, and every other run omits it.

The history lists the 50 newest runs that are not covered, and at most the 50 newest covered runs newer than the oldest of them: at most 100 runs, newest first by the time of each run's first journal line (its queue line, or the `parent` or `covered` line of a gate that never queued), then by pid and process start.

A run fallen back onto or lent its parent's slot counts toward the 50; a covered run does not (one that took a slot late does), so a parent with many nested covered gates never crowds other runs out. The 7-day prune keeps every run the history lists, the parent run of each listed covered or fallen-back run, whose row takes its status, held time and verdict from it, and every open run.

The history also carries `open`, every [open run](GLOSSARY.md#open-run) uncapped, newest take first, then by pid and process start, each in the same row shape as `runs`. A run can be in both lists, so a reader dedupes them by `pid` and `processStart`.

The prune keeps an open run whatever its age, until its gate stops being live. An open run whose `take` has rolled out of both journal files is not found: every roll makes the cache read both files again from the start. On a busy PC the two files hold about a day of runs, so an open run older than that drops out of `open`.

It also carries `pid` (a number), `processStart` (the ISO string from the journal) and `running` (a bool, true while the run's gate is live by the same pid-and-start check as the tickets', for every run): a covered run's `state` stays `covered` while it runs.

It also carries, after `slot`, `slots` (the slot names the run took, such as `["Local\\gate-slot-0"]`, `[]` when none) and `machine` (null, or for a critical gate's `take` `{counts, otherHeld, otherClaims}` or `{counts, otherHeld, snapshotError}`). A `fallback` line clears `slot`, `slots`, `machine`, `started` and `waitedSeconds`.

A run a fallback sent back after it took a remote slot (`rebooted`, `unreachable` mid-run, or any other fallback after a remote `take` line) keeps that take as `remoteTake`: `{"started": <ISO>, "waitedSeconds": <number or null>, "fellBackAt": <ISO>}`, the remote `take`'s time and wait and the `fallback` line's time (`RemoteTake` in `gate-state`). A run that tried two builders keeps the last attempt's `fallback` and `remoteTake`.

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

The claim is written before the `take` line, for a local, remote, fallback or late take and for a critical gate's (its holds line, then `gate-critical.json`, then `take`), so a reader that sees a `take` line finds that gate's claim already written.

The exception is a claim that could not be written: the gate says so and still journals the take, and the slot stays held. A gate killed between its claim and its take leaves a claim with no `take` line, which a reader judges by the claim's liveness ([Claim](#claim)) like any other.

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

`remote.json` keeps the PowerShell gate's format (T1), read by `gate-router` as [Routing](#routing) describes: `host`, `slots`, `repos`, the optional `data`, the optional `builders` and the optional per-repo `outputs`, `failureOutputs`, `inputs`, `siblings` and `desktop` and the optional `warmup`, `hooks`, `sendBuild` and `declarations`, read afresh by each routed gate and ignored as a whole with one line when unusable.

A host of one letter is refused (`host must not be a single letter, which a git host:path URL reads as a drive`).

The optional top-level `data` is the `host` builder's tailnet IPv4 address, read exactly as a `builders` entry's `data` is below, with the reasons worded `data must be an IPv4 address` and `data must be a unicast IPv4 address`.

The optional `hosts` is an array of strings naming further builders, which `gate push`, `gate check --builder` and `gate run --builder` reach; an unpinned routed run goes to `host` or a routed `builders` entry, never to a `hosts` entry. Each entry passes the checks `host` does, with the same reasons worded `hosts entries ...`.

The push order is `host`, then `hosts` in file order, an entry equal to an earlier one in any letter case skipped. A `hosts` that is not an array of strings, or holds a bad entry, never affects routing: `gate push` alone refuses the file with `push: ignored <path>: <reason>`.

The optional `builders` is an array of further builders that a routed run can go to, each `{"host", "os", "root", "slots", "data", "routed", "repos", "outputs", "failureOutputs", "crossBuild"}`:
- `host` is the builder's ssh alias, passing the checks the top-level `host` does (`Builder::alias`).
- `os` is `windows` or `macos` in any letter case.
- `root` is the builder's work root. It is required for `macos`: an absolute path rooted at `/`, each name made of ASCII letters, digits, `.`, `_` and `-`, none empty, `.` or `..`. For `windows` it defaults to `D:\gate-remote` and must be `X:\` followed by names joined by `\`, each made of ASCII letters, digits, `.`, `_` and `-`, none starting or ending with `.` or a reserved device name; `/` is refused (`builders[N].root must be an absolute drive path for a windows builder`).
- `slots` is 1 to 16, default 1.
- `data` is an optional string, the builder's tailnet IPv4 address in dotted form, the one address a [data channel](GLOSSARY.md) dials; a value that is not an IPv4 address makes the file unusable (`builders[N].data must be an IPv4 address`), and so does the unspecified address, the broadcast address or a multicast address (`builders[N].data must be a unicast IPv4 address`).

  A builder with no `data`, and every `hosts` entry, send every byte over ssh, exactly as before. Only a `windows` entry's `data` is used: a `macos` entry with one still gets no channel.
- `routed` is an optional boolean, default `true`; a value that is not `true` or `false` makes the file unusable (`builders[N].routed must be true or false`). With `false` the entry is a not-routed builder: `repos` may be empty or missing and no unpinned run picks or tries it, while `gate push`, `gate check --builder` and `gate run --builder` reach it like any `builders` entry.
- `repos`, `outputs` and `failureOutputs` are the entry's own, shaped like the top-level ones.

A bad entry, an alias that repeats the top-level `host`, an earlier `builders` entry or a `hosts` entry in any ASCII letter case, or a `builders` that is not an array makes the whole file unusable: `gate: ignored <path>: builders[N]...; running locally`. The top-level `outputs` apply to `host` alone; a `builders` entry with no `outputs` brings nothing back when it is `macos` and uses the default output globs when it is `windows`.

A `macos` entry's `outputs` are honoured, since the Unix box brings outputs back.

`crossBuild` is an optional object of repo name (matched in any letter case) to a runtime identifier (`Rid`) such as `win-x64`, on a `macos` entry alone: it opts the repo's routed `dotnet build` and `dotnet test` on that builder into a cross build, so the outputs come back built for Windows ([The Unix box](#the-unix-box)).
Its `Rid` is held to the box's rule (ASCII letters, digits, `-` and `.`, not starting with `.`), and the repo needs a non-empty list in the same entry's `outputs`.
A bad `crossBuild` is a soft problem, never an unusable file: a value that is no object, a `crossBuild` on a `windows` entry, a `Rid` that is no string or no runtime identifier, or a repo without a non-empty `outputs` list on the entry drops that entry's cross builds alone, and the rest of the file is read as it stands.

`failureOutputs` is an optional object of repo name to an array of globs, beside `outputs`, with the same glob rules (non-empty `-like` patterns, no `[` or `]`) and the same key on a `builders` entry, whose list wins for a run on that builder (an entry that does not name the repo has none). It is absent by default and has no default or derived globs. The files under them come back only from a failed command (Routing, Outputs).

A bad value makes the file unusable, with the refusals of `outputs`: `failureOutputs must be a JSON object`, `failureOutputs.<repo>[<i>] is not a non-empty string`, `failureOutputs.<repo>[<i>] is not a valid wildcard pattern`, `failureOutputs.<repo>[<i>] is not a valid output glob: '[' and ']' are not allowed`, and the `builders[N].failureOutputs...` forms.

The transport root, the box line, the pack commands, the push URL, the payload paths, the path map and the PC's caches follow the entry's profile (`Profile`: alias, `os`, `root`). A Windows builder's `root` is honoured; a macOS builder's box line is `<root>/bin/gate-box box <verb> --stdin`, its job is named `gate-remote-<key>-<12 hex>` and its paths are joined with `/`.

A `hosts` entry is a Windows builder with the default root and the top-level `slots`, and an id of its own, so its remote pool and its hash-cache paths are not `host`'s; its outputs are `host`'s. Under `--test-remote-transport local:<folder>` (and the other test transports) a Windows entry's root gives way to the transport's folder, and a macOS entry keeps its own root.

`warmup` is an array of repo names, matched in any letter case, that consent to the repo's `.gate/warmup.toml` warming its builder work folders (`Config::warmup_consented`); a repo it does not name, or a file without the key, has no consent.

A value that is not an array of non-empty strings consents to none and leaves the rest of the file usable, and `warmup_consented` gives its reason. `gate check` asks the consent only of a repo `repos` lists. A routed run reads the consent from the same `remote.json` read its routing decision used, one read per run.

`hooks` is an array of repo names, matched in any letter case, whose heavy gates started inside a git hook (`GIT_INDEX_FILE` set and non-empty) may route (`Config::hooks_allowed`, `Config::hooks_name_a_repo`). A hook gate of a repo it names routes like any heavy gate, with the remote-tagged `queue`, `take` and `release` and the usual routed lines. One of a repo it does not name stays local silently, as does every hook gate when the key is absent, empty or unusable.

A value that is not an array (`hooks must be an array of repo names`) or holds an item that is not a non-empty string (`hooks[N] is not a non-empty string`) consents to no repo and leaves the rest of the file usable; routing says nothing about it, and `gate check` says it as a missing item, `check: here: remote.json: <problem>`. A hook gate whose place git cannot read stays local silently too. A consented hook run whose first word is `git` or `prek` still stays local silently.

The key is for check-only hook commands (a build, clippy, a format check): a routed fixer's patch comes back to the worktree under prek's stash. The hook entry calls `gate run ... -- <heavy command>`.

The `git commit` that runs the hook is a commit run ([Commit run](#critical-drain-and-nesting)) on `--slot heavy` or `light`, so it holds a slot for its hooks: a heavy commit covers the hook's gate, which still routes when its patterns match and takes its own remote slot, and a light commit does not cover a heavy hook gate, which takes a heavy slot of its own.

**Declarations folder.** `declarations` is an object mapping a repo name, matched in any letter case, to an absolute folder: `{"declarations": {"<repo>": "D:\\notes\\<repo>\\gate"}}`.

For a repo that cannot commit its `.gate` files, the folder holds `toolchain.toml`, `siblings.toml`, `desktop.toml`, `local.toml`, `env.toml`, `warmup.toml` and `windows.toml`, each read only when the repo has no file of that name: the snapshot's tree for `toolchain.toml` in a routed run, the worktree on disk otherwise. A file is taken whole from one place, so a repo's own file wins even with problems, and a file the folder lacks is simply absent.

The folder asks no new consent: `desktop` and `warmup` stay in `remote.json`, and `local.toml` and `env.toml` need none, and the warm-up hash stays the SHA-256 of the file's bytes wherever they came from.

The folder must be an absolute drive path (a letter, `:`, a separator and a name, with no share, device or `\\?\` path) on a fixed local drive, a real folder and no link or junction. No name of the path may end in `.` or a space or hold `:`, which also refuses `.` and `..`. A drive whose letter is a `subst` onto a share is not local; a letter whose device cannot be read counts as no `subst`.

The folder is opened as itself and held while a file is read beneath it, the file read as a `.gate` file is (a link refused, 64 KiB at most). Routing, `gate warm` and `gate check` read through `route::read_declaration`, a routed run's manifest through `route::read_declarations_copy`.

A repo whose `.gate` is there as something other than a folder has answered that it holds no file, so nothing is read from the declarations folder for it. A `.gate` that is a link or a junction, or cannot be opened, is the repo's own problem and likewise wins.

A folder that cannot be used reads nothing and the run goes on as if the repo had no copy.

The log alone says `gate: declarations for <repo> not read: '<value>' <why>`, `<why>` being `is not a folder on a fixed local drive`, `does not exist`, `is not a folder`, `is a link or a junction` or `cannot be read: <error>`; a `declarations` value that cannot be used says `gate: declarations for <repo> not read: remote.json's declarations cannot be used: <problem>`. The line is said once a run, however many reads reach the folder.

A file taken from the folder logs `gate: .gate/<name> read from <full path>`, and a routed client keeps that line in the log after a fallback. [`gate check`](#gate-check) says both on its output.

The repo's own file wins whenever the worktree has one, readable or not (a folder, over 64 KiB, a `.gate` that is a junction, or one that cannot be read). When the folder holds anything by that name the log says `gate: .gate/<name> shadows <full path>`, the copy only opened, never read.

A routed run says `gate: .gate/toolchain.toml shadows <full path>` once when the snapshot has its own manifest and the folder has a copy. A problem with a file taken from the folder is worded with its full path as written, for routing `gate: not routed: <full path>: <problem>`.

A routed client that cannot use `remote.json` logs `gate: ignored <path>: <reason>; reading no declarations folder`, also when the snapshot has its own manifest.

ops-tower reads only `slots`.

A repo's `.gate/desktop.toml`, which declares the commands that need the builder's desktop, is read by `gate_router::desktop::read_desktop`: one key, `patterns`, an array of at most 32 non-empty `-like` patterns (`patterns = []` declares none). A leading byte order mark is skipped and any other key is a problem.

The first problem wins, the count checked before the entries: `declares no patterns`, `patterns is not an array (TOML <type>)`, `patterns[<i>] is not a string (TOML <type>)`, `patterns[<i>] is empty`, `patterns[<i>] is not a valid wildcard pattern: "<text>"`, `declares more than 32 patterns`, `<key> is not a key (patterns)`, plus the UTF-8 and parse failures. Routing reads it for a matched command ([Routing](#routing)).

A repo's `.gate/windows.toml`, which declares the paths whose change keeps a run off a macOS builder, is read by `gate_router::windows::read_windows`: one key, `paths`, an array of at most 32 non-empty `-like` patterns matched against a whole root-relative `/` path, letter case ignored and `*` crossing `/`; an entry `../<sibling>/...` matches that declared sibling's changed paths. A leading byte order mark is skipped and any other key is a problem.

The first problem wins, the count checked before the entries: `declares no paths`, `paths is not an array (TOML <type>)`, `paths[<i>] is not a string (TOML <type>)`, `paths[<i>] is empty`, `paths[<i>] is not a valid wildcard pattern: "<text>"`, `paths[<i>] names the sibling "<name>", which .gate/siblings.toml does not declare`, `paths[<i>] names a sibling but no path in it` (for `../<name>`, `../<name>/` and `../<name>*`), `paths[<i>] names a sibling, but <siblings file> has a problem`, `declares more than 32 paths`, `<key> is not a key (paths)`, plus the UTF-8 and parse failures.

Any problem counts the change as unknown ([Routing](#routing), Windows paths). `paths = []` reads as no entries and asks no git.

`.gate/local.toml` is read by `gate_router::local::read_local` and `.gate/env.toml` by `gate_router::env::read_env`, all three through the shared `list.rs` `read_list`, so the same one key, cap, byte order mark and problem texts hold with `patterns` (`local.toml`) or `prefixes` (`env.toml`) in place of the key.

Each `env.toml` entry must be ASCII letters, digits and `_` (`prefixes[<i>] is not ASCII letters, digits and _: "<text>"`) and not start `GATE_` in any letter case (`prefixes[<i>] starts with GATE_, whose names are the gate's: "<text>"`).

A repo's own `.gate/toolchain.toml`, which declares the tools and workloads a routed run's builder needs, is a separate file in the snapshot, not part of `remote.json`, or the repo's declarations folder's copy when the tree has none: [Routing](#routing) (Toolchain manifest) describes its format.

A repo's `.gate/warmup.toml` (or its copy in the declarations folder when the worktree has none), which declares the warm-up commands, is read by `gate_router::warm::read_warmup`: `commands`, an array of 1 to 8 commands, each a non-empty array of non-empty strings, and the optional `timeout`, whole seconds from 1 to 86400, 1800 when missing.

A leading byte order mark is skipped, any other key is a problem, and any problem means no warm-up. `gate warm` reads the file ([`gate warm`](#gate-warm)), and so does a routed run of a consenting repo, for the file's hash, and `gate check` ([The warm-up items](#the-warm-up-items)), which gives every problem; a file problem does not stop a routed run, and no route starts a warm-up.

A warm-up command is worded for the builder as a routed run's words are. A command whose first word is `git` or `prek`, named by any path or sent as that program's name (`C:\x\git.exe.exe status`), is a file problem, `gate: warm: .gate/warmup.toml command <n> starts with <word>, which a routed run never sends`; a first word naming another program by a path outside the worktree is sent as the program's name ([Routing](#routing), the in-worktree path rewrite), not a file problem.

The judge, `gate_router::warm::judge`, says where a work folder's warm-up stands, given the repo's consent, the file, the work folder's record and whether its warm-up lock is held; `gate check`'s warm-up item reads it, and a routed run does not.

In order: no consent or no file is no warm-up; a file problem is a problem; a held lock means a warm-up is running, whatever the record says; an unreadable record is a problem; no record, a `warming` record with a free lock, or a `warm` record of another host or file is due a warm-up; a `warm` record of this host and the file's hash is ready.

A `failed` record of this host, file hash and gate build is failed until its backoff has passed, 30 minutes after `commands` and 60 seconds after `refused`; a failure of another host, file or build, or one written in the future, has no backoff and is due a warm-up. Every `<…>` text is escaped to one line, and a value read from a record is cut to 200 characters plus `…` first.

A routed run of a repo with consent and a `.gate/warmup.toml` that parsed routes from the worktree's first run, whatever the work folder's warm-up record or lock says: the route reads neither, and the run call carries `warmth: {"consented": <hash>}` with the file's hash, which only words the cold note. A file problem keeps no run here (`gate check` and `gate warm` report it).

A repo with no consent or no file, or a file that did not parse, routes with `warmth: null`. A routed run reads the record only to drop it after a `layout` fallback ([Output lines](#output-lines)), and probes the lock only while it waits for its work folder's key (a running `gate warm` for that key falls the run back `busy`).

A new worktree's work folder is prewarmed only by a `gate warm` someone runs; a routed run in it is a cold run, and its cold note ends `; prewarm the next new worktree with gate warm` for a repo whose warm-up is set up ([Cold run](GLOSSARY.md#cold-run)).

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

  When the parent ends while the command still runs, the gate looks for a stand-in (below); finding none it says `gate: the parent gate <pid> ended; taking a <kind> slot for the rest of this run`, queues a ticket of its own kind with the run's first arrival, and takes a slot as soon as one frees. The claim is written and the journal records a `take` with `parentPid` naming the ended parent, and the run's end journals a `release` of that slot and lets it go as any slot is.

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
- **The in-box gate.** A routed run's box gives its command `GATE_BOX_JOB`, the name of the run's job ([The box side](#the-box-side)). A gate that reads it (`Environment` in `resolve.rs`, beside `GATE_SLOT_HELD`) trusts it only when the name has the box's shape, `Local\` or `Global\` and then `gate-remote-<key>-<12 hex>`, and that job holds this gate's own process: asked from the gate's own handle, with no ancestor walk, since a box run's job lets nothing break away.

  The check runs first, before the slot marker is read, and a trusted job wins over a marker.

  A name of another shape is ignored without opening a job. A job that does not hold the gate says `gate: ignored GATE_BOX_JOB: this gate does not run inside its job`; one that cannot be opened or queried says `gate: ignored GATE_BOX_JOB: cannot judge its job: <error>`. Either way the gate goes on as without the variable and removes `GATE_BOX_JOB` from its own command's environment.

  A trusted gate (`Nesting::InBox`) is in-box, whatever slot it asked for. It takes no slot, ticket or claim, never waits and never routes, the router seeing a covering parent, and says `gate: inside a routed run; running without a slot on this builder`. It prints no cores hint and no route hint and opens no durations, so `--timeout auto:N` uses its floor.

  It still runs its command with its own job, ceiling, stall watchdog, log and verdict, at the priority and watchdog demand of the slot it asked for, a critical gate running as heavy. Its journal `take` has `slots: []`, `waitedSeconds` 0 and the kind as asked (critical as heavy).

  Its command gets no `GATE_SLOT_HELD`, and gets `GATE_BOX_JOB` and `GATE_BOX_EXE` as the gate had them, so a gate nested deeper is in-box too. A dropped `--no-build` says `gate: dropped --no-build: <reason>; building on no slot`.
- A critical gate under a heavy or light marker is refused (K7) on stderr only, exit 2, before the log mutex, with nothing queued or journalled: `gate: --slot critical cannot run under the <held kind> slot of gate <pid>: it would wait for that slot for good; run the critical command outside that gate` (`-Slot critical` through the PowerShell spelling).
- A heavy gate under a light marker takes its own slot and says `gate: the <held kind> slot of gate <pid> does not cover a <kind> gate; taking a <kind> slot` just before its local take (after a Local route or a fallback, before its wait lines); a gate routed to the builder never says it.
- **No slot.** A `none` run takes nothing and is never held by the drain or the barrier. Its marker is `GATE_SLOT_HELD=none:<pid>:<start>[:<h>]`. A gate under a `none` marker (the case `Nesting::Beside`) takes its own slot as a top-level gate would: a critical child takes the machine and is never refused (K7 applies to heavy and light markers only).

  Such a gate prints no nesting line and is never lent a slot, and it writes the pause marker, for its no-slot parent's stall window, only while it waits for a local slot behind a critical request. Its `take` names `parentPid`.

  A `none` gate under a heavy, light or critical parent is covered: it journals `covered` with kind `none`, prints `gate: running under the <held kind> slot of gate <pid>` and hands the parent's marker on.
- **One request at a time.** A critical gate queues a ticket of kind `critical` and runs after every earlier live critical ticket, in arrival order, each through the critical mutex ([Kernel object names](#kernel-object-names)). An abandoned critical mutex counts as taken, with the `took <name>, which a killed gate had left held` line.
- **The drain.** With the mutex held, the gate waits for every earlier live heavy ticket, and for every earlier light ticket when the light count less the beside allowance is above 0 (always, for an alone request, whose allowance is 0). Remote tickets are ignored, and so is a ticket whose gate already holds a live claim and only failed to delete its ticket.
- **The take.** It then takes every heavy slot of the counts as they stand, and on every pass the free light slots, lowest index first, until it holds all but the beside allowance of the light count: the allowance is the number of light gates that may run beside the critical request, whichever light slots they hold: `min(2, light count)`, or 0 while a live critical ticket, its own included, says `alone`.

  It does not choose slots to leave; the slots it leaves free are the beside slots of this drain.

  It takes light slots only on a pass where no more light gates than the allowance run, so it drains past those within the allowance. It counts and takes holding the beside mutex ([Kernel object names](#kernel-object-names)): the light gates running are the light slots of the count some gate has open that it does not hold (a gate between its take and its claim included, and a slot that cannot be probed counted as running) and the other gates' live light claims past the count.

  A pass that cannot take the mutex or list the claims folder takes nothing, and one that took a slot wakes the waiters.

  A slot a killed gate left is a takeover, journalled in `abandoned`, and the gate deletes that gate's claim, since it writes none for the slot.
- **The hold.** Once every slot it takes is held and no gate runs lent its light parent's slot, the gate rewrites its ticket with `holds: true`, keeps it until it ends, and writes the one claim `slots\gate-critical.json`. Its `take` and `release` list the slots held ([Journal](#journal)).

  It counts the lend markers and rewrites the ticket under the beside mutex, so a gate lent a slot after the count sees the ticket and is not lent. A rewrite that fails is said once, is no hold and is tried again on the next pass; so is a claims folder that cannot be listed or holds an entry that cannot be read.

  While lend markers are live the gate rewrites its ticket once with `lendsClosed: true` and waits for those lent gates to end. No light parent lends its slot again, so the drain waits only for the lends already made.
- **Commit run.** A gated command whose first word's file stem is `git` (any letter case, `git.exe` and a full path included) and whose first non-option word is `commit`, on any slot but critical.

  Git options before the subcommand that take a value (`-C`, `-c`, `--git-dir`, `--work-tree`, `--namespace`, `--super-prefix`, `--config-env`, `--attr-source`) are skipped with their value unless joined by `=`; `--help`, `-h`, `--version`, `--exec-path` without `=`, `--html-path`, `--man-path` and `--info-path` before it make it no commit run, as does `commit-tree`. A wrapper or an alias is not recognised. It never routes (a `git` first word never does).

  Its slot is the caller's: `--slot heavy` or `light` takes that slot, and `--slot none` takes a light one and says `gate: a commit's hooks take a slot: pass --slot heavy or --slot light` once on stderr and in the log, before the slot wait (not under a covering parent, nor inside a box run).

  The command runs at BelowNormal like any run. The slot is held for the whole command, hooks included, and the journal's `queue`, `take` and `release` carry the kind and `"commit": true` ([Journal](#journal)).

  It hands its command `GATE_COMMIT_SLOT=<its marker>` besides `GATE_SLOT_HELD`. A gate in its hooks whose `GATE_COMMIT_SLOT` equals its parent's marker knows it runs in a commit's hooks: a heavy commit covers every gate there but a critical one (a `covered` line), while under a light commit a heavy gate takes a heavy slot of its own or is lent one, as under any light parent.

  A covered gate still routes as a gate under no parent would: one that routes takes its own remote slot (a routed gate keeps its durations), and on a fallback says the `running under` line once and journals `covered` before it replans. A dropped `--no-build` under a light commit says `gate: dropped --no-build: <reason>; building on a heavy slot`.

  A critical gate, or an `--alone` one, in a commit's hooks is refused, exit 2, on stderr only with the earlier log left in place: `gate: inside a git commit's hooks; a critical or --alone gate cannot run there`. The commit's own cores hint names its hooks ([Cores hint](#cores-hint)).
- **The barrier.** A gate that finds a live critical ticket that arrived before its own (by `arrived`, then pid; its own ticket never counts) is held back by the earliest such request, whether or not that request holds the machine yet. Gates that arrived before the request run first and are waited for. A heavy gate tries no slot. A light gate tries any light slot while fewer light gates than the beside allowance run beside the request, and none once that many do.

  While a live critical ticket says `alone` (the earliest such ticket, ahead of the gate or not, once a request holds the gate back) the allowance is 0 and no light gate tries a slot.

  The light gates beside a request are the other gates' live light claims, whichever slots they hold, counted by a gate that holds the beside mutex from its count to the write of its own claim, so a gate between its take and its claim is counted. A pass that finds the mutex held by another gate has no room. The allowance is taken from the light count as it stands on each pass. A remote wait has no barrier.

  A local light gate the barrier holds back with no room beside the request (a plain request whose allowance is used up, or an `--alone` one) may leave its wait to route: it is a [rerouted light gate](GLOSSARY.md), described in [Routing](#routing), "What routes".

  The pass that finds no room, with the beside mutex not busy, ends the wait as held back instead of sleeping, once per run and only for a gate that asked the routing decision at its start, is neither lent nor covered, and passed no `--builder`.
- **The lend (N2).**

  A heavy gate under a light parent (the one nesting case where the parent's slot does not cover the gate) is lent the parent's slot, whichever light slot the parent holds, takes no slot of its own and says the lend line, while a critical request is pending and none refuses a new lend.

  A request is pending when another gate has a live critical ticket, else when the critical mutex is open; the mutex is not read when a ticket could not be read, since it may be a critical request's. A request refuses lends when its ticket says `holds` or `lendsClosed`, an alone request exactly as a plain one: its draining never closes them earlier, since a light gate whose heavy child was refused would wait for that child while the request waits for the light gate's slot.

  A lend that rests on the critical mutex alone is not made after an unreadable ticket, and the gate queues.

  Once the critical gate holds every slot it takes and closes its lends, nothing is lent, and the child waits behind it like any gate.

  The rule is checked before the gate queues, when the gate is lent without routing, queue line or ticket, and on every pass of its wait once it has queued, when it deletes its ticket and wakes the waiters. A lend is decided holding the beside mutex, which the gate holds until its lend marker ([Claim](#claim)) is written; one that cannot take the mutex is not lent that pass, and one whose marker cannot be written is not lent: the gate queues, or waits on.

  The marker stays from before the command to its end, and a critical gate holds the machine only once no marker is live. The lent gate hands its command its own marker, `GATE_SLOT_HELD=heavy:<its pid>:<its start>[:<h>]`, not the parent's.

  Every gate nested in its command is therefore covered by the lend: it journals `covered` with the lent gate as `parentPid`, says `gate: running under the heavy slot of gate <pid>`, and asks for no slot or lend of its own. A critical gate nested there is refused (K7), naming the lent gate. The lent gate journals `parent` with `parentPid` and no `take` or `release`; a gate lent as it waited has its `queue` before the `parent`.
- **Alone.** `gate run --slot critical --alone` (launcher `-Alone`) is a critical gate that holds the whole machine, the light slots a plain critical gate leaves beside it included, for a timing or profiling run: it waits for every running gate, heavy and light, takes every slot, and later gates queue behind it. In every other respect it is critical (High priority, the `crit` durations class, one request at a time). Its ticket says `alone: true` ([Ticket](#ticket)).

  `--alone` with any slot but critical is a usage error, exit 2 (`gate: --alone needs --slot critical`, `gate: -Alone needs -Slot critical` through the launcher). `--alone` under a parent gate's slot marker, a box run's included, is refused likewise before anything is queued: `gate: --alone cannot hold the machine alone from inside a gate's command` (`-Alone` through the launcher). A routed run is untouched.

  The gate cannot stop an ungated process, so the hold keeps no promise about them; the load snapshot below is the evidence of what ran. An older gate.exe still running after a reinstall reads the ticket as a plain critical request and may take a beside light slot while it waits.
- **The load snapshot.** An `--alone` run measures the machine's load in two windows of 3 s each (`load.rs`, over `Gpu3dWindow` of `gate-win32`): one just before the command starts, after the holds line, and one at the run's end before the verdict, on every end branch: a pass, a failure, a ceiling kill, a Ctrl+C, a command that cannot start (127), a gate that fails, and a Ctrl+C before the command starts.

  It adds about 6 s to the run. The lines go to the log only, never to the console, the journal or the log tail the verdict prints.
  - A Ctrl+C ends the window being measured at once, so the run does not sit out the rest of it; the CPU headline may then read `the system's CPU times did not move over the window`.
  - Every figure of the CPU lines is read over one span, the window's, and every load line is one line: line breaks in another gate's command become spaces, and a command over 200 characters is cut to 199 and `…`.
  - `gate: load at <start|end> over <s.s> s: <b.bb> of <n> cores busy outside this gate`.
  - Up to 8 lines `gate: load at <when>:   <name> <pid> <c.cc> cores`, the busiest first, each above 0.01 cores and outside this gate's tree (the gate and its job's processes), then `gate: load at <when>:   unattributed <c.cc> cores`. The unattributed figure is the headline minus the listed lines, never below 0, so it is never understated.
  - `gate: load at <when>: GPU 3D <p>% on adapter <luid>` per adapter, or `gate: load at <when>: GPU 3D: counter unavailable (<why>)`. The GPU is read on a thread of its own that has until the window plus 5 s, so a hung PDH call never holds the run. The reasons: `<call> returned 0x<hex>` for a PDH failure, `timed out`, `cannot start its thread: <error>`, and `its thread ended without a reading` (a panic on that thread prints nothing to the console).
  - `gate: load at <when>: other gate <pid> holds <kind> <index>: <command>` per live claim of another gate (the claim of a gate no longer running is left out), or `gate: load at <when>: other gates: none`.
  - Failures, each in place of what it could not measure: `gate: load at <when>: cannot read the system's CPU times: <error>`, `gate: load at <when>: the system's CPU times did not move over the window`, `gate: load at <when>: cannot list the processes: <error>`, `gate: load at <when>: other gates: cannot read <path>: <error>` and `gate: load at <when>: cannot list this gate's job: <error>; its processes count as outside this gate` (at most once per block).

    A wait on the interrupt that fails says `gate: load at <when>: cannot wait on the interrupt: <error>; the window was waited out`.
- **The pause marker (N3).** A gate waiting for a slot under a parent gate's marker that does not cover it keeps the empty file `paused\<parent pid>-<parent start>-<pid>-<start>.pause` in the state root. It writes the file on every pass of the wait while a live critical ticket that arrived before its own exists (the barrier), setting its last-write time explicitly each time, so the file's age shows the gate is alive.

  It deletes the file when that stops and however the wait ends: take, lend, timeout, Ctrl+C or a panic. A write or delete that fails is said once per wait, each, and the wait goes on.
- **Reading the pause markers.** `PauseReader` (`pause.rs`) answers on each watchdog sample whether a gate under this one waits behind a critical request. No `paused` folder means no pause.

  A marker counts when its name parses, names a parent the gate reads for (below), its child gate still runs with the start in the name (a start that cannot be read counts as not running), it was written at most 30 s ago (exactly 30 s is live) and the child's ticket exists. A marker gone while it is read is skipped. On the first marker that counts, the answer is whether a critical request is pending (a live critical ticket, else the critical mutex open).

  A folder, the waiting tickets or the critical mutex that cannot be read is an error the caller says once, and the parent pauses nothing: the watchdog says it once per watch, counts that sample as not paused, and reads again on the next.

  A gate reads for itself (its own pid and start) and, when its parent's slot covers it, also for that parent, since the gates under its command name their markers after the parent whose marker the command gets.

  A lent gate hands its command its own marker, so it reads only for itself; every gate nested in it is covered, waits for nothing and writes no pause marker. A marker named after that parent counts only when its waiting gate runs in this gate's job: a sibling the parent started beside this gate names it too, and must not keep a hung gate from being stalled.
- **Lines**, on stderr and in the log:
  - `gate: waiting for the machine (critical): <h> heavy and <l> light slots held (<r> light slot|light slots beside it), <n> earlier waiters`, the first time a pass falls short; a critical gate that takes the machine on its first pass says nothing.
  - `gate: still waiting for the machine (critical): <status> (<s> s)`, with the same status, on each heartbeat.

  A gate asked for `--alone` says `the machine alone (critical)` where these lines and the holds line say `the machine (critical)`.

  `<r>` is the beside allowance (the take, above): `min(2, light count)`, 0 while an alone request waits or holds. The slots other gates hold count each live lend marker as a heavy holder.

  On both lines, when no earlier waiter and no earlier critical gate is ahead and every holder the drain waits for has a usual, the status ends `, the last frees in about <eta>`, or `, the last is <over> past its usual <p50>, ceiling <ceiling>`, naming the latest expected end instead of the soonest, in the figures of [Slots](#slots), step 6.

  The holders are the heavy slots some gate has open, the light slots past the count that live claims hold, and of the light slots some gate has open, all but the allowance of those whose usual ends come soonest, each by its live claim's `expected`, and each live lend marker by its own `expected`. It is given when only lent gates block, and dropped when a marker cannot be read or has none.
  - `gate: holds the machine (critical) at <yyyy-MM-ddTHH:mm:ss.fffffffZ> after <s.d> s: counts heavy <H>, light <L>; other gates hold <h> heavy and <l> light slots (<r> ... beside it); other live claims: <pid> <kind> <index> <command>, ...`, the claims ending as `none` or `unknown (<reason>)` when they could not be listed.
  - `gate: waiting for a heavy slot behind the critical request of gate <pid>`, from a heavy gate the barrier holds back, on its first pass whether or not a slot is free, with `<pid>` the earliest request's gate; then `gate: still waiting for a heavy slot (<s> s)` on each heartbeat.
  - `gate: waiting for a light slot beside the critical request of gate <pid>`, from a light gate the barrier holds back, likewise, then `gate: still waiting for a light slot beside the critical request of gate <pid> (<s> s)`. A light gate held back with no room whose command would route prints neither: it says the line below instead.
  - `gate: held back by the critical request of gate <pid>; sending this light run to <alias>`, from a rerouted light gate, then the route lines ([Routing](#routing)), then the run goes on the builder. Prose, not a stable line.
  - `gate: waiting for a light slot behind the request of gate <pid> that holds the machine alone`, and `gate: still waiting for a light slot behind the request of gate <pid> that holds the machine alone (<s> s)`, from a light gate the barrier holds back once a request to hold the machine alone is live, in place of the two lines above. `<pid>` is the earliest live alone request, ahead of the light gate or not.
  - `gate: running under the <held kind> slot of gate <pid> while a critical request drains; taking no slot`, from a lent gate, said before its command starts.
  - `gate: interrupted while waiting for the machine`, on Ctrl+C: the journal gets `abandon`, everything taken is released and the exit is 0xC000013A.
  - `gate: cannot read the claims in <dir>: <error>`, said once, for the claims folder or an entry of it that cannot be read. An entry that cannot be read keeps the critical gate from holding the machine on that pass, since it may be a lend marker.
  - The lend marker lines of [Slots](#taking-a-slot), step 7: `cannot write the lend marker`, `cannot delete the lend marker`, `ignored the lend marker` and `cannot delete the dead gate's lend marker`.
  - `gate: could not write the pause marker <path> (<reason>)` and `gate: could not delete the pause marker <path> (<reason>)`, each said once a wait.
  - `gate: could not read the pause markers in <folder> (<reason>); a gate under this one pauses nothing`, and the same with `the waiting tickets in <folder>` or `the critical mutex <name>`, from the watchdog, once per watch; likewise `gate: could not read the processes in this gate's job (<reason>); a gate under this one pauses nothing`.

## Watchdog

All kept, with each knob's default unchanged:

- The sampler (W1): every 5 s while the command runs, it reads, in order, the pause state, the bytes the command has written to its stdout and stderr (counted by the readers of its pipes), the job's pids, one process snapshot, then the WSL VM, the system times and the job's accounting; nothing goes through WMI.

  Each sample carries two stamps: `began`, taken before its reads, and `at`, taken after them, next to the system times and the job's accounting. The `interval`, the load-adjusted clock, the paused time, the free share and the exit use `at`; the PowerShell gate stamps once, before its reads.
- Stall (W2): no output, no CPU (at least 1% of one core over the window), no new process in the job, no WSL VM CPU, no nested gate paused behind a critical request and no live nested gate in the job, for `--stall` seconds: `gate: STALLED: ...`.

  The quiet time runs from the last progress sample's `at` to the latest sample's `began`, not to the moment the watcher checks: a watcher starved of CPU wakes late, and the time it spends in its reads, and after them up to the check, is not the command's quiet time, so the next sample decides.

  A quiet command is therefore killed up to one sample period after its window, plus the read time of the last progress sample and of the latest sample, never before; the PowerShell gate measures to the check and can kill earlier. The ceiling and the backstop read the check's own time.

  Progress as the watchdog crate counts it resets the stall clock when the sum of the bytes the command wrote to its stdout and stderr changed (it only grows), the CPU of the job and of the processes outside it under its members (the escaped tree, below) since the last progress reached 1% of one core over the stall window together, a pid appeared in the job that the last sample did not have, a process outside it appeared that the last sample did not have (by pid and start), a nested gate is paused, or the job holds a live nested gate.

  Output is the command's own bytes alone: a line the gate writes to the log (a `gat` line, a process line included) never resets the stall. A new pid in the job, or the job's CPU, still counts as progress, so a job that keeps starting processes never stalls on a local run, whatever its process lines say.

  A nested gate is a `gate.exe` in the job other than the watching gate that started at or after the command's start (a routed or local gate, on any slot; it matters most for a `--slot none` or light parent of a routed hook build, whose child writes next to nothing and uses next to no CPU here). Its own watchdog judges its command, so the parent is not stalled while it lives.

  Only the pause stops the load-adjusted clock: a nested gate's time is counted as any other and in the backstop's wall time, so `gate: TIMED OUT after ...` and `gate: BACKSTOP: ...` still end the run. A gate outside the job (an escaped process, or under a packaged or Store pwsh, which runs outside it) does not count.

  Worst case, a wedged nested gate, or any process named `gate.exe` left in the job, holds off the stall until the parent's ceiling and backstop. The box side's ceiling runs with no stall window and is unchanged.

  The stall clock also resets, for a `wsl` command, when the WSL VM's CPU counts as progress (see WSL below).

  A build server outside the command's job and its escaped tree is not progress: only the job's own processes and the escaped tree count, a per-run server inside the job or under a member included.

  A process waiting in the kernel on another process's work is not progress either. With Smart App Control on, a first launch of freshly built images waits in each new image's reputation check through Defender, with no output and next to no CPU of its own: about 0.4-1.15 s per image was measured on the PC, so a freshly built engine loading a few hundred new DLLs can be killed as STALLED.

  With it off, a fresh exe starts in about 8 ms and a fresh DLL loads in about 2 ms. The builders turn it off (`install-toolchains.ps1`, step `Smart App Control off`); elsewhere, turn it off or pass a longer `--stall` to the first run after such a build. The watchdog does not read that wait per sample, because nothing it can read tells it from a hang: a hung pipe read waits with the same wait reason, a scanner's CPU runs whether or not the job waits, and a polling hang moves the I/O counters.
- The load-adjusted clock and the ceiling (W3): each sample adds `interval * free`, the interval running between the samples' `at` stamps, to the clock `--timeout` runs on (for `auto:<n>`, the ceiling the take resolved; the backstop below uses it too); a sample taken while a nested gate is paused adds to the paused time instead; `gate: TIMED OUT after ...`.

  The free share is 1 for a High-priority command; otherwise `busy` is the share of the interval's system time that was not idle, the cores others held are `max(0, busy * cores - own cores)` with `own cores` the CPU gained over the interval by the job and by the escaped tree (below), and `free = clamp((cores - others) / demand, 0.05, 1)`. `demand` is every core, `min(cores, 2)` for a light gate, or 1 for a gate on no slot, so its ceiling stays near wall time on a loaded PC.
- The backstop (W4): wall time less paused time reaching 5 times the ceiling, whatever the load: `gate: BACKSTOP: ...`. One sample's precedence is STALLED, TIMED OUT, BACKSTOP.
- Sampler failure (W5): `gate: sampler failed: <message>; watching by wall time only`, then the backstop alone watches. The pause markers are still read, and a paused stretch, that of the failing sample included and running to the failure's own fresh stamp, is left out of the backstop's wall time.

  A job's process list is not a failure when it names fewer processes than the job counts: while short-lived members come and go, Windows answers a read with room to spare that way, so a read that did not fill its buffer is taken as it is. The list grows only when Windows says it needs more room, to the job's count plus 16, and fails after 8 reads with `the job's process list kept growing past <n> entries over 8 reads`.
- WSL (W6): for a command whose first word is `wsl` or `wsl.exe` (a path allowed, case-insensitive), the VM's CPU since the last progress reaching a tenth of one core over the stall window counts as progress. The VM's CPU is not added to the command's own CPU, since the VM is the only signal of the work inside it; a `wsl` relay among the job members' descendants is an escaped process like any other, and its CPU counts as the command's own.

  The rule watches a local run; on the box, the ceiling runs with no stall window and the WSL rule off (`boxside/ceiling.rs`), and the heartbeat applies its own VM rule to a `wsl` command (The box side, below). A kill ends `wsl.exe` but not the processes inside the VM.

  The reader is `gate_win32::wsl_vm_cpu` (`times.rs`): the sum of kernel and user time over every process named `vmmemWSL`, read in-process through `NtQuerySystemInformation(SystemProcessInformation)`, never through a child process (an unelevated `OpenProcess` on the VM process is denied, and so is HCS). Plain `vmmem` is not read, since it can be any Hyper-V VM.

  The undocumented entry layout is checked: a matched entry whose create time is zero or more than a day past now, or whose times are negative, is an error.

  No `vmmemWSL` (a cold start) is silent: there is nothing to count, and the next reading is a fresh baseline, as it is when the VM restarts between samples (its CPU fell).

  A read that fails is said once per run, by the first sample whose read fails, on stderr and in the log: `gate: WSL VM CPU unavailable (<message>); retrying each sample, and until a read succeeds this wsl command counts only output and job CPU as progress`. A failed read at the watch's start alone says nothing, and every sample retries.
- ssh (W6a): a local STALLED kill of a command whose first word is `ssh` or `ssh.exe` (a path allowed, case-insensitive; not `sshd`, `ssh-keygen` or `wsl ssh ...`; `is_ssh_command` in `gate-watchdog`) says one more line on stderr right after the STALLED line, `SSH_STALL_LINE`: `gate: a command run over ssh shows no CPU on this machine, so it reads as stalled here; routed with gate run --builder <host>, it is watched by the builder's own CPU`. The line goes to stderr and the log, not to the journal or the release text (the kill text stays the STALLED line alone), and a TIMED OUT or BACKSTOP kill and the box side do not say it.
- A command that exits on its own before the kill keeps its status (W7).
- Build servers: the per-run server settings of [Command launching](#command-launching) (the run id as `SharedCompilationId` and `MSBUILDNODEHANDSHAKESALT`, `MSBUILDUSESERVER=0`, `DOTNET_CLI_USE_MSBUILD_SERVER=0`, `MSBUILDDISABLENODEREUSE=1`, `UseSharedCompilation=false`), so each run has its own MSBuild nodes and no compiler server idles in its job. No gate classifies, lowers or raises a build server.

  The stall check and the load figures count the job's own processes and the escaped tree only: a build server that is neither in the job nor under one of its members is neither progress nor the command's own CPU in the free share, and a server in the job, or escaped from a member, is one of its processes like any other.

    Each run has build servers of its own because a shared one is not the run's to end or to count: a server another run started would be neither in this run's job nor under its members, so a run's end could not wait on its whole tree, its CPU could not be told from a neighbour's, and a kill could not tell which server it may end.

    A server a member starts lives in the member's job, since none of the launchers (MSBuild's node launcher, Roslyn's `BuildServerConnection`) asks for breakaway and the gate's job allows none, so the run's end ends it with the job.

    The approach rejected is sparing shared servers: classifying build server images and command lines, counting their CPU only while the job held a build client, lowering or raising their priority, and leaving them out of the escaped-tree kill.

    It needed a classifier that tracks every toolchain's server names, and it could not tell a server a run needs from one that merely runs. Node reuse stays off so MSBuild nodes end with the invocation, and the compiler server's pipe id is the run's, so it is reused within the run only.

  The watch hands the pure crate raw facts: the job's CPU, the escaped tree's per-process CPU and the VM's CPU as an option (`None` for a command that is not a `wsl` one, or with no VM).
- Kill, on a STALLED, TIMED OUT or BACKSTOP alike: the diagnostics block (X3), then `TerminateJobObject(job, 124)` and the end of the processes that left the job (below), `gate: terminated the job's <n> process(es)` (with the escaped processes named, below), `gate: diagnostics written to <log>`, a 5 s wait for the command and exit 124 (X2).

  A failure inside the gate while the command runs terminates the job and ends the escaped processes first (X4); one after the command's own exit ends none of them.
- **Run end.** Once the command exits, clean or failing, and before the verdict is reached, the gate ends every process the command left in its job, with exit code 124 (`end_leftovers` in `crates/gate/src/run.rs`).

  A process the job's list still names but whose own handle shows it ended is not counted as left: the list names a process for a while after it exits, seconds on a loaded machine. So a leftover's last output is in the log the verdict's marker scan reads and a `gate: cannot end the run's job: …` line comes before the verdict line. The exit status and verdict are unchanged.

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

    When the process lines end (`Job::finish_events`), the job waits on its running members, then posts a finish packet to the port. The thread, the port's one reader, takes it after every message queued before it, so a member it had not yet taken when the command ended still gets its `spawn` and `exit` lines before the finish runs; the call waits for that within what is left of the same budget.

    It runs the finish itself when the thread has ended, the post fails or the budget is spent, so the gate's exit stays bounded by the caller's wait. A member that lives through the whole budget leaves the thread no turn.

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
  - Up to six sample lines, `gate:   sample at <w> s (load-adjusted <a> s, free <p>%): out <n> B, err <n> B, job cpu <s> s, <n> processes, escaped cpu <s> s`, with `out` and `err` the bytes the command wrote to its stdout and stderr, or `gate:   samples: none` when no sample was taken. `<w>` is the sample's `at` stamp.

    A `wsl` command's sample line also carries `wsl vm cpu <p>%` before the process count: the VM's CPU in percent of one core since the previous reading (`Watch::last_vm_percent`), `wsl vm cpu unavailable` when that sample's read failed, and 0% with no VM. Any other command shows nothing there.
  - `gate:   ... diagnostics cut at <n> ms` when the 2000 ms gather budget ran out. A gathering that failed writes `gate: diagnostics` then `gate: diagnostics failed: <message>` in its place.
  - The working set comes from `gate_win32::working_set` (`None` for a gone or exited process). stderr gets one line, `gate: diagnostics written to <log>` (also in the log), after the terminated line; the block's lines are not on stderr.
- A command still running 5 s after its job was terminated is reported: `gate: the command was still running 5 s after its job was terminated`. A job that cannot be terminated says `gate: could not terminate the command's job: <error>`. A killed run is not scanned for markers, and its release verdict is `stalled`, `timed-out` or `backstop` by the kill line that ended it.
- What a sample reads: the job's CPU by accounting (exited members included), the job's pids created at or after the command's start (a pid that answers access denied is not a member), the bytes the command wrote to its stdout and stderr, and the system times.

  It also takes one process snapshot with the job's members read before and after it, and reads the CPU of the escaped tree under the members (`crates/gate/src/watch.rs`).

  A critical run counts as High priority for the free share, and a light gate's demand is `min(cores, light threads)`.
- Ctrl+C and Ctrl+Break (X5): the gate handles them itself and prints `gate: interrupted; stopping the command` during a run. The command got the same Ctrl+C through the shared console, so the gate waits up to 5 s for it to exit on its own and exits with its status, then terminates whatever is left in the job and the processes that left it.
- A command still running after those 5 s gets `gate: the command was still running 5 s after the interrupt; terminating its job`, the job is terminated with 0xC000013A and the gate exits 0xC000013A. Either way the verdict skips the marker scan and the release is journalled, with verdict `failed` when the status is 0xC000013A.
- Ctrl+C between taking the slot and launching the command: `gate: interrupted before the command started`, no command runs, the exit is 0xC000013A and the release verdict is `unknown`.
- Ctrl+C while queued: `gate: interrupted while waiting for a <kind> slot`, the ticket is deleted, the waiters are woken, an `abandon` line is journalled and the exit is 0xC000013A. No `still waiting` line follows it. A critical gate says `gate: interrupted while waiting for the machine` and also lets go of everything it took so far.
- A console close ends the gate at once, because Windows ends the process when the handler returns; the dead-gate cleanup reclaims its ticket and claim ([Abandoned slots](#abandoned-slots)). If the handler cannot be installed the gate says `gate: cannot install the Ctrl+C handler, so Ctrl+C ends the gate at once: <error>`.

The arithmetic (stall progress, the free share, the clock, the three kills and their lines, the box heartbeat) is a pure crate driven by samples and an injected clock; the Win32 layer takes the samples. The cores hint's average, peak, free cores and verdict are in it too. The kill lines are:

- `gate: STALLED: no output and no CPU for <quiet> s (machine free <p>% on average); killed <command> and its children`, followed for an `ssh` command by the ssh line (W6a)
- `gate: TIMED OUT after <timeout> s of load-adjusted time (wall <wall> s, machine free <p>% on average); killed <command> and its children`
- `gate: BACKSTOP: <wall> s of wall time (5 x <timeout> s ceiling, load-adjusted <adjusted> s, machine free <p>% on average); killed <command> and its children`, where the middle clause reads `machine load unknown: the sampler failed` after a sampler failure.

`<p>` is the free share averaged over the wall time the ceiling counted (paused seconds left out), a whole number rounded half to even, 100 while nothing is counted; the seconds are rounded the same way. After a sampler failure only the backstop is read.

A builder counts a routed run's ceiling with the same arithmetic ([The box side](#the-box-side)) and says the lines with the host: `gate: TIMED OUT on <host> after <timeout> s of load-adjusted time (...)` and `gate: BACKSTOP on <host>: <wall> s of wall time (...)`, the host left out (`gate: TIMED OUT after ...`) when the payload names none; its stall stays the client's.

The box heartbeat (BW9) is the box wrapper's command-time heartbeat, not the wait heartbeat: a pump thread writes `gate-remote-heartbeat: <cpu ms>` every period, and while a command is watched only when the CPU used since the last beat is at least 1% of the time elapsed since it.

A period with no beat due flushes instead, and the box's flush probes stderr for a gone client ([The box side](#the-box-side)). A failed CPU read writes `gate: remote heartbeat stopped: <message>` and ends the heartbeats, never the run. `gate box run` is the crate's caller ([The box side](#the-box-side)).

## Command launching

- **The argv arrives byte-exact.** `gate.exe` reads its own command line (`GetCommandLineW`), splits it by the Microsoft C runtime's rules, and takes every word after the first `--` as the command, unchanged (C3). The PowerShell binder workarounds (C3's rejoined colon words and flattened arrays) do not exist.

  A word PowerShell splits before `gate.exe` starts reaches the command split. In a native-command call from pwsh, an unquoted word that starts with `-` and a letter or `_` is cut where its name ends: at `:` after the first bare `--` (`-RAGlb=X:/a` becomes `-RAGlb=X:` and `/a`, `-p:Name=Value` becomes `-p:` and `Name=Value`), and at `.`, `[`, `,`, `(` or `{` with or without one (`-RALog=out.txt` becomes `-RALog=out` and `.txt`).

  The gate's command line is then byte-identical to two words the caller meant, so the gate passes them on as two. A pwsh caller quotes such a word (`"-RAGlb=X:/a"`), puts `--%` before the `--`, or calls from Git Bash, which leaves it whole (GATE-527).
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
  - A name with no directory whose extension is one of PATHEXT's (compared whole, its dot included, ignoring case) is looked for as written in each absolute PATH folder.
  - A name with no directory whose extension is not one of PATHEXT's (`Godot_v4.7.2-stable_mono_win64_console`, a routed program name) is looked for in each absolute PATH folder as written, then with `.exe` appended; no other extension is ever appended, so `foo.1` never finds `foo.1.cmd`.
  - A bare name is looked for in PATH × PATHEXT, the folders outer and the extensions inner, as `Get-Command` orders them.
  - The current folder and relative PATH entries are never searched, and an extensionless shim is never found.

  The program's path is then made a full path (`.` and `..` folded) before the batch check, and that path is the application.

  A name found nowhere is a start failure. Unlike the PowerShell gate, which lets CreateProcess search for the program (the current folder among others), the gate never passes an unfound name on. This is deliberate, for security: a repo cannot plant a `git.exe` at its root. The `gate-win32` layer passes the path as `lpApplicationName`, so CreateProcessW searches no folder (not the gate's, the current one, System32 or PATH).

  A start failure prints `gate: could not start <word> (<resolved>): <message>` and exits 127, releasing the slot (C7); the lines are in [Output lines](#output-lines).

  A program the gate's own tool probes and SDK read find along `PATH` (`program_output` in `remote/git.rs`) and judge against the worktree is followed through links, and the file the link leads to is the image started.

  Its command line's first word stays the name it was found by, not the target's, because a multi-call program picks what to be from argv[0] alone: rustup's `rustc.exe` and `cargo.exe` are links to `rustup.exe`, and started by the target's name they would answer as rustup. A batch target still runs through the system folder's cmd.exe, given the target.

  The worktree refusal covers only the file launched: a program it starts in turn (a multi-call proxy, a shim) is not judged.
- **Process creation.** The `gate-win32` layer is the contract the run uses (`Launch` → `Suspended` → `Running`). The command is created suspended with an explicit environment block, assigned to the gate's job `Local\gate-job-<gate pid>`, then resumed (kept, X1, P1). Creation flags are `CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT` plus the priority class, with no new console and no new process group.

  The job has no limits: no breakaway, no kill-on-close. (This is `gate run`; the box's command is created with `CREATE_BREAKAWAY_FROM_JOB` and a restricted non-elevated token, [The box side](#the-box-side).) (A `test-seams` build's `--test-job-breakaway` makes the job allow breakaway, `JOB_OBJECT_LIMIT_BREAKAWAY_OK`, still without kill-on-close: `Job::create_breakaway`.)

  It holds the command and what the command's descendants start, except a packaged desktop app started by a non-packaged descendant, which runs in a job of its own with what it starts; a kill ends those too ([Watchdog](#watchdog)). A job name already held is refused. If assigning to the job or resuming fails, the command is terminated with exit code 1 first, and a suspended command dropped without being resumed is terminated with exit code 1.

  The desktop-app policy `PROCESS_CREATION_DESKTOP_APP_BREAKAWAY_DISABLE_PROCESS_TREE` is set, as the PowerShell gate sets it. Per Microsoft's UpdateProcThreadAttribute remarks it matters only to Win32 apps packaged through the Desktop Bridge, whose children it keeps inside the desktop app runtime environment.

  So a packaged app the gate starts itself keeps its tree in the job: the command.

  `PipedLaunch` creates its command with the policy too and, on a system that cannot set it (before Windows 10 1703), without it, saying so in `Suspended::note`.

  A packaged desktop app started by any non-packaged descendant (the Store's pwsh or `winget` under a `cmd`, a git hook's pwsh) runs in a job of its own, outside the gate's, and no documented control keeps it in: only a direct child of the gate, assigned explicitly, with the policy, stays in. `IsProcessInJob(h, NULL)` and the job's `TotalProcesses` tell it in one run; `winget --version` under `cmd` is the packaged reproducer on Windows 11 and Server 2025.

  The layer refuses a `.cmd` or `.bat` application (any case, trailing dots and spaces ignored): the caller wraps batch targets in `"<system folder>\cmd.exe" /d /v:off /s /c "<line>"` itself. It refuses a NUL in the application, command line or working folder before anything is opened.

  A local run's command writes to no file: `LaunchOutput::Pipes` makes two new pipes of a 1 MiB buffer, one for its stdout and one for its stderr, whose read ends the gate keeps and reads ([Log and tail](#log-and-tail)). A reader that falls behind does not block the command at once on a full pipe. stdin is `NUL`, opened `OPEN_EXISTING`.

  The gate opens the log itself, to append, and writes every line of it. A process the command left running holds no handle to the log, so it does not block the next run on that log; a second gate on a live log is kept out by the log's mutex ([Kernel object names](#kernel-object-names)).

  It inherits exactly three handles, through `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`: stdin from `NUL`, stdout and stderr each to the command's end of its pipe. The gate's own handles to them are not inheritable: the command inherits copies made under the spawn lock, which exist only while the lock is held, and a failed copy is a setup error, `copy a handle for the command to inherit: <message>`.

  The spawn lock is process-wide and is held while the gate starts any process, so only the launched command inherits the copies. A process the gate starts itself goes through `gate_win32::spawn`, which holds the lock for the start alone; one started any other way could inherit the copies if it started while they are open.

  The command shares the gate's console, so Ctrl+C reaches it.

  A nested gate is told from the slot marker in its environment, honoured only when the marker binds to the parent's job ([Critical drain and nesting](#critical-drain-and-nesting)), not from its own job alone: its job nests inside its parent's when the nested gate is in the parent's job, but under a `cmd` and then a packaged pwsh, or a git hook's pwsh, it is in no job of the parent's, and the kill reaches it as one of the processes that left the job.

  The start-failure message is the system message for the CreateProcessW error code, as `Win32Exception.Message` gives it, with no `(os error N)` suffix. That failure alone maps to status 127; every other start failure is the gate's own.
- **Priority.** BelowNormal, or the gate's own class when that is already BelowNormal or Idle; High for a critical gate and any gate under a critical marker (kept, P2).
- **Environment.**

  Every command gets the gate's environment with these set over it (names compared case-insensitively): `GATE_SLOT_HELD=<kind>:<pid>:<start_ms>[:<h>]` (the held slot's marker, `none` too, or the parent's for a covered gate; a gate lent its light parent's slot holds none and hands `heavy:<its pid>:<its start>[:<h>]`, naming itself), `SharedCompilationId` and `MSBUILDNODEHANDSHAKESALT`, both the run id, `MSBUILDUSESERVER=0` (over an inherited value), `DOTNET_CLI_USE_MSBUILD_SERVER=0` (kept, P3), `MSBUILDDISABLENODEREUSE=1` (over an inherited value), `UseSharedCompilation=false` (over an inherited value), and `GATE_STATE_DIR=<root>` under a non-default root.

  The caller's `--env` pairs ([Command environment](#command-environment)) are set over the gate's environment first, so the settings above come last and no pair can undo one.

  A commit run ([Commit run](#critical-drain-and-nesting)) also gives its command `GATE_COMMIT_SLOT=<its marker>`, the same text as its `GATE_SLOT_HELD`; any other gate removes `GATE_COMMIT_SLOT` from its command's environment.

  `GATE_BOX_JOB` and `GATE_BOX_EXE`, given to a box run's command ([The box side](#the-box-side)), pass through a trusted in-box gate to its command, which gets no `GATE_SLOT_HELD`; any other gate removes `GATE_BOX_JOB` from its command's environment.

  The vendor-source quotes for these settings, the cold-start cost of a per-run server and the priority a server runs at are in [research/2026-10-25-per-run-build-servers.md](research/2026-10-25-per-run-build-servers.md).

  `UseSharedCompilation=false` keeps Roslyn's compiler server from idling in the command's job for up to ten minutes after a build: a command that waits for its whole job, as Godot's console wrapper does, never ended while the server held it, and the stall kill fired. The same setting is on a `gate box run` command and a routed one.

  Under the default root an inherited `GATE_STATE_DIR` is removed. The gate's own environment is never changed.

  The run id is `gate-{pid}-{start ms}`, with `-{root hash}` added under a non-default state root. Each nested gate has its own id. So each run gets its own .NET compiler server, and MSBuild's node reuse is off: with it on, the nodes idle about 15 minutes inside the command's job, so anything in the command waiting on its whole tree would hang. The gate ends what the run left at its [end](#watchdog).

  The batch refusal's `%NAME%` check answers for this exact block.

  The block is built with names matched and sorted as Windows does (`CompareStringOrdinal`, case-insensitive over all of Unicode), and a later pair wins.
- **Working directory.** The caller's (P5), or the `--cd` folder when given, which a relative `--log` is read against too ([Run folder](#run-folder)).

## Log and tail

- Every line of the log is `<stamp> <tag> <text>`: the stamp is the local time to the microsecond with its numeric UTC offset (`2026-10-22T09:31:38.123456-07:00`), the tag says whose line it is, and the text follows one space (a line with no text ends at the tag). The tag is `out` for a line of the command's stdout, `err` for one of its stderr, and `gat` for everything the gate writes, its `gate: ` notes keeping that text after the tag.

  So `rg ' (out|err) '` keeps the command's lines and `rg -v ' gat '` likewise. The format is `crates/gate-wire/src/stamp.rs`'s.

  The log's directory is created and an earlier log, and the `<log>.err` the PowerShell gate wrote beside it, are removed at start. The gate opens the log to append and writes it through one lock, so no line lands inside another. The command writes to two pipes the gate reads, one reader each; stderr is therefore interleaved with stdout where it happened, and a line is stamped as it is written.

  CR LF, LF and a lone CR each end a line, the bytes read as UTF-8 with U+FFFD for what is not, and a line with no end is cut every 64 KiB into lines of its own. The stamp is read under the log's lock, just before the write, so the stamps of the lines written live never decrease down the log.
- The gate's own lines are the run's notes, each stamped when it is said and written to the log live. A note said before the log is open is held, then written, with its own stamp, behind the log's first line. A hard-killed gate's lines are only on stderr.

  The verdict lines are written after the tail is printed, so the log ends with them and the printed tail does not hold them. Notes stand in the order said among the command's lines: the expected-time line comes before the command's first line.
- Process lines are `gat` lines about the command's job. The first line of the log is the root line, the gate itself: `gate pid=<pid> ppid=<ppid>`. Each process that joins the job has `spawn pid=<pid> ppid=<ppid> <image>` (`?` for a parent or an image that could not be read; an image of more than 260 characters is cut and ends `…`).

  When a process ends it has `exit pid=<pid> code=<code> ran=<s>.<ffffff>s`; a signal (a Unix box's) reads `signal=<n>` for the code, an end that could not be read `code=?`, and a Windows status of 0x80000000 or more is written in decimal followed by its hex (`code=3221225477 (0xC0000005)`).

  A process that joined but could not be read is `spawn pid=<pid> missed (<why>)`. One still in the job when the watch ends is given 2 s to end, then gets `left pid=<pid> (still running at the end of the watch)`. `process lines stopped: <why>` says the job's lines ended early, so a process still running then has no exit or left line. The `ppid`s let a reader rebuild the process tree: the command's own process has the root line's pid as its parent.

  A spawn line carries the process's kernel create time as its stamp and an exit line its kernel exit time, so they can sit a few milliseconds out of order with the lines about them; the stamp is the truth. A process line is not the command's output: it never resets the stall, and the console tail leaves it out.
- Lines end at CR LF, LF or a lone CR, as .NET's `ReadLine` ends them, for the marker scan and the tail. Both decode the log as UTF-8 lossily: an invalid byte reads as U+FFFD. The scan, the tail and the last-line read behind the verdict read the text after the tag, a line with no stamp whole, so a pattern anchored with `^` anchors at the text.

  Marker line numbers are numbers of the raw log's lines, process lines included. The gate writes the tail to stdout as UTF-8 (through the wide console API when stdout is a console), as text with no stamp or tag.
- The failure-marker regex is built in (kept, V2): `^Build FAILED\.|error CS\d+|: error |Test run summary: Failed!|No test projects were found|^\s*failed: [1-9]|gate: (TIMED OUT|STALLED|BACKSTOP|TRANSFER TIMED OUT|TRANSFER FAILED)`.

  `No test projects were found` makes a `dotnet test` that found nothing to run (a Windows-only test project on a macOS builder) a failure even when the command exits 0; a Microsoft.Testing.Platform run of zero tests needs no marker, since it exits 8 unless the repo asks otherwise.

  It is matched case-insensitively, per line. A zero exit whose log matches becomes status 1 with `gate: command exited 0 but its log reports a failure -> <log>`; on any non-zero status up to 40 matching lines are printed to stdout as `<line number>:<line>`; a killed run is not scanned. `--no-markers` turns the scan off. The scan skips a line whose text starts `gate: bringing the results back from `, the retry line of a routed transfer, whatever the failure's own text holds. After a `gat` line that starts a macOS box's cross build (`gate: cross build for <rid>: …`, [The Unix box](#the-unix-box)) it skips every `out` and `err` line, and still scans `gat` lines.
- The scan reads the log streaming and never whole: at most 64 KiB of any one line, and it stops after 40 matches. The tail is read backwards from the end of the log, at most 1 MiB of it; when the last line is longer than that window, the tail shows `… ` followed by its last 4096 characters, starting on a whole character. Each printed tail or marker line is cut at 4096 characters, ending ` …`.
- At the end the readers of the command's pipes get 2 s to reach the end of them (`gate: the command's output readers did not finish within 2 s; their last lines may be missing` when one does not), the job's last process lines are written, and the log is sealed: no later line of the command and no process line reaches it, and the gate's own lines still do.
- Then the last `--tail` lines go to stdout, taken from the log without the process lines and without the lines of a kill's diagnostics block (dropped by position, so the command's own lines are never lost), then the verdict line ([Output lines](#output-lines)) (kept, V3). A routed run prints no tail: the box's output already reached the console live as it arrived ([Routing](#routing), the run call), so the verdict follows it directly; its failure markers are still printed.
- A panic after the slot is taken terminates the job, ends the readers and process lines as at the end of any run, appends the gate's held notes to the log (when completing the log fails, the notes are kept for the next write), deletes the claim and journals `release` with `status` null and `verdict` unknown.

## Routing

Routing is off on a machine with no `remote.json` in `%LOCALAPPDATA%\gate\`: nothing is read and nothing is written.

- **What routes** is kept (T2): a heavy gate, not `--local`, under no covering or lending parent (a commit run's slot is not one for this rule, [Commit run](#critical-drain-and-nesting)), outside a git hook (`GIT_INDEX_FILE` set to a non-empty value) unless `remote.json`'s `hooks` names the repo, whose first word is not `git` or `prek`, in a git worktree of a repo `remote.json` lists, whose words with in-worktree paths rewritten (T3) match one of that repo's patterns.

  A light gate the critical barrier holds back also routes (**A light gate held back**, below).

  **Which builder.** The run goes to the first routed `builders` entry, in file order, whose own `repos.<repo>` patterns match the Windows-form rewritten words, else to `host` when the top-level patterns match. A warm-up always goes to `host`. A command matching a `.gate/desktop.toml` pattern is never picked by a `macos` entry, nor is any command of a change that touches a `.gate/windows.toml` path or cannot be told to.

  A `sendBuild` repo's `--no-build` test picked by a `macos` entry drops `--no-build` and sends no build, unless its last build ran here (the built-here record, [Glossary](GLOSSARY.md)): a dotnet `test` or `run --no-build` of that kind is kept off the Mac as a Windows-only test is, and goes to the next Windows builder with a send, else runs here. A run pinned with `--builder` never asks the record. The words sent to a `macos` builder use `/` inside the recognised path spans. A repo listed only in a `builders` entry is routed ([`gate check`](#gate-check) treats it so).

  **Pinned run.** `gate run --builder <host>` routes a heavy run to the one builder `<host>` names, which must be `host`, a `hosts` name or a `builders` alias (routed or not), matched ignoring ASCII case as `gate check --builder` matches.

  The pin needs the repo's consent and a command matching one of its patterns: the pinned builder's own listing when it names the repo (its `builders` entry's `repos`, routed or not; the top-level `repos` for `host`), else the first match across the repo's routes, any builder; a `hosts` entry has no listing of its own.

  It replaces the builder pick and the next-builder list, and a pinned run never takes a slot a light parent lends, so it routes whatever a critical request is doing. The flag is never passed to nested gates.

  A pin the decision refuses is a usage error (exit 2) before any slot or journal line, and before the log is readied, so an earlier file at `--log` is left as it was and the decision's notes go to stderr.

  It says `gate: --builder <host> cannot route this run: --local keeps it here`, `... only a heavy run routes, and this one is --slot <light|none|critical>`, `gate: --builder needs a usable remote.json: there is none` (or `<path>: <reason>`, or the `hosts` problem), `gate: --builder <host> is not a builder remote.json lists`, or `gate: --builder <host>: this command is not one the repo routes (none of its patterns matches the command)` (or `(neither <host> nor a routed builder lists repo <repo>)`).

  Any other keep-here reason says `gate: --builder <host> cannot route this run: <why>`, with `<why>` one of `a parent gate's slot covers this gate`, `git, prek and an empty command never route`, `a gate started in a git hook routes only for a repo remote.json's hooks names`, `the gate's folder is in no git worktree`, `it asks for a desktop, which a macOS builder does not have`, `a word still holds a \, which a macOS builder reads another way`, `it may test a Windows-only project, which a macOS builder cannot run`, `the change touches <path> (<file>)`, `cannot tell whether the change touches <file>'s paths: <why>`, `a word of the command is not valid Unicode`, or the text of another kept-here reason.

  A pinned run its builder refuses (any reason), cannot reach, whose client cannot start, or whose work-folder key another run holds, ends with exit 1 and `gate: remote <host> <reason>; --builder given, so not running here` (or `gate: remote <host> client could not start: <detail>; --builder given, so not running here`).

  No other builder is tried, nothing runs here and no `fallback` event is journaled: the journal holds `queue`, `take` and a `release` with status 1, and no `take` when the run ended before a slot.

  **A light gate held back.** A local light gate whose wait the critical barrier holds back with no place beside the request ([Critical drain and nesting](#critical-drain-and-nesting), "The barrier"), a plain request whose beside allowance is used up or an `--alone` one, asks the routing decision once more, with the heavy slot.

  It does so only when it is not lent, not covered, not pinned with `--builder`, and asked the decision at its start. When its command would route, it says `gate: held back by the critical request of gate <pid>; sending this light run to <alias>`, then the route lines, and runs on the builder; it never prints its local waiting line.

  A command that would not route waits on as before and says nothing new, as do `--local`, `--builder`, covered, lent, in-box and commit gates.

  Such a run takes a builder remote slot only when one is free at once and another slot of that pool stays free after it ([Slots](#taking-a-slot)).

  It never waits for a remote slot or for the work-folder key, a held key being `busy`. A fallback for any reason (`full`, `busy`, a refusal, `client`) sends it back to its local wait with its first arrival and first queue instant kept; its log holds the announce and route lines, then the fallback line. It asks to route once per run, and the next-builder walk below asks later builders as heavy.

  Its remote journal lines (`take`, `release`, `abandon`, `fallback`) carry `routedFor` and `criticalPid` ([Journal](#journal)); its `queue` line is the local, untagged one.

  **The next builder.** A builder that sends a routed run back for `full`, `busy` (its own refusal), `unreachable`, `version`, `parity`, `install` or `tools` is followed by the next capable builder before the run goes to the PC. The candidates are, held by alias, each `builders` entry listing the repo whose own patterns match the command, in `remote.json` file order, then `host` when its patterns match, minus the pick.

  Each is tried at most once, and a `sent` rerun stays inside its builder's attempt.

  Every other reason (`client`, `inputs`, `lfs`, `layout`, `desktop`, `rebooted`, `root`) runs here at once, as does a work-folder key held by another run of the worktree (`busy` before any remote slot: the key is the worktree's on every builder). The key lock is taken once and held across the attempts; each attempt takes a remote slot of its builder's pool, after the refusing builder's slot is let go.

  A later candidate is built when it is tried, through the same pick pipeline as the first pick, a fresh `Machine` per candidate, and `remote.json` as it reads then. The macOS, backslash, Windows-only-test and Windows-paths steer-offs skip it silently.

  A skip for any other reason (the file no longer lists the builder for the repo or no longer matches the command, or the decision would now keep the run here) goes in the log only, as `gate: not trying <alias>: <reason>`, `<reason>` the text `LocalReason::reason` gives after the line's own prefix and without `; running locally`, such as `ignored <path>: repos is missing`, and the walk goes on to the next candidate.

  The route-time values (the warm-up consent hash, the declared env prefixes, the route lines said once) stay the first pick's. Each candidate's own route lines (the dropped `--no-build`, no outputs, siblings) are said when it is tried, after the line naming it ([Output lines](#output-lines)).

  **Words sent to a `macos` builder.** Beyond the recognised spans, a relative word written with `\` (`src\Duel.sln`, `-p:Props=src\a.props`, an MSBuild `/p:Name=value` switch's value) is sent with `/` when it names something the snapshot carries: a file `git --literal-pathspecs ls-files -z --cached --others --exclude-standard` lists (limited to the gate's folder when it is below the root), or a folder holding one, compared in its exact letter case.

  A glob is judged by the folder before its first wildcard, and a leading `./` and a trailing `/` are dropped. Every candidate goes in one git question, asked only when a word is a candidate; a git that cannot answer carries nothing and the log says `gate: cannot tell what the snapshot carries: <why>`.

  A word that is still written with `\` after that keeps the command off the Mac, silently: it goes to the next builder that is no `macos` one, else `host`, else runs here.

  Those words are a path the snapshot does not carry, a value starting with `\` or a drive, a value with a `..` segment, a regex, a list or a response file (`@`), and script text: the body after `bash` or `sh` `-c`, or after a PowerShell host's `-c` or `-Command` (the scan of a PowerShell host's switches stops at `-File`, `-f` or `--`).

  A word holding a path end (`,` `;` quote `)` `|` `&` `<` `>`) or a line break is script text or a list. A `windows` pick keeps its words as written and asks no snapshot question.

  **Windows-only tests.** A `macos` pick of a `dotnet test` (the verb in any letter case) goes to the next builder that is no `macos` one, else `host`, else runs here, with nothing new on stderr, when a test project the run covers is Windows-only or its projects cannot be read; a later `macos` candidate of such a run is skipped silently.

  `--builder` naming a `macos` builder for such a run is refused: `gate: --builder <host> cannot route this run: it may test a Windows-only project, which a macOS builder cannot run`.

  A project is Windows-only when every `TargetFramework` and `TargetFrameworks` value it sets itself contains `-windows`, or an `IsTestProject` or `IsTestingPlatformApplication` element carries the Condition `!$([MSBuild]::IsOSPlatform('Windows'))` with the value `false`.

  A solution (`.sln`, `.slnx`, `.slnf`) counts when any member that is a test project is Windows-only; a test project is one whose file names `Microsoft.NET.Test.Sdk`, `xunit`, `MSTest`, `NUnit`, `TUnit`, `IsTestProject` or `IsTestingPlatformApplication` as a whole word. Members resolve against the solution's folder and must stay inside the worktree.

  What is judged is the project or solution the command names, else the first word after the verb that is a project file, a solution file or a folder holding one, under the gate's folder, else the gate's folder (its one project file, else its one solution file); any other word is taken as an option's value.

  What cannot be read counts as Windows-only: none or several files, a link, a file over 1 MiB, a target framework set from a property, a member outside the worktree, a solution that does not parse. For these the log says `gate: kept off the macOS builder: <reason>`. A command whose words name a response file (`@`) cannot be read either and leaves the `macos` builder without a note.

  **Windows paths.** A `macos` pick, whatever the command, of a repo whose `.gate/windows.toml` declares at least one path goes the same way (the next builder that is no `macos` one, else `host`, else here) when the change touches a declared path or cannot be told to; a later `macos` candidate is skipped silently. Git runs only for such a pick, never for another builder or a repo with no file or no entry.

  The log says `gate: kept off the macOS builder: the change touches <path> (<file>)`, the first changed path a declared path matches, or `gate: kept off the macOS builder: cannot tell whether the change touches <file>'s paths: <why>`, both escaped, `<file>` being `.gate/windows.toml` or the path of the declarations-folder copy that was read.

  `--builder` naming a `macos` builder for such a run is refused on stderr, exit 2, with `gate: --builder <host> cannot route this run: ` and the same words as the note.

  The change of a repo is what differs from the merge-base of its `HEAD` with `refs/remotes/origin/HEAD`, else `refs/remotes/origin/main` when that one names no commit (a dangling `origin/HEAD`); each is asked with `git rev-parse --verify --quiet <ref>^{commit}`, so a symbolic `origin/HEAD` is followed and a plain ref naming a commit counts. Exit 1 moves on to the next ref, any other status is git's failure line.

  It is the tracked paths `git diff --name-only --no-renames -z` lists against that merge-base, committed, staged or not, a rename counted under both names, then, only when none of them is declared, the untracked ones `git ls-files --others --exclude-standard -z` lists. Every git call of the rule runs as `git --no-optional-locks ...`, so it never takes `.git/index.lock`.

  The worktree's own change is read only when some entry is not a `../<sibling>/` one, then each sibling an entry names, in declared order, only while no earlier repo touched a path.

  All its git calls share one deadline of 10 s, each call limited to what is left of it and to the decision's own git limit; the heavy-route hint first checks that the command's words are valid Unicode, then reads the change with one value for its deadline and per-call limit, 5 s or the test seam's limit when that is shorter, and an unknown change there gives no hint. A call ended at the time left of the deadline says `its git calls did not answer within <deadline>`.

  Before any git runs, every declared sibling (`checked_siblings` over the opened siblings, not only those an entry names) is put through the checks a routed run's siblings pass (a link or junction, a missing folder or `.git`, one `remote.json` does not allow, the worktree's own folder, a duplicate).

  A failure steers nothing and reads no change: the run stays here with the sibling's own reason, `gate: not routed: sibling <name> <reason>`, and `--builder` naming a `macos` builder is refused with `gate: --builder <host> cannot route this run: sibling <name> <reason>`.

  Root entries match only the worktree's own paths, and a sibling's paths only the entries starting `../<name>/`, so `*.props` never matches `../yaat-server/Directory.Build.props`.

  The `<why>` is one of `HEAD names no commit in <root>` (an unborn branch only), `<root> is a shallow clone` or `HEAD is detached in <root>`; a `HEAD` git cannot read otherwise gives git's failure line.

  It is also one of `<root> has no refs/remotes/origin/HEAD or refs/remotes/origin/main`, `HEAD and <upstream> have no merge-base in <root>` (`<upstream>` the ref used; `git merge-base` exit 1 only), git's own failure line `git <args> failed in <root> (status <n>): <last lines>` for any other failure.

  Or it is one of `git <args> failed in <root>: its output passed 16 MiB` (a listing past the cap ends git at once), `git listed a path that is not valid UTF-8 in <root>: <path>`, `the output of git <name> in <root> is not valid UTF-8`, `its git calls did not answer within <deadline>`, or the text of the file's problem.

  A paths entry that names a sibling not in `.gate/siblings.toml` is a file problem, so that change counts as unknown. Format: [remote.json](#remotejson) (the `.gate` readers).

  A matched command still runs here, saying why, when a word names a path outside the worktree (a drive-relative, root-relative, share, device or climbing form included, see [the path rewrite](#routing)) the gate's folder holds nothing the snapshot sends, or a word names a gitignored project input the snapshot leaves out ([Routing](#routing), the decision).

  The one exception is a first word naming a program by an absolute path, which the path rewrite sends as the program's name unless it refuses the name.

  `--no-build` is dropped from the builder's words of a dotnet `test` or `run` command, saying so (T4); `build`, `msbuild`, `pack` and `publish` keep it everywhere. A fallback here decides `--no-build` again for the run here, as a Local route does (below), whatever the refusal: the word stays when the built-here record says the last build of what the command builds ran here, or the worktree has no nothing-back record and every project the command builds has `obj\project.assets.json`, so the run tests the PC's own build, else it drops.

  The record is what an earlier routed step (a repo's test-all build) leaves when it built only on the builder, where a `dotnet test --no-build` here would find no build output.

  The route-time dropped line of a non-send run is true of the builder run it planned, so it can precede a fallback that keeps the word.

  A step that really did build here pays a second local build on a refusal, a cost accepted. A fallback that drops no word reuses the first plan.

  A routed `dotnet test` of a repo `remote.json`'s `sendBuild` lists keeps its `--no-build` on the builder by sending the PC's build first, when its own words name one project inside the worktree (a project file, or `--project`), set `-c` or `--configuration` to one plain name, and name no output option (`-o`, `--output`, `--artifacts-path`).

  Nor may they set an output property (`OutputPath`, `BaseOutputPath`, `UseArtifactsOutput`, `OutDir`, `Platform`, as `-p:`, `/p:`, `-property:` or `/property:` in any letter case). A run the listed rule sends nothing of keeps the drop, unless the built-here record sends it (below).

  Nor may the project's build files name one of those properties as text: the project file named (each project file of the folder `--project` names), and every `Directory.Build.props` and `Directory.Build.targets` from the project's folder up to the worktree root.

  Such a file that cannot be read counts as naming one, and the log says `gate: not sending the PC's build: cannot read <path>: <error>`. Such a route carries both word sets, and says no dropped line at route time. A `dotnet run --no-build` and every other `--no-build` run the listed rule does not send keep the drop, unless the built-here record sends it (below).

  **The send from the built-here record.** A routed dotnet `test` or `run --no-build`, of a repo `sendBuild` lists or not, whose last build ran here sends the PC's build in the same way, the listed rule asked first. Its last build ran here when the worktree's built-here record ([State root](#files)) has an entry of each project the words build, under the `-c`, `-f` and `-r` the words set (letter case ignored, an absent word equal to an absent one).

  What the words build: a project they name is that project; a solution they name is each project it lists that lands inside the worktree, a `..` entry inside the worktree counting, which the send cannot take, so a run that matches stays here (below); naming nothing, the gate's folder's project when it holds exactly one project file, else, for `build` and `test` only, the projects of its one `.sln` or `.slnx` when it holds no project file.

  The record is asked at most once per pick, and never of a run pinned with `--builder`.

  The send takes a `test` or a `run`, with one plain `-c` name or none (which builds `Debug` and sends `bin/Debug`), the same output option and output property bans, and the same check of the build files, which with no `-c` also count a `Configuration` property they set as moving the output. A `dotnet run` that names no project or `.cs` file, and a `dotnet test` that names no project, solution or folder, send the one project file of the gate's folder; more or fewer take the run here.

  A repo `sendBuild` lists whose listed rule sends nothing reads the record just as an unlisted repo does, including when the pick is a `macos` entry, which the record steers off (a Mac cannot take the PC's Windows build): the next Windows builder with a send, else here. A send that does not land says `gate: dropped --no-build: the PC's build could not be sent` in place of the usual dropped line.

  When the last build ran here and the send cannot be made (words or build files the send cannot take, a solution, or no Windows builder left after the steer-off), the run stays here keeping its word, saying `gate: not routed: the last build of <project> ran here; --no-build kept`, `<project>` the project's key or the solution as named.

  The Rust client sends the build after the inputs and the LFS content: the files under `<project>/bin/<configuration>/` and the project's two restore files, `<project>/obj/*.nuget.g.props` and `*.nuget.g.targets`, through the `inputs` verb's diff and put with `SentBin` ([The box side](#the-box-side)).

  It has its own nonce, its own `.tmp` files (`<key>-<nonce>.sent-build-diff` and `<key>-<nonce>.sent-build.tar.zst`), its own PC hash cache `remote\<key>.sent-build-hashes`, and its own part of the passed line's split, `sent build <n> s`. A put says `gate: sent build: <n> file(s), <x> MB (<y> MB compressed) in <s> s`; a diff that needs nothing says `gate: sent build: <n> file(s) unchanged` in the log.

  A send that lands runs the kept words with the run payload's `SentDigest`, and writes neither the outputs-pending marker nor the nothing-back record.

  No send is made while the last build's outputs are not back: when the worktree's `remote\<key>.nothing-back` record stands (one that cannot be read counts as standing) and names no configuration or the run's own `-c`, in any letter case, the PC's `bin` may be older than the builder's build, so the run takes the drop, its log saying `gate: remote build not sent: the last build's outputs are not back: <record> stands`. A record of another configuration does not stop the send.

  A send planned from the built-here record is exempt from that veto only while its entry still matches: the client reads the record again under the key lock, for the project key the route matched (`SendPlan`'s `project_key`, which is empty for a send planned because `sendBuild` lists the repo) and the settings of the send plan's words. Every routed build clears the record, so a standing entry says a build here came after the last routed one; an entry gone by then leaves the veto to apply.

  A send that does not land takes the drop: a restore file missing on the PC, no file under the bin folder, a manifest past a cap, a refusal, a failure or a lost connection says `gate: remote build not sent: <why>` in the log, then the dropped line, and the builder runs the dropped words; for a send planned from the built-here record the dropped line is `gate: dropped --no-build: the PC's build could not be sent`. Before the dropped line the built-here record is removed, under the key lock the run holds, since the builder builds (a nothing-back veto included). It never falls back here; a Ctrl+C ends the run, and the box's `install` refusal falls back `install` as an inputs call's does.

  A run call the box refuses with the `sent` tag, its work folder no longer holding the build sent, is made once more under a new nonce with the dropped words and no `SentDigest`, after the dropped line, which clears the built-here record as above; it never runs here.

  A run with a send that falls back here for any other reason (`busy`, `full`, `tools`, `layout`, `parity`, `inputs`, the send's `install`, a client that cannot start) decides `--no-build` again for the run here, as every fallback does, and says the local dropped line, after the fallback line, only when it drops the word. The builder's dropped line from the send is not repeated by the fallback.

  A dotnet command is one whose first word's file name without extension is `dotnet`, or is `pwsh` or `powershell` with `--` followed by a `dotnet` word (a script handed a dotnet command to run), each compared in any letter case. A dotnet command's own words run from its `dotnet` word to its next `--`, and only they are read: a `--no-build` after that `--`, or before a wrapping script's `--`, belongs to another program and stays, so plain `dotnet test -- --no-build` keeps the word.

  A dotnet command with a word starting with `@` (a response file) is opaque, since the gate cannot see its words: it drops no word, gets no `-p:SourceRevisionId` or `-p:PathMap` word, writes no nothing-back record and clears none.

  A run that stays local (the Local route) drops the same `--no-build` words before it takes a slot, when it would find no build here, saying `gate: dropped --no-build: <reason>; building on a heavy slot`, `gate: dropped --no-build: <reason>; building on its critical slot` for a critical run, or `gate: dropped --no-build: <reason>; building on the parent gate's <heavy|critical|light> slot` under a covering or lending parent.

  A routed run is never dropped here, but its fallback here is, by the same rule, before its plan is made. `<reason>` is one of two:
  - `the last build ran on <host> and brought nothing back`, when the worktree's nothing-back record stands (`<host>` is the record's builder, else `the builder`) and does not list the project the command names as built here (below).
  - `<project> has no obj\project.assets.json here`, asked only when no such record stands.

  Neither is asked when the built-here record says the last build of what the command builds ran here under its own settings (below): a run here keeps its word then, whatever the nothing-back record holds, and the obj check is not made.

  A dropped run is a build, so its slot only rises: a light run takes a heavy slot, a heavy one stays heavy, and a critical one stays critical with its own request and nesting. Its queue, take and release journal lines, its ticket and its claim show the kind it now runs as (`heavy` for a light or heavy run) and the command without `--no-build`.

  A light child of a light parent is no longer covered after the drop, and a heavy run the drop makes uncovered asks its light parent again whether it lends its slot. The covered, ignored-marker and could-not-judge lines are said after the drop is decided and name the final kind.

  An `--alone` run measures the command as given, so one whose `--no-build` would be dropped is refused instead, exit 2, before it takes a slot, with nothing journaled; its log is readied only after the drop is decided, so an earlier `<log>` and `<log>.err` stay untouched: `gate: refused: --no-build cannot stand: <reason>; an --alone run measures the command as given, so build first (a passing local build of the project clears the record) and run it again`.

  The obj check covers the project the command names (a project file, or `--project`), else the projects a named `.sln`, `.slnx` or `.slnf` lists, else the worktree's tracked `*.csproj`, `*.fsproj` and `*.vbproj` (`git ls-files`). A run of a project the record lists as built here keeps its word only when its own `-c`, `-f` and `-r` match the record's (otherwise it drops as usual), and the obj check still applies: the word is kept only while the project has its `obj\project.assets.json`.

  The check is skipped, and the word kept, when the worktree root's `Directory.Build.props` or `Directory.Build.targets` names `UseArtifactsOutput`, `ArtifactsPath`, `BaseIntermediateOutputPath` or `MSBuildProjectExtensionsPath`. Solution entries that are not plain names inside the worktree are skipped.

  Inside is judged by canonical path, both sides resolved: another spelling of the worktree (a `subst` drive, a junction) counts as inside, and a project folder inside the worktree that is a junction leading outside counts as outside.

  Props and solution files are read only as regular files up to 1 MiB; otherwise the gate says `gate: cannot read <path>: it is a link, a junction or not a file` or `gate: cannot read <path>: it is larger than 1 MiB`, and the word is kept. Printed paths escape control characters. When git fails the gate says `gate: cannot find the command's git worktree: <error>` or `gate: cannot list the worktree's projects: <error>` and keeps the word.

  The git calls of the drop and the clear run with `LC_ALL=C`. The clear prints nothing when git is missing or the folder is outside a repository.

  The nothing-back record ([State root](#files), [Glossary](GLOSSARY.md)) is written by every routed dotnet build (verb `build`, `test`, `publish`, `pack`, `run` or `msbuild`, with no `--no-build` of its own) that ends on the builder, a failed, killed or interrupted run included, unless the run's outcome says its outputs were all brought back.
  The Rust remote client says `yes` only when the outputs archive installed complete (skipping at most files it cannot rewrite), or when the box packed nothing for a PC whose sent manifest held files, and no name was skipped (the box's `gate: remote outputs: skipped ` line); every other end of the outputs step is `no`, and an end that never reaches the outputs step is `unknown`. No and unknown count alike: the record is written.

  A routed dotnet build that passed (status 0) whose outputs list (the repo's own, with the derived bin) was all brought back, not every file the build wrote, removes the record instead, as a passing build here would: only when its words are whole (they name a solution, or nothing at the worktree root), its verb is not `msbuild`, and its `-c`, `-f` and `-r` match the record's; otherwise the record is left as it was, not rewritten, and it adds no key to `built`.

  A record that cannot be removed says `gate: cannot remove <path>: <error>`, as the clear here does.
  A routed build that leaves nothing back writes a fresh record, replacing one that stands, so its `built` is empty.

  A passing local dotnet `build`, `test` or `run` (no `--no-build`; never `msbuild`, `pack` or `publish`) of one project inside the worktree, with no output option (`-o`, `--output`, `--artifacts-path`), no runtime-part option (`-a`, `--arch`, `--os`, `--ucr`), no target switch (`-t:`, `/t:`, `--target:`, or with `=`) and no `-p:`, `/p:` or `--property:` setting of `OutputPath`, `BaseOutputPath`, `UseArtifactsOutput`, `OutDir`, `Platform`, `Configuration`, `TargetFramework` or `RuntimeIdentifier` (a build that moves its output, or may not build, adds no key), adds that project's key to the record's `built` list, when the record's `-c`, `-f` and `-r` are the run's: the key is the project's folder (a project file's folder, the `--project` folder, or a positional folder argument that exists inside the worktree, read against the working folder) relative to the worktree root, lexical, lower-cased, `/`-separated, `.` for the root.

  A later `--no-build` run of that project keeps its word while the record stands, the obj check still applying; a solution or root build here still removes the record. A key already listed is not added twice, and a record not of its format is left as it is; one that cannot be read says `gate: cannot read <path>: <error>`.

  The add waits for the worktree's work-folder key mutex, the one a routed run holds, up to the key wait, and says `gate: cannot add to <path>: <reason>` when it cannot take it ([Output lines](#output-lines)). An add that would take the record past 4096 bytes says `gate: cannot add to <path>: the record is full` and leaves it unchanged.

  A passing routed dotnet build that did not bring everything back also says which folders it left stale ([Output lines](#output-lines)); that line reads the outputs outcome and never changes the record.

  **The restore here.** A routed dotnet build that passed and brought everything back runs `dotnet restore <target>` on this PC when the target has no `obj\project.assets.json`, so a later `--no-build` run here keeps its word ([Output lines](#output-lines) for its lines).
  Only a build that builds in place is a candidate: the verb is `build`, `test` or `run`, with no output option, `--artifacts-path`, runtime part, target switch or output or setting property (`builds_in_place`, as the nothing-back record's add reads it); `publish`, `pack`, `msbuild` and a build that moves its output get no restore and no line. It is a direct dotnet command, not one wrapped in a script, with no response-file word.
  The target is the project or solution the command names, else the first word after the verb that is no option or option value the gate knows and names an existing file or folder, read against the working folder. A target that is not a `.csproj`, `.fsproj`, `.vbproj`, `.sln` or `.slnx` file gets the skip line.
  The restore's words are `dotnet restore <target>` with the command's own restore-affecting words (`-r`/`--runtime`, `--use-current-runtime`, `--source`, every property form, `-c` as `-p:Configuration=`) and `--disable-build-servers`; never its framework, a word after its own `--`, or a value of an option the gate knows (`--blame-hang-dump-type` and `--blame-crash-dump-type` included, and a `true` or `false` after `--sc` or `--self-contained`).
  It runs after the run has let its slot, remote hold and work folder's key go and before the verdict's closing lines, which the run holds back so they stay its last, on no slot, in the command's working folder, in a kill-on-close job of its own, with the environment a run here of its words would get.
  With no covering parent it gets no `GATE_SLOT_HELD`; under a commit run (covered) it gets the commit's marker and `GATE_COMMIT_SLOT`.
  Its bound is the least of 300 s and what is left of the routed command's own ceiling (a raised one included) counted from the routed call's start; `--test-restore-limit-ms` replaces the 300 s.
  Its output goes to `<log>.restore.log`, removed and created new, beside the log.
  The checks run in this order, each ending the restore: the assets check (silent when the assets file exists, or when the worktree's build files may move intermediates), the target kind, Ctrl+C already pressed, and under 10 s of the ceiling left.
  A Ctrl+C during the restore reports it interrupted whatever its exit code. The run's status and verdict stay the routed command's however the restore goes, and it neither writes nor clears the nothing-back record.

  A routed dotnet `--no-build` command whose word the builder dropped (the builder built, and nothing comes back) writes the record when none stands, whichever client ran it and whether or not the repo has output globs, with the builder and the `-c`, `-f` and `-r` its words set; it never rewrites or clears a record that stands.

  When the record's presence cannot be read it says `gate: cannot read <path>: <error>` and writes nothing. When the route withheld the run's outputs (the repo has output globs), the Rust client also writes the outputs pending marker for it ([The remote client's run](#routing)), so the next routed run of the worktree sends a full manifest.

  A fallback after a refusal writes none. A record that cannot be written says `gate: cannot write <path>: <error>`.

  A passing build here clears the record only when all three hold: its verb is not `msbuild`; it names a solution, or names nothing and runs at the worktree root; its `-c`, `-f` and `-r` words match the record's, ignoring letter case, an absent word equal to an absent one. A build that names one project, or runs from a subfolder, does not clear it. A record that cannot be removed says `gate: cannot remove <path>: <error>`.

  A record that cannot be read counts as present, saying `gate: cannot read <path>: <error>`: a record wrongly taken as present costs one build here, one wrongly taken as missing a `--no-build` run that finds no build.

  **The built-here record.** A passing local dotnet `build`, `test` or `run` that builds in place (as the nothing-back record's add reads it, never `msbuild`) writes `remote\<key>.built-here` ([State root](#files)) with an entry for each project it built that lands inside the worktree, under the `-c`, `-f` and `-r` its words set, in place of any entry the project had.

  The projects are the ones the words build: the project named, each project a named solution lists (its `..` entries inside the worktree count), or, naming nothing, the working folder's one project file, else for `build` and `test` only the projects of its one `.sln` or `.slnx` when it holds no project file.
  Every entry carries `seq`: a write gives every project of that write the record's largest `seq` plus one (an entry without one reads as 0).

  A record that would pass 64 KiB drops other entries oldest `seq` first, ties by key, never the entries that write makes, and says `gate: built-here record trimmed to <n> entries` in the log. An entry over the cap by itself is not written, saying `gate: built-here record not written: entry over <cap> bytes`, the record left as it was. A record that is missing, cannot be read or is not of its format reads as empty, and a write replaces it.

  The write takes the work folder's key, the mutex a routed run of the worktree holds, with a brief retry of 2 s.

  The same take removes `remote\<key>.outputs-from`, as the PC's outputs are no longer what a builder last sent. A key still held after the retry writes no entry and says `gate: built-here record not written: work folder <key> is held`; the build then says `gate: waiting for the work folder's key to remove <path>` and takes the key once more, bounded hard at the key-wait bound (90 s), which no builder drain marker holds off, to remove the outputs-from record.

  When that take fails too, the outputs-from record is removed without the key, saying `gate: removed <path> without the work folder's key: <why>`.

  A build that also adds to the nothing-back record uses the key taken here and never waits for it twice; when it never got it, the add says `gate: cannot add to <record>: held for <ms> ms` (or the failure's text) and leaves the nothing-back record standing, the conservative direction. A write that fails otherwise says `gate: cannot write <path>: <error>`.

  A routed run clears the record, removing the file, before each attempt on a builder, under the key lock the run holds, when its command as given is a dotnet command that builds (any target, no `--no-build` of its own), or the route dropped its `--no-build` with no send planned.

  A planned send that falls back to the dropped words clears it too: when the nothing-back veto stops the send, when the send does not land, and when the box refuses the run for the sent build and the rerun drops the word. The scripted client clears it when it takes the drop for a planned send.

  A record that cannot be removed refuses the attempt before anything is sent: the run says `gate: remote build not sent: cannot remove the built-here record: <why>`, then the `inputs` fallback line, and goes on here, the journal recording `inputs`. Two costs are accepted: the bounded removal of the outputs-from record may delete a correct one, which costs the next routed run a full send, and the clear before each attempt also runs for an attempt that falls back before anything builds.

  Each routed run pushes a new snapshot commit, and the .NET SDK stamps the source revision into every assembly's informational version, so without a pinned revision a routed build recompiled every project on every run.

  A routed dotnet command whose verb (the next non-empty word after `dotnet`, in any letter case) is `build`, `test`, `publish`, `pack`, `run` or `msbuild` therefore gets `-p:SourceRevisionId=<the worktree's HEAD sha>` on the builder's words only: after the dotnet command's own words, before its own `--`, else at the end (a wrapped dotnet command gets it before dotnet's own `--` too).

  The word is not added when one of the dotnet command's own words already sets `SourceRevisionId` (a property switch, as [below](#routed-path-map)). The sha comes from `git rev-parse --verify -q HEAD` in the worktree root, asked once of every routed run, whatever its command (the light-slot route hint reads no HEAD); when that fails or the branch is unborn the run routes without it.

  A routed dotnet command that keeps its `--no-build` for a send of the PC's build gets neither this word nor the `PathMap` word, since it builds nothing on the builder.

  The same sha is sent to the builder as the environment variable `SourceRevisionId`, after the caller's `--env` pairs, so the command sees it whatever its words: MSBuild's git targets there would otherwise read the box's HEAD, the run's snapshot commit, and stamp a new revision on every routed MSBuild run.

  It is not sent when a caller's `--env` names `SourceRevisionId` (in any letter case; the caller's wins), when HEAD is unborn or unreadable, or when adding it would put the variables over the cap of 64 names or 32 KiB. Nothing is printed in any case. A warm-up sends no `Env`, so its commands get no pair. A local or fallback run, the journal and the queue record never show the pair.

  The SDK keeps an explicit value (its git targets fill `SourceRevisionId` only when it is empty) and a `-p:` global property overrides a repo's own `<SourceRevisionId>`, so the builder's DLLs read `1.0.0+<HEAD sha>` as a local build's do, and a no-change routed build compiles nothing. The fallback here, the journal and the claim keep the command as given, and no output line says the word was added.

  <a id="routed-path-map"></a>The same dotnet commands also get `-p:PathMap=<box tree>\=<PC root>` on the builder's words, right after the `SourceRevisionId` word (else where it would go), so the builder's dll and pdb record the PC's paths and a routed build's bytes can equal a local build's. It is added whatever the HEAD, since it does not depend on it. For a `macos` builder the pair is `<tree>/=<PC root>`, the tree joined with `/` and ending in one `/`.

  There is one pair for each tree of the run, the main tree first, then each carried sibling's in carried order, joined by `%2C` (MSBuild reads a bare `,` as the start of another property).

  The pairs are those the outputs install makes (`outputs::trees_of`, shared with the word): the box side is the tree's folder under the builder's work folder and ends in exactly one `\`, so a tree `key` never maps a folder `key2`; the PC side is the root as git spells it, with no trailing `\`, since the compiler adds the separator and a second one would record a doubled one.

  A warm-up's commands get the same word as a routed run's.

  The word is not added when the dotnet command's own words set `PathMap`, `ContinuousIntegrationBuild` or `DeterministicSourcePaths` (the caller chose the recorded paths); the `SourceRevisionId` word has its own opt-out, `SourceRevisionId`, and the two are judged apart.

  A property switch is an optional `-`, `--` or `/`, then `p` or `property` in any letter case, then `:` or `=` and a list of settings separated by `;` or `,`; or the switch as its own word followed by a word holding `=`. A setting's name is read after trimming white space and `"`. `dotnet run -p proj.csproj` sets nothing. A response-file (`@`) word makes the command opaque, so it gets neither word.

  A run whose PC roots (the main tree's and each sibling's) hold anything but ASCII letters and digits, `.`, `_`, `-`, `~`, `\`, a drive colon and a leading `\\server\share`, or that start with a `\\?\` or `\\.\` prefix, gets no `PathMap` word at all: the builder's command line cannot carry the character.

  The run's log (a warm-up's log, `remote\warm\<key>.log`, for a warm-up) gets `gate: no PathMap word: the root {root} holds '{c}', which the builder's command line cannot carry`, the root, the character and any error text escaped as `\u{...}`; it is no output line on stderr. The same note says a root or the builder's root that cannot be told.

  The limits of the word:

  - The PC's full paths now reach the builder: its run log, its `obj` files and its pdbs hold them.
  - The PC side is git's spelling of the root. A user who builds locally through a `subst` drive or a junction gets a local build recording the path they built through, which differs from the word's, and the in-place output rule does not fire there.
  - MSBuild reads a `Directory.Build.rsp` on its own, and a props file can set `PathMap` too; the opt-out sees only the dotnet command's own words, so either can set `PathMap` (or the other two) without the opt-out seeing it, and the gate's word is added regardless.
- **The decision is `gate-router`'s**, pure, over facts the caller gives it and the decision asks for lazily, in this order. A light or critical gate, or one under a covering or lending parent, stays local silently and reads nothing. `--local` stays local, saying `gate: not routed: --local`. Only then is `remote.json` read: no file stays local silently, and a file that is not a file, cannot be read or is refused stays local saying `gate: ignored <path>: <reason>; running locally`.

  After the file: a git hook when `hooks` names no repo at all (absent, empty or unusable; the rule then asks git nothing more), a command with no non-empty word, or a first word whose file name without extension is `git` or `prek` (`git.exe` and a full path included, in any letter case) stays local silently, as does a matched command whose first word the path rewrite sends as a program's name that is `git` or `prek` (`C:\x\git.exe.exe status`).

  Git is asked only then: a folder in no worktree (any non-zero `git rev-parse` exit), a repo the file does not list (the main checkout's folder name as a repo name, in any letter case), a git hook whose repo `hooks` does not name (matched in any letter case, silently, with no line and no `notRouted`) and words no pattern of the repo matches stay local silently, the first matching pattern winning.

  A fact that cannot be read stays local saying `gate: not routed: <reason>`. A machine with no `remote.json` costs no git call.

  The repo's `.gate/siblings.toml` is read once the file lists the repo, before the words are rewritten, so the rewrite knows the siblings; a gate that stays local before that point never reads it.

  A command no pattern matches stays local silently whatever the file holds.

  A matched command with a word outside the worktree and its siblings stays local saying `gate: not routed: <word> is outside the worktree`, unless the word names a path ([`Rewritten::outside_path`], the absolute path with `\`, `..` resolved and a `subst` drive replaced by its target; none for a share or device path, a drive-relative path, or a word that is a script's text or a list) in another git worktree of a listed repo.

  In that case the run says `gate: not routed: the command works in <tree>; run it with --cd <tree>` and the `queue` event's `notRouted` is `another worktree`.

  The tree is the nearest folder from the path up that holds a `.git` file or folder, read from metadata alone with no git started and without following a link, so a `.git` that is a link or a junction holds none. It must be absolute on a local drive (`GetDriveTypeW` says neither network nor missing), and neither the run's root nor a carried sibling's, judged by folder id with the spellings compared in any letter case when an id cannot be read.

  Its repo is named from its main checkout's folder, read from the `.git` file's `gitdir:` line and the git folder's `commondir` file (a link is not followed), and must be a repo `remote.json` lists. Any other case, an unlisted or unreadable repo among them, keeps the plain outside line and writes no `notRouted`. The route hint of a light run (`would_route_on_heavy`) skips this lookup, since it only changes the line of a run kept here.

  Only a matched command acts on what the file says, in this order, each keeping the run here with its line.

  First a problem with the file (`gate: not routed: .gate/siblings.toml: <problem>`, the first problem, on one line, control characters and reordering marks escaped; for a copy in the declarations folder the file is named by its full path in place of `.gate/siblings.toml`).

  Then, for declared siblings: a name the builder cannot lay out (the same `.gate/siblings.toml:` line, checked again whatever the reader checked, before consent).

  Then a declared sibling `remote.json` does not allow (`gate: not routed: sibling <name> is not allowed in remote.json`; every sibling is not allowed when the `siblings` value cannot be used, the line then ending `: <problem>`).

  Then a root folder name the builder cannot lay out (`gate: not routed: the worktree folder name <leaf> cannot be laid out on the builder`); a sibling named like the root's own folder (`gate: not routed: .gate/siblings.toml: siblings[<i>] is the worktree's own folder: "<name>"`).

  Then each sibling's folder, in declared order, the first fault stopping the look: no worktree beside the root (`gate: not routed: sibling <name> not found beside the worktree`), or a refusal (`gate: not routed: sibling <name> <reason>`, [Output lines](#output-lines)).

  Last, by folder id: a sibling whose folder is the worktree's own (`is the worktree's own folder`) or an earlier sibling's (`is the same folder as sibling <other>`), reached under two names.

  A sibling's folder is present when it is a real folder that is a direct child of the root's parent, `<parent of the root>\<name>`, opened by its name inside the parent without following a link (a link or junction is refused as `is a link or a junction`, a file as `is not a folder`), holding a `.git` that is a folder or a linked worktree's file and not a link or junction (`has a .git that is a link or a junction`); with no `.git`, or nothing there, it is missing.

  A failure to read says `gate: not routed: cannot read the sibling folder <path>: <error>`.

  The folder ids are the volume serial and 128-bit file id of each open folder. A repo with no file declares no siblings and nothing changes, except that the root's own folder id is read for every matched command, siblings or none, once the declared siblings are found.

  A root whose id cannot be read keeps the run here with `gate: not routed: cannot read the id of the worktree folder <root>: <error>`, since the worktree lock of every routed run is named by it, and the client checks the folder it opens against it.

  A matched command that would otherwise route and names a gitignored project input carries the files to the builder as inputs (`carried_inputs`, after the nothing-sent check). Each file is an exact path in the run's inputs after the `[carried]` mark (`CARRIED_INPUT`), which no repo glob can equal, and a repo with no `inputs` entry gets an inputs step for such a run.

  The command stays local, journalling `notRouted: "gitignored input"`, when the files cannot go. The caps, checked in the router before any push, are 64 MiB of raw bytes and 2,000 files across all carried candidates, saying `gate: not routed: <path> is gitignored and a command's inputs carry at most 64 MiB in 2000 files; its files hold <n> MiB in <k> files`. A sibling pair says `gate: not routed: <path> is gitignored, and a sibling pair does not send inputs yet`.

  A candidate whose files cannot be listed says `gate: not routed: <path> is gitignored and its files cannot be sent: <why>`, `<why>` being the walk's or git's text, `it is inside a nested repository`, `the folder is a link or junction`, `<file> is left out of the files its folder sends` or `cannot read <path>: <error>`. A pinned run says `gate: --builder <host> cannot route this run: <reason>` with the same reasons.

  A candidate is an existing file with an extension of `.csproj .fsproj .vbproj .proj .sln .slnx .slnf .targets .props .rsp .runsettings`, or, in a `dotnet` command after the verb, a folder in which every project or solution file directly inside is gitignored; a candidate an `inputs` glob covers is not one, since `inputs` already sends it ([remote.json](#remotejson)).

  `git check-ignore` runs first on a candidate's own project files, so a tracked candidate never walks. An ignored `.props`, `.targets`, `.rsp` or `.runsettings` file, and an ignored project or solution file at the root, carry only themselves.

  An ignored project or solution file below the root carries its folder's gitignored files, as does a folder candidate. The sweep is recursive and skips `bin` and `obj` below the swept folder, links and junctions, nested repositories, the forms the inputs pipeline excludes, and files past `PATH_DEPTH_LIMIT` parts.

  Candidates come from whole words, switch values after `=` or a colon, MSBuild lists split on `;` and `,`, and `@file`. Never read: script text, words after a bare `--`, and the values of the dotnet output options (`-o`, `--output`, `--results-directory`, `--diag`, `-d`, `--artifacts-path`, `-bl`, `--binaryLogger`, `-fl`, `-flp`, `--fileLoggerParameters`).

  One `git check-ignore -z --stdin` call judges every candidate. When it fails, each candidate is asked alone; one git refuses (in a submodule, say) gets the log note `gate: cannot tell which words are gitignored: <why>`, and the run routes with the light-run hint withheld.

  A command the repo keeps on this PC is declared in its `.gate/local.toml`, `patterns = [...]` of at most 32 `-like` patterns matched against the rewritten words joined by single spaces, like the `repos` and `desktop` patterns. The check runs right after the `repos` patterns match and before the siblings and desktop checks, on the file read as `desktop.toml` is (guarded, at most 64 KiB, else from the declarations folder), and needs no consent in `remote.json`, since it only narrows routing.

  The first pattern the words match keeps the run here with `gate: not routed: .gate/local.toml keeps it here (<pattern>)` (escaped). A file that cannot be used keeps every routed command of the repo here with `gate: not routed: .gate/local.toml: <problem>`, the first problem, on one line and escaped. Both journal `notRouted: "declared local"`, a copy from the declarations folder is named by its full path, a light run so kept gets no route hint, and `gate check` does not report the file.

  A command that needs the builder's desktop is declared in the repo's `.gate/desktop.toml` and consented to in `remote.json`'s `desktop` ([remote.json](#remotejson)); the check runs after the siblings checks and before the outside-path and nothing-sent checks, on a command the `repos` patterns matched, and the file is read from disk as it stands, through the same guarded read as `siblings.toml` (`.gate` and the file not followed as links, at most 64 KiB), or, when the worktree has no such file, from the repo's declarations folder ([remote.json](#remotejson), Declarations folder).

  The first declared pattern the command's rewritten words match decides, matched against the same words joined by single spaces as the repo's `repos` patterns. No file, `patterns = []` or no matching pattern asks for nothing.

  A matching pattern that `remote.json` does not list for the repo, letter case included, keeps the run here with `gate: not routed: desktop pattern '<pattern>' not allowed in remote.json` (escaped), the line ending `: <problem>` when the `desktop` value cannot be used.

  A file that cannot be used keeps every routed command of the repo here with `gate: not routed: .gate/desktop.toml: <problem>`, the first problem, on one line and escaped; a copy read from the declarations folder is named by its full path in place of `.gate/desktop.toml`.

  A consented match makes the run a desktop run: `RemoteRun.desktop` is true and the run call asks for the desktop (the client's Desktop bullet, below).

  git is found along the gate's own `PATH` (absolute entries only) and run by its full path from the gate's folder, with the `GIT_*` variables that point it at another repository, index or config removed and `core.fsmonitor` off and `core.hooksPath` pointing at a folder that cannot exist (inside the git program's own file), so a gate in a hook that exports `GIT_DIR` is judged by the repo it is in (it routes when `hooks` names that repo, [remote.json](#remotejson)) and no hook a repository's config names runs.

  Each call gets 60 s, after which git is killed and the gate runs here saying `gate: not routed: git did not answer within 60 s` (in whole milliseconds, `1 ms`, when `--test-git-limit-ms` sets a limit that is not a whole number of seconds).

  The limit is long because a loaded PC took git over 10 s in tiny repos, which kept heavy gates here exactly when the builder would help most; a git that truly hangs costs a heavy gate a minute.

  `gate run` answers those facts in `route.rs`, before it queues and before any command starts. The routing file is `%LOCALAPPDATA%\gate\remote.json` (no file when `LOCALAPPDATA` is unset or empty). The hook rule reads `GIT_INDEX_FILE`: set and non-empty means a git hook, which routes only when `hooks` names its repo.

  The worktree comes from one `git rev-parse` run in the gate's folder, which is the `--cd` folder when given: a caller in a main checkout gates a worktree's command with `--cd <worktree>`.

  It reads (root, prefix below the root and common git folder), and a folder below the root asks `git ls-files` whether the snapshot would send anything there (tracked or untracked and not ignored, and still on disk). A `subst` drive is followed through `gate_win32::device_target`. A word that is not valid Unicode keeps a command that would route here, saying so.

  The queue line carries `notRouted` `--local` for a heavy `--local` gate, `another worktree`, `declared local` or `gitignored input` for the runs those reasons keep here, the run's `--reason` text after it, and the remote tag for a routed one.

  When the remote client has a part missing, the routed run falls back before it takes a remote slot: the `fallback` line has reason `client`, the gate says `gate: remote <host> client could not start: ...; running here`, and the later `take` and `release` carry no remote fields.
- **The routing file's text** is JSON with `//` and `/* */` comments and trailing commas allowed outside strings, and a leading byte order mark skipped, as the PowerShell gate's reader allows. Keys match in any letter case, and unknown keys are ignored.

  An object holding two keys that differ only in letter case anywhere in the file refuses the whole file, with the PowerShell reader's error text. Single-quoted strings are refused (`Conversion from JSON failed with error: ...`), a recorded deviation: the PowerShell reader accepts them.

  `host` is an ssh alias or `user@host`, each part of ASCII letters, digits, `.`, `_` and `-` and not starting with `-`: the host reaches ssh's argument list, a git `host:path` URL and ssh_config `%h` expansion, and only that shape is safe in all three.

  Any other host refuses the file (`host must be an ssh alias or user@host of letters, digits, '.', '_' and '-', not starting with '-'`), a recorded deviation: the PowerShell reader accepts any non-empty string. An IPv6 literal or a `host:port` is written as an alias in `~/.ssh/config`.

  `slots` is a whole number 1 to 16 (1 when missing), `repos` an object whose values are arrays of non-empty `-like` patterns, and `outputs` an optional object of the same shape whose globs may not hold `[` or `]`; a repo with no `outputs` entry brings back `**/bin/**` and `**/.godot/mono/temp/bin/**`.

  `failureOutputs` is an optional object of the same shape with the same `[`/`]` ban, read by the same table reader; the route puts the repo's list (the builder entry's own for a `builders` entry) in `RemoteRun.failure_outputs`, none when the repo is not named, with no default.

  `inputs` is an optional object of the same shape with the same `[`/`]` ban (`is not a valid input glob: '[' and ']' are not allowed`), read by the same table reader; a repo with no `inputs` entry sends nothing, and there is no default list. The route carries the repo's list as `RemoteRun.inputs` and `RemoteCall.inputs`, kept for a dotnet `--no-build` command, whose outputs alone are withheld.

  `sendBuild` is an optional array of repo names, matched in any letter case, whose routed `dotnet test <project> -c <configuration> --no-build` may send the PC's build so the builder keeps the word, whether or not the project was built here (`Config::send_build_allowed`); a repo it does not name, or a file without the key, sends no build by this rule. A listed repo's run the listed rule sends nothing of (a `run`, a `test` with no project or configuration) reads the built-here record as an unlisted repo's does, and a macOS pick is steered off it the same way ([Routing](#routing)).

  A value that is not an array (`sendBuild must be an array of repo names`) or holds an item that is not a non-empty string (`sendBuild[N] is not a non-empty string`) lets no repo send and leaves the rest of the file usable; routing says nothing about it.

  A cargo command instead (its first word's file stem is `cargo` in any case; one a `pwsh` script is handed after `--` is not) brings back `target/*/*.exe`, `target/*/*.dll` and `target/*/*.pdb`: each profile folder's own files, never `deps/`, `build/` or `incremental/`. A repo's own `outputs` list replaces either default.

  A routed `dotnet build` to a Windows builder, in a repo whose own non-empty `outputs` list stands, also brings back the bin of the folder it builds: `{folder}/bin/**` is appended to the repo's list unless already there (`derived_outputs`, `target_folder` and `bin_glob` in `decide.rs`, called from `remote_run`).
  The folder is a project file's own folder, a `--project` folder (`--project X`, `=X`, `:X`), a lone folder word (`dotnet build src/Yaat.Client`), or the gate's own folder for a bare `dotnet build` run below the repo root (never at the root). A path is taken relative to the gate's folder with `\` read as `/`, empty and `.` segments dropped and `..` resolved against the gate's folder; a target that climbs above the repo root, or is absolute, derives nothing.
  A routed `dotnet test` of a project without `--no-build` derives its project's bin the same way, so a later local `--no-build` run of it finds its test exe.

  Nothing is derived for a solution, a verb other than `build` or `test`, a `test` with `--no-build`, an output option (`-o`, `--output`, `--artifacts-path`), an output property (`OutputPath`, `BaseOutputPath`, `UseArtifactsOutput`, `OutDir`; `Platform` still derives, its output staying under `bin/`), an empty `--project`, an explicit empty list `[]`, a macOS builder, or a repo with no own list (which already gets `**/bin/**`). A dropped `--no-build` still clears all outputs.

  `siblings` is an optional object of the same shape as `outputs`, but each value an array of folder names the builder can lay out (`is_layout_name`): the sibling worktrees the machine lets that repo carry, its repo name matched in any letter case and each sibling name exactly. A repo with no entry is allowed none.

  A `siblings` that is not an object, or holds a value that is not an array of such names, never refuses the file: it allows no sibling, and the rest of the file is read as usual ([Sibling worktrees](#routing)).

  `desktop` is an optional object mapping a repo name, matched in any letter case, to an array of the `.gate/desktop.toml` patterns the machine lets that repo run on the builder's desktop (`Config::allowed_desktop`); a declared pattern is consented only when listed exactly, letter case included. A repo with no entry is allowed none.

  A `desktop` that is not an object, or holds a value that is not an array of non-empty strings, never refuses the file: it consents to none and the rest is read as usual. The reasons are `desktop must be a JSON object`, `desktop."<repo>" is not an array of strings` and `desktop."<repo>"[<i>] is not a non-empty string`, which the routed run's not-allowed line appends.

  `declarations` is an optional object mapping a repo name, matched in any letter case, to the absolute folder holding that repo's `.gate` files it cannot commit (`Config::declarations_folder`); see [remote.json](#remotejson), Declarations folder. A `declarations` that is not an object, or holds a value that is not a non-empty string, never refuses the file: it names no folder and the rest is read as usual.

  The reasons are `declarations must be a JSON object` and `declarations."<repo>" is not a non-empty string`.
- **The `-like` matcher** is PowerShell's wildcard match, case-insensitive, over the words joined by single spaces. It folds case by simple case folding. Inside a `[...]` set, a non-ASCII member whose .NET case-insensitive equivalents differ (the Kelvin sign and `k`) can match in PowerShell and not here.
- **The in-worktree path rewrite** (T3) finds a path wherever a drive letter, a colon and a separator start one inside a word (`--flag=X:\a`, `-p:Name=X:\a`, a script's text), each start judged on its own.

  A path runs to the first of `, ; " ' ) | & < >`. In a word that holds one of those or a line break (a script word: a script's text or a list) white space ends a path too; in any other word, one argument, a path runs through its spaces.

  The router keeps this `subst` model for anyone who routes from a subst drive; its tests use a made-up drive letter, never a mapping of the machine they run on.

  A drive other than the root's is followed through `subst` mappings as a chain, at most 26 hops with a visited set, and the root's own letter is never asked; a cycle, an unreadable device or a target that is not a `\??\` path gives no spelling, and the run stays local.

  A bare first word (no separator, drive or `.`/`..`) becomes `./name` only when that file exists in the gate's folder; otherwise it stays bare, for the builder's `PATH`.

  A first word that names a program by an absolute path outside the worktree and its siblings is sent as the program's name, the file name with a trailing `.exe` removed, so the builder finds it on its own `PATH` (a Godot or dotnet install named by its full path).

  A Godot console word of a run with a chosen Godot version runs that version's install instead (Toolchain manifest, below).

  The run says once, first among its routed lines, `gate: routed <given> as <name>` (both escaped), and the fallback here, the journal and the claim keep the words as given. Such a word is never written `./name`.

  The name is refused, and the word stays outside (the run stays here with `gate: not routed: <word> is outside the worktree`), when the file name holds any of `* ? " < > |`, white space or a line break, or is empty once `.exe` is removed; when any path segment after the drive (folders and file name) holds `:`, holds `~` followed by an ASCII digit (a short name), ends in `.` or a space, or is empty, `.` or `..`; or when the word holds a second drive path (`X:\a\x.exe;C:\other`).

  With siblings declared, a path inside a sibling's root (every spelling the root can have, `subst` drives included) is sent as `..` once for each level of the gate's folder below the worktree root and once more, then `/<name>` as declared and the rest, with the separator the in-worktree edit uses.

  A relative path is inside a sibling when its `..` climb exactly to the root's parent and then enters a declared sibling (names compare in ASCII letter case only; `yaat-server2` is not `yaat-server`). Any other climb above the root, a word naming the parent itself, and leaving a sibling again (`../yaat-server/../x`) are outside, and keep the run here with `gate: not routed: <word> is outside the worktree` (or the `--cd` line of the decision above).

  An absolute path whose `..` leaves the root it starts below and lands in the worktree or in a declared sibling is sent as where it lands; landing anywhere else is outside.

  Windows stops `..` at the root of a drive or share, so a `..` past that root is outside, never a climb to the root's parent. The floor comes from the spelling of the gate's folder: a folder spelled on a drive, a `\\?\X:` path or a UNC share whose root is the worktree root or a folder below it sits fewer folders below that root than it sits below the worktree root, and a relative climb past the difference is outside.

  A path on a bare `subst` drive that stands for a root (`S:\..`) never leaves that root either.

  Every word is judged for outside at each path start: the start of the word, after white space or a line break, after `, ; " ' ) | & < > ( =`, after a leading `@` (and an `@` right after any path start), and after the colon of a switch that starts its white-space token or follows a `"`, `'` or `,` (`-bl:`, `/p:`).

  The forms below keep the run here with the same `is outside the worktree` line. A command's first word that names a program by an absolute path is judged first, as above: it is sent as the program's name, or, when the name is refused, it is outside like any other path.

  Drive-relative text is a letter and `:` not followed by `\`, `/` or `:`, anywhere in a word or at any path start (`C:..\..\secret`, `-bl:C:..\x.binlog`); `-c:Release`, `/t:Build`, `$env:PATH` and `crate::mod` are not. Root-relative text is a single `\` at a path start, or a single `/` only where a value begins (after `=`, a switch colon or `@`); `/p:...`, `/c` and nextest's `test(/^run::/)` stay switches or text.

  A share or device path at any path start is outside: two separators, or a drive right after `\\?\`, `\\.\`, `//?/` or `//./`. Relative text that climbs out of the worktree (or past the drive-root floor above) is outside at every path start, not only at the word's.
- **The work folder** on the builder is named `<repo name>-<key>`, the worktree root's last segment cut to letters, digits, `-` and `_` and 8 hex digits of the SHA-256 of the lower-cased root ([Glossary](GLOSSARY.md), work-folder key).
- **The routed run's life** is kept (T5) and built in `remote/mod.rs` and `run.rs`: its ticket is heavy with `remote: true` and queues apart from the local waiters, which never count it ahead of them, nor it them; before that it waits up to 90 s for its work folder's key while another run holds it (`--test-key-wait-ms` in a test build), holding no PC slot and journaling only its remote-tagged `queue` line, and falls back `busy` only past that bound or when a warm-up of the folder holds or takes the key; it then waits up to 60 s in all for a remote slot, across every builder it tries (`--test-remote-wait-ms` in a test build); it takes one with a claim of kind `remote` of its own and calls the remote client with the route, the log and the limits (`--stall` as given, the interrupt).

  A client that runs the command to its end hands back its status, which the gate passes through, and the run releases the slot with the remote tag.

  A call the builder refuses `full` (its own slots all held, [Builder-side slots](#builder-side-slots)) is no fallback at first: while some of the run's remote budget is left, the run says once `gate: waiting for a slot on the builder <alias> (its slots are all held)`, keeps its remote slot, polls the builder with the preflight alone every 5 s (`--test-drain-poll-ms` in a test build), so a poll neither sends the worktree again nor repeats the run's start lines, and says `gate: still waiting for a slot on the builder <alias> (<s> s)` each heartbeat (`--test-heartbeat-ms`). The first poll the builder does not refuse `full` ends the wait, and the run then makes its one call, its wall and held time counted from just before that call; only the wait's own time counts into the run's 60 s remote budget. A poll the drain refuses goes on as the wait behind the drain, and a call refused `full` after a wait behind the drain waits for a builder slot in turn. A Ctrl+C during it ends the run as one before the command started. Once the budget is spent, the last `full` is the refusal below.

  On `full` (no slot within the wait, or none within what the builder wait left of the budget), a refusal (status 75 with a reason) or a client that cannot start, it journals `fallback` while still holding the remote slot (none was taken on `full`), deletes the claim, lets the slot go, and tries the next builder when the reason is one the run retries and one is left ([Which builder](#routing), The next builder), else runs here, taking a heavy slot with its first arrival kept in the ticket.

  The attempt's lines stay in the log once, as notes: the client's own bytes in the log are removed, and the local run writes its log afresh.
- **The remote client** (GATE-43) is built in parts.

  The seam a routed run calls is `RemoteClient` in `crates/gate/src/remote/mod.rs` (the call, the outcome, the scripted client); part A, the snapshot, and part B1, the transport, the routed lock, the preflight, the push and the run call, and part B2, the sync-back, part C1, the watchdog, the stop, Ctrl+C, the sync-back's deadlines and the output cap, and part C2, the lost connection and the reboot, are built, and every routed run uses them (The remote client's run, below).

  The snapshot copies the real index to `<git path>/gate-remote-<pid>.index` (a leftover copy and its `.index.lock` are removed first; the copy is empty when the repo has no index). With `GIT_INDEX_FILE` on the copy it runs `git add -A`, `git write-tree` and `git commit-tree --no-gpg-sign` with no parent, under the identity `gate-remote <gate-remote@localhost>` and with `GIT_AUTHOR_DATE` and `GIT_COMMITTER_DATE` removed.

  The copy is always removed, and the real index and the worktree are never touched, so both are byte-identical before and after a snapshot. The snapshot takes a list of roots, one a repository: the main one, then a pair's siblings in declared order.

  For a pair every git call about a repository is given `GIT_CEILING_DIRECTORIES` set to the root's parent, so git never reaches a repository above it.

  Once the repository's place is checked (its top folder is its root, see [Output lines](#output-lines)), each later call is given `GIT_DIR`, `GIT_WORK_TREE` and `GIT_COMMON_DIR` outright, as git printed them, so a `core.worktree`, a `commondir` or a `gitdir:` written after the check cannot point it elsewhere; the place is asked again after each save and a different one keeps the run here.

  Every git call runs with `core.hooksPath` off. A clean filter in a repository's own config still runs during the snapshot's `git add`: the repository's config is the user's, an accepted risk.

  The local ref of the snapshot a builder last received is per builder: `refs/gate-remote/<key>` for `host`, `refs/gate-remote/<id>/<key>` for a builder with an id (a `builders` or `hosts` entry, the id its pool and caches use). The bridge commit has the snapshot's tree and, as its parent, the receiving builder's own ref of the key, else the newest of that builder's refs not ending `-back`; with none it has no parent. The push writes the builder's ref once the builder has the snapshot.

  The bridge is what keeps a push small: a snapshot has no parent, so git leaves out only what the bridge's parent reaches, and only when the builder holds that parent. One ref shared by two builders would hand each a parent the other received, and every switch between them would send the whole repository.

  Every git call of the client goes through `remote/git.rs`, which the routing decision's git calls share: it removes the `GIT_*` location variables, applies the call's time limit (`--test-git-limit-ms`, one value for the routing decision's calls, the client's and the `--test-remote-snapshot` client's), starts git through `remote/launch.rs` (suspended, assigned to an anonymous kill-on-close job of its own and resumed, with its output on pipes read by a thread per stream), reads and drops stderr where nothing reads it and ends git's whole job on Ctrl+C.

  The calls the client reads (`Git::output`: the sync-back's and the snapshot's) read git's output up to 16 MiB and keep the last 64 KiB of its stderr; a call past the cap is ended and fails with `git <args> failed in <folder>: its output passed 16 MiB`. The routing decision's calls are not capped. A Ctrl+C that ends git is an interrupt.

  The sync-back's `update-ref` and `apply` and the snapshot's `update-ref` run on a hidden console and answer to no interrupt, so a Ctrl+C cannot end git while it holds a ref lock in the git folder.

  Once the fetched back commit is confirmed as this snapshot's, every step up to a successful `apply` (the changed-file listing, the worktree check, the patch and the `apply` itself) that fails or is interrupted first keeps the back commit at `refs/gate-remote/<key>-back` and says so:
  - a failure: `gate: the remote run failed: <reason>; the changes are kept: see git diff <snapshot 8 hex> <back 8 hex>`;
  - an interrupt: `gate: the run was interrupted; the changes are kept: see git diff <snapshot 8 hex> <back 8 hex>`, with the interrupt's own status;
  - a keep that fails too: `gate: the remote run failed: <reason>; the changes could not be kept either: <why>`, or after an interrupt `gate: the remote run failed: the run was interrupted, and the changes could not be kept: <why>` (status 1). When the fetched objects could not be moved into the repository (Sync-back, below), the owner line is said first and `<why>` is `see the line above`.

  A patch that applied but cannot be removed fails the run (`gate: the remote run failed: cannot remove <patch>: <error>`) and keeps nothing: the changes are already in the worktree.

  Once git has exited, its readers get the call's drain, 2 s, to reach their pipes' ends, else the call fails with `cannot read git's output: it did not end within <n> s of git's exit`, `<n>` being the drain in whole seconds (a drain that is not a whole number of seconds is spelled in ms, `<n> ms`; the same holds for `dotnet --list-sdks` and the tool probes, with `<name>` their program).

  `--test-git-drain-ms` replaces the 2 s for the routing decision's calls, the run's duration-key place, the client's, the `--test-remote-snapshot` client's and the drain of this PC's `dotnet --list-sdks` read. A failure line carries the last 10 of git's lines joined by ` | `, escaped and cut to the last 4 KiB at a whole escape.

  A call that places a folder (`route::PLACE_ARGS`, a `git rev-parse`: the run's duration-key place, the routing decision's place and `gate check`'s place) gives its readers, once git exits by itself, as long as git itself may take, or the call's drain when that is longer (`git::placing_drain`), the seam's drain included; a git past its limit keeps the ordinary drain.

  The places the duration store's rebuild makes answer the run's Ctrl+C: a raised interrupt ends the call at once, its drain included, unless git's output has already arrived. The other placement calls take no interrupt.

  The call's job is ended at git's exit, so a reader that is not starved ends at once and the longer drain costs nothing; a starved gate that lost the place would key the folder by its path's shape, and `gate check` would fail with `check: cannot read git's output: it did not end within 2 s of git's exit`. The heavy hint's placement call, made while the run holds its light slot, keeps the ordinary drain.

  Of the test seams, `--test-remote-snapshot` and `--test-remote-transport` give the client a stand-in. The first makes the snapshot of each worktree of the call, the main one then a pair's siblings in declared order, says `gate: remote snapshot <sha> bridge <sha or none>` on stderr and in the log, one line a worktree, and ends with status 0.

  A snapshot that fails says `gate: the remote run failed: git <args> failed in <root> (status <n>): <git's lines joined by ' | '>` and ends with status 1, with no fallback.

  Ctrl+C ends it with `STATUS_CONTROL_C_EXIT` and no line. A log that cannot be written gives status 1, with no fallback.
- **The remote client's run** (GATE-43, parts B1, B2, C1 and C2) is built in `remote/{client,parity,payload,retry,syncback,transport,watchdog}.rs`, in every build. Every routed run goes through the Rust client over ssh (`RustClient` in `remote/client.rs`, chosen in `remote/mod.rs`).

  In a build with the `test-seams` feature, `gate run --test-remote-transport local:<folder>` (or `local:{alias}={folder}|{alias}={folder}`, a folder for each builder) stands in for the builder with a local folder, and `--test-remote-snapshot` and `--test-remote-client` take precedence over the ssh client.

  The Rust client's builder root is `D:\gate-remote`, and its ssh is `ssh.exe` found along the gate's own `PATH`: `ssh.cmd` and `ssh.bat` are never looked for, in the client and in `gate push` alike ([`gate push`](#gate-push)). ssh is looked up only when a run routes, in the client's `ready()` as the routed attempt starts, before any key lock, ticket or slot; a run that never routes never looks for it.

  The client's ssh calls start in the Windows system folder with `NoDefaultCurrentDirectoryInExePath=1`, and every git call the gate makes carries that variable (`NO_CURRENT_FOLDER` in `remote/git.rs`), so neither ssh nor a program it starts by name is found in the gate's working folder, which may be the worktree.

  The client runs the file the ssh found leads to through links, and refuses it when that file lies inside the run's worktree, with the same check the dotnet read has.

  A client that cannot start falls back `client` and says `gate: remote <host> client could not start: <why>; running here`; a run with no usable ssh says `cannot run ssh: <message>` before any key lock, ticket or remote slot. A `cannot run ssh: <path> lies inside the worktree` is said after the remote slot is taken, since the worktree is known only to the run call.

  A repo name that is not `[A-Za-z0-9_-]+` is never sent to the builder: the push and the sync-back fail the run with `cannot push to the repo '<name>': a repo name is letters, digits, '_' and '-'`, as a client failure (`gate: the remote run failed: ...`).

  The phases run in this order. Any `gate: the remote run failed` line ends the run with status 1 and no fallback; Ctrl+C at any point after the snapshot ends the run as an interrupt.
  - **Lock.** The run takes the routed lock, `Local\gate-[<root hash>-]routed-<16 hex>` ([Kernel object names](#kernel-object-names)), before the remote slot, and holds it for the whole remote attempt (for every builder the run tries). A lock held elsewhere is waited for up to 90 s (`--test-key-wait-ms` in a test build) with no PC slot held; the journal shows the run's remote-tagged `queue` line only.

    A lock still held past the bound says `gate: remote <host> busy; running here` and falls back `busy` before any queueing. A warm-up lock on the key, probed before the wait and at each still-waiting heartbeat, that holds or takes the key makes the run fall back `busy` at once instead of waiting it out. A lock that cannot be opened or taken says `gate: cannot open the mutex <name>: <error>` or `gate: cannot take the mutex <name>: <error>`, then the busy line.
  - **Hold.** The run takes the worktree locks of its main root and each carried sibling ([Sync-back](#routing), The worktree lock) and, for a pair, holds its folders ([Sibling worktrees](#routing)); one it cannot hold keeps the run here ([Output lines](#output-lines)).
  - **Snapshot.** Part A's snapshot and bridge commit, for each repository of a pair.
  - **Run line.** The run call's cmd line is the fixed `"C:\Program Files\gate\bin\gate.exe" box run --stdin`, the same whatever the run holds; every other `gate box` call's line differs only in its verb. The request, base64 of compact UTF-8 JSON, is the first line of the call's stdin, followed for the run call by the outputs manifest line, so no request size is capped by a command line. The box reads the request line with a cap of 16 MiB, newline included ([The box side](#the-box-side)).
  - **Preflight.** `gate box preflight` over ssh. Status 255, ssh's own status for a lost connection, says `gate: remote <host> unreachable: <last stderr line>` and falls back `unreachable`; the line is the last `\r` update of the last stderr line, its first 4 KiB, escaped (Escaping, below).

    Status 9009 or 2 (no `gate box` verb there), or a Unix shell's 126 or 127 (a box program it cannot run or does not find), says `gate: not routed: <host> has no gate box that takes this run (preflight status <n>)` and falls back `version`.

    A status 1 with the box's install line (`gate: the builder's gate install is not trusted: <why>`, Install tripwire in [The box side](#the-box-side)) among the preflight's lines says `gate: not routed: <host>'s gate is not installed where only administrators can change it (<why>); run gate check --builder` and falls back `install`; the why is cut to 4 KiB and escaped.

    The same line and fallback answer the inputs call, and the run call when the install line is the call's only line, before the start marker; a run call that carries the `install` refusal tag (status 75) says the same line from the box's install line after the tag. No stop call follows a refused install, since nothing started.

    A status 1 with the box's work-root line (`gate: the builder's work root is not locked down: <why>; run the installer's "Gate work root" step (...)` or `gate: the builder's work root is busy: <why>`, Work root in [The box side](#the-box-side)) among the preflight's lines, none after a start marker, says `gate: not routed: <host>'s work root is not locked down: <why>` or `gate: not routed: <host>'s work root is busy: <why>`, then `gate: remote <host> root; running here`, and falls back `root`; the why is cut to 4 KiB and escaped.

    The install line is read first.

    An older box never says the line, so its refusal stays `preparing ... failed (status 1)`, and an older PC gate fails such a run with status 1 rather than falling back.

    Any other non-zero status says `gate: preparing <builder root> on <host> failed (status <n>)` and ends the run with that status. A reported version text other than this PC's whole one, build id included, says `gate: not routed: <host> runs gate <theirs|unknown>, this PC <ours>; run gate push` and falls back `version`; the builder's version is its first 4 KiB, escaped.

    The client also reads the preflight's `gate: remote warm <none|64 lower-case hex>` line, only the line right after the last `gate: remote version` line: anything else there, no such line, or a warm line elsewhere reads as no warmth.

    It reads `gate: remote new work tree` the same way, only exactly that line right after that warm line: anything else there, or an older box that never says it, reads as a work tree that is not new.

    A run whose preflight said the work tree new is a cold run, unless it is a warm-up, whose tree is new by design. Its run call's `Timeout`, the client's command limit and the passed line's ceiling become `min(2 x ceiling, ceiling + 1800 s)`, for an explicit `--timeout` and `--timeout auto` alike; the claim's `timeoutSeconds` and the stall window are unchanged, and the client's backstop is 5 x the raised ceiling + 60 s.

    Before the run call it says once, on stderr and in the log, `gate: remote cold: this worktree's work folder on the builder is new or was cleared; the ceiling is raised from <a> s to <b> s; a .gate/warmup.toml lets gate warm prewarm the next new worktree`. For a repo with warm-up consent and a warm-up file the note ends `; prewarm the next new worktree with gate warm` instead.

    A routed run starts reading Tailscale's path to the builder once its call returned, as `gate check --builder` reads it, and journals it on its `release` line ([Journal](#journal)); a read that cannot start says `gate: tailscale path not read: <why>` and never changes the run's outcome. The run's end waits out a read caught between creating its program and putting it in its kill-on-close job; a read whose launch comes after the gate began ending creates nothing (`cannot run <program>: the gate is ending`).

    No run falls back for the warm line (a warm-up's end and `gate check --builder` read it): a routed run goes to the builder whether or not its work folder is warm, and a new one is a cold run.
  - **Parity.** After the preflight and before the push, `remote/parity.rs` checks that the builder's `gate: remote sdks` line (from the preflight) has what the snapshot needs. A builder with no sdks line, or a tree that is not .NET (no `global.json`, `.sln`, `.slnx` or `.csproj` anywhere in the snapshot), skips the SDK part of the check; the tools of the repo's `.gate/toolchain.toml` are judged either way (Toolchain manifest, below).

    A lacking builder says `gate: remote toolchain mismatch: <repo> needs <need>; <host> has <sdks>` once for each need, in the order the needs are listed, then a single `gate: remote <host> parity; running here`, and falls back `parity` (status 75).

    A need is an SDK or a tool the manifest declares; each kind words the builder's side of its own line (for an SDK, its SDKs, or `none`; for a tool, the version the builder found, or `none`). The SDK needs come first, then the tools' in the manifest's order.

    `<need>` is `.NET SDK <version> (rollForward <policy>)` for a pin, the policy defaulting to `patch`, or `a .NET <major> SDK` without one, where the major is that of this PC's newest SDK (none listed: no check). A pin is met by dotnet's own rule for the nine rollForward policies and `allowPrerelease`, versions read as `x.y.znn` (feature band and patch).

    A .NET tree with no readable pin is refused unless the builder has an SDK of this PC's newest major, after `gate: global.json has no readable sdk pin; checking the SDK major only`. A `global.json` has no readable pin when it is not UTF-8, is past the git runner's 16 MiB cap, or has member names not in dotnet's exact case (`sdk`, `version`, `rollForward`, `allowPrerelease`).

    It also has none when its version is not three-part (`8.0`, `8.0.100.1`, `v8.0.100` and a version with surrounding spaces are not), when its `rollForward` is not a string naming one of the nine policies (matched ignoring case), or when its `allowPrerelease` is not a JSON boolean.

    `//` and `/* */` comments outside strings are stripped before parsing, a block comment counting as one space. Only the first JSON value is read, as dotnet reads it: a stray `}`, other text or a second value after it is ignored. A root `global.json` is found whatever the letter case of its name.

    This PC's SDKs come from `dotnet --list-sdks`; if that cannot be read the run says `gate: cannot read this PC's SDK list: <why>; checking no SDK major` and checks no major. A snapshot whose file listing passes 16 MiB, or holds a path that is not UTF-8, says `gate: cannot list the snapshot's files: <why>; skipping the toolchain check` (`<why>` the over-cap line, or `a path is not valid UTF-8`), and the run goes remote unchecked.

    **Bounded SDK read**: `dotnet --list-sdks` runs like the client's git calls, through the same start-and-wait function in `remote/git.rs`: in an anonymous kill-on-close job of its own, on the gate's console, with the run's interrupt and a 30 s bound (the test seams `--test-dotnet-limit-ms` and, for `gate check`, `--test-sdk-limit-ms` lower it), its standard output read up to 16 MiB and the last 64 KiB of its standard error kept.

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

    **Toolchain manifest** (GATE-206, GATE-209): a repo's `.gate/toolchain.toml` declares what a routed run's builder must have beyond the .NET SDK, and `remote/toolchain.rs` reads it as data from text the caller hands over.

    [`gate check`](#gate-check) reads it from the worktree on disk and says what this PC lacks; a routed run with parity reads it from the snapshot's git tree (the only one of the repo's seven `.gate` files read there; `siblings.toml`, `desktop.toml`, `local.toml`, `env.toml`, `warmup.toml` and `windows.toml` are read from the worktree on disk) and refuses the route when the builder lacks a tool it declares (below).

    `toolchain::meets` compares a found version with a minimum. A repo's sibling worktrees are not declared here but in `.gate/siblings.toml` ([Sibling worktrees](#routing)), whose problems keep a run local.

    The file has a `[tools]` table of `name = "version"` and a top-level `workloads` array of .NET workload ids. A version is a minimum, "this or later", written `major[.minor[.patch]]` with the missing parts 0 (`"24"` is 24.0.0), and `""` means the tool is present at any version; a routed run reads the `godot` and `unreal` entries as exact pins instead (below).

    The known names are `rust`, `cargo-nextest`, `node`, `pnpm`, `pwsh`, `godot`, `unreal`, `ffmpeg`, `git-lfs` and `netfx-sdk`; an unknown name is kept and flagged unknown, for the consumer to skip with a line. A leading byte order mark is skipped.

    An optional `[env]` table of `NAME = "unreal"` names variables the box sets to the root of the Unreal Engine install the run chose, the folder `UE_ROOT` gets (below). `"unreal"` is the only value, and at most 64 entries are read.

    A repo's minimums are the lowest versions its own files require: `rust-version`, `rust-toolchain.toml` or the edition, `packageManager`, the launcher's `#requires`. Where nothing pins a floor, the minimum is the oldest toolchain the builders install (rust 1.89, the first with edition 2024's let chains); where neither applies, the entry is `""`. pwsh is `7.4` everywhere, since the launcher needs PowerShell 7. A minimum set higher than the repo needs would refuse a builder that can run it.

    A file that is not UTF-8 or does not parse is unreadable whole, and the line for it is `gate: cannot read .gate/toolchain.toml: <why>; skipping the manifest's checks`, `<why>` being `line <l>, column <c>: <toml message>` or `not valid UTF-8 (...)`.

    A routed run words a manifest problem its own way, `gate: .gate/toolchain.toml: <problem>; skipping it` (`gate: <full path>: <problem>; skipping it` for the declarations folder's copy), for a whole file that cannot be read as for a bad entry, `<problem>` escaped.

    A bad entry is skipped alone and named in the reader's problems: `tools.<name> is not a version: "<text>"`, `tools.<name> is not a string (TOML <type>)`, `tools is not a table (TOML <type>)`, `workloads is not an array (TOML <type>)`, `workloads[<i>] is not a string (TOML <type>)`, `workloads[<i>] is empty` and `<key> is not a key of the manifest (tools, workloads, env)`.

    The `[env]` problems are `env is not a table (TOML <type>)`, `env has more than 64 names`, `env.<name> is not a string (TOML <type>)`, `env.<name> is not "unreal": "<value>"`, `env.<name> is a name the gate sets or refuses` (not an ASCII letter or `_` followed by letters, digits and `_`, or starting with `GIT_` in any case, or a name `--env` refuses).

    Also `env.<name> is named twice` (in any ASCII case; the first is kept) and `env.<name> names the unreal install but the manifest pins no unreal` (the name is still sent).

    **On a routed run**, the client lists the snapshot's tree once before the preflight, with `git ls-tree -r -t -z <commit>` (a sibling's listing keeps `--name-only`): the file names parity needs and the `.gate` folder and `toolchain.toml` candidates for the manifest are read from that one listing. The `.gate` folder and `toolchain.toml` are found ignoring letter case, and the exact-case regular file wins.

    The file is used only when it is a regular blob, read by its object id; a submodule, a symbolic link, a folder, or several spellings of it with none in exact case gets a skip line saying why. A repo with no manifest says nothing.

    A snapshot whose tree has no manifest reads the repo's copy in its declarations folder instead ([remote.json](#remotejson), Declarations folder), a tree's manifest winning even when it is skipped. A tree whose `.gate` is a file reads no copy either.

    The copy is read from the PC's disk with the guarded read of `.gate` files, not from the snapshot, and the log alone says `gate: .gate/toolchain.toml read from <full path>`, a line the client keeps in the log after a fallback too. A problem with it is worded with its full path in place of `.gate/toolchain.toml`.

    The preflight's `Needs` carry the tools the manifest declares whose names the box knows how to probe (the known names above), each with its minimum, or empty for any version. A name outside them is skipped with `gate: .gate/toolchain.toml names unknown tool <name>; skipping it` (`gate: <full path> names unknown tool <name>; skipping it` for the declarations folder's copy) and not sent.

    `godot` is sent whenever the repo pins a Godot version, on every routed run whatever its command: the manifest names `godot`, or a `*.csproj` of the snapshot names `Godot.NET.Sdk/<version>`. Its value is the distinct pins joined by `,` (the box ignores the text and reads only the name), or empty for a manifest entry of `""`. A manifest `godot` entry wins over every project pin; a manifest that is unreadable, or whose `godot` value is no version, does not stop the project pins from applying.

    A repo pinning nothing sends no `godot` need and is judged as before, except for a command whose first word is a Godot console word (below), which sends a `godot` need with no versions so the builder lists its installs (`gate: remote tool godot <versions>` shows in its output). Every other command of an unpinned repo sends what it always sent.

    The builder answers each tool with a `gate: remote tool <name> <version|present|missing|skipped: <why>>` line. A tool it found missing, or older than the minimum, is a need, and the route is refused as under Parity: one `gate: remote toolchain mismatch: <repo> needs <name> <min> or later; <host> has <version|none>` line per need (`<name>` alone when the manifest sets no minimum), then the parity fallback line, and the run goes local.

    `godot` is judged as an exact pin, not a minimum. The builder's answer is the list `gate: remote tool godot <v1>,<v2>,...` of its installed versions, split on `,`. A pin is met when one installed version equals it on the parts the pin gives (`4.7.2` exactly, `4.7` any `4.7.x`, `4` any `4.x.y`), a prerelease install reading as its release; a four-part SDK version reads as its first three parts.

    Every pin must be met, else the mismatch line is `gate: remote toolchain mismatch: <repo> needs godot <pins, comma separated> exactly; <host> has <found>`, `<found>` being the builder's versions joined by `, `, `none`, `not probed (<why>)`, or its own answer when that is no version list; for a manifest entry of `""` (any version) the line reads `needs godot`.

    An answer that is no version list is a mismatch whenever a pin needs checking, so the command runs here rather than on a builder that may lack the version. A slow install that drops off the builder's list reads as missing, a needless fallback and a known limitation.

    After the preflight the client chooses the Godot version the run uses, from the repo's pins among the versions the builder reported: no pin, or a manifest entry of `""`, chooses none (see below for a Godot command); one pin chooses the highest reported version that meets it (`4.7.2` itself, `4.7` the highest `4.7.x`); a pin no reported version meets chooses none.

    A chosen version, spelled `x.y.z`, is sent as the optional `GodotVersion` of the run payload and of a warm-up's, and left out when none is chosen. The box resolves it ([The box side](#the-box-side), the command's environment).

    A repo whose pins differ, a repo that pins no Godot version and a manifest entry of `""` choose none. What a command does then depends on its first word. A Godot console word is a bare name with no directory part whose name, after one trailing `.exe` is removed, matches `godot*console*` ignoring ASCII case with no `.` after the last `console`; `.cmd`, `.bat`, `.ps1` and any other extension are not.

    A Godot-console command of a repo pinning several versions falls back `parity`, saying `gate: the repo pins several Godot versions (<pins, comma separated>); a routed run needs one` after the mismatch lines and before the parity line, on the console and in the log. A pin no reported version meets is the parity mismatch, with its own lines and no new one.

    A Godot-console command of a repo that pins no version, or whose manifest has `godot = ""`, runs on the newest install the builder listed: that version is chosen and sent as `GodotVersion`.

    A repo pinning no version says, on stderr and in the log, `gate: warning: the repo pins no Godot version, so this run uses <x.y.z>, the newest on <host>; pin the version it needs in .gate/toolchain.toml ([tools] godot = "x.y.z") or as the .csproj's Godot.NET.Sdk version`, once the run is known not to be refused and before it starts; `godot = ""` says nothing. A run refused for another need says no warning.

    Such a command stays here, refused `parity`, when the builder reports no Godot install: a repo pinning no version says `gate: the repo pins no Godot version and <host> lists no Godot install; a routed run needs one`, and `godot = ""` has only the parity mismatch's own lines. When the builder gives no version list for godot (its probe was skipped, or it answered with no version), either says `gate: <host> did not list its Godot installs; a routed run needs one`.

    A repo pinning no version also stays here, refused `parity`, when its pins could not be read (its tree could not be read, its `.gate/toolchain.toml` exists but cannot be read, or a project file's Godot pin was left unread), saying `gate: the repo's Godot pins could not be read; a routed run needs one`. A missing manifest is not unreadable, and a manifest entry of `""` settles the version whatever a project file pins.

    Every other command of the repo routes as before, with no `GodotVersion`.

    A warm-up counts as a Godot-console command when any of its commands' first word is a Godot console word. It then sends the same `godot` need, follows the same rules with the same lines (said in its log, and on the console of a foreground `gate warm`), and carries the chosen version as its payload's `GodotVersion`. A warm-up the rules refuse is recorded `failed`, refused, with the reason `the builder refused: parity`.

    `unreal` is an exact pin judged as `godot` is, and read from the manifest only (no project file names it): `"5.8"` matches any 5.8.x, `"5.8.1"` that one, and `""` any version. A manifest `unreal` entry is sent as an `unreal` need like any manifest tool, and judged on every routed run whatever the command.

    A manifest that names `unreal`, at any version, also needs a `netfx-sdk` of 4.6.0 or later, the .NET Framework SDK `UnrealBuildTool` looks for: `toolchain::read` adds it at the end of the tools, and a `netfx-sdk` the manifest declares itself is raised to at least 4.6.

    It is sent and judged as a minimum like any tool, the highest of the SDKs the builder lists weighed, so a builder with none, or only older ones, falls back with the need `netfx-sdk 4.6.0 or later`; `gate check` here and `--builder` judge it the same way.

    The builder's answer is `gate: remote tool unreal <v1>,<v2>,...`, and an unmet pin is the mismatch line `gate: remote toolchain mismatch: <repo> needs unreal <pin> exactly; <host> has <found>` (`needs unreal` for `""`), `<found>` as for godot.

    When the builder cannot be probed and the entry is `""`, the line is `gate: <host> did not probe unreal: <why>; not checking it`.

    After the preflight the client chooses, from the main repo's manifest pin, the highest reported version that meets it and sends it, spelled `x.y.z`, as the optional `UnrealVersion` of the run payload and of a warm-up's; a pin of `""` chooses the highest reported version, and no pin, or a pin no reported version meets, chooses none, and the field is left out. The box sets `UE_ROOT` from it (The box side, the command's environment).

    The names of the manifest's `[env]` are sent as the optional `EngineEnv` of the run payload and of a warm-up's, an object of `{NAME: "unreal"}`, whenever the manifest has a usable name, an engine chosen or not; the box sets each to the chosen engine's folder. A builder whose gate predates `EngineEnv` ignores it.

    A tool the builder did not probe, gave no answer for, or found at a version that cannot be read against a minimum gets `gate: <host> did not probe <name>: <why>; not checking it` and is not a need. The lines come in this order: the SDK step's own, then the manifest's problem lines, then the per-tool skip lines in the manifest's order, then the mismatch lines.

    A text from the manifest or the builder is escaped, and the builder's cut to 4096 bytes, so neither can forge a line.

    `toolchain::standing` judges a tool against a minimum from what its probe found (met, too old, missing, not readable, skipped) in one place, which `gate check`'s report shares and a routed run applies to every tool but a pinned `godot` or `unreal`, which both judge with `pinned`; `gate check` judges a manifest `godot` or `unreal` pin through `pinned` too, so its report and a routed run agree. `--test-remote-interrupt-at manifest` raises the interrupt just before the manifest is read.

    Two pins a repo already declares are read beside it: each tool's version in `.config/dotnet-tools.json` (the release part, so `1.0.0-beta.1` is 1.0.0; read as strict JSON, as dotnet reads it, so a comment or a trailing comma makes the file unreadable, and a tool id given twice counts once, with its last version) and the `Godot.NET.Sdk/<version>` a project file names, on its root `<Project Sdk=...>` or an `<Sdk Name="Godot.NET.Sdk" Version=...>` element directly inside it (an `<Import Sdk=...>`, a deeper `<Sdk>` element and a `min=` version are not read).

    The dotnet tools' versions are read and nothing judges them yet. The Godot pin is an exact version on the parts it gives, a four-part SDK version reading as its first three, and is judged on every routed run (above).

    The project files are every `*.csproj` the snapshot holds, at any depth and in any ASCII letter case: at most 64 files of at most 256 KiB each, read with at most two git calls, the sizes first and then one bounded batch read. A file that cannot be read says `gate: cannot read the Godot pin of <file>: <why>; not checking it` and gives no pin; files past the 64th say `gate: <n> more project files not read for a Godot pin; not checking them`.

    `global.json` stays the SDK pin, read by `parity.rs` as above.
  - **Push.** `git push --no-verify --no-recurse-submodules --progress [--receive-pack=…] -- <host>:<builder root>/<repo>.git +<snapshot>:refs/gate/<key> [+<bridge>:refs/gate-bridge/<key>]`, with `GIT_SSH_COMMAND` carrying the ssh options.

    A Windows builder's `--receive-pack` is `pwsh -NoProfile -NonInteractive -Command $env:GIT_CONFIG_COUNT='1'; $env:GIT_CONFIG_KEY_0='receive.autogc'; $env:GIT_CONFIG_VALUE_0='false'; git-receive-pack '<path>'`, so a push never starts a gc on the builder.

    Success says `gate: pushed snapshot <8 hex> in <s> s (<size>|up to date)` and moves the builder's local snapshot ref (`refs/gate-remote/<key>` for `host`, `refs/gate-remote/<id>/<key>` for a builder with an id).

    The builder keeps one bare repo per repo name, `<builder root>/<repo>.git`, shared by every key of that repo; the key is only the ref's name.

    The size is read from git's last `Writing objects: 100%` line, ignoring `remote:` lines, and only a number with a unit (or `bytes`) counts as one, else the line says `up to date`. Failure says `gate: pushing the snapshot to <url> failed (status <n>)` and falls back `unreachable`.

    A pair pushes each repo in order, the main one first, under one transfer clock started before the first push, and says `gate: remote pushed snapshot <repo> <8 hex> in <s> s (<size>|up to date)` for each; a failure says `gate: pushing the snapshot of <repo> to <url> failed (status <n>)`. The run's push time is the pushes' sum.

    The push is given `GIT_ALLOW_PROTOCOL`, `ssh` (`file` for the test transport's local bare repositories), which git reads in place of every `protocol.*.allow`, so no `pushInsteadOf` or protocol setting a repository's config names can turn the push into a protocol helper's.
  - **Inputs.** In `remote/inputs.rs`, for a run whose repo declares `inputs`; a run without them, and without a gitignored project its command names, builds no manifest, makes no inputs call and says nothing, unless the carried marker is there (below).

    A run that names one carries its files as `[carried]` entries, with or without `inputs`. The manifest is built before the preflight: the files the globs match, kept when `git check-ignore` reports them ignored, each hashed once (the hash cache below), so a refusal at the preflight or the parity check sends nothing.

    The calls are made after the push and before the run call, and the run call's request carries `InputsDigest`, the SHA-256 of the manifest line.

    The diff call (`gate box inputs`, `Step` `diff`) is fed the request and the manifest line and answers the paths whose copy is missing or differs. The put call (`Step` `put`) is made only when the diff named a file, fed the request, the manifest line and then an archive of exactly those files, in the outputs archive's format. A diff that names none says `gate: inputs: <n> file(s) unchanged` in the log.

    A glob that matches nothing, a match git does not report ignored (it is in the snapshot) and a file inside a folder holding a `.git` of its own (another repository, which git cannot be asked about) are each said in the log and skipped.

    Never sent, whatever the globs: any `.git` path, `.lfsconfig`, `.env`, `*.env`, `.env.<name>` and `.envrc` at any depth, matched without regard to case, the gate's own archives and stages under `.tmp` (named for the work folder's key) and the run's own log and `.err` file, found by canonical path. `.tmp` is otherwise reachable.

    Caps: 50,000 members and a 64 MiB manifest, counted on what the globs match, and 4 GiB per put, counted on the files the diff named (`--test-remote-inputs-budget-bytes` replaces the 4 GiB). A set past a cap is not sent. A file that changes between its hashing and its packing is not sent either, and its entry is dropped from the hash cache.

    The hash cache is `remote\<key>.inputs-hashes` in the state root, beside the outputs one and apart from it, since the outputs saves drop every file the outputs walk no longer finds. It holds the manifest line of the last built manifest's files last written well before that build, is saved when a build changes it, and is the same shape and rules as the outputs cache ([Manifest time](#routing)). There is no PC record of what the builder holds: the diff call is the truth.

    The worktree's `.tmp` is held through the step. The diff's answer (`<key>-<nonce>.inputs-diff`) and the archive (`<key>-<nonce>.inputs.tar.zst`) are made new there, opened by handle with delete-on-close, so a gate killed outright leaves none, and each put attempt reads the archive at a position of its own.

    The diff is watched as a fetch is, by the stall window on its output and its own transfer ceiling (1800 s) counted from its own start. The put is watched while the archive is being fed by the later of the last byte fed and the last byte of output, and once it is fully fed by the transfer ceiling alone, since the builder installs without a word. Either call is made once more when it failed before the banner.

    A carried path the run's inputs exclusions match (the list above, the run's own log and `.err` among them) is dropped from the manifest, neither sent nor said missing. The router leaves the run's own log and `.err` out of the files it sweeps from a carried folder (they reach the sweep only for a pinned `--builder` or `--alone` run, since an ordinary run removes its log before routing), and makes no `git check-ignore` call when no candidate swept a file.

    The carried marker is `remote\<key>.inputs-carried` in the state root (the builder's cache folder), written empty once an inputs step that carried a command-named path has landed; `gate: inputs: cannot write the carried marker <path>: <error>` says it could not be.

    A later routed run of a repo with no input globs and no siblings, finding the marker, sends the empty manifest: the box's diff deletes the carried copies it recorded, the run call carries that manifest's `InputsDigest`, and the log says `gate: inputs: removed the carried copies of an earlier run` in place of the unchanged line. The marker and the inputs hash cache are then removed (`gate: inputs: cannot remove <path>: <error>` for one that stays), so the next run makes no inputs call.

    That cleanup is best-effort: any failure but Ctrl+C logs `gate: inputs: the carried copies were not removed: <why>`, the run goes on with no `InputsDigest`, and the marker stays for a later run.

    A carried path the step does not send ends it as an `inputs` refusal, logging `gate: remote inputs not sent: <path> was named by the command but could not be sent`.

    Any failure of either call refuses the route with reason `inputs`: console `gate: remote <host> inputs; running here`, log `gate: remote inputs not sent: <reason>` and the run goes on here. The reason is the box's last error line without its leading `gate: `, since the log line carries its own.

    The reasons include `the box named <path> twice`, `the put stalled: no bytes sent or received for <n> s`, `the put passed its transfer ceiling of <n> s`, `the diff stalled: no output for <n> s`, `the diff passed its transfer ceiling of <n> s`, `git check-ignore failed: status <n>: <git's stderr>`, the builder's last error line, a count the builder installed other than the one sent, and a manifest or put past a cap.

    A cut call says no `STALLED` or `TRANSFER TIMED OUT` line, since the log scan would fail the run here after the fallback. A run the box refuses with the `inputs` tag (its work folder no longer holds the inputs sent) falls back the same way, saying `the builder's work folder does not hold the inputs sent`.

    A Ctrl+C during the step ends the run as an interrupt with no fallback. A call whose input could not be fed logs `gate: the inputs call's input was not sent: <error>`.

    A sibling pair whose repos declare inputs is refused with reason `inputs` and `a sibling pair does not send inputs yet`, before any inputs call. `--test-remote-watch-call inputs-diff` names the diff and `inputs` the put; `--test-remote-interrupt-at inputs` raises the interrupt after the put's or diff's first output byte.
  - **Git LFS.** In `remote/lfs.rs`, for a run whose snapshot holds LFS pointers; a run without any makes no LFS call and says nothing. After the snapshot commit the PC lists the tree's blobs of at most 1024 bytes (`git ls-tree`) whose `filter` attribute at that commit is `lfs` (`git check-attr --source <commit>`, which needs git 2.40), and reads them in one `git cat-file --batch`.

    A blob that is exactly a spec-v1 pointer (its `version https://git-lfs.github.com/spec/v1` line, `oid sha256:<64 hex>`, `size <n>`, in that order and nothing else) is a pointer; any other blob, a raw file committed under `filter=lfs` included, is left alone. A tree with no `.gitattributes` is not asked at all. Two pointers naming one object at two sizes, or more than 50,000 objects, refuse the route.

    The listing reads the tree's attributes as the builder's checkout does: `-c core.attributesFile=` and `GIT_ATTR_NOSYSTEM=1` on every call. A repo that uses LFS and whose common git folder's `info/attributes` is not empty is refused `lfs`, since the PC's `check-attr` reads that file and the builder's checkout of the snapshot never sees it.

    The objects come from the worktree's LFS store, `<common>\lfs\objects\<aa>\<bb>\<oid>`, or the `objects` folder of `lfs.storage` when the repo sets it. Each is hashed as it is packed, and one that is missing, of another size or hash than its pointer names, or that changes while it is packed refuses the route `lfs`, the log line naming the pointer's path and saying to run `git lfs fetch`. The PC never downloads: an object its own store lacks is the user's to fetch.

    The calls are made after the push and the inputs and before the run call, and the run call's request carries `LfsDigest`, the SHA-256 of the manifest line (`<oid> <size>` entries, space-separated, in the order of the first pointer naming each). The diff call (`gate box lfs`, `Step` `diff`) is fed the request and the manifest line and answers the oids the builder's store lacks or holds at another size.

    The put call (`Step` `put`) is made only when the diff named an object, fed the request, the manifest line and an archive of exactly those objects in the inputs archive's format, each member named by its oid. A diff that names none says `gate: lfs: <n> object(s) present` in the log.

    Caps: 50,000 objects, a 64 MiB manifest and 16 GiB per put (`--test-remote-lfs-budget-bytes` replaces the 16 GiB). The worktree's `.tmp` is held through the step and the diff's answer (`<key>-<nonce>.lfs-diff`) and the archive (`<key>-<nonce>.lfs.tar.zst`) are made there as the inputs' are.

    Both calls are watched as the inputs' are (the diff by its stall window and its own transfer ceiling, the put by the later of the last byte fed and the last byte of output while it is fed, then by its ceiling) and each is made once more when it failed before the banner. `--test-remote-watch-call lfs-diff` names the diff and `lfs` the put; `--test-remote-interrupt-at lfs` raises the interrupt after the first output byte of either call.

    Any failure of either call, an object the PC lacks, a listing git fails (the log says `cannot list the LFS pointers of <repo>: <why>`) or a manifest past a cap refuses the route with reason `lfs`: console `gate: remote <host> lfs; running here`, log `gate: remote lfs not sent: <reason>`, the reason a box line without its leading `gate: `.

    A run the box refuses with the `lfs` tag falls back the same way, saying `the builder's LFS store does not hold the content the snapshot's pointers name`, or `the builder could not make the run's LFS scratch folder` when the box's last line is `gate: remote cannot make the run's LFS scratch folder: <why>`. A sibling pair whose repos hold pointers is refused `lfs` before any call, saying `<repo> holds LFS pointers, and a sibling pair does not send LFS content yet`.

    A Ctrl+C during the step ends the run as an interrupt with no fallback.

    The PC also asks the preflight for `git-lfs` at version 3.1.3 or later whenever the snapshot's tracked `.gitattributes` files use the LFS filter, whether or not it holds a pointer: the box's checkout then runs git-lfs for every file under the filter. A tracked `.gitattributes` is read as it is on disk, its first 1 MiB for a line that is not a comment holding the word `filter=lfs`, and a file over that cap counts as using LFS.

    A manifest that declares `git-lfs` is weighed at the higher of its own minimum and 3.1.3, in the need sent, in the judgement and in `gate check --builder`. A builder without it, or older, falls back `parity` with the line naming the version found. `gate check --builder` reports the same need for a repo whose tracked `.gitattributes` name the filter.
  - **Run call.** The line goes over `ssh -o RequestTTY=no -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=4 -o ForwardAgent=no -o ClearAllForwardings=yes -o Compression=yes -- <host> <line>`, each argument separate.

    `-o RequestTTY=no` leads the options of every `gate box` call and `gate push`'s, so stdin never passes through a pseudo-terminal. The last option, `-o Compression=yes`, is the run call's alone: the preflight, stop, boot and outputs calls, git's ssh command and `gate push` ask for none. The run call's compression covers both directions of its connection: the command's output and protocol lines one way, the request and the outputs manifest the other.

    The box's output passes through to the log and the console, except the box's heartbeats and its machine lines, which the client hides from both and keeps out of the lines it holds: the start marker `gate-remote-started: <nonce>` and the refusal tag `gate-remote-refused: <nonce> busy|tools|layout|inputs|sent|lfs|desktop|install|drain|full`, each naming this run's `Nonce` and read on stderr only, as a whole line, and the kill tag and the adjusted line below.

    The client matches those protocol lines on the raw line, before any unstamping, so a command's own line, stamped by the box, never reads as one.

    Every other line is unstamped once before it is read: the log gets the line as it came, a line that came with no stamp (an older box's, a setup call's) stamped `gat` on arrival, and the console and the lines the client holds get its text alone. A process line is logged and nothing else: it is neither echoed nor held, and it never marks the call's progress, so the stall watch of a routed run sees only the box's other lines and its heartbeats. The client's own lines in the log are `gat`.

    The kill tag `gate-remote-killed: <nonce> timed-out|backstop` is said by the box just after its own kill line, and the client counts a box kill only on this run's tag with a status 124 (a status 124 alone is also what a stopped job or the command itself may end with). The client takes the run's verdict from the tag's kind, never from the log's last kill line.

    The adjusted line `gate-remote-adjusted: <nonce> <s.sss>` carries the load-adjusted seconds the box counted, said once the command's output has drained unless the sampler failed, read like the tag on stderr only and the last one winning; another run's, a malformed one or one whose seconds are not a finite number is a line like any other.

    A status 75 is a refusal only when this run's refusal tag came on stderr before any start marker (`busy`, `tools`, `layout`, `inputs`, `lfs`, `desktop`, `install` or `full`, the fallback reason; `full` is a builder whose own slots are all held, the preflight's tag read as the run's is; `drain` is a refusal the run waits on, never a fallback reason, see Builder drain below); a tag after the marker is ignored, so a command that prints the human busy, tools, layout, inputs, lfs, desktop or install line, or a tag of its own, and exits 75 ends with status 75 and no fallback.

    The `sent` tag, read the same way, is no fallback: the client makes the run call once more with `--no-build` dropped ([Routing](#routing)).

    Safety rests on order, not on the nonce being secret (the request is on no command line, but the nonce still appears in the run id, the pipe names and the archive name): the box says the marker before the command can run any code (before it creates the command's process, or, in a desktop run, while the command the helper created is still suspended), and the client ignores every tag once it has heard the marker.
  - **Builder drain.** While a drain barrier stands on a builder ([The box side](#the-box-side), `drain`; `gate drain` raises it, [`gate drain`](#gate-drain)), a routed run from any repo that meets it waits on its PC for the lift and then runs on that builder in arrival order; the drain is never a fallback reason.

    Every call a routed run makes to the box (preflight, inputs, lfs, run) carries the payload keys `Repo` and `NoTicket`, and `Session` and `Pc` when there are any; `Session` is 32 lower-case hex digits made once per run and kept across its attempts and polls, `Pc` is `COMPUTERNAME` (left out when unset or empty), and `NoTicket` is true for a warm-up and for `gate check --builder`, false otherwise.

    The preflight, inputs and lfs calls each carry a fresh `Nonce` too, so the client reads the `drain` tag on them as on the run call. After a `drain` tag the client takes the box's `gate: drain place {n|none}` and `gate: drain barrier {raised} {lapses}|none` lines (stderr only, before any start marker) into the run's view of the drain and echoes neither. A drain with no barrier line has nothing to wait out.

    A call the drain refuses ends its attempt `End::Draining`, wherever in the attempt it came (the preflight, the send, the LFS put, the PC-build send or the run call): the snapshot and worktree locks are released, and the work folder's key and the remote slot are kept.

    The run says once `gate: waiting behind the builder drain (raised {hh:mm} UTC, lapses {hh:mm} UTC); place {n} in line`, the times in UTC from the box's Unix seconds, the parenthesis left out when the box said no barrier stands and `; place {n} in line` when it gave no place.

    It then polls every 5 s (`--test-drain-poll-ms` in a test build) with a preflight alone that sends its `Session`, `NoTicket` false and no needs, built by the word of a full attempt's preflight, and says `gate: still waiting behind the builder drain ({s} s; place {n})` every 30 s (`--test-heartbeat-ms`; `({s} s)` with no place). A poll the drain still refuses updates the barrier and place it last saw.

    Any poll it does not refuse starts a fresh attempt with fresh limits, and one whose preflight passed first says `gate: the builder drain lifted; starting the routed run`, or `gate: the builder drain lapsed at {hh:mm} UTC without a lift; starting the routed run` when the last barrier seen had lapsed by then. An attempt the drain refuses again says the waiting line again.

    An unreachable poll, or one the client killed, counts as waiting while the last barrier seen has not lapsed, said in the log only (`gate: the builder is unreachable while its drain stands (it lapses at {hh:mm} UTC); still waiting`); once it has, or when the drain had no barrier, the run falls back `unreachable` as for any builder. Ctrl+C during the wait ends the run `interrupted_before_start`, the key and slot given back as for any run.

    While it waits the run holds a drain marker, `remote\drain\<builder>\<pid>-<session>` ([Files](#files)), open sharing nothing for the whole wait. While a live marker stands, other waits on this PC end in no fallback: the remote-slot wait for that builder's pool goes on past its 60 s, and the waits for a work folder's key (a routed run's, and the nothing-back record's) go on past 90 s for a marker of any builder, since the key is the worktree's on every builder.

    A key wait held past its bound says once `gate: still waiting for the worktree's key past its bound: a routed run waits behind a builder drain`.

    A wait whose limit is zero is never held off, and a warm-up's own key wait keeps its bound and ends busy. A marker that cannot be held says `gate: cannot hold a drain marker in <folder>: <error>; the run waits without one`, and the run keeps waiting.

    The wait is no part of the run's time: on admission the run's start and its slot's hold restart, so the recorded duration, `--timeout auto` and the hang hint leave it out, and no journal format changes. What stays counted: the slot claim's `started`, which other waiters' holder ETA reads, and a pinned run's `heldSeconds` after a fallback past the lapse.

    A warm-up that meets the drain says `gate: warm: the builder is drained; this warm-up ends without warming` ([gate warm](#gate-warm)), and `gate check --builder` reports it ([With `--builder`](#with---builder)).
  - **Desktop.** A routed run whose `RemoteRun.desktop` is true (Routing, the desktop paragraphs) sends `Desktop: true` and `DesktopWait` in its run call's payload (`payload::run_word`), `DesktopWait` being the usual remote wait, `REMOTE_LIMIT` (60 s), which the box caps at the run's `Timeout`. No other call sends either key, a warm-up included; the scripted test client's recorded call has a `desktop` key.

    A desktop run holds a normal remote slot while it waits for the builder's desktop, so other routed runs of this PC can fall back `full` meanwhile, and the wait counts against the client's backstop.
  - **Warm-up call.** A call whose `warmth` is a warm-up (`Warmth::WarmUp` in `remote/mod.rs`) runs the phases above with the run call's payload built by `payload::warm_word`: the run's keys, `Timeout` always among them, with `WarmCommands` and `WarmHash` in place of `Command` and no `Outputs`, `FailureOutputs` or `OutputsFull` key.

    Its limits are a run's, from the call's `timeout`. `gate warm` makes such a call. A routed run's call of a repo whose warm-up is set up carries `warmth: {"consented": <hash>}` instead, which only words the cold note.

    The run call's end is classed by `warm_end` in `remote/client.rs` and handed back as `RemoteOutcome.warm`. It is warm only on status 0, after the start marker, with the call's last stderr line that is neither a heartbeat nor a protocol line exactly `gate: remote warm {H}` for the `WarmHash` sent. A Ctrl+C is deleted: it leaves no record.

    On the box a warm-up skips `RunOutputs::read`, `back_commit` and `bring_back`, since it brings nothing back and changes no PC worktree state, but it still clears the back refs.

    Any other end is a failure: a refusal when it came before the start marker, as no command ran (the box's refusal tag with status 75, or a status, kill or lost connection before the marker, or an end of the client's own), and the commands' failure when it came after the marker (any non-zero status, the box's kill, a lost connection), and a status 0 the box did not confirm. A status 0 the box did not confirm is reported as status 1.

    The failure's reason is built from the end alone, never from a line the builder or the commands said: `command stopped with status {n}`, `the builder ended the warm-up before its commands with status {n}`, `the box did not confirm the warm hash`, `the builder refused: {reason}`, `the warm-up was killed: it ran out of its budget or went silent`, `the connection to the builder was lost`, `the client could not send the warm-up` or `the client failed`.

    A warm-up touches no outputs state (it reads, saves and marks none, whatever globs the call holds) and syncs nothing back; a lost connection is handled as a run's is. The key mutex a caller holds around it can be taken with a wait (`hold_key_waiting` in `remote/client.rs`): it waits in slices of 100 ms, looks at the console interrupt between them, and a Ctrl+C ends the wait.
  - **Outputs.** In `remote/outputs.rs`, for a run with output globs or failure output globs (`failureOutputs`); a run without either sends `Outputs` `[]` and `OutputsFull` false, no manifest, and says nothing here, and writes no marker unless the route withheld its outputs (a dotnet `--no-build` command the builder built): that run writes the pending marker after the push, as below, and no end of it clears the marker.

    The client builds the manifest ([The outputs module](#the-box-side)) before the run call, hashing every matching file.

    The manifest covers both lists. When it cannot be built over both, the client builds it again over the output globs alone. When that build succeeds, the run says `gate: failure outputs not asked for this run: <why>` and carries no failure globs.

    When it fails too, the run ends as a failed manifest build does, with no such line. A run with no output globs whose build fails carries nothing, as one with no globs at all. A run with only failure globs writes the pending marker and sends a manifest like one with output globs, but its passing command brings nothing back.

    **Manifest time** (GATE-23): SHA-256 streams, with no whole-file read into memory, and only the PC hashes every file (the box hashes changed files only; `obj` is not under the default globs). No timing line is added to routed runs.

    The ignored test `remote::outputs::tests::measure_the_manifest_time_of_a_real_tree` times the walk and `manifest_line`, cold (an empty hash cache) and warm (the cache the cold build saved, loaded back), over the tree `GATE_MEASURE_ROOT` names (globs from `GATE_MEASURE_GLOBS`, `;`-separated, else the defaults).

    Measured on a critical slot over the three largest built trees on the owner's PC: the walk takes at most 0.25 s, and the hash is nearly all the time, so the time follows the bytes under the globs. 2,230 files of 208 MiB take 0.4 to 1.6 s and 2,255 files of 632 MiB take 0.8 to 1.9 s, but 22,199 files of 29.5 GiB take 45 to 48 s, whether or not the files were read just before (the PowerShell manifest takes 36 to 41 s on the same tree).

    That is past a few seconds, so the client keeps a hash cache (`outputs/cache.rs`) in the state root at `remote\<key>.outputs-hashes` (`remote\<id>\<key>.outputs-hashes` for a `builders` entry): a file whose size and modified time, as the open file's handle reads them (`GetFileInformationByHandle`), match its cached entry takes its hash from the cache instead of being read again. The file is one outputs manifest line, as the box is sent one, read back with the manifest's own parser.

    The cache is a hint checked against each file's stamp, not a record of a completed outputs step. It is saved right after each manifest the client builds, whatever the run's end; a build that fails or is interrupted saves nothing, and one that would save what the cache already holds skips the write. A file the install replaces gets a new stamp, the time its attempt began its snapshot.

    An outputs install that completes (no member skipped but ones it cannot rewrite) therefore also saves the hashes of the files it put in place, so the next run does not rehash them.

    Each entry takes the stamp the installed file's handle reads after the move and the future-time clamp, with the SHA-256 the unpack verified against the box's manifest, under the 2 s rule below measured against the PC's clock when the moves began: a file clamped to the PC's clock is not kept. The stamp read after the move is the snapshot time (below), so a run that took over 2 s keeps its hashes under that rule.

    A member the install rewrites after the hash check (the files that record a box tree's path) is not kept, its bytes no longer being the hash's. The install loads the cache file again, adds its entries and saves once; a failed, refused, partial or interrupted install saves nothing.

    An entry is saved only when its modified time is at least 2 s before the moment the walk began, so a file written again within the same tick at the same size is never taken from the cache; a modified time in the future is never saved. A saved entry whose stamp matches is trusted. A file the walk no longer finds drops out at the next save.

    A hit still checks for Ctrl+C, opens the file and checks its final path against the worktree's, as a read does, and trusts the cached hash only when the stamp the open file's handle reads equals the cached entry's.

    That stamp is also the one the manifest line carries, read from the handle before the file is hashed, not from the folder listing, which can lag a file still being written; so a file whose listing lagged is hashed once and sent with its true stamp, and the install does not refuse it as changed during the run.

    The running length the size cap counts is corrected after each hash by the difference between the listing's and the handle's stamp digits, so the cap still stops hashing before the next file. On a mismatch the file is hashed. Only the hash is skipped. The box's own walk and listings keep listing stamps.

    A missing cache counts as empty. One that is unreadable, corrupt or over 64 MiB gives no hash, and the next save replaces it even when it keeps no entry. A failed save is not said: either costs one full hash and changes nothing else, and no line is printed about the cache.

    The save writes `<key>.outputs-hashes.<pid>.tmp` beside the cache and renames it over it. Two routed runs of one worktree are not locked against each other: the last writer wins.

    While the pending marker stands (`OutputsFull`), the manifest is built without reading the cache, and the hashes that build read are saved: the full send is the recovery path, so it reuses no hash that may be wrong. A full send caused only by a builder switch (below) still reads the cache, which holds this PC's own hashes whichever builder ran. A moved worktree, whose key changes, starts with an empty cache, and deleting `remote\<key>.outputs-hashes` resets it.

    Accepted residuals: a copy that keeps a file's size and modified time over other bytes takes the old hash from the cache; a network share whose clock runs more than 2 s behind the PC can write a file whose modified time already reads as settled; and a gate killed mid-save leaves its `<key>.outputs-hashes.<pid>.tmp` beside the cache.

    Once the push succeeds, and before the run call, it writes the pending marker `<state root>\remote\<key>.outputs-pending` (an empty file; a marker whose existence cannot be read counts as present), so a run refused at the preflight or by the parity check leaves none.

    The payload carries the globs as `Outputs`, the failure output globs as `FailureOutputs` (only when there is one, never on a warm-up) and `OutputsFull` (true when the marker was there or the outputs-from record names another builder, below), and the manifest goes as the line after the request on the run call's stdin, which is then closed; a pre-banner retry feeds both again. A marker that cannot be written says `gate: the remote run failed: cannot write the outputs pending marker <path>: <error>`.

    A call whose input could not be sent logs `gate: the <verb> call's input was not sent: <error>`, only when the call reached the box's banner and ended neither refused nor with ssh's 255, since any other end says why on its own lines. An input still being sent 2 s after the call ended logs `gate: the <verb> call's input was still being sent 2 s after the call ended`, and a feeder thread that panicked `gate: the <verb> call's input feeder panicked`.

    The client accepts only the last stderr line starting `gate-remote-outputs: `, kept as it is heard so a stdout flood cannot push it out, and echoes it like any box line. It must be `gate-remote-outputs: 0` or `gate-remote-outputs: <n> <key>-<this run's nonce>.tar.zst` with n above 0.

    The client looks for a stderr line starting `gate-remote-outputs-size: ` (a line on stdout never counts) and takes the archive's size in bytes from it only when it is the stderr line heard immediately before the accepted outputs line (heartbeats and stdout lines in between do not count) and its value is whole digits that fit; a size line anywhere else, any other value, or no such line (a box not yet pushed), gives no size and the pull's lines lack the total (with a box too old to send the line, a command's own last stderr line of that form is read as the size, which only misstates the size and ETA on the start and progress lines).

    The size is a line of its own, which the box says just before its outputs line, so an older PC, which compares everything after the count with the archive's name, never keeps it.

    **Builder switch.** A run with output globs or failure output globs also reads the outputs-from record `<state root>\remote\<key>.outputs-from` ([State root](#state-root), Files), which holds the alias of the builder whose run last brought the worktree's outputs back whole, and sends `OutputsFull` when it names another builder than this run's, since that builder's work folder holds files its own run never wrote.

    Aliases are compared ASCII-case-insensitively; a record that is missing, unreadable, empty or over 512 bytes counts as another builder, so a worktree's first routed run, and its first after the record is gone, sends every output that differs once. A full send caused only by the record reads the hash cache; only the marker's full send rehashes the tree.

    The record is written, over whatever it held, by a passing run with output globs whose box packed nothing and whose outputs the route did not withhold, and by a complete outputs install (the install that also keeps the hash cache's entries, above). It is never written by a withheld run, a failed run's report pull, or a failure-only run that brings nothing. A write that fails says `gate: cannot write <path>: <error>` and changes nothing else.

    After an accepted `0` the marker is removed (`gate: cannot remove the outputs pending marker <path>: <error>` if it cannot, and the status stands); so does an install that ends as the Outputs install below says, and every other end leaves it, so the next run sends `OutputsFull`.

    An accepted line naming an archive leaves the marker; the fetch of the archive and its install (the Outputs fetch and Outputs install phases, below the sync-back) follow. After a failed run the marker is never removed, an accepted `0` included, and the fetch that follows an archive is only of the failed run's test reports and failure outputs (Outputs fetch).

    A `--no-build` run whose outputs were withheld never writes the marker through its manifest, and never clears it or the outputs cap record on a passing end, with `failureOutputs` too. A failing withheld run still brings back its failure outputs, whether or not its send of the PC's build landed.

    A run says at most one of these lines (a passing run then exits 1; a failed run keeps the command's own status, see the box side): `gate: remote outputs not brought back: the worktree changed during the run` (wins when it applies), `gate: remote outputs not brought back: the manifest could not be built: <msg>` (`<path> resolves outside the worktree`, `it is larger than the box takes (64 MiB)`, checked as the manifest is built before the rest is hashed, or the read error), or a third.

    The third is `gate: remote outputs not brought back: the box printed no 'gate-remote-outputs: <n> <archive>' line`.

    A passing run that sent a cross build and heard the box's own stamped standard-error line that the cross build brought nothing back (`gate: remote outputs not brought back: the cross build failed` or `the cross build left a non-Windows file`) ends on the command's status (0) instead, with the outputs left not brought back so the next `--no-build` run builds first. A copy of that line the command printed on its own stream, or the line on a run that sent no cross build, does not count and the run ends 1 with the third line.

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
    - A git failure here ends the run with status 1 and `gate: the remote run failed: ...`, with no fallback, except a failure as a connection fails, which is retried as below.
    - **Retry of the transfers after the command.** The `ls-remote`, the `fetch` and the outputs pull (a failed run's pull included) are each made up to 3 times in all, after pauses of 5 s and then 15 s, saying before each retry `gate: bringing the results back from <host> failed (<reason>); trying again (<n> of 2)`. `<reason>` is the failure's own text without its `gate: ` prefix. The pre-banner retry of each call nests inside each attempt.

      A git call is retried on status 128 only when a line git or ssh said holds one of `Could not read from remote repository`, `the remote end hung up unexpectedly`, `early EOF`, `Connection reset`, `Connection closed`, `Connection timed out`, `Broken pipe`, `kex_exchange_identification` or a pre-banner phrase, in any case, and when the call was cut at its own deadline (the `ls-remote`'s 60 s).

      The outputs ssh call is retried only on 255, even after it has sent bytes. A stall, the transfer ceiling and any other git failure are never retried.

      Every attempt of a call shares its first attempt's 1800 s transfer ceiling; with less than the pause left, the call is not made again and the run ends TRANSFER TIMED OUT as before. The sync-back's recorded time counts its retries and pauses. A Ctrl+C before a retry or in its pause ends the run as an interrupt.
    - **When every attempt fails.** The run's last line is `gate: TRANSFER FAILED: the command exited <n> on <host>; <reason>`. After a command that exited 0 the status is 74 and the journal verdict `transfer-failed`; after one that exited `<n>` other than 0 the status stays `<n>` and the verdict `failed`. A run ended this way is left out of the learned durations.

      The outputs pull's own `gate: remote outputs not brought back: ...` line is then not said, the TRANSFER FAILED line carrying its reason.

      A retried outputs call that finds the archive gone (non-zero, with the box's `gate: remote outputs archive ... does not exist` line) ends TRANSFER FAILED with the first attempt's reason. The pull's ended line sums the bytes received over every attempt. A pair's sync-back retries each call alone, not the whole sync-back, and its TRANSFER FAILED line ends `; the changes are kept: see git diff in each repo at refs/gate-remote/<key>-back` when a repo's back commit was kept.
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

    After a failed run it fetches only when the box packed an archive (see the box side), the PC's manifest was built and a glob of the run's outputs names a `TestResults` folder or the run has failure output globs; a failed sync-back that returned a status does not stop it. A failed run whose box said no outputs line or `gate-remote-outputs: 0`, whose manifest was not built, or with neither kind of glob says nothing about outputs.

    The failed run's fetch is the same pull with `the failed run's test reports` where the lines say outputs (`the failed run's outputs` when its set holds a failure output glob; the lines below keep the first wording): `gate: pulling the failed run's test reports from <builder>: <n> file(s)[, <size>]`, the three progress forms as `gate: pulling the failed run's test reports: ...`, `gate: pulling the failed run's test reports ended after <f> s with <size> received`, the watchdog's `fetching the failed run's test reports` and the hang hint `pulling the failed run's test reports from <builder>`.

    Its failures say `gate: the failed run's test reports not brought back: <reason>` (a fetch past the size cap only says that), and every end keeps the command's own status, except that a watchdog stop or a Ctrl+C during the pull still ends as 124 or an interrupt. It never clears the pending marker and never counts toward or clears the outputs cap record, and the attempt's outputs stay not brought back.

    The pull says `gate: pulling outputs from <builder>: <n> file(s), <size>` before the call (without `, <size>` when the box said no size), and every 30 s from the pull's start (before its first call, a retry of the outputs call included; each period is said once across all the pull's calls, so a retry never says a period an earlier call said) while it runs, `gate: pulling outputs: <done> of <total>, <rate>/s, about <eta> left`, on stderr and in the log.

    `<eta>` is `<m> min` from 60 s, else `<s> s`. With a size and nothing received yet the line is `gate: pulling outputs: <done> of <total>, 0.0 MB/s`; without a size it is `gate: pulling outputs: <done> so far, <rate>/s`. The rate is the mean since the pull's start, and every figure of the start, progress and ended lines is `<x.x> MB` of 1048576 bytes, the success line's own megabyte, with no GB form (a half rounded away from zero).

    A pull that was stopped, cut, failed or interrupted says `gate: pulling outputs ended after <f> s with <size> received` before the other lines of its end (`<f>` to one decimal, counted from the fetch's start to the archive's last byte, or to the call's end when it received none).

    `<size>` counts the bytes this pull received from zero at its start, a retry of the outputs call included; for a watchdog cut it is the figure at the cut, taken in one snapshot with the stall line's seconds and timed from the call's own start, the same bytes the `fetch was stopped by the watchdog after ...` line below gives.

    The client calls `gate box outputs` over ssh with the payload's location word and `Name` `<key>-<nonce>.tar.zst`, built from this PC's own key and nonce, never from the box's line, and copies the call's stdout raw into `<worktree>\.tmp\<key>-<nonce>.tar.zst` (`.tmp` created when missing), marking progress (which feeds the stall watch and the `outputs` interrupt seam) only after each chunk is written into the sink and counted.

    The archive and the pending marker are kept for the install. The call's stderr goes to the log only.

    **Data channel** (`remote/channel.rs`; the box's half is the `outputs` verb's, [The box side](#the-box-side)). A `windows` builder that names `data` (the top-level `host`'s own or an entry's, [remote.json](#remotejson)) has the pull carried on a [data channel](GLOSSARY.md) instead of the call's stdout, with the ssh call as the control.

    The outputs call's payload gains `Nonce`, drawn afresh for each attempt, and `Channel` (`Address` the builder's `data`, `Peer` this PC's own address toward it, found by a UDP connect to `data` that sends no packet, and `BoxProof` and `PcProof`, two 128-bit proofs drawn for the attempt from the same random source as the pipe names, as 32 lower-case hex digits each).

    The payload is on the call's stdin, so no proof is on a command line, in the log, the journal, a note or an `End` text; the proof type has a redacting `Debug` and no `Display`.

    A builder with no `data`, a `macos` builder and a build without the test seams given a loopback `data` take the ssh pull unchanged. A Mac box also refuses a `Channel` it is sent.

    When the box's stderr says its port line `gate-remote-channel: <nonce> <port>` for this call's nonce and the box's size line, the PC dials `data` from its own address on the port, which must lie in 47100-47131 (a loopback `data` in a test build takes any port), within 10 s, in 200 ms slices that only look whether the call has ended.

    It reads the box's proof first and sends nothing unless it matches, then sends its own, reads exactly the size the box's size line gave, and sends the SHA-256 of what it read as its ack. It then shuts the connection down and closes it first.

    The PC keeps no idle timer of its own: each connect, proof and chunk marks the call's progress, so the stall watch covers a silent channel, and the thread closes the archive before the socket, within the call's 2 s drain of its end. A box that gives no size line gets no dial.

    No length or digest is taken from the channel. The pull is whole only when the box's last ssh stderr line `gate: data channel sent <bytes> bytes sha256 <hex>` names the bytes and SHA-256 the PC counted; otherwise nothing is installed from it. A whole pull logs `gate: data channel: pull <bytes> in <s> s` (seconds to one decimal), to the log only. The box removes the archive only after the PC's ack matched.

    The channel decides nothing about the pull's fate. A channel that did not complete (a refused connect, proof or archive, a drop before the ack) is answered by the box's `channel` refusal tag with status 75 over ssh once the control call has exited, and the pull is made again over ssh from byte 0, with `gate: data channel not used: <why>; sent over ssh` in the log only.

    A socket error alone never starts the ssh pull, and after the ack matched nothing does.

    `<why>` is the first that applies: the PC's own failure (for example `data address <addr> is loopback`, `cannot read this PC's address toward <addr>`, `the box's proof does not match`, `the box closed the channel after <got> of <size> bytes`), `the box said no archive size`, the box's own `gate: data channel not completed: ` text, `the channel did not end` (the PC's thread outlasting its 2 s drain), then `the box did not complete it`.

    What the channel moved before the fallback counts into the pull's total in its ended line.

    The test seam `--test-remote-channel-fault <kind>` ([The remote client's run](#routing)) drives each failure: the PC acts on the box's port line after its dial. It needs `--test-remote-transport` and exists only in a `test-seams` build.

    - A fetch the watchdog stops says its `gate: STALLED: ...` or `gate: TRANSFER TIMED OUT after 1800 s fetching the outputs` line (the ceiling counted from the fetch's own start), then `gate: remote outputs not brought back: the fetch was stopped by the watchdog after <size> in <n> s (<size>/s)`, and the run ends 124.

      The size is what the client's raw copy had received when the watchdog cut it, `<n>` the whole seconds since the fetch began (at least 1) and the rate the size over `<n>`, so a slow network (most of the archive arrived) reads apart from a hung builder (nothing did).
    - Any other failed fetch says `gate: remote outputs not brought back: <reason>`: the box's last stderr line (escaped, its first 4 KiB), `the outputs call failed (status <n>)`, `the outputs call sent a line longer than 64 KiB` or `cannot write <path>: <error>`. A passing run becomes 1. The partial archive is removed; a removal that fails says `gate: cannot remove the partial outputs archive <path>: <error>`.
    - A Ctrl+C during the fetch, or noticed by the end of an outputs call however it ended, ends the run as an interrupt with no `stop` call, and removes the partial archive.
    - A fetch that already sent a byte is never retried as a pre-banner drop; one dropped before any byte is retried like the other calls, up to three times. The pull as a whole is made again after a connection failure, bytes sent or not (see the retry of the transfers after the command, under the sync-back).

    **Fetch cap** (GATE-137): the raw relay is bounded by the stall window, which any byte resets, and the transfer ceiling of 1800 s counted from the fetch's own start, and also by a size cap, so a broken or hostile builder cannot make the archive grow without bound. The cap bounds the archive only; what a builder's output may write to the gate's log has a cap of its own, the log cap (see The output cap below), which counts the outputs call's standard error but not the archive.

    The fetch is capped at zstd's worst case for the box's outputs budget, `gate_inputs::archive::sent_limit(outputs_budget)` (`limit + limit/64 + 1 MiB`, saturating, about 50.8 GiB for the 50 GiB budget), the one formula `read_members` in the same module also uses, so every archive the box can legitimately send fits.

    A counting writer around the relay's sink (`remote/client.rs`) writes no byte past the cap and raises a flag, and the watch cuts the call, including one that exits between ticks.

    The partial archive is removed and the gate says `gate: remote outputs not brought back: the archive passed <cap>` (`the archive passed 50.8 GiB` at the real cap); nothing is installed and the outputs-pending marker stays.

    Each hit is counted in the worktree's outputs cap record, `remote\<key>.outputs-capped` in the state root. The first hit in a row keeps the run's status, so a pass stays 0. A second hit in a row fails the run (exit 1) and says, after the cut's line, `gate: the builder's outputs passed the cap again, <n> runs in a row; this run fails`; the record is kept, so every further hit in a row fails too.

    Every other end of an outputs step removes the record: an archive fetched whole, removed before its install, so outputs brought back, an install that refused and a Ctrl+C during the install all clear it; a box that packed nothing (`gate-remote-outputs: 0`); or a fetch that failed another way, a watchdog kill included.

    A run that made no outputs call (refused, fallen back, failed, lost, or a pass the client turned to 1 before the fetch) leaves it, and so does a Ctrl+C during the fetch. A failed run's fetch of its test reports and failure outputs neither counts a hit nor removes the record, whatever its end; past the cap it says `gate: the failed run's test reports not brought back: the archive passed <cap>` and no more.

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

    A PC file whose stamp (size and last write) differs from the manifest's, or that the manifest lacked, is hashed: when its size and SHA-256 equal the member's, the member is already in place and is not a change. It is not moved, not counted in the success line, said by no line, and leaves the result and the marker as a complete install does. A file with other bytes is a change.

    A member the install rewrites after unpacking (`MvcTestingAppManifest.json`, the coverage roots files) is in place whenever its PC file changed during the run or was made during it, by whichever writer (a local build, an IDE, a second gate): these files hold only the roots of the tree that built them, so such a file already records the PC's roots, which is what the rewrite aims at. It is kept as written and treated like any in-place file, at the last check just before the move too.

    A member whose rename fails because something holds the PC's file open (a sharing violation, a lock violation or access denied, and the PC's file cannot be opened for deletion while sharing everything; when it can, the rename is tried once more) is skipped as in use, as the checks skip a file they cannot replace: the in-use line is said, the marker kept, and the install goes on with the rest.

    A failure with no PC file there, or with the PC's file free (the staged file was the one held), is a failure.

    A PC file of that kind deleted during the run (a Clean or a Rebuild) counts as unchanged: the rewritten member is moved into its place. One the PC did not touch is rewritten and moved as before.

    `MvcTestingAppManifest.json` and `.msCoverageSourceRootsMapping_*` are rewritten from the box's path of each tree of the run to that tree's root on the PC (a run of one repo: the work folder to the worktree; a pair: `{Work}\{leaf}` to the worktree and `{Work}\{name}` to that sibling's worktree), the JSON-escaped form first, then the raw one, case-insensitive, the longest path first.

    A tree from a Unix box (its work folder starts with `/`) is rewritten by its own rule: a file whose name ends `.json` gets the JSON-escaped PC root and any other (`.msCoverageSourceRootsMapping_*`) the plain one, only the prefix replaced and the `/`-separated rest kept, so a coverage mapping reads `C:\src\app/tests/...`. A tree from a Windows box is rewritten as above. A `.trx` is not a records-a-root file and is never rewritten.

    A coverage roots file holds lines `{csproj}|{originals;…}={mapped}`, written by Microsoft.CodeCoverage's `MsCoverageReferencedPathMaps` target, one per SourceRoot of a referenced project; the collector reads it at the test session's start, matches each PDB document path against the mapped side and swaps that prefix for the original side when the result exists.

    Each package folder is a SourceRoot too, so the file also records the builder's NuGet packages root, which the install does not rewrite: it changes a report only for a source file that comes from a package, in a deterministic CI build (the SDK's SourceRoot map takes precedence over the routed `PathMap`) or under `ExcludeAssembliesWithoutSources=MissingAny` (GATE-528; the fix is GATE-534).

    A path is matched only at a whole path component: the next character is not an ASCII letter or digit, `.`, `_` or `-` (the characters a folder name the builder lays out is made of), or the text ends, so `{Work}\yaat` never rewrites part of `{Work}\yaat-server` and `{Work}.lock` is left alone. Each box path is computed in one place (`boxside::payload::trees`), shared by the box and the client.

    Each file is decoded by its byte order mark, UTF-8 (with or without one) or UTF-16 in the byte order the mark names, and written back in the same encoding with the same mark and its last write kept; a file with no mark is read as UTF-8 only, and no other encoding is guessed.

    A rewritten file over 16 MiB, or whose body is not text in that encoding (not UTF-8, or UTF-16 of an odd length or with an unpaired surrogate), is skipped, not refused, as below. The client does not skip the back-commit paths again; the box does.

    Each file is then moved with a rename, after a link and junction check on the way, its last write set in the stage before the move to the snapshot time and the PC file checked against the manifest once more just before the rename (a file found holding the member's bytes only then is left in place, as above).

    **The snapshot time.** The last write every moved file takes, whatever the box wrote, is the time the attempt began its snapshot on the PC, clamped to the PC's clock as the moves begin; a sibling pair's roots take the same attempt's time, and a retried attempt takes its own.

    A file left in place, a skipped file and an in-use file keep their own times. A source changed on the PC after the snapshot is then newer than every output the run built from it, so incremental tools (MSBuild, Godot's assembly reload) rebuild; the box's own time could be older than a source restored locally afterwards.

  A member git tracks, one whose target is in use or read-only, or one of the rewritten files above that cannot be rewritten, is skipped, not refused: `gate: remote outputs not fully brought back: <path> is tracked by git`, `... <path> is in use or read-only`, `... <path> cannot be rewritten (it is not UTF-8 or UTF-16 with a byte order mark)` or `... <path> cannot be rewritten (<n> bytes, over 16 MiB)`.

    Each is one line per file before the success line (a member both tracked and in use gets the tracked line only); the other files install and a passing status stays 0.

    A file that cannot be rewritten is not installed, so the PC keeps whatever file it had. A file starting `FF FE 00 00` (UTF-32 little-endian) is not text and is skipped the same way. An install whose only skips are such files is complete: it clears the marker, keeps the hashes of the files it moved in the hash cache and counts as brought back (the worktree's nothing-back record is cleared); only the tracked and in-use skips make an install incomplete.

    The tracked and in-use skips keep the pending marker, so the next routed run sends `OutputsFull`; a run whose only skips are files that cannot be rewritten clears it, since a refetch would skip them again.

    A failed run's test reports and failure outputs are installed the same way, with the same snapshot time, and only the members under the failed run's set (the globs narrowed by `Globs::test_results_only` followed by the failure output globs, `gate_inputs::failure::failed_run_globs`) and each line naming `the failed run's test reports` where it says outputs: `gate: the failed run's test reports: <n> file(s), <MB> MB (<MB> MB compressed) in <s> s (pulled in <s> s, <rate>)`, `gate: the failed run's test reports not brought back: <reason>` and `gate: the failed run's test reports not fully brought back: <path> <why>`.

    No end of it clears the pending marker, never touches the cap record, and the run is never marked as outputs back; the hash cache keeps the hashes of the files it placed. A set holding a failure output glob says `the failed run's outputs` in place of `the failed run's test reports` in each of these lines.

    The success line is `gate: outputs: <n> file(s), <X.X> MB (<Y.Y> MB compressed) in <Z.Z> s (pulled in <F.F> s, <rate>/s)`: `<n>` counts installed files only (a file left in place is not counted), the seconds run from the start of the fetch to the end of the install, `<F.F>` is the fetch alone, from its start to the archive's last byte (to the call's end when it received none), and `<rate>` the compressed size over it in the same MB (`<x.x> MB/s`), and each decimal rounds a half away from zero.

    A fully installed run clears the pending marker.

    - A passing run becomes 1 with the archive and stage removed and nothing installed, and says `gate: remote outputs not brought back: ` then `the outputs archive <name> cannot be opened: <detail>`, `the archive member <path> is not under the output globs`, `<path> changed during the run`, `<path> is not a file` or `cannot rewrite <path>: <error>` (the file could not be read or written).

      The checks before the moves name every member that fails them, not the first: one `gate: remote outputs not brought back: <path> changed during the run` (or `<path> is not a file`, or the read error) line per member in archive order, at most 10, then `gate: remote outputs not brought back: <n> more files changed during the run` when there are more. These refusals keep the marker.

      A refusal that a refetch of the same archive would repeat, because the archive's own members decide it, clears the pending marker, so later routed runs send `Outputs`, not `OutputsFull`.

      Those are a member not under the output globs (which includes a `.lfsconfig` or a `.git*` path) and every refusal the unpack makes of a member for what it is: a link or folder entry, a `.git` form, a short name, a bidi character, a reserved device name, the member and folder count limits, a future last write, a path that is not relative, a member after the list, a long name, an archive with no list or a list that is not a list of paths, sizes and SHA-256s.

      Every other refusal keeps it, since the next fetch may succeed: a fault reading the stream (an I/O error, a hash mismatch, a stream cut short, a bad zstd frame), the archive cannot be opened, `<path> changed during the run`, `<path> is not a file` (the PC has a folder where the output goes), a failed move, a failed or over-cap `git ls-files`, a `--bring-tracked` glob that cannot be compiled, or an interrupt.

      The same holds for the git runner's line when the `git ls-files -z -s` of the tracked check failed or wrote past its 16 MiB cap, and for a `--bring-tracked` glob that cannot be compiled.
    - A move that fails after others were made says `gate: remote outputs not brought back: <n> of <m> files were installed before <path> failed: <detail>` (a file that changed between the checks and its move gives `<path> changed during the run`, unless it is then in place, which leaves it); the files already moved stay and the status is 1. `<m>` counts the files to be moved, and `<n>` the files done, those left in place or skipped as in use at their move included.
    - A Ctrl+C during the moves says `gate: remote outputs not brought back: <n> of <m> files were installed before the interrupt`, then the interrupt ending, with the marker kept and no `stop` call; one before the first move says nothing extra.
    - The archive and the stage are removed on every end; a failure says `gate: cannot remove the outputs stage <path>: <error>` or, after a whole fetch, `gate: cannot remove the fetched outputs archive <path>: <error>` (a cut fetch still says "partial").

    **Tracked members** (GATE-148): the box skips the back commit's paths, but that is no control against a broken or hostile builder, which can send a file git tracks that sits under the globs (a checked-in file under `**/bin/**`), and the install would overwrite it. A member that counts as tracked is therefore not installed: `gate: remote outputs not fully brought back: {path} is tracked by git`, and the run keeps its status, as an output in use does.

    A member counts as tracked when the worktree's git index lists its path, when it is under a submodule (a gitlink, mode 160000, initialised or not: the superproject's index does not list a submodule's files), or when a folder above it inside the worktree holds a `.git` file or folder of its own (a nested repository the outer one does not track). A folder whose `.git` cannot be looked at counts as holding one.

    The index is read with one `git ls-files -z -s` over the whole of it through the client's git runner, intersected with the members in Rust, case-insensitively; there are no pathspecs, since a member list can pass the command-line limit and pathspecs are glob-read. A record of another shape than `<mode> <object> <stage>\t<path>` refuses the install.

    An index path that is not UTF-8 makes every member in its folder count as tracked (every member below the folder, when the part that is not UTF-8 is a folder's name): Git for Windows checks such a name out under a decoded name the install cannot match exactly, and a spurious skip costs a full send while a miss overwrites a tracked file.

    An archive with no members asks git nothing. A listing past the runner's 16 MiB cap, or a failed call, refuses the install with the runner's line; an interrupt is an interrupt.

    A skipped member carries its reason, so a line ends `is tracked by git`, `is in use or read-only` or `cannot be rewritten (…)`, and each counts as not fully brought back; the first two keep the pending marker, the third does not.

    A repo that checks build outputs into git opts in per call with `--bring-tracked <glob>` (repeatable, matched with the output-glob matcher), which installs tracked members matching it as before. The matcher's exclusions hold for it too, so it never brings back a path the output globs never do (`.git*`, `.lfsconfig`, `.tmp/`, `.env` files, and `TestResults/` unless the glob names that folder).

    A glob the matcher cannot compile refuses the install as `the --bring-tracked glob '<glob>' cannot be matched: <detail>`. Nothing in `remote.json` turns the refusal off for a repo.

    A repo whose build regenerates a file it tracks under the globs therefore keeps the pending marker on every routed run, and each next run sends `OutputsFull` without the hash cache, until its caller passes `--bring-tracked`.

    **Never `.git*` or `.lfsconfig`** (GATE-155): an output glob such as `*`, `*.*` or `**` would match a root `.lfsconfig` or a `.gitattributes` at any depth, since the exclusion arms (`EXCLUDED_ARMS`, compiled once as `EXCLUDED`) leave out only the `.git/`, `.tmp/` and `TestResults/` folders and `.env` files (a glob naming a `TestResults` folder is held by `EXCLUDED_BEYOND_TEST_RESULTS`, the arms without `EXCLUDED_TEST_RESULTS`).

    That would give a builder a second way to reach the files a sync-back never writes.

    `Globs::matches` is therefore false for any `.git*` path component and a `.lfsconfig` name, matched as the sync-back matches them (`is_git_s`, `is_lfs_config` in `gate-inputs`'s `names`, which `remote/syncback.rs` imports through `outputs/names.rs`), with the same normalised spellings: `\` as a separator, a cut at the first `:`, trailing dots and spaces stripped, and 8.3 aliases. `Globs::enters` is false for a `.git*` folder, so the box's walk never enters one.

    The box never packs such a file, and the PC's install refuses one that still arrives through the existing `the archive member <path> is not under the output globs` line.

    **Never a name the PC's install refuses** (GATE-166): a builder output can have a name that is not plain (the member rule under **The outputs module** below): an 8.3-shaped `m~10.txt`, a trailing dot, a device name such as `COM1.txt`, a bidirectional mark, or more than 64 parts.

    So can a file named as the archive's own list, `.gate-outputs.json` at the root in any letter case, or a name that is not Unicode. Packing one would fail the pack, lose the whole archive and turn a passing run into 1.

    `Globs::matches` is therefore false for a path that is not plain (`is_plain_path` in `gate-inputs`'s `names`, which `gate_inputs::pack` applies in its pack check, and `gate_inputs::archive` applies to every member it reads) or is the list name.

    `Globs::enters` is false for a folder whose own name `is_plain_part` refuses (the 64-part limit is the file's, not the folder's). The walk passes over a name that is not Unicode. The PC's manifest walk leaves all of these out silently.

    The box's pack skips them and says so once, before its `gate-remote-outputs:` line, and the run keeps its status: `gate: remote outputs: skipped <n> file(s) with names this PC cannot take: <first path>`, with `, …` after the path when n is above 1.

    The line counts the files the pack would have taken but for the name (the back commit's paths and the PC's unchanged copies left out, as for any file), names the first by relative path, `/`-separated, a name that is not Unicode read lossily, cut to at most 1024 bytes at a character boundary and followed by `…` when cut, and escaped as other box lines escape paths. It is not said when the pack fails.

    The walk reads long names, so an alias-shaped name such as `l~10.txt` or `GITATT~1` is a real file and is counted, though the PC never takes it. Only a name spelled as a `.git*` component or a `.lfsconfig` (letter case, `\`, a `:` stream and trailing dots and spaces aside) stays silent, as does an excluded path.

    A folder the walk does not enter because its name is not plain or not Unicode (`bin/x~1/`, `bin/con/`, `bin/report./`) is read through a verbatim `\\?\` path only to count the files below it that the globs select, which are never packed (`Globs::counts_below`). That read enters no excluded or `.git*`-spelled folder and no folder of 64 parts or more. A folder or entry it cannot read leaves those files uncounted and the walk goes on, so the outputs are still brought back.

  A pre-banner failure, which is exit 255 (128 for git) with empty stdout, no start marker heard and a first stderr line starting `Connection timed out during banner exchange`, `kex_exchange_identification: ` or `banner exchange: `, is made again up to three times, each after a 2 s pause and the line `gate: <host> did not answer the ssh banner; trying again (<n> of 3)`, until a call gets past the banner; the call counts as unreachable only when every one of them failed before it.

  This rides out a builder whose ssh server starts no session for half a minute.

  The bounded `stop` and `boot` calls keep a single retry, said as `(1 of 1)`. A run call whose box said the start marker is never retried or read as a pre-banner failure, whatever its first stderr line: the command may have started.

  Two pre-banner failures of the run call fall back `unreachable`; the sync-back's `ls-remote` and `fetch` are retried the same way, up to three times (four attempts in all), and a call that fails before the banner on all four ends the run with status 1. A connection failure of either that gets past the banner is retried as a transfer after the command (under the sync-back).

  **The watchdog** (`remote/watchdog.rs`, `remote/client.rs`) watches the preflight, the push, the run call, the sync-back's fetch and the outputs fetch by the run's stall window and two limits (the run call's backstop and the transfer ceiling) on three clocks.

  The setup (the preflight and the push) is cut at the transfer ceiling, 1800 s whatever `--timeout` says, counted from the attempt's start.

  The ceiling itself is counted by the builder ([The box side](#the-box-side), `run`), in load-adjusted time from the command's start, so a slow push does not use the command's time. The run call is cut only at its backstop, 5 x `--timeout` + 60 s of wall time counted from the run call's own start, for a box that cannot end its run. Each fetch after the run call, the sync-back's and the outputs', is cut at the transfer ceiling counted from its own start.

  A `--stall` under 15 s is raised to 15 for a remote run (the box's heartbeats come every 5 s) and says `gate: --stall <n> raised to 15 for a remote run (heartbeats come every 5 s)`. It looks every quarter of the window, at most once a second.

  A run call killed by the client says `gate: STALLED: no output or remote CPU for <n> s` or `gate: BACKSTOP on <host>: no end after <w> s of wall time (5 x the <n> s ceiling + 60 s, the raised ceiling for a cold run); killed the run call and stopped the command`, stops the command on the builder and ends the run with status 124, with no fallback.

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

  **The log cap.** One call may write at most 256 MiB of the builder's output to the gate's log, its standard output and standard error counted together, whole lines and the run call's pieces alike. The bytes counted are the bytes written, the stamp and tag of each line among them; the line the cap writes in its place is stamped `gat`.

  The piece that would pass the cap is not written: the log gets one line in its place, `gate: the builder's output passed 256 MiB; the rest is not logged`, said once by whichever of the call's two readers passed it first, on a line of its own even when it cuts into a long line passed on in pieces, and nothing more of that call's output.

  A reader's charge and its write are not one step, so a piece the other reader was cleared to write just before can land after the cut line: at most one piece per stream, and the log still holds no more than the cap of the call's output. The console keeps echoing a run call's output past the cap, so the end of the run is still seen live, and the run keeps its status.

  Each call counts afresh, so a flooded preflight still leaves the run call's output logged. The client's own lines, the box's protocol lines and heartbeats (never logged) and the outputs archive (a raw copy, capped by the fetch cap) are not counted. The test seam `--test-remote-log-cap-bytes` lowers the cap, and the line names the lowered size as a whole number of its largest binary unit when it is one (`64 KiB` for 65536).

  **Escaping.** Every text a builder supplies that a `gate:` line quotes is escaped: control characters, U+061C, U+200E, U+200F, U+202A-U+202E, U+2066-U+2069, U+2028 and U+2029 are written `\u{..}`. That covers the preflight's last line, the builder's version in the version refusal, and every `git ... failed` line of the snapshot, the saved ref and the sync-back. A failed git line lists git's stdout first and its stderr last, keeping the last 10 lines and 4 KiB so git's `fatal:` line survives.

  The client's git calls also drop `GIT_CONFIG_COUNT`, `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM` and every `GIT_CONFIG_KEY_n` and `GIT_CONFIG_VALUE_n`, so the caller's git configuration never reaches the builder's repo, and `GIT_SSH_VARIANT` and `GIT_EXTERNAL_DIFF`, so neither the ssh option syntax nor the diff a sync-back applies can be changed from outside.
- **Sibling worktrees** (GATE-19, GATE-213), the declaration, the machine's consent, the decision, the rewrite, the box's layout, the client's send, the PC's sync-back and the outputs install and stale lines built: some repos' builds reach into a sibling repo by relative path, and a routed run that sends one worktree's snapshot would compile something different on the builder without any error.

  A repo declares its siblings in a tracked file of its own, `.gate/siblings.toml`, beside the [toolchain manifest](#routing) and apart from it, so a fault in one never changes what the other means.

  A declaration alone carries nothing: the machine consents per repo in `remote.json`'s `siblings`, `{"siblings": {"<repo>": ["yaat-server"]}}` ([remote.json](#remotejson)), so a tracked file cannot make this PC send a folder it was never told it may.

  The file has one key, `siblings`, an array of folder names, each a sibling worktree at `../<name>` of the worktree root: `siblings = ["yaat-server"]`. `siblings = []` declares none; a file without the key, or with any other key, is a problem. Only the routed repo's own file is read: a sibling's `.gate/siblings.toml` is never followed, so a pair is one level deep.

  A name is one or more ASCII letters, digits, `.`, `_` and `-`, not starting or ending with a dot, not a reserved device name (`CON`, `PRN`, `AUX`, `NUL`, `COM1` to `COM9`, `LPT1` to `LPT9`, whatever follows the first dot), not repeated in another letter case, and not the root's own folder name; at most 8 names. The same rule decides whether a root's own folder name can be laid out. A leading byte order mark is skipped.

  Unlike the toolchain manifest, any problem keeps a matched run here: a sibling left out would build something different with no error. [`gate check`](#gate-check) lists every problem of the file and each declared sibling's consent and folder beside the worktree root.

  The reader's problems are `<key> is not a key (siblings)`, `declares no siblings`, `siblings is not an array (TOML <type>)`, `siblings[<i>] is not a string (TOML <type>)`, `siblings[<i>] is empty`.

  Name faults: `siblings[<i>] is not a folder name of letters, digits, '.', '_' and '-' not starting or ending with a dot, not a device name: "<name>"`, `siblings[<i>] repeats an earlier name: "<name>"`, and `siblings names more than 8 folders` (at the ninth name, reading no further), `line <l>, column <c>: <toml message>` and `not valid UTF-8 (...)`.

  The file is read from disk as it stands, never from a snapshot, and only once a routed command matched. `.gate` is opened as itself and held, and `siblings.toml` is opened by its name inside that folder object (`gate_win32::open_beneath`, an `NtCreateFile` relative to the held handle), never following a link, so nothing swapped in for `.gate` after the open is read. Both are opened sharing reading, writing and deletion, so a checkout or an editor's save is never blocked.

  A `.gate` that is a link or a junction says `.gate is a link or a junction`, and one that cannot be opened `.gate: cannot read it: <error>`; a `.gate` that is a file, or no `.gate`, holds no siblings.

  A `siblings.toml` that is a link, a junction or a folder says `is a link, a junction or not a file`, one over 64 KiB `is larger than 64 KiB`, and one that cannot be read `cannot read it: <error>`, each after `gate: not routed: .gate/siblings.toml: `.

  A file that is another kind of reparse point (a deduplicated one) is read through its filter from a handle opened again (`gate_win32::reopen_through_filter`), which must be the same file (`gate_win32::same_file`), else it is the not-a-file problem.

  A repo that cannot commit the file may have its copy in the repo's declarations folder that `remote.json` names ([remote.json](#remotejson), Declarations folder). It is read the same guarded way, and only when the worktree has no `.gate/siblings.toml`: a worktree file that exists wins even with problems, and a file is taken whole from one place. A `.gate` that is a file holds no siblings and reads nothing from the folder. The consent in `remote.json`'s `siblings` is asked all the same.

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

  Parity is checked for each repository in order: a sibling's SDK need is read from its own `global.json` and its lines name it, and the tools, with the Godot pin, are the main repository's manifest and project files alone. A builder that lacks anything says one `gate: remote toolchain mismatch: <repo> needs ...` line per need and falls back `parity` once.

  `remote/held.rs` holds a pair's folders from the check until the client lets them go: the root's parent is opened once, following links, on a local volume (a network volume is refused), and the root and each sibling are opened beneath it as themselves, never through a link, each with its `.git` entry (a folder or a linked worktree's file, no reparse point of any kind). Each is checked against the exact path the route was decided for and, the root and a sibling, its folder id.

  Every handle shares reading and writing but not deletion, so none of these can be renamed, removed or swapped for a link while held. Once git has said where each repository's git folder and common folder are, those are held too, so a linked worktree's git folder, which lies outside the pair's parent, is covered; one reached through a link or a junction is refused. A recheck reads every path again and opens each as git does, to see that it still leads to the held folder.

  Two repositories whose common git folders have one folder id share a repository and keep the run here. The refusals are the `gate: not routed: <who> <why>` lines of [Output lines](#output-lines), the main repository said by the root's folder name.
- **Inputs** (GATE-22): a routed command can need ignored files only the PC has, and the snapshot sends only tracked and untracked-not-ignored files. A repo's optional `inputs` list of ignored-file globs, beside `outputs` in the routing file, names the ignored files a routed run sends to the builder's work folder before the run, compressed in the outputs archive's format and hash-diffed against what the work folder holds, so an unchanged input costs nothing.

  The choices behind it:

  - Secrets do not go to a builder, with no opt-in: a `.env` file never travels, so a repo whose command needs a `.env` on the builder cannot route that command, and the run says nothing is wrong and fails on the builder; the repo keeps that command local.
  - There are two calls (a diff, then a put of what differs) and no PC record of what the builder holds. The rejected shape folds the inputs into the run call and trusts such a record, and a stale record turns into refusals.

    The cost is one more ssh setup (about 2 s) per routed run of a repo that declares inputs, even when nothing changed; a repo with no `inputs` list pays nothing. Both calls come after the push and before the run call, so a refused route uploads nothing and the run's clocks do not count the upload.
  - A failed step runs the command here, with its own fallback reason, so the journal shows a builder that fails the step for every run of the repo. A repo whose inputs pass the 4 GiB cap never routes those commands, and the line names the cap.
  - The box writes inputs with the same token and owner as the checkout and rewrites no path, so whatever a command can read of the checkout it can read of its inputs (a desktop run's included).
  - A repo name in `inputs` that matches no repo sends nothing, and `gate check` says nothing about inputs.

  Built: the `inputs` key and the route's `RemoteRun.inputs`, the box's `inputs` verb with the run's `InputsDigest` check ([The box side](#the-box-side)), and the client's part (the manifest, the diff and put calls, the hash cache, the lines and the `inputs` fallback reason), described under Inputs in [The remote client's run](#routing).

  Inputs have their own exclusion set, not the outputs': `.tmp/` is reachable, but a client never sends a path with a `.git` component, `.lfsconfig`, `.env`, `*.env`, `.env.<name>` or `.envrc` (at any depth, matched without regard to case), the gate's own archives and stages, or the run's log and `.err` file. The box refuses a manifest that names a `.git*` name, `.lfsconfig` or a `.env` file, or a path that is not plain and relative.
- **Git LFS** (GATE-469): a repo that keeps assets in Git LFS routes with them. The PC sends the LFS objects the snapshot's pointers name to a protected store beside the repo's bare repository on the builder, and the box's checkout, whose LFS filter is live, writes their content into the work folder with no network access. It reuses the inputs verb's plumbing (diff, then put, the work folder's lock, a protected stage, the watch and the fallback).

  Built: the PC's listing and calls, the box's `lfs` verb, the run's `LfsDigest` check, the checkout's pins and post-check, the back commit's pins and the `lfs` fallback reason, described under Git LFS in [The remote client's run](#routing) and under `lfs` and the LFS checkout in [The box side](#the-box-side).

  A repo with no tracked `.gitattributes` naming `filter=lfs` is unaffected: it sends nothing, makes no scratch folder and needs no git-lfs on the builder.

  What the design rests on in git-lfs, read in its source:
  - A smudge trusts a local object by its size alone and never hashes it, and deletes one of the wrong size.
  - Before reading, a smudge copies or links `<alternate>\..\lfs\objects\<oid>` when present. A work folder's alternates name the repo's `<Bare>\objects`, so `<Bare>\lfs\objects` is already a read path.
  - A clean keeps an existing object of the same size.
  - A tracked `.lfsconfig` may set `lfs.url`, `remote.*.lfsurl`, `lfs.fetchinclude`, `lfs.fetchexclude` and `lfs.skipdownloaderrors` but never `lfs.storage`, and git config overrides it.
  - With `fetchexclude`, `skipdownloaderrors` or `GIT_LFS_SKIP_SMUDGE` a smudge writes the pointer and reports success.

  The store's trust is therefore its writers: nothing re-hashes an object at checkout, so the oid name, a verified install and no lower-integrity writer are the record.

  **One store per repo, not per builder.** The store is `<Bare>\lfs\objects\<aa>\<bb>\<oid>`, beside the bare repository every work folder of the repo shares, so a new worktree resends nothing and git-lfs reads it through the alternates with no config.

  A store shared across repos was rejected: an unelevated box or a command on the box's own token could corrupt every repo's assets, a prune would need every repo's refs and locks, and the diff would tell any PC which other repos' content the builder holds. No prune is built, so a store grows (GATE-471).

  The invariants and where each holds:
  - **Writers.** Only a put's install writes the store.

    `<Bare>\lfs`, its `objects` and every folder under it are made with the box's protected security and adopted only when they hold it (an `lfs` folder with the shape git-lfs's plain `CreateDirectory` leaves is moved aside and made new instead, never walked; any other folder refuses), an object is installed by a rename that creates it new and never replaces one, and an unelevated box, whose protected folders are labelled Medium, refuses every `lfs` call and any run carrying an `LfsDigest`, so no Medium-integrity process can write it.

    A store lock, `<Bare>\lfs.lock`, made and adopted the same way, is taken shared by every put, diff and run, so a prune that opens it alone waits for them all.
  - **Content.** Each object is hashed on the PC as it is packed and again on the box as the put unpacks it: a member whose SHA-256 is not its name, or whose size is not the manifest's, refuses the whole archive. A run is checked out only against objects that are plain files of one name and the pointer's size, held open without delete sharing from the cover check through the back commit.
  - **Readers.** The checkout reads the store only through git-lfs's alternates path, never with a Medium writer in reach. A run's git-lfs writes only into the run's scratch folder.
  - **No download, no silent pointer.** Every box git call that may run the LFS filter is pinned through `-c`, never written into `.git\config`, where the command's own git would read it:

    `lfs.url=http://127.0.0.1:0/` (a URL that needs no name lookup, is never proxied and nothing can listen on), `lfs.remote.searchall=false`, empty `lfs.fetchinclude` and `lfs.fetchexclude`, `lfs.skipdownloaderrors=false`, `lfs.standalonetransferagent=`, git-lfs's own `filter.lfs.process`, `smudge` and `clean`, and `filter.lfs.required=true`, with `GIT_LFS_SKIP_SMUDGE` and `GIT_LFS_SKIP_DOWNLOAD_ERRORS` stripped from the box's git environment (`GIT_REDIRECTS`).

    The tree's attributes are read as the builder's checkout does, with `-c core.attributesFile=` and `GIT_ATTR_NOSYSTEM=1` on the PC's listing and the box's git calls.
  - **The command.** A run with an `LfsDigest` never falls back to the box's own token when the restricted start fails; it fails the start, as the pnpm and bun installs do.

  **The scratch folder.** The box alone decides whether a run has one, per repo, after that repo's fetch and before its checkout: when the run carries an `LfsDigest`, or `git grep -q -F -e filter=lfs <snapshot> -- ':(glob,icase)**/.gitattributes'` exits 0.

  It is `{root}\lfs-run\{key}-{nonce}-{16 hex}` (`lfs-run-medium` on an unelevated box), made new with the protected security of a put's stage and removed when the run ends, and it is git-lfs's `lfs.storage` for the run's checkouts and back commit, so no smudge or clean of the run touches the work folder's `.git\lfs` or the store: git-lfs links the store's objects into it through the alternates, and a plain file committed under the filter checks out as its own bytes.

  The 16 hex digits are random, so a run that reuses a killed box's nonce makes its own folder and the old one waits for the 24 h stage prune. A scratch folder that cannot be made, or an `lfs-run` another account made, refuses the run `lfs`.

  A run without a scratch folder checks out and back-commits with `lfs.storage=<lock>\lfs`, a path git-lfs cannot make, so any file that reaches git-lfs fails closed: its snapshot does not declare the filter in a tracked `.gitattributes`, and a case variant, an untracked `.gitattributes` or the repo's `info/attributes` is what made the filter run.

  Known limits are booked: the work folder's and the builder's global git config and `info/attributes` (GATE-470), an untracked `.gitattributes` on the PC (GATE-517), the prune's noise for a held stage (GATE-518), naming `info/attributes` in the refusal (GATE-519), GATE-524 and GATE-525.

  A sibling pair holding pointers is refused `lfs` before any call. A changed LFS path in a back commit is refused at sync-back as before, and a back commit adds nothing to the store.
- **The per-host failure record** (GATE-27), planned, not built: a record in the state root holds a builder's failure reason and an expiry. A routed gate that finds an unexpired record says `gate: not routed: <host> <reason> <n> s ago`, runs here and journals `fallback` with the reason, rather than paying the snapshot, preflight and push again for the same reason.

  The windows per reason: `unreachable` a short one (60 s, distcc's default backoff); `parity` and `tools` until the snapshot's `global.json` or tool manifests change; `busy` and `full` are load, not faults, and are never recorded.

  The record also counts box-side losses (exit 255, `rebooted`, `unreachable` after the start, a `stop` that leaves processes behind) apart from command failures, which never count; past a threshold nothing routes to that builder until a preflight succeeds.
- **The host** is the one `gate.exe` validated against the allowlist (the routing file's text, above). `gate-router` carries it in the routed run as a `Host` newtype that only the routing file's reader builds, so no host string the reader did not check reaches a process. The Rust remote client starts ssh with each argument separate and `--` before the destination, and keeps the allowlist too, since a git `host:path` URL cannot carry `--`.

## The box side

A builder runs the same `gate.exe`, under the hidden `gate box` subcommand, from the admin-owned `C:\Program Files\gate\bin`. The exe is pushed to every builder the local `remote.json` names, over ssh, by `gate push` ([`gate push`](#gate-push)), not by the local install script.

A routed run first learns the builder's gate version; when its whole version text, build id included, differs from its own, the run falls back to local, saying `gate: not routed: <host> runs gate <version>, this PC <version>; run gate push`, and journals `fallback` with reason `version`. A builder running another build of the same package version falls back too.

Builders are configured only in the local `%LOCALAPPDATA%\gate\remote.json`. The repo, its docs and its tests name no builder; the suite's remote cases run against a stub.

The box keeps the PowerShell wrapper's lines for the verbs and the refusals, and adds tagged machine lines the wrapper never emits: the start marker, the refusal tags, the kill tag and the adjusted line (Run call, in [Routing](#routing)). `gate box` is what the Rust remote client calls on a builder, so it runs whenever a routed run goes through the remote client.

**Install tripwire.** The box runs elevated, so a gate any other process of its user could replace is not trusted. Before any verb runs or stdin is read, `gate box` judges its own image with `gate_win32::check_installed_file` (the file, its folder and every folder above it are owned and changeable only by SYSTEM, Administrators and TrustedInstaller).

One that fails says `gate: the builder's gate install is not trusted: <why>` on stderr (the why escaped, one line) and exits 1, whatever the verb. The `preflight` says `gate: remote version <v>` on stdout first. A PC client that knows the tripwire reads the install refusal before the version and falls back; one built before it reads only the status, so it reports the preparation failed (status 1) and its run fails rather than falling back.

The preflight also judges the desktop helper beside the box the same way, and refuses with the same two lines and status 1 when the helper is there and not trusted; a builder with no helper file passes it.

A desktop run's hand-off judges the helper again before it makes the control pipe, and refuses with the run's `install` refusal tag bound to its nonce, then the install line, exit 75; a helper file that is not found stays a `desktop` refusal. The `register-desktop` verb judges the helper before it registers the task, so an untrusted helper is its `failed: <why>`.

The client turns the refusal into a fallback to a local run with reason `install` (Preflight, in [Routing](#routing)), and `gate check --builder` reports it. Routed code is trusted, and a refused install falls back to local. The user's own Medium-integrity processes are outside the trust boundary ([The desktop run's design](#the-desktop-runs-design)).

**Work root.** The box runs elevated and builds in the root `<root>` of its work folder `<root>\work\<key>`, so a root another account may write in could redirect what it runs or deletes. The `preflight` judges and holds the root with `gate_win32::check_work_root` after its payload is read and before any record is read, git step or folder, and refuses it before anything is made or run.

The check refuses: a root DACL with any allow entry, for a SID other than SYSTEM, Administrators and the box's user, that grants a write-class right (CREATOR OWNER and OWNER RIGHTS entries are judged by what they grant, and the owner by the owner rules); a folder above the root up to the volume root that lets another account delete or replace what is below it (DELETE on the folder, FILE_DELETE_CHILD on its parent).

It also refuses a root or root parent that is a reparse point, a root whose final path is not its own path (a `subst` drive, a junction above it), and a root on a network volume. The held root's final path is read again after the walk, so a folder swapped above it meanwhile is refused too.

It also refuses a reparse point at the root or its parent; a final path other than the expected one (a `subst` drive); and a network volume.

A missing root is made, locked, only after its parent and the folders above it pass, and created beneath the held parent; a root this check made and then refuses is deleted through its handle. The root is then held (`FILE_GENERIC_READ`, no delete sharing) until the preflight has readied its folders, so no other account can rename or delete it meanwhile.

A refusal says `gate: remote version <v>` on stdout, then one stderr line and exit 1: `gate: the builder's work root is not locked down: <why>; run the installer's "Gate work root" step (install-toolchains.ps1 -Only "Gate work root")`, the why escaped, with `; it was made by this check and cannot be removed: <error>` added to the why when a root it made and refused cannot be removed.

A root that is otherwise safe but that another process holds open with delete access, or without sharing reading, says `gate: the builder's work root is busy: <root> is open in another process; retry when it is closed` the same way (exit 1, no installer step). On a builder's very first preflight, a concurrent preflight may see the just-made root as busy once; it passes on retry.

A `Work` that is not `<root>\work\<key>` is a usage error, `gate: box payload Work must be <root>\work\<key> to lock its root down: <work>`, and so is a single-repo `Bare` that is not `<root>\<name>.git`, `gate: box payload Bare must be <root>\<name>.git to lock its root down: <bare>`, each exit 2 with no version line. The client turns either line into a fallback to a local run with reason `root` (Preflight, in [Routing](#routing)), and `gate check --builder` reports it.

Each verb is one ssh call today, and the call's cost is its connection, not its work. Measured warm, setup takes about 2.1 s (TCP about 0.2 s, key exchange and auth 1.5 s, the session 0.4 s) against 0.2 to 1.0 s of verb work, so a routed run's three verb calls spend about 6.4 s, 30 to 37% of a 17 to 26 s routed build, before its git transfers pay the same again.

A routed run therefore moves to one long-lived connection carrying every verb, `gate box serve`, and the per-call client stays until that lands (GATE-70 holds the measurement, and GATE-109 the work).

The payload is one base64 word (standard alphabet, padded) of UTF-8 JSON, sent as the first line of the call's stdin (never on a command line), holding the wrapper's keys: `Key`, `Bare`, `Work`, `Lock`, `Index`, `OutputFolder` and `Job`.

A `run` payload also holds `Command` (or, for a warm-up, `WarmCommands` and `WarmHash` in its place), `Subfolder`, `Identity`, the optional `Env`, `Outputs`, the optional `FailureOutputs` (a list of globs read like `Outputs`; an older box ignores it), `Nonce`, `OutputsFull`, `Timeout`, `Host` and the optional `InputsDigest`, `SentDigest`, `LfsDigest`, `GodotVersion`, `EngineEnv` and `CrossBuild` (a macOS builder's key: an object of `Rid` and `Words`, with `Outputs`; [The Unix box](#the-unix-box)).

An `outputs` payload also holds `Name`.

`inflight` reads only `Root`.

`drain` reads `Root` and `Action` (`raise`, `status` or `lift`).

The optional `Session` (1 to 64 ASCII letters, digits, `_` and `-`: the run's drain session id; absent or null for a PC older than the drain) and `NoTicket` (true or false, false when absent: a refused call takes no place in the drain's line) are parsed by `gate_wire::Payload` for the drain's admission, which `preflight`, `run`, `inputs` and `lfs` read.

The optional `Pc` and `Repo` (texts, null or absent for an older PC; else `gate: box payload Pc is not a text` or `gate: box payload Repo is not a text` and exit 2) are what the admitted session's record keeps.

`inputs` and `lfs` read `Key`, `Bare`, `Work`, `Lock`, `Nonce` and `Step`, and `inputs` the optional `SentBin`.

A warm-up's `run` payload sets `WarmCommands` (1 to 8 commands, each a non-empty list of non-empty texts) and `WarmHash` (64 lower-case hex digits, the hash of the warm-up file the commands come from), sets no `Command`, and asks for no outputs. The PC's client sends it (The remote client's run, Warm-up call) for `gate warm`. It may carry `GodotVersion` too, and its commands resolve Godot as a run's do.

`preflight` also reads an optional `Needs`, an object of tool name to minimum version text (`""` for any version; the box ignores the text, and for `godot` the PC sends the manifest's own pin when the manifest names one, else the project files' distinct pins, joined by `,`), which the PC builds with `remote::payload::preflight_word` from the tools it wants probed; it is omitted when there are none, so a payload without needs is the same bytes as before.

Unknown keys are ignored, and `Outputs`, `OutputsFull` and `OutputFolder`, which the outputs transport owns (GATE-44), are read only by `run` (the outputs) and `outputs` (`Name`, `OutputFolder`); `preflight`, `stop` and `boot` ignore them.

`Timeout` is the run's ceiling in whole seconds, 1 to 2147483647, which the box counts in load-adjusted time; absent or null means no box ceiling and no adjusted line, and anything else is refused before the lock with `gate: box payload Timeout is not a whole number of seconds` and exit 2. `Host` is the PC's `remote.json` name for the builder, which the box's kill lines name, control characters escaped; absent leaves it out of them.

A `run` payload may also hold `Desktop` (true or false, false when absent or null, else `gate: box payload Desktop is not true or false` and exit 2) and `DesktopWait` (whole seconds, 1 to 2147483647, else `gate: box payload DesktopWait is not a whole number of seconds` and exit 2), which together ask for a desktop run, the command on the builder's logged-on desktop (The desktop run, below).

A warm-up payload with `Desktop` true is refused with `gate: box payload asks a warm-up for the desktop` and exit 2.

For `run` the payload also needs a `Snapshot` key: exactly 40 lowercase hex characters, the commit the run checks out. Anything else is refused before the lock and the checkout with `gate: remote run payload's Snapshot '<value>' is not a 40-hex commit sha` and exit 2.

The value is empty for a missing key or JSON null, the string as given with control characters escaped, and compact JSON for any other non-string. Two differences from the wrapper are deliberate: the echoed value has its control characters escaped, and the check is stricter than the wrapper's .NET `$`, which accepts a trailing newline.

Every path comes from the payload and nothing is written under the gate's state root; `gate box` takes no `--state-dir`. Test seams are `test-seams`-gated flags, never environment variables (but the three below, and the two of the Unix box's prune, [The Unix box](#the-unix-box)), and sit before the verb (`gate box --test-... run --stdin`) in a `BoxSeams` struct on `BoxRequest`, apart from the `gate run` seams, so `--test-print-request` is unchanged.

The box seams go in any order before the verb, each also as `--<seam>=<value>`. `--test-heartbeat-ms <n>` sets the period of the command heartbeat, and `--test-box-console fail-before` makes the command's UTF-8 console fail before the command starts (the only accepted value, else `gate: --test-box-console must be fail-before, got '<value>'`).

`--test-box-token refused` makes the start with the command's restricted token fail as refused, so the box falls back to its own token (the only accepted value, else `gate: --test-box-token must be refused, got '<value>'`).

`--test-box-park-launch <path>` parks the launch after the donor is in the job and before the output lock, writes the empty file `<path>.parked`, and goes on once `<path>` exists (at most 600 s).

The box's ceiling has three seams: `--test-box-sample-ms <n>` replaces the 5 s sample period, in milliseconds, `--test-box-sampler-fail true|false` makes every sample fail (`true`), so only the backstop watches and no adjusted line is said, and `--test-box-free-percent <1-100>` makes the samples find that share of the machine free, so the load-adjusted clock runs at that share of wall time.

`--test-box-kill-leaves <n>` stands in for the number of processes the last ending of a ceiling or backstop kill leaves (a whole number above 0), so the box says its note for a job that is still not ended.

`--test-box-git-limit <step>:<ms>` replaces the bound of the one git step it names (as the box's lines name it, `checkout`, `git init --bare`, `push lib`) with `ms` milliseconds, every other step keeping its own; a value that is not `<step>:<ms>` with `ms` a whole number above 0 is `gate: --test-box-git-limit must be <step>:<ms>, got '<value>'`.

`--test-box-git-armed-by <path>` makes the bound `--test-box-git-limit` gives its step start only once `<path>` exists, checked at each look of the step's wait: until then that step has no bound, and the steps the limit does not name keep their class bound from their start. Without this flag the limit's bound starts with its step; given without `--test-box-git-limit` it is `gate: --test-box-git-armed-by needs --test-box-git-limit`.

`--test-lfs-budget-bytes <n>` replaces the 16 GiB a `gate box lfs` put unpacks, in bytes.

`--test-box-root-walk-stop <folder>`, which the keyed `gate box prune` also honours, ends the preflight's walk over the folders above the work root at that folder, which and those above it go unjudged (any path; an empty value is no value), so a case does not depend on the host's folders.

The other six box seams are `--test-outputs-budget-bytes <n>`, which replaces the 50 GiB outputs budget of the box's trim, in bytes, `--test-box-failure-cap-bytes <n>`, which sets the per-file cap on a failed run's failure outputs to `n` bytes and the total to `4n` (the Windows box's alone; the Unix box always uses 256 MiB and 1 GiB), `--test-inputs-budget-bytes <n>`, which replaces the 4 GiB a `gate box inputs` put unpacks, decompressed, in bytes, `--test-dotnet-limit-ms <n>`, which replaces the 30 s bound of the preflight's `dotnet --list-sdks` read, in milliseconds, `--test-probe-limit-ms <n>`, which replaces the 10 s shared bound of the preflight's tool probes, in milliseconds, and `--test-probe-drain-ms <n>`, which replaces the 2 s the output readers of each probe and of the preflight's `dotnet --list-sdks` get to finish after the program exits, in milliseconds.

Three seams of the MSYS keeper and the command's `PATH` are environment variables, read only in a `test-seams` build (the installed binary ignores them), because a stand-in is a program plus words or a folder the box reads in several places.

`GATE_TEST_BOX_MSYS_KEEPER` is the program and its words, joined with `|`, started in the keeper's place (the lookup and the owner check are skipped; the token, job, console and echo stay), and `GATE_TEST_BOX_MSYS_KEEPER_MS` is the echo limit in milliseconds in place of 30 s (a value that is no whole number is the keeper's why, `GATE_TEST_BOX_MSYS_KEEPER_MS is not a count of milliseconds: <value>`). The suite's `fake-tool` is the stand-in.

`GATE_TEST_BOX_GIT_BIN` is the folder used in place of the trusted Git `bin` at the front of the command's `PATH` (the lookup and the trust check are skipped; a folder inside the run's own folders is still refused), since the suite's boxes run from a folder this user owns.

  `GATE_TEST_BOX_TRUST=check` makes a `test-seams` build of `gate box` judge its install (Install tripwire, in [The box side](#the-box-side)); without it such a build does not, since the suite's boxes run from `target\`, a folder this user owns. A build without the feature always judges it. The desktop seam's stand-in helper is not judged.

A seam is refused as the `gate run` seams are: no value, given twice, and a value outside its rule (`gate: --test-outputs-budget-bytes must be a whole number above 0, got '<v>'`, `... must be at most 2147483647 ...`, `... needs a value`, `... is given twice`).

The desktop run has five more: `--test-box-desktop-session active|none|locked|other-user|disconnected` makes the box read the console session as that state (the box's own session, with the box's account at it, or nobody, another user, a locked or a disconnected one), `--test-box-desktop <mode>` starts the stand-in helper (`fake-desktop-helper.exe`, beside the box's program) in place of the registered task, at below normal priority in a kill-on-close job, with the helper's own seams set and every other variable of the box's environment inherited (the three seams the box sets itself are never inherited): `ok`, or a fault of the helper (`exit-before-hello`, `hang`, `wrong-version`, `lie-handle`, `exit-after-create`), or a task mode (`task-not-registered`, `task-set-up-wrong`, `task-failed`, `task-no-pid`) that stands for that refusal of the task without starting anything.

`--test-box-desktop`, given without a session seam, also stands for an `active` session. A desktop run uses no console donor and ignores the console and token seams.

`--test-box-desktop-mutex <unique>` names the desktop lock by a unique part under the box's own user in place of `gate\desktop`, and `--test-box-desktop-ms <n>` sets the desktop run's connect, message and end limits to `n` milliseconds, raising or lowering each, and lowers one turn of the wait for the desktop lock to at most `n`.

`--test-box-desktop-message-ms <n>` sets only the message limit, over `--test-box-desktop-ms` in either order, leaving the connect and end limits to it or to their built values (refused as `gate: --test-box-desktop-message-ms must be a whole number above 0, got '<v>'` and `... is given twice`).

`--test-box-register ok|fail` is the seam of the `register-desktop` verb: the registration succeeds, or fails as the seam refusing, in place of the Task Scheduler's work (the verb takes no `--stdin`, so the seam goes before it).

The rules: `ok` or `fail` for `--test-box-register`, a whole number above 0 and at most 2147483647 for the heartbeat, the sample period, the outputs and inputs budgets, the dotnet limit, the probe limit and the kill's leftover count, 1 to 100 for `--test-box-free-percent`, `true` or `false` for `--test-box-sampler-fail`. Without the feature each word gets the box usage line.

Every `gate box` verb first limits the box's DLL search to System32 (`gate_win32::set_default_dll_directories_system32`), before it reads the request or loads a library by name; a failure says `gate: the box cannot limit its DLL search to System32: <error>` and exits 1.

The box gives each run's command its own build servers: the command's environment is the box's own with `SharedCompilationId` and `MSBUILDNODEHANDSHAKESALT` set to `gate-box-<Nonce>` (the payload's `Nonce`), `MSBUILDUSESERVER=0`, `DOTNET_CLI_USE_MSBUILD_SERVER=0`, `UseSharedCompilation=false` and `MSBUILDDISABLENODEREUSE=1`, each replacing any inherited value (names compared case-insensitively).

So no other run's command, on the box or on the PC, uses the compiler server or the MSBuild nodes this one starts, and no node idles in the job after its build (an idle node would keep alive anything in the command that waits on its whole tree).

The box's own git calls, tool restore and package install do not get these settings.

The servers a run starts live in its job and are ended at its end, so every routed build starts its own nodes and compiler server; none is kept warm between runs.

`git` and `dotnet` are found with the gate's own program resolution, which never searches the current folder.

The box reads the request, before the verb and before any lock, as the first line of stdin, ended by its newline or the end of input (one newline, and a carriage return before it, is not part of it), reading at most 16 MiB and never waiting past the newline; it has no deadline, as a client that stops sending is ended by its own watchdog.

All of these print one line on stderr and exit 2: no request, `gate: the box request is missing on standard input`; 16 MiB without a newline, `gate: the box request on standard input reaches 16 MiB without a newline`; stdin failing, `gate: the box request on standard input could not be read: <error>`; a line that is not UTF-8 or a payload that is not base64 JSON, `gate: box payload is not base64 JSON: <reason>`.

Built verbs:

- **`boot`** prints `gate: remote boot <UTC time>` on stdout (ISO 8601, 7 fractional digits, `Z`; the system time less the tick count since boot) and exits 0.

  When the time cannot be read it prints `gate: remote could not read the boot time: <message>` on stderr and exits 1, so the client's boot query after a lost connection says the call failed rather than taking a status 0 with no time. The preflight says the same line and goes on: an unreadable boot time fails no preflight, and the client then makes no boot query.
- **`preflight`** first judges and holds the builder's work root (Work root, above), then creates the bare repo at `Bare` when missing and the work folder at `Work` with `.git`, `origin` set to `Bare`, `core.autocrlf false` and `core.eol lf`, and writes `.git\objects\info\alternates` as `<Bare with />/objects` and a line feed, without a byte order mark, unless the file already holds exactly that.

  On a tree it has just made with `git init`, the preflight writes the config itself: it takes `.git\config.lock` (as git does), reads `config` beneath the held `.git`, and when that config is `git init`'s own (a single `[core]` section, no `autocrlf` or `eol`, a plain UTF-8 file with no U+FEFF, last line not ending in `\`) it writes the `origin` remote (`url`, `fetch = +refs/heads/*:refs/remotes/origin/*`) with `core.autocrlf=false` and `core.eol=lf` into the lock in one write, quoted as git quotes values, and renames the lock over `config`.

  Any other config, or one it cannot take, gets its lock deleted and the git steps run as before (`config --get remote.origin.url`, `remote add`, two `config` calls).

  Right after its drain admission and before anything else it does for the run, a preflight with a `Session` on a root that has a `slots.json` takes a builder-side slot for its session ([Builder-side slots](#slots)): a session that already holds one keeps it, else it takes the lowest free one with a session claim. With every slot held it says `gate: remote version {v}` on stdout, then on stderr and unstamped `gate-remote-refused: {nonce} full` (when the payload has a `Nonce`) and `gate: remote builder is full: {held} of {slots} slots held`, and exits 75, having made no bare repository, work folder or record. A slot pool that cannot be read or written says the version line, then `gate: slots failed: {why}` on stderr, and exits 1.

  A preflight that fails after its take lets the slot go before it returns, as a run's end does (`gate: slot release failed: {why}` on stderr when the claim could not be removed); one that passes keeps it for the run. A preflight with no `Session`, or on a root with no `slots.json`, takes nothing.

  The preflight takes the work-folder lock `{Work}.lock` (made when missing, its write time never stamped) around its reads of `.layout`, `.warm` and `.command` and its writes inside the work folder, and lets it go before its SDK and tool probes.

  A lock another process holds leaves the work folder alone: it writes nothing there, exits 0 and still prints `gate: remote new work tree` when `.git\HEAD` is missing, and the run that follows is refused `busy`. A lock that cannot be opened (a folder, a link) prints `gate: remote could not open the work folder lock <path>: <error>` on stderr and exits 0, with nothing written there either.

  A link, a junction or a file at the tree's `.git`, `.git\objects` or `.git\objects\info`, or anything but a file at `.git\config`, is never followed: the preflight readies nothing there (no git step, nothing written) and the run repairs it under the lock.

  It readies the work folder's repository only when the folder's layout record says a run of one repo or is missing; a record that says a pair, `changing`, or cannot be read leaves the folder to the run.

  With a `Pair` in the payload the preflight makes the folder above `Work` and one bare repo per repo, `Bare` for the main checkout and `{root}\{stem}.git` for each sibling (`{root}` is `Bare`'s folder, `{stem}` the sibling's name with every character but an ASCII letter, digit, `-` and `_` made `-`), and touches nothing inside `Work`.

  It prints on stdout, in order: `gate: remote created <Bare>` (only when it created the bare repo), `gate: remote version <gate version>` (the same text as `gate --version` after `gate `, build id included), `gate: remote warm <64-hex>` or `gate: remote warm none` (the hash in the work folder's warm record `{Work}.warm`, read before the folders are readied; `none` when the main tree's `.git` or the record is missing, or the record is not a plain file holding exactly 64 lower-case hex digits, a line end included), the boot line, `gate: remote sdks <v1,v2|none>`, and one `gate: remote tool <tool> ...` line for each tool of the payload's `Needs` (Tool probes, below); git's own output passes through.

  A payload without `Bare` or `Work` prints `gate: box payload lacks <key>` and exits 2.

  Right after the warm line it prints `gate: remote new work tree` when any of these holds, as the run then finds the folder cold.

  First, it laid out the run's own single work tree now, that is when nothing had the name `.git` in the work folder. Second, any entry is at `{Work}.command` (the interrupted-command record below), looked at as itself and never opened or followed. Third, the layout record names another layout than the run's, a pair's first run having none yet, as the run then clears the whole folder.

  The layout comparison is read before the folders are readied, for a single layout and a pair alike, so a pair's first routed run is cold.

  After the boot line and before the SDK lines it prints `gate: remote path: Git's bin is not trusted here; bash resolves by the machine PATH` when the box finds no trusted Git `bin` for its commands' `PATH` (the command's environment, under the MSYS keeper below). The client relays preflight lines into the log only, so the line never reaches the console.
- A failing git step prints `gate: remote <step> failed (status <n>)` and exits with that status; a git that cannot start prints `gate: remote <step> failed: <OS message>` and exits 1.

  The same `failed: <message>` form covers a config read or write the preflight made itself: `gate: remote git remote add failed: cannot write <config>: <why>`, `gate: remote git remote add failed: cannot open|read <path>: <why>` and `gate: remote git remote add failed: cannot write <config>: config.lock could not be removed: <why>`. The steps are `git init --bare`, `git init`, `git remote add`, `git config core.autocrlf` and `git config core.eol`.

  On a pair run a step's line names its repo after the step: `gate: remote git init --bare failed for <leaf|sibling>` (status `n` and message forms as above), `gate: remote git init failed for <name>`, `gate: remote git remote add failed for <name>`, `gate: remote git config core.autocrlf failed for <name> (status <n>)`; a run of one repo says them as above.

  Every git step the box runs has a bound: 5 minutes for a step that reads or writes refs, config or a listing (`init`, `config`, `remote add`, `cat-file`, `grep`, `check-ignore`, `write-tree`, `commit-tree`, `rev-parse`, `diff-tree`, `update-ref`, the LFS listing), 30 minutes for one that moves content (`fetch`, `checkout`, `lfs checkout`, `clean`, `add -A`, `push`).

  A git that does not answer in time is ended with its process tree and the step says `gate: remote <step> failed: git did not answer within <n> s` and exits 1 (a sync-back step says it in the `gate: remote sync-back failed: <message>` form, after the command). A git that cannot be waited on says `gate: remote <step> failed: cannot wait for git: <error>`.

  A git step that ends with processes left in its job says a line naming the step, and one whose job could not be terminated says `gate: remote could not stop job git <step>: <error>`.

  While a git step waits, the box probes stderr, so a client that is gone ends it: the run exits 255, a `git clean` after a failed checkout included. A `git clean` that exits 255 by itself is the clean's failure (`gate: remote clean failed (status 255)`), not a gone client.

  A step that writes the shared bare repository (`update-ref -d` in the bare, `push`, `git init --bare`) is the exception: it runs on a hidden console to its end or its bound, and still starts after the client is gone, so a killed git leaves no `packed-refs.lock` the box never removes; every other step is not started once the client is gone.

  `gate box stop` during a git step that is not such a write ends that step and its process tree at once: `gate: remote <step> failed: the job <job> was stopped` and status 1. After a stop no git step that a stop ends is started (nothing is synced back, no `add -A`). A git that exits by itself is never reported stopped. A stop signal that cannot be read says `gate: remote cannot read the stop signal of job <job>: <error>`, at most once per run.

  Before its first git step a run, or the lay-out of a work folder, removes each tree's `.git\index.lock`, `.git\HEAD.lock`, `.git\config.lock`, `{Work}.index.lock` and the bare's `refs\gate-back\<key>.lock` (the lock sweep), through handles held one folder beneath another, never following a link: behind a junction at `.git` it touches nothing.

  A lock another handle holds open is skipped, and the git that needs it reports it. A folder at a lock's name is left and refuses the run (75) with `<path> is a folder where git's lock file goes`.

  A bare repository that is missing its `HEAD`, `objects` or `refs`, or a tree whose `.git` holds no `HEAD`, is initialised again, said as `gate: remote created <Bare>`. Before it re-initialises a bare with no `HEAD`, the box removes the bare's root `HEAD.lock` and `config.lock` the same way (the only names in a bare's root it removes); a folder at either fails the step `gate: remote git init --bare failed: <why>` with status 1.

  That repair runs under the bare's own lock `{root}\{repo}.git.lock`, a file beside the bare that the preflight opens with no sharing and never deletes, since every key of the repo shares the bare and the work-folder lock is the key's. The bare is checked whole before the lock, and again once it is taken (a preflight that held the lock may have made it), then the lock removal and `git init --bare` follow.

  A lock another preflight holds is opened again every 100 ms up to the short git bound plus 30 s (330 s), then the preflight fails `gate: remote <bare> is being made by another preflight and was not whole within 330 s`. A whole bare takes no lock.

  A folder or file that cannot be made prints `gate: remote could not create the folder <path>: <error>` or `gate: remote could not write <path>: <error>` and exits 1. An SDK list that cannot be read prints `gate: remote could not read the SDK list: <message>`, omits the sdks line and still exits 0; the client then skips the parity check.

  The SDK list is read as this PC's is ([Bounded SDK read](#routing)): `dotnet` found on the box's `PATH`, run in the system folder with `NoDefaultCurrentDirectoryInExePath=1`, in a kill-on-close job of its own, on the box's console, its standard output read up to 16 MiB and the end of its standard error kept for a failed read's reason, which then ends in its last lines (it reaches neither the box's stderr nor the client's log otherwise).

  The box has no interrupt; the job is killed at a 30 s bound (`--test-dotnet-limit-ms` lowers it).

  Past the bound the line reads `gate: remote could not read the SDK list: dotnet --list-sdks did not answer in 30 s`. The other `<message>`s are `dotnet --list-sdks wrote more than 16 MiB`, `dotnet --list-sdks exited with status <n>` and `cannot run dotnet: <why>`.

  Tool probes (`boxside/probe.rs`; the line itself is `gate_wire::tool_line`): for each tool of the payload's `Needs`, in payload order, the box runs a probe and says one line on stdout after the sdks line, all once every probe has ended: `gate: remote tool <tool> <version>`, `gate: remote tool <tool> present` or `gate: remote tool <tool> missing`, or `gate: remote tool <tool> skipped: <why>`. The box only reports; the PC compares the minimums.

  The probes are `rustc --version` for `rust`, `cargo-nextest nextest --version` for `cargo-nextest` (the plugin directly, never through `cargo`, so no `.cargo\config.toml` above the folder can alias it), `<name> --version` for `node`, `pnpm` and `pwsh`, `ffmpeg -version` and `git-lfs version` (read from its `git-lfs/<x.y.z>` line). `godot` and `unreal` are probed as the paragraphs below say.

  `netfx-sdk` runs no program: it reads the subkeys of `HKLM\SOFTWARE\Microsoft\Microsoft SDKs\NETFXSDK` in the 32-bit registry view, where `UnrealBuildTool` looks, and counts a subkey named as a version (one to three dot-separated runs of digits) whose `KitsInstallationFolder` is an absolute path to a folder that exists, at most 64 looked at. It says `gate: remote tool netfx-sdk <v1>,<v2>,...` ordered by number without repeats.

  With none, or a key that cannot be read, it says `gate: remote tool netfx-sdk missing`, never `skipped`, so a run that needs it does not go where it cannot be found. Test-seams builds accept `--test-netfx-key <path>` on `gate box preflight` and `gate check`, a key under `HKEY_CURRENT_USER` read in place of the machine's.

  The `godot` probe reads `GATE_GODOT_ROOT`, an environment variable of the builder naming a folder of side-by-side Godot installs, one per subfolder, each holding `Godot_console.exe`. A subfolder is named for its install's version in full canonical form, `x.y.z` (`4.7.2`: three parts, no leading zero in a part, as `04.7.2` and `4.3` are not), since a run finds an install by that name, `<root>\<version>\Godot_console.exe`.

  The probe lists the direct subfolders of that folder (the first 64 entries, in path-name order), runs each install's `Godot_console.exe --version` under the batch's deadline and says the versions found as one line, `gate: remote tool godot <v1>,<v2>,...`, ordered by number and without repeats.

  A version is the leading dot-separated digits of the first line of stdout, three parts as printed or two with `.0` added (`4.3.stable.mono.official.<hash>` is `4.3.0`), each part spelled with no leading zero (`04.07.2` is `4.7.2`); an install that prints a version and exits non-zero is still listed.

  An install under `GATE_GODOT_ROOT` whose folder is not named exactly the version it reports is left out of the list, as a run could not find it by name. With no `GATE_GODOT_ROOT` the probe runs the `Godot_console.exe` found on `PATH`. A root that names no install is `missing`. Subfolders whose name is not valid Unicode, that hold no plain `Godot_console.exe`, past the first 64, or that cannot be read are left out.

  When no install gives a version the line is `gate: remote tool godot skipped: <why>`, the first reason met: `cannot run <install>: <install> lies inside the worktree` (an install inside the run's own folders, the folder above `Work`, `Work`, `Bare` and the outputs folder, is refused unrun; a `gate check` refuses the repo root), `cannot run <exe>: <path> is a link or a junction` (a subfolder or the exe, being any reparse point: a link, a junction, an app execution alias or a cloud placeholder), `named <folder> but reports <version>` (an install of the root whose folder is not named the version it reports; said only when no other install lists), `GATE_GODOT_ROOT <path> is not an absolute path on a drive`, `cannot read GATE_GODOT_ROOT <path>: <error>`, `no answer in <limit>` (the listing was cut by the deadline with nothing read) or `Godot_console.exe --version gave no version: <line>` (the first line of stdout, cut to 200 characters with `…`, escaped, when a version part has over ten digits or the line leads with no version).

  The `unreal` probe reads `GATE_UNREAL_ROOT`, an environment variable of the builder naming a folder of side-by-side Unreal Engine installs, one per subfolder, each named for its version in full canonical form, `x.y.z`, as `GATE_GODOT_ROOT`'s are, since a run finds an install by that name, `<root>\<version>`.

  No program is launched: each install's version is read from `<sub>\Engine\Build\Build.version`, a JSON file whose integers `MajorVersion`, `MinorVersion` and `PatchVersion` give it (a byte order mark is accepted; at most 64 KiB; opened without following links, and a file with more than one hard link is refused).

  The probe lists the root's direct subfolders under the same 64-entry cap as the godot probe's, which the two share, and the batch's deadline, and says `gate: remote tool unreal <v1>,<v2>,...` ordered by number without repeats.

  There is no `PATH` fallback: with no `GATE_UNREAL_ROOT`, or a root naming no install, the line is `gate: remote tool unreal missing`. An install whose folder is not named the version it reports is left out of the list.

  When no install gives a version the line is `gate: remote tool unreal skipped: <why>`, the first reason met: `GATE_UNREAL_ROOT <root> overlaps <folder>, which a run writes` (a root inside one of the run's own folders, or holding one), `GATE_UNREAL_ROOT <root>: <path> is a link or a junction`, `named <folder> but reports <version>`, `GATE_UNREAL_ROOT <x> is not an absolute path on a drive` or `no answer in <n> s`.

  The probe runs on a thread of its own under the batch's deadline, so a slow root never delays another tool's probe, and a listing the deadline cuts short answers `skipped: no answer in <n> s` even when it had read some versions, so a pin is judged not probed rather than against part of the list.

  A `Build.version` that cannot be used gives `<file> gives no integer MajorVersion, MinorVersion and PatchVersion`, `<file> is over 64 KiB`, `<file> is not UTF-8` or `cannot read <file>: <why>`.

  Each program is found through `PATH` and `PATHEXT` as `dotnet` is (a `.cmd` shim runs through the system folder's cmd.exe; a `.ps1` is not run) and runs in the system folder on a hidden console, in the bounded runner of `remote/git.rs` (`program_output`).

  The probes run in parallel, started at the same time as `dotnet --list-sdks`, under one shared 10 s deadline (`--test-probe-limit-ms` lowers it), each given 2 s after the tool exits for its output readers to finish (`--test-probe-drain-ms` changes it, for `dotnet --list-sdks` too); one still running at the deadline is killed with its process tree, and one with no time left is skipped without being launched.

  The version is read from the first non-blank line of stdout, else of stderr: the first run of two or more dot-separated numbers after an optional `v`, its first three kept. A line with no such run, a number longer than 10 digits or a non-zero exit gives `present`; a program that cannot be found gives `missing`.

  The `<why>` of a skip is `unknown tool`, the `godot` and `unreal` reasons above, `no answer in <n> s` (or `<n> ms` when the limit is not whole seconds), `no answer in <limit>, and could not be stopped: <error>`, `the probe failed` (followed by `: <panic text>` when the probe panicked), or the runner's failure text: `cannot run <program>: <why>`, `<program> <args> wrote more than 16 MiB` or `cannot run <program>: cannot read the system folder: <error>`.

  A payload whose `Needs` is not an object of texts prints `gate: box payload Needs is not an object of texts; probing nothing` on stderr, probes nothing and still exits 0.
- **`run`** runs the payload's `Command` (a list of texts), or, for a warm-up, each of its `WarmCommands` in order (Warm-up, below), in `Work` (in `Work\{Leaf}` for a sibling pair), and exits with the command's own whole 32-bit status after a successful run. Its stdout is the command's stdout, LF-terminated; its stderr is the command's stderr plus the box's own lines.

  A run with a `Session` under a root that has a `slots.json` holds a builder-side slot ([Builder-side slots](#slots)) from after its drain admission to its end: the slot its preflight took, kept; or, when that claim lapsed and was taken, a free one; or with every slot held it is refused as the preflight is, `gate-remote-refused: {nonce} full` and the full line on stderr, exit 75. A slot pool that cannot be read or written says `gate: slots failed: {why}` and exits 1, and a pair run also says `gate: remote back commits: failed`.

  The run's slot is let go when the run returns, with `gate: slot release failed: {why}` on the box's stderr when its claim could not be removed. The release is armed only once the work-folder lock is taken, so a second call of a session refused `busy` leaves the live run's claim alone. A run refused `busy` because another session's run holds the same work folder, or whose work lock cannot be taken, keeps the slot it took until that slot's 120 s lease lapses; only a run that took the work lock releases its slot when it ends.

  Stamped lines: in `run` alone (every other verb says its lines as it did), every line but the protocol lines and the heartbeat is written `<stamp> <tag> <text>` as a local run's log holds it ([Log and tail](#log-and-tail)), so the PC writes it to its log as it came.

  The command's stdout lines are `out` and its stderr lines `err`; the box's own lines, the forwarded output of git and of the tool and package restores, the root line `gate pid=<pid> ppid=<ppid>` for the box itself, and the process lines of the command's job are `gat`.

  Bare, byte for byte: the start marker, every refusal tag (`busy` included), the kill tag, the adjusted line, the outputs line and the heartbeat. The process lines come from a thread of its own under the output lock, and none follows the box's final lines. The stamp is read once the line's lock is held.

  On the desktop route the command's parent is the helper, a process outside the run's job, so the hand-off writes `gate: the desktop helper pid=<h> started the command pid=<c>`, and the process tree joins the command to the box through it.

  The heartbeat: `gate box run` writes `gate-remote-heartbeat: <cpu ms>` on stderr every 5 s, on `gate-watchdog`'s `BeatClock` and `Pump`. Before the command starts (from just before the checkout) and after the command's wait ends it is not CPU-gated, so every period writes a beat, the first one period after it begins.

  While the command runs it writes only when the CPU of the command's job since the last beat reaches 1% of the time since it. The command's build servers are its run's own and run in the job, so a build server outside the job counts for nothing.

  For a command whose first word is `wsl` or `wsl.exe` (`is_wsl_command`, the test the local watch uses), the WSL VM's CPU (process `vmmemWSL`, read as W6 reads it) also writes a beat when it reaches a tenth of one core over the beat period, since the work runs in the VM outside the job.

  A period with no beat due flushes instead, and the box's flush probes stderr with a zero-byte write under the output lock. A beat, a flush or a probe that fails with a closed, broken or not-connected pipe (`ERROR_NO_DATA`, `ERROR_BROKEN_PIPE`, `ERROR_PIPE_NOT_CONNECTED`) sets the run's client-gone flag, which the stdout and stderr forwarders set on any write error, so a quiet command whose client is gone is found within a period.

  A probe that fails any other way stops the probing for the rest of the run while the beats go on, and the box then sees a gone client only when a beat fails to write.

  The ceiling: with a `Timeout` in the payload, the box watches the command from its start with the local watchdog's arithmetic (`boxside/ceiling.rs`; sampling every 5 s, demand every core at normal priority, the stall left to the client through the heartbeat).

  At the load-adjusted ceiling it ends the job and says `gate: TIMED OUT on <host> after <n> s of load-adjusted time (wall <w> s, machine free <p>% on average); killed <command> and its children`; at 5 x the ceiling of wall time it says `gate: BACKSTOP on <host>: <w> s of wall time (5 x <n> s ceiling, load-adjusted <a> s, machine free <p>% on average); killed <command> and its children`, the clause reading `machine load unknown: the sampler failed` after a sampler failure.

  At a kill the box ends the job and drains the command's output first, then says, in order, the kill line, `gate-remote-killed: <nonce> timed-out|backstop` and the adjusted line `gate-remote-adjusted: <nonce> <s.sss>`, and exits 124. It makes no back commit (`refs/gate-back/<key>`) and packs no outputs.

  When ending the job fails or leaves processes, the box tries once more to end it. If the job is still not ended, it says first `gate: note: the box's ceiling|backstop kill left <n> process(es) running in job <job>` (or `gate: note: the box's ceiling|backstop kill could not end job <job>: <error>`), then the kill line, the tag and the adjusted line as on any kill, and exits 124: the run is still a kill.

  A drain that ends with the output still held open says `gate: remote command's output is still held open by a leftover process; stopped reading`; the drain's rule is under "Output is forwarded" below. A client gone during the kill drain gets the box's client-gone status, not a kill.

  The PC journals a tagged kill with the tag's verdict whatever its log holds, applies no sync-back and fetches no outputs. A command's own exit 124 (no tag) is an ordinary status with a sync-back.

  `gate-remote-adjusted: <nonce> <s.sss>` is also said as any watched command exits, once its output has drained, unless the sampler failed. A failed sample says `gate: sampler failed: <message>; watching by wall time only` and only the backstop is read from then on.

  The start marker: just before it launches the command, before the donor console and the launch, the box says `gate-remote-started: <nonce>` on stderr once, with the payload's `Nonce`; a launch then refused or stopped has said it too. A warm-up says it once, after every tool and package restore and before its first command, and not again before the later ones.

  Warm-up: a `run` with `WarmCommands` lays out and readies the work folder, restores the tools and the pnpm, bun and npm packages of every command, as a run does, then removes `{Work}.warm`, says the start marker and runs each command in order as a run's command is run (the stop signal looked at before each, and a client gone found by the heartbeat ending it with 255 and no line of its own).

  It makes no back commit and packs no outputs, and on a pair run never says `gate: remote back commits: ...`. The payload's `Timeout` is one budget for all the commands: each command's ceiling is what the earlier ones left of it, in whole seconds rounded up, counting each one's load-adjusted seconds (its wall time once the sampler failed).

  The first command that ends non-zero or is killed ends the warm-up with `gate: remote warm-up stopped at command <k> of <n>` on stderr and that command's status (124 for a kill). A budget spent before command `k` ends it with `gate: remote warm-up ran out of its <s> s budget before command <k> of <n>` and exit 124.

  Once every command passed, the box writes `{Work}.warm` holding exactly `WarmHash` and says `gate: remote warm <hash>` last, exit 0. A record that cannot be removed or written says `gate: remote could not remove <Work>.warm: <error>` or `gate: remote could not write <Work>.warm: <error>` and exits 1.

  The `CpuSource` is the job's own CPU, which only rises: the job's accounting keeps what its ended processes used. Another run's busy build on the box does not keep a silent idle command alive, except a `wsl` command: the VM is shared, so another run's or Docker Desktop's WSL work counts for it. The VM's idle keep-alive (about 0.03 core) stays under the tenth-of-a-core floor.

  A heartbeat line never splits a forwarded line: the pump writes under the output lock the forwarders take. The run's git children (fetch, checkout, clean, the back ref reset, the back commit's add, write-tree, rev-parse, commit-tree and push, and diff-tree) have their output forwarded the same way, line by line; the box's own lines are written whole under std's stream lock, which the pump's writes take too. A line still waiting for the lock when the readers stop is dropped.

  After git exits, the box drains its pipes by the same rule as a command's output (below). A pipe a leftover process still holds says `gate: remote git's output is still held open by a leftover process; stopped reading` and the run goes on.

  A client that closed the connection while git's output was forwarded ends the run with 255 and `gate: remote the client closed the connection during git <step>`, the step's name saying git once (`during git init`, not `during git git init`). A pipe that cannot be read says `gate: remote could not read git's stderr: <error>` (or `stdout`).

  A failed CPU read writes `gate: remote heartbeat stopped: cannot read the CPU of job <name>: <error>` (or `... cannot list the processes of job <name>: <error>`) and a thread that cannot start writes `gate: remote heartbeat stopped: cannot start the heartbeat: <error>`; the heartbeats end and the run does not, and with them the heartbeat's finding of a gone client: a forwarded line's failed write still finds it.

  A failed read of the WSL VM's CPU writes `gate: remote heartbeat cannot read the WSL VM's CPU: <why>` at most once per pump, and the beats go on from the job's CPU. Every later tick reads the VM again; the first good reading after a failure is only a baseline, and the next one can beat.

  Ending the heartbeat waits at most 2 s (`Pump::stop_within`), so a pump stuck on a client that stopped reading is left behind and the work folder's lock is released.

  Payload refusals exit 2, each on stderr. The command's own: `gate: box payload Command is not a list of texts`, `gate: box payload lacks Command`, `gate: box payload Subfolder is not a text`, `gate: box payload Identity is not an object of texts`, `gate: box payload Identity may set only GIT_AUTHOR_* and GIT_COMMITTER_* names: <name>` and `gate: box payload Identity <name> holds a NUL`.

  `Env`'s: `gate: box payload Env is not an object of texts`, `gate: box payload Env has a malformed name '<name>'` (empty, or holding a character outside printable ASCII U+0021 to U+007E, or `=`), `gate: box payload Env gives <name> a value holding NUL`, `gate: box payload Env sets <name>, which the gate sets itself` and `gate: box payload Env sets <name>, which git would read over the gate's own git settings`.

  Also `gate: box payload Env sets <name>: GATE_ names are the gate's`, `gate: box payload Env sets <name>, a machine path the builder keeps`, `gate: box payload Env names <name> twice` and `gate: box payload Env is over its cap of 64 names or 32 KiB`. A null `Env` is not set.

  `GodotVersion`'s: `gate: box payload GodotVersion is not a version: '<value>'` for any value but a string of exactly three dot-separated parts of one to ten digits each with no leading zero (except `0`), as it is joined into an install's path; a value that is no string shows as compact JSON, shortened and escaped. A null `GodotVersion` is not set.

  `UnrealVersion`'s: `gate: box payload UnrealVersion is not a version: '<value>'`, exit 2, for any value `GodotVersion` refuses and shown the same way, as it is joined into an install's path. A null `UnrealVersion` is not set.

  `EngineEnv`'s: `gate: box payload EngineEnv is not an object of texts`, `gate: box payload EngineEnv gives <name> a value other than "unreal": '<value>'`, `gate: box payload EngineEnv sets <name>, a name the gate sets or refuses`, `gate: box payload EngineEnv names <name> twice` and `gate: box payload EngineEnv is over its cap of 64 names or 32 KiB`, the names shortened and escaped.

  `EngineEnv` and `Env` share the one cap, counted over both raw objects. A null `EngineEnv` is not set. A malformed `EngineEnv` is refused when the request is read, before the engine folder is probed; a warm-up may carry it though it may not carry `Env`.

  A warm-up payload's: `gate: box payload sets both Command and WarmCommands`, `gate: box payload lacks WarmHash`, `gate: box payload asks a warm-up for outputs` (an `Outputs` or `FailureOutputs` glob or `OutputsFull` true), `gate: box payload asks a warm-up for Env` (an `Env` that is not null, even `{}`), `gate: box payload WarmCommands is not a list of 1 to 8 commands, each a non-empty list of non-empty texts` and `gate: box payload WarmHash is not 64 lower-case hex digits: <value>`.

  A `WarmHash` beside a `Command` is ignored, and a null `WarmCommands` is not set.

  The work folder's: `gate: box payload Lock must be <Work>.lock`, `gate: box payload Index must be <Work>.index`, `gate: box payload Key is not a work-folder key: <key>` and `gate: box payload Job is not this work folder's job name: <job>`.

  The sibling pair's, when the payload has a `Pair` (`{"Leaf": "<name>", "Siblings": ["<name>", ...]}`, the main checkout's folder name and one to 8 sibling folder names).

  Its names: `gate: box payload Pair is not an object of a Leaf text and a Siblings list of texts`, `gate: box payload Pair name '<name>' is not a folder name of letters, digits, '.', '_' and '-' not starting or ending with a dot, not a device name`, `gate: box payload Pair names no sibling`, `gate: box payload Pair names more than 8 siblings` and `gate: box payload Pair names <a> and <b>, equal ignoring case` (the leaf counts as a name).

  Its work folder and bare repositories: `gate: box payload Key <key> is not the work-folder key of the Pair's Leaf <leaf>` (the key is `<repo name of Leaf>-<8 lower-case hex digits>`), `gate: box payload Work must be <root>\work\<key> with a Pair: <work>`, `gate: box payload Bare must be <root>\<name>.git with a Pair: <bare>` and `gate: box payload Pair sibling <name> would share the bare repository <stem>.git` (two repos whose bare repositories are equal ignoring case).

  `SiblingSnapshots`, an object of exactly the pair's sibling names, each a 40 lower-case hex digit commit, is read by `run` alone: `gate: box payload SiblingSnapshots is not an object`, `gate: box payload SiblingSnapshots must name exactly the Pair's siblings: <names>` and `gate: remote run payload's SiblingSnapshots <name> '<value>' is not a 40-hex commit sha`.

  `Identity` may hold only `GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL`, `GIT_AUTHOR_DATE`, `GIT_COMMITTER_NAME`, `GIT_COMMITTER_EMAIL` and `GIT_COMMITTER_DATE`.

  `Env` is the variables for the command, the PC's `--env` pairs ([Command environment](#command-environment)) and the gate's `SourceRevisionId` pair after them, sent only when there is one: an object of texts of at most 64 names and 32 KiB of names and values together, the cap read from the parsed object before any pair is checked. A name is one or more printable ASCII characters (U+0021 to U+007E) but `=`; a value may hold anything but NUL.

  It refuses a name the gate sets (`GATE_SLOT_HELD`, `GATE_COMMIT_SLOT`, `GATE_STATE_DIR`, `SharedCompilationId`, `MSBUILDNODEHANDSHAKESALT`, `MSBUILDUSESERVER`, `DOTNET_CLI_USE_MSBUILD_SERVER`, `UseSharedCompilation`, `MSBUILDDISABLENODEREUSE`, `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_<n>`, `GIT_CONFIG_VALUE_<n>`, and .NET's Event Log logging settings in any spelling: the `__` and `:` forms, a `DOTNET_` or `ASPNETCORE_` prefix, the provider's full type name).

  It also refuses `GIT_CONFIG_PARAMETERS` and `GIT_CONFIG` (git reads them over the gate's own `safe.directory` entry), any `GATE_` name, and the machine paths `PATH`, `PATHEXT`, `TEMP`, `TMP`, `SystemRoot`, `windir`, `ComSpec`, `USERPROFILE`, `HOME`, `APPDATA`, `LOCALAPPDATA` and `ProgramData`, and `GODOT_PATH` and `UE_ROOT`, which a run sets itself (the command's environment, below). Names are compared without regard to ASCII case.

  A name given twice in any ASCII case is refused; names are ASCII, so that is also how Windows compares them. A refusal line shows the name shortened to 200 characters and escaped.

  The pairs apply to the command only, never to the box's own steps (the git steps, the tool restore, the package install).

  `Key` is `[A-Za-z0-9_-]+` and is the last name of `Work`; `Job` is `Local\gate-remote-<Key>-<12 hex digits>`. `run`, `stop` and `preflight` check the key; `run` and `stop` check the job.

  The lock `<Work>.lock` is opened with no sharing and without following links, must be a plain disk file, and is held for the run's life; a held lock prints `gate-remote-refused: <nonce> busy` then `gate: remote work folder <key> is busy` and exits 75. A lock that cannot be opened prints `gate: remote could not open the work folder lock <path>: <error>` and exits 1.

  A run, `inputs` and `lfs` set the lock file's last-write time to now when they take it and again when they let it go, so the prune reads the last take or release as use; a stamp that fails at the take prints `gate: remote could not stamp the work folder lock <path>: <error>` and the run goes on. The preflight takes the lock without stamping it.

  `Work` must not end with a separator (`gate: box payload Work must not end with a separator: <work>`, exit 2). Every git call in a tree passes `-c core.hooksPath=<Lock>\hooks -c core.fsmonitor=false`, a hooks path that cannot exist while the box holds the lock, so a repository's own hooks (one a build installed) never run in the box's git; fetch and push name the repo's bare repository by its path, never `origin`.

  Every git call in a tree also passes `--git-dir=<tree>\.git --work-tree=<tree>`, so neither a `core.worktree` nor a repository above the tree is followed.

  The payload is data, refused unless well formed; the command, and whatever it writes into `Work`, is the user's own code running as the user, and is trusted. It runs non-elevated, with a restricted copy of the box's token (the command's token, below).

  The layout of the work folder: a run of one repo's tree is `Work` itself; a pair's are `Work\{Leaf}` for the main repo and `Work\{sibling}` for each sibling, each with its own bare repository and its own commit (`Snapshot` for the main repo, `SiblingSnapshots` for the others).

  The layout record `<Work>.layout`, beside the lock, says which layout `Work` holds: `single`, `pair {Leaf} {siblings}` (the siblings sorted without regard to ASCII case, one space before each, so the same siblings in another order are one layout) or `changing` while the folder is being cleared. A missing record says `single`. It is read and written as the tools record is, through a handle that refuses a link, a folder or a file with more than one name.

  The records beside the lock, one file each, named `Work` plus: `.lock` (the run's lock), `.index` (the sync-back's copy of the index), `.layout` (above), `.inputs` (the copies the `inputs` verb left), `.sent-build` (the PC's build the `inputs` verb's sent build left), `.tools` (the dotnet tool restore), `.packages` (the pnpm, bun and npm package restore) and `.warm` (the hash of the warm-up file whose commands last passed in the folder, 64 lower-case hex digits, no line end, written only by a warm-up) and `.command` (the interrupted-command record, below).

  Each is described where its feature is.

  The warm record is removed before a warm-up's first command, when a run finds the layout changed (before `changing` is written) and when the work folder is missing and the run makes it, so it never outlives the folder state it describes. The preflight reads it without taking the lock, sharing reading, writing and deletion.

  The interrupted-command record `<Work>.command` holds `{UTC start, round-trip format} {job}`, the job being the run's `Local\gate-remote-{key}-{nonce}` name. The box writes it, flushed to the disk, just before it resumes a command, once the command is in its job, and removes it once the command exited with no stop signal set and the client still there.

  A command whose launch is refused, or whose record cannot be written, leaves none (a partial one is removed; a write failure says `gate: remote could not write <Work>.command: <error>` and exits 1). A warm-up's command writes one only when none is at the name, and removes only its own. A record found by a later run is therefore of a command that was ended mid-run: killed at its ceiling, or its client lost.

  The interrupted-command clear: under the lock, with the work folder held and its layout the run's, a run that finds the record clears what that command left before any tree is readied.

  It removes `{Work}.warm`, `.packages` and `.tools`, so no skip-if-unchanged rule trusts the folder; says `gate: remote clearing what an interrupted command left in the work folder`; ends the recorded job until it holds no process (a job no longer there is fine).

  It then removes every file below `Work` written since the recorded start less 60 s of slack for a stepped clock and coarse file times, never inside a `.git` and never behind a junction. Last it says `gate: remote cleared <n> file(s) an interrupted command left in the work folder` and removes the record, so a clear cut short runs again.

  A file's freshness is read through its own handle, not the folder listing, and a file another process holds without delete sharing is tried 5 times, 500 ms apart.

  The record is read as the tools record is. One that is not UTF-8 or of another form has no start and no job: the clear takes every file and ends no job. A job name must be the work folder's key in any letter case; one held by an object that is not a job counts as no job.

  A record that cannot be read as a plain file (a link, a folder, a file of more than one name) is replaced by an empty one, made as `{Work}.command.new` and renamed over it by handle so some record stands at every instant. When the work folder is missing, or its layout record names another layout, the recorded job is ended before anything is removed, the whole folder is cleared and the record is removed last.

  A job that exists but the box may not end is opened to be listed. One holding a process refuses the run; one holding none says `gate: remote: the recorded job <job> could not be opened (access denied); the clear goes on without ending it` and the clear goes on.

  A job that may be neither ended nor listed is judged by its owner, which the system lets the box read: owned by the box's user or by Administrators, which own the box's own jobs, it refuses the run; any other owner, or an owner that cannot be read, says the same line and the clear goes on.

  A clear that cannot finish refuses the run `layout` (status 75), the record kept: `gate: remote could not clear what an interrupted command left in the work folder: <file or job>: <error>`, where the error is the file's own, `it may not be ended and still has <n> process(es)` or `it may be neither ended nor listed, and its owner <sid> owns the box's own jobs`, then the `layout` line saying `what an interrupted command left in it could not be cleared`.

  The client's `gate: remote cold` note follows the preflight's new-work-tree line, so the run after a clear has the raised ceiling.

  Under the lock, before any checkout, the run makes `Work` when missing, opens it as itself and holds it (a link, a junction or a file there is refused, never followed), and, when the record names another layout than the run's, writes `changing`, removes every child of `Work` (each as itself, a link never followed), readies each tree and writes the run's layout.

  A `changing` record left by a clear that did not finish clears again. A run of one repo in a folder already of that layout whose `.git`, `.git\objects` and `.git\objects\info` are each a folder of its own, and whose `.git\config` is a file when there is one, readies nothing here: the preflight readied the tree. A pair run readies its trees every time.

  Readying a tree makes the tree and its `.git` folders of their own (a link, a junction or a file at either is removed as itself and the folder made again), then does what the preflight does for `Work`: `git init`, `origin` set to the repo's bare repository, `core.autocrlf false`, `core.eol lf`, and the alternates file naming that repo's own bare repository's objects, rewritten when it holds anything else.

  A folder found at the alternates file's name is never walked, deleted or followed: it is renamed beneath its held parent to `alternates.gate-moved-<n>` (the first free `n` from 1 to 100, never replacing an entry) and the box says `gate: remote moved aside the folder <old> to <new>`; every other thing there is deleted as itself.

  Before its first git step, a run holding the lock replaces a link at `.git\objects` or `.git\objects\info` with a folder and removes a `.git\config` that is not a file, as itself; git then writes a fresh config. A config that cannot be removed refuses the run with `layout`.

  A folder the box cannot clear or lay out refuses the run: `gate-remote-refused: <nonce> layout` then `gate: not routed: the builder could not clear <repo>'s work folder: <why>`, said together under the output lock, with the command never started, and exit 75. `<repo>` is the main repo's bare repository's name without `.git`.

  The PC reads the 75 plus the tag as fallback reason `layout`, says `gate: remote <host> layout; running here`, runs the command here and passes the box's line on as it came. A git step that fails while a tree is readied keeps its own line and status instead.

  A payload with an `InputsDigest` (64 lower-case hex digits, else `gate: box payload InputsDigest is not 64 lower-case hex digits: <value>` and exit 2) is checked once the work folder is laid out and before the checkout: the `<Work>.inputs` record must exist, hold that digest, and name only copies still in `Work` with the size, last-write time and file id the box left them with, and the tree's `.git\info\exclude` is rewritten from the record (the `inputs` verb below).

  Otherwise the run is refused with `gate-remote-refused: <nonce> inputs` then `gate: remote inputs of work folder <key> are not the ones sent: <why>`, said together under the output lock, the command never started, and exit 75. A folder moved aside from the exclude file's name says its `moved aside` line, under the lock.

  A payload without `InputsDigest` skips the check. The PC reads the `inputs` tag as fallback reason `inputs` and says `gate: remote <host> inputs; running here`, with `gate: remote inputs not sent: the builder's work folder does not hold the inputs sent` in the log.

  A payload with a `SentDigest` (64 lower-case hex digits, else `gate: box payload SentDigest is not 64 lower-case hex digits: <value>` and exit 2) is checked the same way against the sent-build record `<Work>.sent-build` (the `inputs` verb's sent build, below), after the `InputsDigest` check.

  Otherwise the run is refused with `gate-remote-refused: <nonce> sent` then `gate: remote build sent to work folder <key> is not the one there: <why>`, said together under the output lock, the command never started, and exit 75. A payload without `SentDigest` skips the check.

  A payload with an `LfsDigest` (64 lower-case hex digits, else `gate: box payload LfsDigest is not 64 lower-case hex digits: <value>` and exit 2) is checked after the fetch and before the checkout, under the work folder's lock; a payload without one makes no such check, and a pair run that carries one is refused `lfs` before any checkout.

  The box lists the pointers from the snapshot itself, as the PC lists them (spec-v1 blobs of at most 1024 bytes whose `filter` is `lfs` at the commit, read with `-c core.attributesFile=` and `GIT_ATTR_NOSYSTEM=1`).

  It requires that their objects' manifest has the digest sent, that the box is elevated, and that each object is in the store as a plain file of one name (a second name, which git-lfs's scratch folder links, is taken) of the pointer's size, opened without following a link and held without delete sharing, with the store's lock held shared, through the back commit.

  Anything else is `gate-remote-refused: <nonce> lfs` then `gate: remote lfs of work folder <key> is not the snapshot's: <why>`, said together under the output lock, the command never started, and exit 75; the PC reads it as fallback reason `lfs`. A scratch folder that cannot be made, or a foreign `lfs-run`, is refused the same way with `gate: remote cannot make the run's LFS scratch folder: <why>` as the second line.

  The scratch folder is made once per run, after each repo's fetch, for a run with an `LfsDigest` or whose snapshot's `.gitattributes` files name the filter (`git grep -q -F -e filter=lfs <snapshot> -- ':(glob,icase)**/.gitattributes'`, [Routing](#routing), Git LFS); a `git grep` that fails prints `gate: remote lfs listing failed (status <n>): <git's first stderr line>` and fails the step.

  The checkout of a repo takes the pins of Git LFS in Routing on every git call that may run the filter, and `lfs.storage` set to the run's scratch folder. After a checkout of a run with pointers the box checks that every pointer's path is a plain file of the pointer's size, since `checkout --force` leaves a file whose stat is clean and a pointer an earlier run left would survive. A pointer path that is not is deleted and checked out again once, and still not is refused `lfs`.

  A file of 1024 bytes or less counts as wrong only when its text is that path's own pointer, so an object whose content is pointer text naming another object is its content.

  A run without a scratch folder pins `lfs.storage=<lock>\lfs`, which cannot be made.

  A checkout that fails there with the tree's attributes (tracked or not, ignored folders passed over) naming the filter removes the tree's untracked files (`clean -fdq`), so a leftover attributes file cannot fail every later run in the folder, and says `gate: remote checkout failed: a file under the LFS filter was found that the run's snapshot does not declare in a tracked .gitattributes; run it with --local`, with git's status.

  Otherwise the line is git's own `gate: remote checkout failed (status <n>)`.

  The steps are the layout, fetch, the commit probe, checkout, clean and the back ref reset (each of the three git steps once per repo on a pair run, the main repo first), the dotnet tool restore, the package restore (pnpm, bun or npm), then the command, then the sync-back. A failing step prints `gate: remote <fetch|checkout|clean|back ref reset> failed (status <n>)` and exits `n`.

  The clean is `git clean -fdq`, with no `-x`, so a work folder keeps its ignored files between runs; a Godot project's `.godot` folder is one, so it persists from one routed run to the next.

  On a pair run the step is followed by the repo's name (`gate: remote fetch <name> failed (status <n>)`), while the steps that ready a tree say `failed for <name>` as the preflight's do.

  After the fetch of `refs/gate/<key>` from the repo's bare repository, the probe runs `git cat-file -e <sha>^{commit}` in its tree. A non-zero exit prints `gate: remote snapshot <sha> is not in the work folder's repository after the fetch` (on a pair run `gate: remote snapshot <sha> is not in the repository of <name> after the fetch`) and exits 1, and a git that cannot start prints `gate: remote cat-file failed: <message>` (`cat-file <name> failed` on a pair run) and exits 1.

  On a pair run every repo's back ref `refs/gate-back/<key>` is deleted before any checkout, so no repo keeps an earlier run's back commit whatever step of this run fails; a run of one repo deletes it in its checkout step.

  The checkout is `checkout -q --force --detach <sha>`, the payload's commit and not `FETCH_HEAD`, so a later push of the key changes nothing a running run runs. The box's git calls for preflight, the probe and the run's own steps share one start and wait path (`preflight::Git`), which carries the bounds, the gone-client probe, the stop and the lock sweep of the preflight section above.

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

  The package restore (`boxside/packages.rs`) runs for a routed command whose first word's file stem is `pnpm`, `bun`, `bunx`, `npm` or `npx`, in any case, after the tool restore and before the command. A run whose commands include more than one such command (a warm-up's) installs once per distinct lockfile folder and manager, in the commands' order, before the first command, and stops at the first install that fails. An install never sees `UE_ROOT`, nor does the dotnet tool restore.

  It looks for the folder to install in, for each command, from the start folder, and walks up from it to the work root (the main tree on a pair run) for the first lockfile (`pnpm-lock.yaml` for pnpm, `bun.lock` or `bun.lockb` for bun, `package-lock.json` or `npm-shrinkwrap.json` for npm, read in that order); a walk that finds none does nothing.

  The npm start folder is the place folder for `npm` and `npx` alike: no option of theirs moves it.

  The pnpm start folder is `--dir` or `-C`, read only among pnpm's own options before the first non-option word, else the place folder.

  The bun start folder is read from `--cwd`. Before the subcommand only `--cwd=<d>` counts and a bare `--cwd` ends the scan. After `run` or `test`, both `--cwd <d>` and `--cwd=<d>` count, up to the script or test path. An option that takes a value ends the scan, as with pnpm. For `bunx` and `bun x` the start folder is the place folder and every `--cwd` is ignored, because bun looks the bin up from the real cwd even with `--cwd=`.

  It runs `pnpm install --frozen-lockfile`, `bun install --frozen-lockfile` with the `bun` on the box's PATH (never `bunx`), or `npm ci --ignore-scripts --no-audit --no-fund` with the `npm` on the box's PATH (never `npx`), in the lockfile's folder in a job of its own named `{job}-packages`, created kill-on-close and never handed to the manager, with the command's restricted token (the box's own is never the fallback) and the box's environment plus `CI=true` and git's `safe.directory=*`.

  Its output is forwarded as the tool restore's is, and it has no time limit (GATE-159). `--ignore-scripts` is hygiene against a package's install scripts, not a security boundary: the command that follows runs the repo's code anyway.

  A repo whose npm packages need their install scripts runs its own install in its warm-up. `npm ci` empties `node_modules` first, so after any change to the folder's package files the box's next restore replaces what those scripts built, and such a repo runs `gate warm` again after each such change.

  Bun and npm are not probed tools: a box without bun or npm refuses at the install's start, as it does for pnpm.

  An npm or npx command restores nothing in a lockfile folder that also holds a pnpm or bun lockfile, whose manager owns the folder's `node_modules`: it prints `gate: remote npm install skipped: <folder> holds a pnpm or bun lockfile` and runs the command.

  The record `{Work}.packages`, beside `{Work}.tools`, is UTF-8 lines `{folder}\t{64 lower-case hex digits}\n`, sorted, `.` for the root and `\` between folder names. A bun line is keyed `bun:{folder}` and an npm line `npm:{folder}`, beside pnpm's bare-folder lines.

  The pnpm hash is SHA-256 over each of `pnpm-lock.yaml`, `package.json`, `pnpm-workspace.yaml` and `.npmrc` that exists in the folder, in that order: its name, a NUL, its bytes, a NUL. The bun hash is built the same way over `bun.lock`, `bun.lockb`, `package.json`, `bunfig.toml` and `.npmrc`, each only when present.

  The npm hash is built the same way over `package-lock.json`, `npm-shrinkwrap.json`, `package.json` and `.npmrc`. The install is skipped while the folder's line matches and `node_modules` there is a real folder.

  An install that starts drops the other managers' lines for its folder before it runs, whether it then passes, fails or is stopped, so a folder holds at most one manager's line and the other managers reinstall on their next command.

  A request or warm-up whose commands need two managers in one lockfile folder installs nothing: it prints `gate-remote-refused: <nonce> tools` then `gate: remote work folder <key> uses both <first> and <second> in <folder>` and exits 75, the two named being the first two of the folder's managers in the order pnpm, bun, npm.

  Before it installs, the box asks git, with its own token, `git -c core.hooksPath=<lock>\hooks -c core.fsmonitor=false --git-dir=<work>\.git --work-tree=<work> check-ignore -q <folder>/node_modules/`, and the install runs only when git ignores `node_modules` there.

  An install that exits, passing or failing, first prints `gate: pnpm install took <s> s` (`gate: bun install took <s> s` for bun, `gate: npm install took <s> s` for npm; ` in <folder>` after it when the folder is not the root of the work folder), after its own output.

  One that cannot start prints `gate: remote could not start the pnpm install: <message>` (`bun install` for bun, `npm install` for npm; `cannot make its restricted token: <error>` when the token cannot be made); a non-zero one prints `gate: remote package restore failed (status <n>)` then `gate-remote-refused: <nonce> tools` and `gate: remote work folder <key> could not restore its pnpm packages` (`bun packages` for bun, `npm packages` for npm), and exits 75 with the command never started, as the tool restore's does.

  A stop already set, a stop during the install and a client that closes the connection give the tool restore's lines naming `{job}-packages`: `the job {job}-packages was stopped before the command started`, `gate: remote stopped job {job}-packages` and `gate: remote the client closed the connection; stopping job {job}-packages`.

  A process that keeps the install's output open after the job ended prints `gate: remote the package restore's output is still held open by a leftover process; stopped reading`.

  The install is skipped, with the command still run, when the folder the manager starts in lies outside the work root (`gate: remote pnpm install skipped: <folder> is outside the work root`), when a folder from the work root down to the lockfile's is a link or a junction (`... <folder> is reached through a link`) or when git does not ignore `node_modules` there (`gate: remote pnpm install skipped: node_modules is not ignored in <folder>`); the bun and npm lines read `gate: remote bun install skipped: ...` and `gate: remote npm install skipped: ...` alike.

  Git that cannot say prints `gate: remote could not ask git whether node_modules is ignored in <folder>: <error | status n>`.

  A lockfile or package file that cannot be read prints `gate: remote could not read the lockfile <path>: <error>` or `gate: remote could not read the package file <path>: <error>`, and a record that cannot be written prints `gate: remote could not write <Work>.packages: <error>`; these exit 1. A record that cannot be read prints `gate: remote could not read <Work>.packages: <error>; installing the packages` and installs. Folders and paths in every line are escaped.

  A `Subfolder` is a folder of the main repo's tree, compared against its snapshot with per-character simple uppercase (the wrapper's OrdinalIgnoreCase): one that is not a folder inside `Work` prints `gate: remote subfolder <sub> is not a folder inside the work folder`, deletes the back ref and exits 2, and one missing from the snapshot prints `gate: remote subfolder <sub> does not exist in the snapshot` and exits 2, `<sub>` with its control characters escaped in both.

  The shared batch refusal (a `.cmd` or `.bat` word, [Command launching](#command-launching)) exits 2.

  The desktop run (a `run` payload with `Desktop` true, `DesktopWait` its wait in seconds) reads the console session twice and takes the builder's desktop lock between the reads.

  The first, cheap read comes right after the work-folder lock is held, before the layout and the checkout: when the console cannot take the command the run is refused with `gate-remote-refused: <nonce> desktop` then `gate: not routed: the builder has no desktop session: <why>`, said together under the output lock, the command never started, and exit 75.

  A refusal of the desktop says no `gate: remote back commits:` line, on a pair run too.

  The whys of the session read are `nobody is logged on at the console`, `another user is logged on at the console`, `the console session is disconnected`, `the console session is locked` and `the console session's lock state cannot be read`.

  Three more come from a read that fails: `the console session cannot be read` (followed by its own line `gate: the console session read failed: <escaped system error>`), `the console session changed while the box read it` (another session was attached to the console between the two reads of which one is), and `the box's own account cannot be read: <error>`.

  The text after `<why>` is escaped into one line.

  After the checkout and the restores comes the desktop stage, in place of the command's start. It resolves the command's program and the run's Godot (`resolved_command`, as a run with no desktop does) first, so a program not found (127) or a refused batch word (2) never holds the desktop; looks at the stop signal; and takes the machine-wide desktop lock, the private-namespace mutex `gate\desktop`, waiting `DesktopWait` seconds at most, or the run's `Timeout` when that is shorter.

  While another run holds it the box says `gate: remote waiting for the builder's desktop: another run holds it` once; a lock taken from a run that ended without releasing it says `gate: remote took the builder's desktop from a run that ended without releasing it`.

  A wait that ends with the desktop still held is refused with the `desktop` tag, `gate: not routed: the builder's desktop stayed busy for <n> s` and exit 75. A lock that cannot be made or waited for is refused with the tag and the why `the builder's desktop lock cannot be made: <error>` or `the builder's desktop lock cannot be waited for: <error>`.

  A stop during the wait ends the run with status 1 and the stopped-before-start line, and on a pair run `gate: remote back commits: failed`. A client gone during the wait, looked for before each turn, ends it with 255 and no line of its own, the lock released and no command started; a pair run's back-commits line is still written, to the dead stream.

  With the lock held the box reads the console session again, and a console that can no longer take the command is refused with the first read's whys. A run that passes the stage hands its command to the desktop helper, and the lock is released when the hand-off is over, however it ended.

  The hand-off creates the run's job (kill-on-close), makes the control pipe and starts the helper through the registered task in the console's session.

  It pins the process the task started by handle and checks it before trusting it: created no earlier than the box's own clock reading taken before the start, in the console's session, running the installed helper image (compared by file identity) and as the box's user. The pipe's client must be that process; a connection from another pid is refused and the connect goes on.

  Then the helper says hello, the box sends the command (program, arguments, working folder, environment and the two output pipe write ends it gives the helper), and the helper reports the child it created suspended. The box pulls the child's handles out of the helper, verifies them by file identity and by facts it reads from its own handles, adopts the child into the run's job, and tells the helper it is done.

  The start marker follows, then the run's usual park seam, stop look and resume, and the command is watched to its end as any run's command is. A zero `DesktopWait` is one try for the lock, with no busy line.

  Before the start marker every failure is a refusal (the `desktop` tag, exit 75, no back commits line), except a helper that is there and not installed where only administrators can change it: that is refused with the `install` tag (Install tripwire, above), also exit 75 with no back commits line. A helper file that is not found stays a `desktop` refusal.

  Every hand-off refusal line starts with `gate: not routed: the builder has no desktop session: `, a helper that never started or connected included, so the prefix names the refusal's tag and does not report the builder's session state.

  The whys after it are `the desktop helper did not start: ` and one of `the control pipe cannot be made: <error>`, `it did not connect in time`, `it did not answer in time`, `it sent a message the box cannot read`, `it sent a message the box did not expect`, `it did not take the box's message in time`, `it closed the control pipe`, `the control pipe failed: <error>`.

  Of the same prefix, about the process the task started: `the process the task started cannot be opened: <error>`, `its exit code cannot be read: <error>`, `it cannot be waited for: <error>`.

  Of the process that answered the pipe: `the process that answered was created before the task ran`, `the process that answered is in another session`, `the process that answered is not the gate's desktop helper`, `the process that answered runs as another user`, `the pipe's client is not the helper the task started`, or `its creation time|session|image|user cannot be read: <error>`.

  The other why is `the desktop helper failed: ` and one of `the command it started cannot be put in the box's job: <error>`, `the command it started is not the process it reported: <error>`, `the <fact> of the command it started cannot be read: <error>`, or `the job does not hold it after the assign`.

  The box's own machinery adds whys of the same prefix: `the command's job cannot be made: <error>`, `the program the box sends cannot be read: <error>`, `the desktop helper's file cannot be read: <error>`, `the system clock cannot be read`, `the box's own user cannot be read: <error>`, `the desktop helper's argument cannot be made`.

  Also of the machinery: `the command's output pipes cannot be made: <error>`, `the command's output pipes cannot be given to the desktop helper: <error>`, `a handle value does not fit the start request: <error>` and `the command's start request is too long for the control pipe`.

  A helper that reports it could not create the command (`Failed`) is not a refusal: the box says the start marker, then `gate: could not start ...` as for any command that cannot start, and ends with 127, after the failure ending below.

  A failure after the box has sent the command ends the helper's side before the box answers.

  The box drops the pipe without `Done` (the helper then ends its own child), waits once, for at most the end limit and watching the run's stop signal, for the helper to exit, ends the helper if it is still there, and sweeps: from a process snapshot (without command lines) it ends each process whose parent is the helper, created after it and in the console's session, re-checking each by its own handle first: it asks every one of them to end, then waits for them all within one end limit for the whole sweep.

  A process the box could not end is said as `gate: remote could not end the desktop command (process <pid>): <error>` (the error `it had not ended in time` for one still running when the sweep's bound runs out), the helper as `gate: remote could not end the desktop helper (process <pid>): <error>`, and a snapshot that failed as `gate: remote could not list processes to end after the desktop command: <error>`.

  The stop lines of a desktop run are written under the output lock, as the heartbeat is.

  The command runs outside the box's own job, created with `CREATE_BREAKAWAY_FROM_JOB` so it leaves the ssh session's job (the box's job refusing breakaway prints `gate: the box cannot start a command outside its own job: <message>` and exits 1). Its console is a donor's UTF-8 console (the console, below), else a hidden console of its own (`CREATE_NO_WINDOW`).

  It is assigned to the payload's named job, which has no limits and no kill-on-close, and inherits exactly NUL, the two pipe write ends and a query-only handle to its job, as copies made under the spawn lock ([Command launching](#command-launching)). The box's git and dotnet calls start through `gate_win32::spawn`, under the same lock.

  The command's token: it starts through `CreateProcessAsUserW` with a restricted copy of the box's token (`CreateRestrictedToken` with `LUA_TOKEN`), so it is not elevated, is lowered to Medium integrity when the box's is above it, has the user as default owner, and has a default DACL granting `GENERIC_ALL` to the user and SYSTEM.

  The ssh logon's NETWORK group (S-1-5-2) is made deny-only in it, so WMI does not treat the command's calls as remote and `taskkill`, `tasklist` and `Get-CimInstance` work from the command (with NETWORK enabled they fail with `ERROR: Access denied`). `gate_win32::NetworkGroup` (`Absent`, `Enabled`, `DenyOnly`, `Disabled`) and `TokenFacts::network` report the group's state.

  The console donor and console keeper `cmd.exe` run with the same restricted token, so no elevated process shares the command's console. The box itself, its git calls and its tool restore keep the box's own token; the pnpm, bun and npm installs run with the command's restricted token, with no fallback to the box's own.

  The restriction makes the command's token non-elevated with NETWORK deny-only, but it is still a network logon's token and lacks INTERACTIVE, so it is not the token a local run's command has; it is not a boundary against the command, whose code is trusted, since the box's elevated git and tool restore later read what the command wrote.

  When the token cannot be made, or a start with it is refused and a start with the box's own token then works, the box prints `gate: the command runs with the builder session's own token: <error>` on stderr, after the start marker, and the command (with the donor and console keeper) runs with the box's own token. A start refused with both tokens says only `could not start`.

  A run with an `LfsDigest` never takes the box's own token: a start with the restricted one that fails says `gate: remote could not start the command with its restricted token, and a run with LFS content never runs with the builder session's own token`, or `cannot make the command's restricted token, and ...` when the token cannot be made, and the run fails.

  Every MSYS program of one user in one session (Git's `sh.exe` and the tools in `usr\bin`) shares one section, `msys-2.0S5-<install key>\<SID>.1`, whose first word is both its set-up lock and its ready mark. The box's git, the command and the warm-up commands share it in session 0, on either token.

  A start-up killed inside the lock (about 2 ms) while another MSYS process keeps the section open leaves it held: every later `sh.exe` of that user then waits out the runtime's 15 s spinlock and dies with an `add_item ... errno 1` error, until no MSYS process of that user is alive (GATE-387).

  So the box starts an MSYS keeper (`boxside/msys.rs`, not the console keeper): a long-lived `cat.exe` of Git's own `usr\bin`, kept for the run or warm-up. It starts in `in_work_folder` right after the heartbeat starts, once the work folder's lock is held and before the checkout's first git call, and ends when the run returns.

  Lookup: from the `git.exe` the box resolves to (the same `PATH` search its git calls use), it walks at most 3 folders up (`cmd\`, `bin\`, `ucrt64\bin\`, `mingw64\bin\`) for the first folder whose `usr\bin` holds both `cat.exe` and `msys-2.0.dll`. A `git.exe` with no such root above it, such as a shim, has no keeper.

  The folder's owner must be Administrators, SYSTEM or TrustedInstaller (`gate_win32::TRUSTED_OWNERS`, the list gate-win32's installed-file check shares). The check is the owner, not write access: it keeps out a Git the user installed in a folder of their own, but an elevated install outside `Program Files` can still be writable by another local user, and the check does not see that.

  The keeper starts on the command's restricted token (the box's own is never the fallback here: the keeper then does not start and says why), in the system folder, on a hidden console, with the box's environment, in an anonymous kill-on-close job of the box's own and never the command's; its three standard streams are pipes the box holds, and the command inherits none of them. Dropping the keeper closes the job and ends it.

  The box writes it the line `gate: msys keeper` and waits for the same line back, at most 30 s, looking at the stop event every 100 ms. The echo shows the runtime has set the section up. A stop during the wait ends it, and the run goes on to its own look at the stop.

  A keeper that cannot start or answer prints `gate: box could not start Git's MSYS keeper: <why>; a Git shell killed as it starts can stall later ones` on stderr once, through the heartbeat's output lock, and the run goes on without it with its own status.

  `<why>` is escaped and names the step that failed: the lookup (`cannot find git: <error>`, `no Git root with usr\bin\cat.exe above <git.exe>`, `cannot read the owner of <folder>: <error>`, `<folder> is not owned by Administrators, SYSTEM or TrustedInstaller`).

  The launch's whys are (`cannot make its restricted token: <error>`, `cannot create its job: <error>`, `cannot start <cat.exe>: <error>`, `cannot run <cat.exe>: <error>`), and the wait's (`it ended without answering`, with `: <the first line of its stderr, at most 200 characters>` when it wrote one; `it did not answer within <s> s`; `it answered '<line>' rather than the line written to it`; `the run was stopped`).

  The lookup's whys come from the real keeper only; a stand-in skips the lookup.

  Until a box with the MSYS keeper is installed, gate's installer (`tools/builder/install-toolchains.ps1`) keeps one MSYS process of the builder user alive as a stopgap: a startup scheduled task as that user, run whether logged on or not, not elevated, with no run-time limit, running Git's `usr\bin\sleep.exe infinity`. It is restarted after every Git for Windows update, so a new runtime never meets an old one's section.

  The command's environment gains `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_<n>=safe.directory` and `GIT_CONFIG_VALUE_<n>=*`, where n is the inherited count (0 when there is none) and the count grows by one.

  The inherited count is read as git reads it (leading whitespace, an optional `+`, decimal digits to the end, at most `i32::MAX`; an empty value is 0); a count git would refuse, or `i32::MAX` itself, leaves the environment unchanged. The work folders are owned by Administrators, which git refuses from a non-elevated token.

  The command's environment is the box's own with the payload's `Env` pairs set over it, each replacing any inherited value; the settings below are applied after the pairs, so none of them can be undone by one.

  A run with a `GodotVersion` resolves Godot on the box. With `GATE_GODOT_ROOT` set in the command's environment, a Godot console word runs only `<root>\<version>\Godot_console.exe`: an install that is missing, or a folder or exe that is any reparse point (link, junction, app execution alias, cloud placeholder), fails as `gate: could not start <word>: <file-not-found text>` with status 127, and never falls back to the `PATH`.

  With no root the word resolves on the `PATH` as before. Any other word resolves as before. The Unix box resolves a Godot console word by the same version rule under its own root ([The Unix box](#the-unix-box), Godot).

  `GODOT_PATH` is then set for every command of the run to the Godot program that resolved: the root's install when the root is set, else, with no root, the program the `PATH` gave a Godot console word, or the `PATH`'s `Godot_console.exe` for any other. It is set after the caller's pairs and the gate's own settings, so none undoes it. When no Godot program resolves, `GODOT_PATH` is removed, an inherited value included. A run with no `GodotVersion` leaves the environment as before.

  A run with an `UnrealVersion` resolves `<GATE_UNREAL_ROOT>\<version>` with the probe's root checks (no link in the root or its ancestors; the root inside none of the run's refused folders; the install's `Build.version` present), once when it reads the request, and sets `UE_ROOT` to it, and each name of the payload's `EngineEnv` to the same folder, for every command of a plain, warm-up or desktop run, after the caller's pairs and the gate's own settings.

  An `EngineEnv` name replaces an inherited value and a caller's `Env` pair for it.

   When it cannot resolve, the run exits 127 with `gate: could not resolve unreal <version>: <why>`, before any desktop lock.

  A run with no `UnrealVersion` removes an inherited `UE_ROOT` and an inherited value of each `EngineEnv` name; a caller's `--env` pair for an `EngineEnv` name is kept. The package restore and the dotnet tool restore run without `UE_ROOT` and without the inherited values of the `EngineEnv` names, whatever the box inherited.

  Every command of a plain, warm-up or desktop run gets the trusted Git `bin` at the front of its `PATH`, so a recipe's bare `bash` resolves to Git's own `bash.exe` rather than WSL's in the system folder, and the command's first word resolves on that same `PATH`. The folder is `<Git root>\bin`, the root being the one the MSYS keeper's lookup finds above the box's `git.exe`, and it must hold `bash.exe`.

  It is used only when it lies outside every folder the run writes (the refused folders of the preflight's SDK read) and every file it puts ahead of the machine `PATH` passes the installed-file check the box's own image gets (`gate_win32::check_installed_file`): no link or junction, no network share, and an owner and access list that let no one but SYSTEM, Administrators or TrustedInstaller change what runs from it.

  Those files are every `.exe` directly in `bin` (listed only after `bin\bash.exe` has passed, which judges `bin` and every folder above it), and the files Git's shell runs from the same install: `usr\bin\bash.exe`, `usr\bin\msys-2.0.dll` and, when it exists, `mingw64\bin\git.exe`. One failing leaves the `PATH` alone. The folder put on the `PATH` is the final path of that `bash.exe`'s folder, in its usual form (no `\\?\` prefix).

  The rest of the `PATH` is kept as it is, inherited entries and a caller's `PATH` pair included. A command with no `PATH` gets one holding only the folder, an empty `PATH` is replaced by it, and a folder already first is not added twice.

  A folder holding `;` or `"`, which would split or quote the value, and a `PATH` or folder that is not Unicode, leave the `PATH` untouched. With no trusted `bin` the command keeps the `PATH` it would have had and the run says nothing; the preflight says so (`preflight`, above).

  The command's environment also gets the box's own names, after the pairs, so no caller can send one (any `GATE_` name is refused): `GATE_BOX_EXE`, the box's own image path, and `GATE_BOX_JOB`, the run's job name. The name is spelled `Global\gate-remote-<key>-<12 hex>` when the box runs in session 0 (an ssh logon, where `Local\` is the global namespace), so a desktop command in the console session reaches the same object, else as created.

  A gate the command starts reads them ([Critical drain and nesting](#critical-drain-and-nesting), the in-box gate).

  A box that cannot read its own path ends the run before any job is made: status 1, `the box's own path cannot be read: <error>`.

  The command's environment also sets `Logging__EventLog__LogLevel__Default=None`, replacing any inherited value, so a .NET host with default logging writes nothing to the Windows Event Log; its console output is unchanged. Of the groups the Application log's default access lets write, a restricted copy of the box's token could hold only Administrators, INTERACTIVE, SERVICE, BATCH and WRITE_RESTRICTED, and the command's token holds none of them.

  On a builder without the setup grant below (a fresh or reimaged one), the host's default EventLog provider would throw `Access is denied` on its first Warning; the variable keeps such a host working wherever the grant is missing.

  Known cost: deny entries naming NETWORK still apply to the command, but allow entries naming NETWORK no longer grant, so a resource whose DACL grants NETWORK alone is open to a command run directly over ssh and closed to the same command routed.

  Known cost: the first non-elevated build in each work folder starts fresh MSBuild nodes and compiler servers, since their handshake includes elevation. A command that creates symlinks needs Developer Mode on the builder, as on the PC; claude-config's provisioning of the builder turns it on (`AllowDevelopmentWithoutDevLicense` = 1).

  Known cost: the variable does not cover a command that writes to the Event Log by another route (`EventLog.WriteEntry`, a logging library's own sink). Nor does it cover a host whose own configuration sets the EventLog provider's level: a category rule (`Logging:EventLog:LogLevel:Microsoft`), which wins over the default, or a configuration source added after environment variables, or a bare `HostBuilder` that never reads them.

  claude-config's provisioning of the builder therefore grants the builder user write access to the Application log: one allow entry for the user's SID, write access 0x2, added to the channel's access SDDL with `wevtutil sl Application /ca:` from an elevated session. That covers every writer to that log under a registered source.

  Any routed command, and anything else run as the builder user, can then write entries under any registered source, so the builder's Application log is not evidence of what the system did. Registering a new event source and writing to any other log still need Administrators, so they fail on a routed run.

  The stop event `<job>-stopped` ([Kernel object names](#kernel-object-names)) is a manual-reset event the run creates, unset, right after it takes the work-folder lock, before the fetch and checkout; one that cannot be created prints `gate: the gate failed while running the command: cannot create the event <job>-stopped: <error>` and exits 1.

  `gate box stop` sets it whether or not the job exists yet (a missing event is silent), then terminates the job when there is one. The run looks at it once just before it creates the job (set: the stop line and exit 1, nothing started), and again after the command is assigned to the job and before it is resumed, on every launch.

  Set means a `stop` came first: the command is ended unstarted, the job is terminated, and the run prints `gate: the gate failed while running the command: the job <name> was stopped before the command started` and exits 1. An event that cannot be read prints `gate: remote cannot read the stop signal of job <name>: <error>; starting the command`, and the command starts.

  The run's job, and the desktop hand-off's, is made with an explicit DACL (`gate_win32::box_job_descriptor`, through `Job::create_secured` and `Job::create_kill_on_close_secured`): SYSTEM and Administrators full access. The box's user gets `JOB_OBJECT_QUERY` only when the box's token is elevated, and full access when it is not, since `gate box stop` and the job-name wait reopen the job as that user and an in-box gate lists the job to judge it.

  A job name already held prints `gate: the job <name> already exists; a run's job name must be new` and exits 1. A command that cannot start prints `gate: could not start <word>[ (<resolved>)]: <message>` and exits 127, the word and the resolved path with their control characters escaped; a failure of the gate after the start prints `gate: the gate failed while running the command: <problem>` and exits 1. If the box panics after the resume, the job is terminated with 124.

  Output is forwarded from the pipes until both end. What the command left in its job is ended first (below), the console's donor excepted, so a leftover there holds no pipe open and costs no drain.

  The drain: once the command's main process has exited, the box waits 2 s for each reader to reach the end of its pipe. A reader still not at the end then is waited for further, up to a 30 s guard counted from the start of the drain, only while it is behind: it holds bytes it has read and not yet written or dropped, or it waits on no read (between reads, or not yet run).

  A read still pending on a pipe holding nothing is a leftover's, and the drain cancels it with `CancelSynchronousIo`, but only a reader that has said it is in its read (a `reading` flag set around each pipe read): a reader still starting, or writing, is never cancelled.

  A reader is a leftover only if its read ended aborted by that cancellation: the line below is said at about 2 s and the reader ends quietly, writing nothing further. A cancelled read that completes anyway has its bytes forwarded and is judged again, and a reader at the end of its pipe is never one.

  A reader writing to the box's own output is never cancelled; the drain waits for it. A reader still behind when the guard passes is stopped, its unread bytes dropped, and the run says the same leftover line. Git's pipes drain by the same rule.

  A process the run's end spares, such as a live nested gate, can still hold a pipe open after the command ended: the box then stops reading and says `gate: remote command's output is still held open by a leftover process; stopped reading`. A pipe that cannot be read prints `gate: remote could not read the command's <stdout|stderr>: <error>`.

  Once the command has exited, before the readers drain and before the outputs are listed, the box ends what the command left in its job, sparing live gates and console hosts as the local run end does, and the console's donor (a spared gate may still be using it; the box ends it itself once the wait for the command is over), with the passes the local run end uses (`end_leftovers`), under the state root a `gate run` with no `--state-dir` resolves.

  A failure says `gate: cannot end the run's job: <problem>` on stderr, the problem worded as the local run end words it (the pass cap's `<n> of its processes still ran after 5 passes; first: <first problem>` included); a state root that cannot be resolved says the fixed line `gate: cannot end the run's job: the box's state root cannot be resolved; ending the whole job` (no path) and the whole job is terminated. The run's status stands either way.

  A failed write by a forwarder to the box's stdout or stderr, or a heartbeat's failed beat or probe (above), means the PC's ssh connection closed, as when the PC's gate is killed, and a closed connection always stops the job, whether it is noticed while the command runs or during the final drain. The box terminates the job with 124 and polls its processes every 100 ms for up to 10 s. It then drains the readers, skips the sync-back, releases the work-folder lock and exits 255.

  On stderr, best effort, it says `gate: remote the client closed the connection; stopped job <name>` once the job is empty. Otherwise it says `gate: remote the client closed the connection; stopping job <name>` followed by the could-not-stop line or the `still has <n> process(es)` line of `stop`. The stdin closing is not the signal. A command that itself exits 255 is indistinguishable from a lost connection, as with the PowerShell wrapper.

  After the command, the sync-back commits the tree when it changed: a copy of the index at `<Work>.index` is staged, and the commit (message `gate-remote back`, parent HEAD) is written to `refs/gate-back/<key>` in `Bare`. A sync-back that fails prints `gate: remote sync-back failed: <message>` and turns a 0 status into 1; an index copy that cannot be removed prints `gate: remote could not remove <index>: <error>` and changes no status.

  The back commit's `add -A` takes the checkout's pins (Routing, Git LFS) with the run's `lfs.storage`, so a clean writes nothing into the store or the work folder's `.git\lfs`; a changed LFS path is refused at the PC's sync-back as before.

  In a run without a scratch folder a failed `add -A` is checked with `git grep -q -F -e filter=lfs --untracked -- ':(glob,icase)**/.gitattributes'` over the work tree, which passes over ignored folders.

  The check, and the one a failed checkout makes, runs with the work tree as its working directory (`WorkGit::quiet_in_tree`): `git grep --untracked` walks the folder git was started in, not the folder `--work-tree` names, and sshd starts the box in the ssh user's profile, where a walk once hung for hours in the profile's junction loop (the ssh token's SeBackupPrivilege gets git past their deny entries). A decoy `.gitattributes` in the box's start folder is not taken for the tree's.

  When it exits 0 the failure says `gate: remote sync-back failed: the command added a file under the LFS filter, which a run without LFS content cannot bring back; run it with --local`, and otherwise it is git's own `add failed (status <n>)` line, with the repo's name on a pair run.

  A check that cannot be started or waited for says `gate: remote leftover-LFS check could not run: <error>` on stderr first, and the checkout's or sync-back's own line follows as for any other answer of no.

  On a pair run the back refs are all or nothing across the repos: every repo's tree is committed first, the main repo's first (the one `Index` file serves each in turn), then every changed repo's commit is pushed to `refs/gate-back/<key>` in its own bare repository. A push that fails deletes the back ref of every repo pushed so far and the failing repo's own, each deletion tried and a failed one said, and the run fails as a sync-back failure.

  A client that closed the connection during that undo ends the run with 255 instead. After all pushes, a pair run says under the output lock `gate: remote back commits: <name> <sha|none>, ...`, the main repo first and `none` for a repo whose tree is unchanged. The outputs step packs the main repo's tree only.

  A pair run that does not push every back commit says `gate: remote back commits: failed` instead, under the output lock so it is the last such line and a line the command printed itself never stands for it: after a failed lock, stop event, back ref clear, layout step, checkout, subfolder check or tools restore, a command that cannot be waited for or that the ceiling killed, and a failed commit or push (after the undo above).

  A failed wait ends the job and drains its output first, so no line of the command follows. A refusal (`busy`, `layout`, `tools`) says no such line, and a run of one repo never does. The PC applies nothing of a `failed` pair.

  A payload with `Outputs` or `FailureOutputs` globs adds these steps (GATE-44):

  - Once the heartbeat beats and before the checkout, `run` reads the PC's manifest from stdin, the line after the request, up to its newline (at most 64 MiB; a manifest that cannot be read is said only once the command has passed), then removes the old archives from `OutputFolder`: each file named `<key>-<32 lower-case hex digits>.tar.zst`, whatever the key, last written more than a day ago, and no other file.

    An archive already gone counts as removed; one that cannot be removed, or a folder that cannot be listed, is said (`gate: remote could not remove the old outputs archive <path>`, `gate: remote could not list the old outputs archives in <folder>`) and fails nothing.
  - Just before the command, unless `OutputsFull`, it lists the files under the globs (size and last write); a listing that fails is said (`gate: remote could not list the outputs before the command: <error>`) and leaves every file counting as changed.
  - After a command that exited 0 and its back commit, or one that ran to its end and failed (nonzero exit, not killed) with its back commit made, with the commit pushed, it reads the commit's paths (`git diff-tree`) and packs the files to bring back into `<OutputFolder>\<key>-<nonce>.tar.zst`, created new.

    It says `gate-remote-outputs: <n> <archive name>` on stderr (`<n>` counts every path the archive lists, copies included), or `gate-remote-outputs: 0` with no archive when nothing is to come back. Just before the first it says `gate-remote-outputs-size: <bytes>`, the pack's own count of the archive's compressed bytes (nothing reads the archive's size back). The paths are read only after the push, so failing to read them fails the outputs and never the sync-back.
  - A file the pack would take but whose name does not fit the archive (not plain, the list name or not Unicode), or that sits below a folder whose name is not plain, is skipped, and the box says `gate: remote outputs: skipped <n> file(s) with names this PC cannot take: <first path>` (`, …` after the path when n is above 1) on stderr once, just before its `gate-remote-outputs:` line; the run keeps its status, and the client echoes the line like any box line.
  - Right after a pack that wrote an archive, the box trims `OutputFolder` to at most 50 GiB of outputs archives (files named as in the first step; other files are neither counted nor removed). It removes the archives last written strictly before the run's own, oldest first (by path among equals), until the total fits; an archive written with the run's own or after it is never removed, so the folder may stay over the budget until the newer run's own trim.
  - The run's own archive is opened sharing reads only, so another run's trim cannot remove it mid-write; such a removal fails, is said as above (`gate: remote could not remove the old outputs archive <path>: <error>`) and that trim goes on to the next oldest. An entry whose metadata cannot be read is said (`gate: remote could not read the outputs archive <path>: <error>`, control characters escaped), left out of the total, and the trim goes on.
  - A run whose own archive alone is larger than the budget removes only its own archive, leaves the others, and says `gate: remote outputs not brought back: the archive is <X.X> GiB, more than the box keeps (50 GiB)`. A run whose own archive is gone before its trim says `gate: remote outputs not brought back: the archive was removed before it could be sent`. Either turns a passed 0 into 1.
  - A pack that fails removes its half-written archive, says `gate: remote outputs not brought back: <reason>` and turns the passed 0 into 1 (a failed run ends on the command's own status, whatever the pack does). A killed command or a failed sync-back packs nothing.
  - A failed command (exit other than 0; a killed one packs nothing) packs only the files matched by the failed run's set: the output globs that name a `TestResults` folder (`Globs::test_results_only` in `gate-inputs`: a folder segment, not the last, equal to `TestResults` in any letter case) followed by the `FailureOutputs` globs (`failure::failed_run_globs`).

    It says its usual `gate-remote-outputs-size:` and `gate-remote-outputs:` lines, or `gate-remote-outputs: 0` when nothing matched. With neither kind of glob a failed run packs nothing and says no outputs line.

    A passing command never packs a file only a failure output glob matches.
  - A file only a `FailureOutputs` glob matches is held to caps in walk order (`gate_inputs::failure`): one over 256 MiB, or one that would take the failure files past 1 GiB in all (it and every later one are left; a file over the per-file cap does not count toward the total), stays on the builder. A file the `TestResults` globs match is never capped.

    Each file left is said, before the size line, as `gate: remote failure output left on the builder: <path> (<n> MiB, past the <cap> cap)`, `<n>` rounded up and `<cap>` `256 MiB` or `1 GiB in all`. The Windows box's seam `--test-box-failure-cap-bytes <n>` sets the per-file cap to `n` bytes and the total to `4n`; the Unix box always uses the built-in caps.
- **`stop`** sets the job's stop event, then terminates the payload's job, taking no lock. It terminates the job again on every 100 ms poll of the job's process ids, until the job is empty or 10 s pass, so a process that joins late is ended; its processes end with 124.

  A terminate after the first that fails is not a failure of the stop: it is said once, `gate: remote could not terminate job <name> again: <error>`, after the other lines. An event that cannot be set prints `gate: remote could not signal the stop of job <name>: <error>` on stderr, and the job is terminated all the same. A stopped job prints `gate: remote stopped job <name>` on stdout and exits 0; no such job exits 0 with no output.

  The stop also ends the run's tool restore job `<name>-tools` and its package install job `<name>-packages` when they exist: it opens each before it sets the event and terminates it after the payload's job, with the same lines and rules for `<name>-tools` and `<name>-packages`. It exits 1 when any job is left with processes or cannot be stopped, else 0.

  Processes still alive 10 s after the termination print `gate: remote job <name> still has <n> process(es) 10 s after it was terminated` and exit 1, and a job that cannot be terminated prints `gate: remote could not stop job <name>: <error>` and exits 1.

- **`outputs`** sends the archive the payload's `Name` names in its `OutputFolder` raw on stdout, then removes it. It takes no work-folder lock, so it never waits on a run, and the drain never refuses it.

  It renews a live drain session. For a session that is dead or missing under a standing barrier it still sends the archive and keeps it, saying `gate: remote outputs kept: the builder is drained` on stderr; a drain error also sends and keeps it.

  A payload without `Name`, `Work` or `OutputFolder`, or with a `Work` or `OutputFolder` that is not the work folder's own, exits 2.

  A `Name` that is not `<Key>-<32 lower-case hex digits>.tar.zst` for the payload's own key (`gate: remote outputs archive name '<name>' is not an outputs archive of this work folder`), an archive that does not exist (`gate: remote outputs archive <name> does not exist`) or one that cannot be read or sent exits 1. The file is opened without following a link, and only a plain file is sent.

  The Unix box answers `outputs` too, with its own folder modes and descriptor walk (Unix box differences: [The Unix box](#the-unix-box), `outputs`); the run does not yet bring outputs back there.

  A payload with `Nonce` and `Channel` (`Address`, `Peer`, `BoxProof`, `PcProof`; a malformed one exits 2 with a usage line that shows no proof, `Address` may not be `0.0.0.0` nor, outside a `test-seams` build, loopback) has the archive sent over a [data channel](GLOSSARY.md) and not on stdout. Only once every check above passed and the archive is open as a plain file does the verb listen on `Address`.

  It listens on a port of 47100-47131 tried from a random start upward with wraparound, bound exclusively (never shared) and not inheritable, and says `gate-remote-channel: <nonce> <port>` on stderr. It takes the first connection from `Peer` within 10 s (any other peer's is closed unread), sends `BoxProof`, reads the PC's 16 bytes within 10 s and checks them against `PcProof`.

  It then sends the archive in 1 MiB chunks over a 16 MiB send buffer, each read and write waiting at most 10 s, reads the PC's 32-byte ack and compares it with the SHA-256 of what it sent. A matching ack makes it say `gate: data channel sent <bytes> bytes sha256 <hex>` on stderr, do what the ssh send would (remove the archive, or keep it under a drain), wait up to 10 s for the PC to close the connection and exit 0.

  Any other end (no connection, a wrong proof, a stalled or gone peer, a wrong ack, a gone client, no free port) keeps the archive, says `gate-remote-refused: <nonce> channel` then `gate: data channel not completed: <why>` and exits 75, so the PC sends over ssh; the why never holds a proof.

  The Windows box's seams are `--test-box-channel-ports <first>-<last>` (any ports within 1-65535 in place of the range), `--test-box-channel-accept-ms <n>` and `--test-box-channel-idle-ms <n>` (replacing the 10 s windows) and `--test-box-channel-send-buffer-bytes <n>` (1 B to 16 MiB), all test-seams only; the fake-ssh test bin hands the words of `GATE_FAKE_SSH_BOX_SEAMS` to an `outputs` call as to the other box verbs.

  A sent archive that cannot be removed prints `gate: remote could not remove the outputs archive <name>: <error>` and still exits 0; a later run's prune takes it. An archive already gone at its removal (another run's trim took it after it was sent) says nothing and exits 0.

- **`inflight`** counts the routed runs holding a lock in the payload's `Root`, for the cutover's drain. It reads only `Root`; a trailing separator is folded off.

  For each `{Root}\work\*.lock` (any case) it opens the file for reading with read, write and delete sharing. A sharing violation means a routed run holds it, since the run and the PowerShell wrapper open the lock share-none. The probe's handle closes at once, so a run that starts after the probe can take the lock, while a run whose open lands during the probe is refused busy (exit 75) and its PC falls back locally.

  It prints `gate: remote in flight <n>`, then `gate: remote in flight <key>` for each held lock, the keys escaped and sorted, and exits 0. A missing `Root` or work folder counts 0. A lock it cannot open for another reason counts as held and reads `gate: remote in flight <key> (cannot read: <error>)`.

  A work folder it cannot list (anything but not found) prints no count line, `gate: remote cannot list <work>: <error>` on stderr, and exits 1. A refused payload exits 2.

- **`inputs`** (GATE-22, `boxside/inputs.rs`) installs a routed run's ignored inputs into the work folder in two calls, the payload's `Step` being `diff` or `put`. The payload holds `Key`, `Bare`, `Work`, `Lock`, `Nonce` (32 lower-case hex digits) and `Step`; a payload with a `Pair` and no `SentBin` is refused with `gate box: inputs for a sibling pair are not taken yet` and exit 2, and any other refused payload exits 2.

  A sibling pair's sent build (a `Pair` with a `SentBin`) is taken: the main repo's tree is `<Work>\<leaf>`, so each path of its `SentBin` and manifest is read as `<leaf>/<path>` below `Work`, the work folder is laid out as the pair's run lays it out (the main repo in `<leaf>`, each sibling in its own folder with its own bare repository), the `.sent-build` record names the `<leaf>/` paths, and the exclude file written is the main repo's, `<Work>\<leaf>\.git\info\exclude`, holding the paths below `<leaf>/` with that prefix cut.

  The diff's `path` lines name the PC's paths, without the `<leaf>/`. A run's `SentDigest` check writes the same exclude file.

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

  A stages folder that already exists is adopted only when it is a folder of its own whose owner, DACL and label are exactly those (`is_protected_folder`), read through the handle held; anything else at its name, one the box cannot open included, refuses the put with `cannot use <path>: its owner, DACL or label is not the box's; remove it` and is never walked, emptied or replaced. A stage of the put's own name already there is refused.

  The root, the stages folder and the stage are held without delete sharing for the whole put, and the stage is removed beneath its handle on every way out; a stage older than a day (any work folder's) is removed by the next put, a stage that cannot be opened or removed says so and stays (`gate: remote kept the stage <path>: <why>`, `gate: remote could not remove the old stage <path>: <error>`).

  Caps: an archive of more than 50,000 members, or that decompresses to more than 4 GiB (`--test-inputs-budget-bytes` replaces it), is refused as the outputs archive's are, and the put fails (`gate: remote inputs put failed: <why>`, exit 1). The step is not watched on the box; the PC watches both calls ([The remote client's run](#routing), Inputs).

  The Unix box answers `inputs` too, with descriptors for handles and its own stage, modes and record fields (Unix box differences: [The Unix box](#the-unix-box), `inputs`); it checks the run's `InputsDigest` too, and refuses `SentBin`.

  A payload with a `SentBin`, `<project>/bin/<configuration>` below `Work` (`/`-separated, a plain path whose second-last name is `bin`, else `gate: box payload SentBin is not a plain <project>/bin/<configuration> path: <value>` and exit 2), sends the PC's build of that project instead of the repo's inputs.

  It installs into its own record, `<Work>.sent-build`, of the inputs record's format and with its own digest, beside `<Work>.inputs`; a layout change removes both records before it clears `Work`.

  A sent build's manifest may name only files below `<project>/bin/<configuration>/` and restore files of `<project>/obj/` itself; any other path fails the step (`the sent build's manifest names '<path>', which is neither below <bin>/ nor a restore file of <project>/obj/`).

  Its diff also fails on a needed path a put could not install to, as a file of a second name, and on a path of its manifest the repo's inputs record holds (`the repo's inputs hold <path>, which the sent build names too`), before anything is uploaded; the client then takes the drop, its log saying `gate: remote build not sent:` with the reason.

  A restore file is a file of an `obj` folder whose name ends `.nuget.g.props` or `.nuget.g.targets`, in either step: one on the box counts as matched whatever it holds, recorded with the manifest's hash; one at a member's target refuses the put (`the builder's own restore file is there`); and a put never deletes one. A diff that finds one restore file of a folder on the box without the other the manifest names fails (`the restore file '<path>' is on the box without '<path>'`).

  Once a sent build's put has every file of its manifest, it deletes each plain file of one name below `<project>/bin/<configuration>/` that neither record holds, reached through handles as above; folders, links and junctions stay, and a file that cannot be deleted says `gate: remote could not remove the leftover build file <path>: <error>`. Both records' paths go into the one exclude file, whichever record an install or a run's check writes it from.

  Deployment prerequisites: no other account may rename or replace `<root>` (a protected root cannot be renamed away by others, since the Authenticated Users grant on `D:\` has no delete-child right); the installer's `Gate work root` step applies and verifies the root's security (owner Administrators, protected DACL); no account outside SYSTEM, Administrators and the builder's user may write in `<root>`; and `<root>\work` is not writable by other accounts.

  The stage's protection and the beneath-handle walk assume it; the box checks the stages folder it adopts, not the folders above it.
- **`lfs`** (GATE-469, `boxside/lfs.rs`) installs a routed run's Git LFS objects into the repo's store in two calls, the payload's `Step` being `diff` or `put`, with the payload keys of `inputs`; a payload with a `Pair` is refused with `gate box: lfs for a sibling pair is not taken yet` and exit 2, and any other refused payload exits 2. The store is `<Bare>\lfs\objects\<aa>\<bb>\<oid>` (Routing, Git LFS).

  Stdin is the request line, then the PC's LFS manifest as one line (space-separated `<oid> <size>` entries, each oid 64 lower-case hex digits, no oid twice, at most 50,000 entries and 64 MiB), then for `put` the archive of exactly the objects the diff named, in the outputs archive's format, each member named by its oid, to the end of stdin.

  A manifest that cannot be read, an unelevated box and any failure of the store refuse the step with `gate-remote-refused: <nonce> lfs` then `gate: remote lfs <diff|put> failed: <why>` and exit 1; a work folder that cannot be locked or laid out is refused the same way, and a busy one prints `gate-remote-refused: <nonce> busy` and the busy line, exit 75, as `inputs` does.

  The diff takes the work folder's lock as `run` does and readies the store under it. The put readies the store once before it makes its stage, outside the work folder's lock, and again under that lock when it installs. Readying the store takes its lock `<Bare>\lfs.lock` shared, opens the bare repository as itself and holds it without delete sharing, then makes `lfs`, `objects` and each `<aa>\<bb>` folder with the box's protected security or adopts it only when it holds exactly that.

  Only the store's `lfs` folder is ever moved aside.

  One found there with an owner, DACL or label that is not the box's, and the shape a plain git-lfs `CreateDirectory` leaves (a DACL without the protected flag and no mandatory label, as a checkout before the box kept the store left it), is renamed to `{Bare}\lfs.gate-moved-<n>` (`n` from 1 to 100, never replacing an entry) and a new protected `lfs` is made, with the line `gate: remote moved aside the folder {Bare}\lfs to {Bare}\lfs.gate-moved-<n>: its owner, DACL or label was not the box's`.

  The takeover never follows a link (a junction at `lfs` is refused) and never walks or empties the moved folder. It opens the folder again as itself with `DELETE` access and without delete sharing, checks its shape once more on that handle (`gate_win32::is_inheriting_unlabelled`), and renames it by that handle. Two first takeovers at once both succeed: the one that loses the race adopts the folder the other made.

  Any other `lfs` whose owner, DACL or label is not the box's, and every other folder (`objects`, the two-level object folders, the stages, the scratch folders), refuses the call with `cannot use <path>: its owner, DACL or label is not the box's; remove it`.

  Four more refusals belong to the move: `cannot use <path>: another process holds it open, so the box cannot move it aside; retry, or close what holds it and remove it`, `cannot use <path>: the box may not move it aside (<error>); remove it`, `cannot use <path>: cannot move aside the folder lfs (<error>); retry, or close what holds a file in it and remove it`, and `cannot use <path>: cannot move aside the folder lfs: lfs.gate-moved-1 to lfs.gate-moved-100 are all taken`.

  The diff prints `gate-remote-lfs: need <n>` then `gate-remote-lfs: oid <oid>` per object, in manifest order, that the store lacks or holds as anything but a plain file of that size that carries only the security its folder hands down. The put prints `gate-remote-lfs: installed <n>`.

  The put unpacks the archive before it takes the work folder's lock, into a stage `<root>\lfs-stage\<key>-<nonce>` made as the inputs stage is, and refuses the whole archive when a member is not an object the manifest names, does not hash to its name, or is not of the manifest's size.

  It then installs each object under the work folder's lock by a rename beneath the held folders that creates the file new and gives it its folder's security; an object already in the store at its size, another put's among them, is kept and its staged copy deleted. The stage is removed beneath its handle on every way out, and the stages' day-old prune covers `lfs-stage` and the run scratch folders. The step is not watched on the box; the PC watches both calls.

  Caps: an archive of more than 16 GiB unpacked (`--test-lfs-budget-bytes` replaces it) is refused as the inputs' is.
- **`register-desktop`** (`boxside/register.rs`) registers the desktop helper's task, `\gate\desktop-helper`, through the Task Scheduler's COM interface (`gate_win32::register_task`, `task.rs`) for the user running the box, and says so on stdout.

  It reads no request and never reads stdin: its ssh line is `gate.exe box register-desktop` with no `--stdin`, and any word after the verb is the box usage. It is run by the builder's setup and by `gate push` on every push ([`gate push`](#gate-push)), not by a routed run.

  The task runs `gate-desktop-helper.exe`, beside the box's `gate.exe`, with the one argument `$(Arg0)`, which `RunEx` fills at each start, on demand (no trigger), with the interactive token of that user at least privilege and no password.

  The `\gate` folder and the task carry a protected descriptor, owner Administrators, full access for SYSTEM and Administrators, and read for that user's SID. A folder that does not exist is made with it; a folder or a task that exists and whose owner or DACL fails the check `run_task` makes (anyone but SYSTEM and Administrators able to do more than read it) is refused, never adopted, and nothing is changed.

  A task that passes is updated in place. Once registered, the task is read back and checked as `run_task` checks it, without being started.

  On success it prints `gate-register: registered \gate\desktop-helper for <user SID>` and exits 0. Otherwise it prints `gate-register: failed: <why>` on stdout and exits 1.

  The whys: `the running user could not be read: <error>`; `the desktop helper's path could not be read: <error>`; `the Task Scheduler did not answer in 10 s`; a refusal, `the task is set up wrong: <what differs>`; and a failed call, `<call>: <system message> (0x<HRESULT>)` (a helper path that is not Unicode or holds a `%`, and a task read back that fails the check, are also `failed`).

  A squatted folder or task is cleared by an administrator deleting it, after which the verb registers again.

  A `test-seams` build given `--test-box-register ok|fail` skips the Task Scheduler: `ok` says the registered line, `fail` says `failed: the test seam refused`.

- **`prune`** (`boxside/prune.rs`) removes the work folders of keys idle for over 24 hours, from `gate box prune --root <folder>` (an absolute path on a drive; anything else prints `gate: box prune --root is not an absolute path on a drive: "<value>"` and exits 2), or the named keys' whatever their age, from `gate box prune --root <folder> --keys <key> [<key>...]`.

  It reads no request and never reads stdin, and any other shape is the box usage. The builder's scheduled task runs the age form every 6 hours ([Builder setup](routing-a-repo.md#builder-setup)).

  The `root`, `{root}\work` and `{root}\pruned` must each be a plain folder, else `gate: remote could not prune: <path> is not a plain folder` and exit 1; a folder it cannot list prints `gate: remote cannot list <path>: <error>` and exits 1. Leftovers in `pruned` are deleted at the start of each sweep, and one that cannot be removed prints `gate: remote could not remove the pruned folder <path>: <error>`.

  For each key under `work` it takes `{key}.lock` (a lock it cannot open prints `gate: remote could not open the work folder lock <path>: <error>`), so a key whose lock a run or a sweep holds is kept, and `inflight` does not see a key whose lock a sweep holds. Idleness is the newest write time among the folder, each tree's `.git\FETCH_HEAD`, the key's records and the lock's write time (the stamp of the last take or release by a run, `inputs` or `lfs`).

  A key idle for over 24 hours loses its records in this order: `.layout` is written as `changing` first; `.warm`, `.packages`, `.tools`, `.inputs`, `.sent-build` and `.command` are removed; the folder is renamed by handle into `{root}\pruned\{key}-{pid}-{nanos}`; `.layout` and `.index` are removed. The lock is released and the renamed folder is deleted through its handle. The `.lock` file stays.

  Every step works beneath held handles and never follows a link. A failed rename leaves `.layout` reading `changing`, so the next run clears the folder whole.

  A key it cannot prune prints `gate: remote could not prune <key>: <reason>` on stderr (reasons include `its work folder is not a plain folder` and `its work folder changed during the sweep`), keeps the key, and the exit stays 0.

  On stdout it prints `gate: remote pruned <key>: idle <h> h` per removed folder and, last, always, `gate: remote prune removed <n> of <m> work folders`.

  The age form deletes no ref: a worktree still alive may have pushed `refs/gate/{key}` just before the lock was taken, and deleting it would fail that run's fetch. Only the keyed form deletes refs, below.

  Under a standing drain barrier ([`drain`](#the-box-side), below) the age form touches nothing and says `gate: remote prune skipped: the builder is drained` on stdout, exit 0. A barrier that cannot be read is `gate: remote prune failed: {why}` on stderr and exit 1. The barrier read is read only: the prune takes no drain lock, and a `drain` entry that is a link, junction or file reads as no barrier.

  **The gc.** After the sweep, once the root passes `check_work_root` (checked only when the root holds a `*.git` name; a refusal says `gate: remote prune gc failed: {root}: {why}`), the age form runs `git --git-dir={root}\{name} -c gc.autoDetach=false gc --auto` in each whole bare under the root, in name order, one at a time.

  Each run is in a kill-on-close job with a 30-minute bound, and the barrier is read before each bare: a standing one ends the loop with `gate: remote prune gc stopped: the builder is drained`.

  A bare whose gc fails says `gate: remote prune gc failed: {bare}: {why}` on stdout and the exit does not change; a gone client ends the loop. The step is named `gc` for `--test-box-git-limit`. This is the cleanup a push no longer starts: a push to a Windows builder runs `git-receive-pack` with `receive.autogc` off (Push, in the remote client's section).

  **The keyed form** takes 1 to 50 key words, each `[A-Za-z0-9_-]+` and not starting with `-`; any other shape is the box usage (status 2), and a repeated key, in any letter case, is pruned once under its first spelling. A word that is no key, past the parse, prints `gate: box prune --keys holds a word that is not a key: "<word>"` and exits 2 before anything is opened.

  It judges no age and touches no key it is not given. Each key's lock is still taken without waiting, so a held lock keeps the key. A link or junction work folder is refused, a key with no folder has its records and refs removed, and a missing `work` folder is made, as a run makes it.

  It judges no age and touches no key it is not given. Each key's lock is still taken without waiting, so a held lock keeps the key. A link or junction work folder is refused, a key with no folder has its records and refs removed, and a missing `work` folder is made, as a run makes it.

  The barrier is read before the sweep and again before each key: under a standing one each remaining key says `gate: remote kept {key}: the builder is drained` on stdout, and an unreadable barrier is `gate: remote prune failed: {why}` on stderr and exit 1.

  For each key whose folder it set aside or found absent, still under the lock, it runs one `git update-ref --no-deref --stdin` in each bare repository `{root}\*.git`, deleting `refs/gate/{key}`, `refs/gate-bridge/{key}` and `refs/gate-back/{key}` themselves, never what a symbolic one points to.

  A `*.git` entry is used only when it is a plain folder holding `HEAD`, `objects` and `refs` (checked beneath a handle that is held, without sharing deletion, while git runs, so it cannot be renamed and swapped); a link, a plain file or a folder lacking any of the three is skipped.

  A bare whose update fails does not stop the others, and the first failure is the key's reason. A key whose lock is held or whose folder could not be set aside keeps its refs. The git step is named `update-ref` for the `--test-box-git-limit` seam.

  Before any git step the root is checked as the preflight checks it (`gate_win32::check_work_root`, honouring `--test-box-root-walk-stop <folder>`). When the check fails no git runs: every key keeps its refs and gets `gate: remote could not prune {key}: its refs could not be removed: {the check's reason}`, while its folders and records are still removed.

  A key the builder never had is left no lock: a `{key}.lock` the keyed form created is deleted as it is let go (a failure says `gate: remote could not remove the work folder lock {path}: {error}`), while a lock that existed before stays.

  It prints `gate: remote pruned {key} (worktree gone)` and `gate: remote kept {key}: in use` on stdout, and `gate: remote could not prune {key}: {why}` on stderr (reasons include `its work folder is not a plain folder`, `its refs could not be removed: {the first failure's reason}`, and `the root's drive is not there`, said for every key when the root's drive is absent; a missing root on a present drive holds nothing and answers `pruned`).

  A lock that cannot be opened, other than one held, and a record left after the rename are each the key's single could-not line, with no `pruned` line. Every key gets exactly one line. It prints no summary line, and exits 1 when any key got a could-not line, else 0.

- **`drain`** (`boxside/drain.rs`, its I/O; the rules both boxes decide by, the barrier, tickets, admission, the lines and the process count, are `gate_wire::drain`) is the builder drain's barrier, from `gate box drain --stdin` with the payload `{"Root","Action"}`, `Action` being `raise`, `status` or `lift`.

  It answers each call once and exits; the PC's caller is `gate drain` ([`gate drain`](#gate-drain)), which loops over it. A missing `Root` or an `Action` that is none of the three is the usage line and exit 2. `preflight`, `run`, `inputs` and `lfs` pass the drain's admission (below), so a standing barrier refuses their new runs.

  Everything lives under `{Root}\drain\`, which the box makes with a High integrity label and no write-up, so a routed command at medium integrity cannot delete the barrier or its own session. A `drain` that is not that folder (a link, a junction, a file, or a folder of other security) is renamed aside as itself and never followed, and the call says `gate: drain moved aside {path} to drain.gate-moved-{n}: it was not the box's protected folder`.

  The root is held as the preflight holds it (read access, no delete sharing), and a raise makes a missing root and `drain\`; a `status` or a `lift` of a root or `drain\` that is not there answers as no barrier and changes nothing.

  - `lock`: the drain lock, shared with no one, held around each read-modify of the files below and never across a wait or a child process; a call that finds it held tries again for 10 s, then fails.
  - `barrier`: `{"Raised","Lapses","Generation"}`, written by a raise. It lapses 30 minutes after the last raise, and a raise while it stands raises the generation by one. A barrier file that does not read stands as one raised at its last write, so a damaged one holds the drain for one lapse and no longer.
  - `lapsed` and `lifted`: `{"At"}`, the time the barrier lapsed or was lifted. A lift removes `barrier` and `lapsed` and writes `lifted`.
  - `tickets\{seq:08}-{session}`: `{"Session","Polled"}`, one per run waiting in line; a ticket is live for 120 s after its last poll, the box's boot or the last lift or lapse, whichever is latest.
  - `sessions\{session}`: `{"Pc","Repo","Command","Job","Called"}`, one per admitted run, live for 120 s after its last call. `sessions\{session}.{pid}` is the holder each admitted box process keeps open, shared with no one, so a session is live while its process lives.

  A call that reads the drain answers with one line each, in this order: `gate: drain barrier {raised} {lapses}` (Unix seconds), `gate: drain barrier none` or `gate: drain barrier lapsed {at}`.

  Then `gate: drain session {age_s} {json}` per live session, the json `{"Pc","Repo","Command"}` with `null` for a field it lacks; then `gate: drain process {pid} prune|git|job` per process the drained test counts; then `gate: drain command {key} unreadable` per work folder whose `.command` record could not be read and whose job no live session names; then `gate: drain tickets {n}` (live tickets); and last `gate: drain state drained|waiting|open`.

  The state is `drained` under a standing barrier with no live session and no counted process, `waiting` under one with any, and `open` with none standing.

  The drained test counts a process whose command line holds `box prune --root {root}` (`prune`), a process whose command line names the root, with every descendant (`git`), a process of a job a `{Work}.command` record names (`job`), and a git, gate or pwsh image whose command line cannot be read, which counts as a match. The call that finds the barrier standing with nothing live removes the sessions it judged dead.

  The exit status is 0 once it answered, 1 after `gate: drain failed: {why}`, 2 on usage. The test seams `--test-box-drain-lease-ms <n>` (the 120 s lease), `--test-box-drain-lapse-ms <n>` (the 30 minute lapse), `--test-box-now <unix s>` (the time the drain judges at) and `--test-box-drain-readable-only true|false` (count only processes whose command line it reads) go before the verb.

  `admit` in the same module is the one admission function the routed verbs pass: in one hold of the lock it admits a call or refuses it, a refusal being the `Refused` place in line with the barrier's raise and lapse times.

  It writes the session record and takes the holder on admission, and takes or renews the ticket on a refusal unless `NoTicket`. The refusal tag word `drain` (`gate_wire::Refusal::Drain`, `gate-remote-refused: {nonce} drain`) is for that refusal; the PC's run waits on it ([Routing](#routing), Builder drain).

  **The gated verbs.** `preflight`, `run`, `inputs` and `lfs` pass `admit` after the work root is held and before their work, with the payload's `Session`, `NoTicket`, `Pc` and `Repo` and the command and job they record. A call with no `Session` on a root with no `drain\` folder makes nothing and goes on.

  An admitted call writes `sessions\{session}` (`Pc`; `Repo` capped at 100 characters; `Command` capped at 300; `Job`; `Called`) and holds `sessions\{session}.{pid}` while it runs. At its end it stamps `Called` with its end time, taking the drain lock if it is free, so a session's 120 s lease runs from the end of its last call. A call passing an empty command or job keeps the recorded ones. `run` records its job and its commands (a warm-up's joined by ` && `).

  A refused `preflight` says `gate: remote version {v}` on stdout, then on stderr and unstamped: `gate-remote-refused: {nonce} drain` (when the payload has a `Nonce`), `gate: drain place {n}` or `gate: drain place none`, and `gate: drain barrier {raised} {lapses}` or `gate: drain barrier none`, and exits 75. Unless `NoTicket`, it leaves a ticket `drain\tickets\{seq:08}-{session}`.

  A refused `run`, `inputs` or `lfs` says the same stderr lines, leaves no ticket and exits 75; `inputs` refuses before it reads its stdin manifest, and `lfs` is tagged `drain`, not `lfs`. A drain error from any gated verb is `gate: drain failed: {why}` on stderr and exit 1 (a preflight says its version line first, and a sibling pair's `run` also says `gate: remote back commits: failed`).

  **Order and liveness.** A ticket is numbered under the lock at a run's first refused preflight, keyed by its session, and renewed on each poll, never renumbered. A run is admitted only when no older live ticket waits, even with no barrier standing, so runs that arrive after a lift queue behind the waiters. `gate check --builder` and warm-ups send `NoTicket` and never queue.

  A session is live while a verb holds its holder, within 120 s of its last call, or while the job its record names (or a `{Work}.command` record) still holds a process, so a spared nested gate after a clean end keeps the drain waiting.

  A dead session never revives: a verb that finds its session dead under a standing barrier is a new arrival, and `inputs`, `lfs` and `run` refuse it with `drain`, the PC restarting from the preflight. A payload with no `Session` is refused `drain`, status 75, under a standing barrier.

  The barrier is files under a lock, not a Windows mutex, whose waiters are not served in arrival order, whose `Global\` names can be squatted and whose private namespace needs elevation.

  The drain lock is also the one lock every take and judge of the builder-side slots runs under (`under_drain_lock` in `boxside/drain.rs`, which readies `drain\` as an admission does and gives the slot code the drain's judge of whether a session is live for a call of its own), so a session's liveness and its slot are decided in one order. A preflight's take comes after its admission has let the lock go, a run's release before its session's holder is let go.

- **`hold`** (`boxside/slots.rs`) is `gate box hold --root <root> --holder <text> --ceiling <seconds> [--wait <seconds>] -- <command> [args...]`: it takes one of the builder's own slots ([Builder-side slots](#slots)) for a command that is not a routed run (a GitHub Actions runner job), runs the command while it holds it and lets it go. It reads no request and never reads stdin, takes its words in any order, each once, and any other shape is the box usage, which has a line for it ([`gate box`](#gate-box)).

  `--ceiling` is whole seconds from 1, `--wait` whole seconds (0 when not given), both at most 604800 (seven days); the command is every word after `--`, at least one, kept as given. The command runs in a job of its own that ends every process in it when closed, with the hold's environment and working folder, its program found on the hold's `PATH`, its stdin `NUL`, and its stdout and stderr passed through to the hold's own as they come.

  A root that is not there or has no `slots.json` that reads says (after the unreadable-count line, for one that does not read) `gate box: this builder has no slot count; run gate push` on stderr and exits 2. A holder that is empty, holds a control character or is over 300 characters says `gate box: the holder '{holder}' {why}` and exits 2. A root the work-root check refuses is `gate: hold failed: {why}`, exit 1.

  It looks for a free slot under the drain lock: with none, it waits up to `--wait` seconds, looking every 5 s (`--test-box-hold-poll-ms <n>` in milliseconds in a `test-seams` build) and saying `gate: waiting for a builder slot: {held} of {slots} held` on its first look and every 30 s, else says on stderr and unstamped `gate-remote-refused: hold full` and `gate: remote builder is full: {held} of {slots} slots held` and exits 75. With a slot it writes its holder claim and keeps `slots\{n}.lock` open, shared with no one, for its whole life; a hold killed by any means frees its slot, since a claim whose lock is not held is dead.

  Its exit status is the command's own. At the ceiling it ends the command's job, says `gate: hold ended at its ceiling of {s} s` on stderr and exits 124. A command it cannot start says `gate: hold could not start {program}: {why}` and exits 127. Any other failure is `gate: hold failed: {why}` on stderr and exit 1; a slot that cannot be let go says `gate: slot release failed: {why}`. The Unix box builds it too ([The Unix box](#the-unix-box), `hold`).

**The outputs module** (GATE-44) is shared by both ends, this exe, in `crates/gate/src/outputs/`. The box's `run` packs with it and the box's `outputs` verb sends the archive. The PC's manifest, the pending marker, the reading of the box's `gate-remote-outputs: ` line, the fetch of the archive and its install are built in the Rust remote client ([The remote client's run](#routing)).

- Globs: each is relative to the work folder's root and `/`-separated, matched without regard to case: `**/` is any number of folders (none included), `**` anything, `*` any run of characters within one name, `?` one character within one name.

  Never brought back, whatever the globs: anything under a `.git` or `.tmp` folder at any depth, a `.env` file (`.env`, `.env.<name>`, `<name>.env`), any path component starting `.git` (`.git`, `.github`, `.gitattributes`) and a `.lfsconfig` at any depth, spelled any way the sync-back's names check reads (`\` as a separator, a cut at the first `:`, trailing dots and spaces stripped, 8.3 aliases such as `GITATT~1` and `LFSCON~1`).

  A `TestResults` folder (any letter case) is never brought back either, except by a glob with a `/`-separated segment exactly `TestResults`, such as `tests/*/bin/*/*/TestResults/*.trx`: that glob brings back the files below the folders it matches, and the box's walk and the PC's install both enter such a folder, with `.git`, `.tmp`, `.env` and `.git*`/`.lfsconfig` still excluded below it.

  `**`, `tests/**` and a segment merely containing the word (`*TestResults*`, `TestResults2`) bring back nothing under `TestResults`. The input globs keep their own exclusions, and outputs come back on a passing command, or on a failed one only for the globs that name a `TestResults` folder.

  The box and the PC must agree on what a glob selects: a PC whose install check is older than the box refuses the whole archive when the box packs a member it does not select. A change to the matcher is installed on the PC first, then pushed to the builders, the order the reinstall already takes.

  Also never brought back: a path that is not plain (the member rule below), the archive's own list `.gate-outputs.json` at the root in any letter case, or a name that is not Unicode, each of which the box's pack skips and says.
- Walk: lists the files below a folder that the globs match, ordered by relative path, and does not enter an excluded folder, a `.git*` folder included, or a folder whose name is not plain or not Unicode, however deep.

  The box's pack also takes from it the files the globs would select but for their names, those below such a folder included, to say them.

  A link or a junction, to a file or to a folder, is neither followed nor listed. A folder that cannot be read, or an entry whose metadata cannot be, fails the walk; a name that is not Unicode does not.
- Manifest: the PC's list of its matching files, sent as one line of JSON and a newline, `{"files":{"<path>":{"size":<bytes>,"modified":<file time>,"sha256":"<hex>"}}}`, paths relative to the worktree root and found without regard to case.

  The box's pack takes each file the manifest lacks, and each it holds that the run created or changed (the box's stat listing from just before the command lacks it or holds another size or last write) whose SHA-256 differs from the manifest's, skipping the paths of the run's back commit. Under `OutputsFull` there is no listing, so every file counts as changed and each that differs from the PC's comes back.
- Archive: a tar stream compressed with zstd (level 3, long-distance matching, a window of log2 = clamp(ceil(log2(total bytes of the chosen files)), 20, 27), so a small archive uses a small window; the reader accepts a window up to 2^27), named `<key>-<nonce>.tar.zst`, each file read once and hashed as it is packed.

  The last member, `.gate-outputs.json`, is a JSON array with one item per chosen path in pack order, `{"path","size","sha256"}` for a member and `{"path","size","sha256","copy":true,"modified":<seconds since 1970>}` for a copy, and the reader checks the members against it after extraction. Both ends stream; memory does not grow with a file's size.

  A **copy** is an item carrying `"copy":true` and having no tar member: the tar holds only the first path of each distinct content that the PC does not already hold, and every later path with that content is listed as a copy.

  The PC holds a content when a file of the manifest sent with the run has its SHA-256 and is outside the paths of the run's back commit; the box never uses a back-commit path as a copy source. A copy carries the box file's own last write as `modified`, not its source's, and the install stamps it as it does a member (the snapshot time, not `modified`).

  Before the pack the box hashes only the chosen files whose size matches another chosen file's or a manifest-held file's, since only those can be copies. Copy or member is decided at the file's turn: a file hashed before the pack becomes a copy only when its size and last write still match the walk and its hash matches an earlier member's, or a manifest hash outside the back commit; otherwise it is packed as a member, with the hash from that read.
- Payload: `Nonce` is 32 lower-case hex digits, `OutputFolder` is `<root>\outputs` for the work folder `<root>\work\<key>`, `Outputs` and `FailureOutputs` lists of non-empty globs holding no `[` or `]` (`gate: box payload FailureOutputs is not a list of texts`, `... holds an empty glob`, `... glob '<glob>' holds '[' or ']'`), `OutputsFull` true or false, and `Name` an archive name of the payload's own key and nonce, so that the outputs verb reaches no file in `OutputFolder` but an archive of this work folder.

  A refusal is a `gate: box payload ...` line.

The reader (`read_members`, with the member checks, the limits, the long names and the list) is `gate_inputs::archive`'s, shared by both boxes; each box writes what it reads through the `Place` trait (`member`, and `copy` from an earlier member or a PC file).

The packer (`Packer`: `add` a member, `copy` an item with no member, `finish` the list and the zstd window) is `gate_inputs::pack`'s, shared by both boxes, as are `select` and the globs; the Windows file reads behind it (`pack_one`, `check_inside`) and the Windows writer, `unpack`, are in `crates/gate/src/outputs/archive.rs`.

`unpack` creates its destination, which must not exist, and refuses an archive for any of these, a member before anything of it is written:

- A member name that is not a plain relative path inside the destination: `.git` in any case, an 8.3 short name (`~` before a digit), a part ending in `.` or a space, any of `<>"|?*:` or `\`, a control or bidirectional character, a device name (`CON`, `PRN`, `AUX`, `NUL`, `CONIN$`, `CONOUT$`, `COM0` to `COM9`, `LPT0` to `LPT9`, also with an extension), or more than 64 parts.

  The rule is `is_plain_path` in `gate-inputs`'s `names`. The globs and the walk apply it too, so the box never packs such a name; the pack still refuses one (`the output '<path>' has a name the outputs archive's reader refuses`), a backstop the walk leaves unreachable.
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

After the members are extracted and checked, the reader hands each copy to the `Place`, and `unpack` makes it in the destination, from the first earlier member with that content, else from the first manifest path whose file below the caller's root still hashes to it (its stamp is not checked).

A copy whose bytes differ from its item is refused (`the outputs archive copy '<path>' does not match its list: ...`), and so is one no PC file still holds (`no PC file still holds the content of <path>, SHA-256 <hex>: tried …`, as above); these two are faults of the transfer or the PC, not of the members, so a refetch may clear them. The caller's `go_on` is asked before each copy, so a Ctrl+C is honoured between a copy's 64 KiB buffers.

`pack` fails on a link or a junction, a file last written before 1970, and a file whose size or last write changes while it is packed. On any refusal `unpack` removes the folder it made and all it extracted there; when that removal fails too, the refusal says so.

The console: `gate box run` gives the command a UTF-8 console through a donor console, as the PowerShell wrapper does. Right after it creates the run's job, the box starts a hidden donor, the system's `cmd.exe /d /q /k` (never one found on PATH), in the job. The donor holds a stdin pipe nobody writes so `cmd /k` does not exit, and lives for the whole run, until the wait for the command is over.

To create the command the box starts a console keeper `cmd.exe` on its own console, so that console stays alive, then takes the output lock, waiting at most 10 s. It leaves its console, attaches to the donor's and sets the input and output code pages to 65001.

It creates the command suspended in the job, without `CREATE_NO_WINDOW`, so the command inherits the donor's console, then checks the stop event and resumes it. It then returns to its own console, the console keeper's (`AttachConsole(ATTACH_PARENT_PROCESS)` as the last resort), ends the console keeper, and puts back its standard handles that were not console handles.

Before the command starts, a donor that cannot start, an attach or code page that fails, or an output lock not had in 10 s (reason `the box's output is blocked`) does not fail the run: the command runs on the plain hidden console and the box says `gate: remote could not give the command a UTF-8 console (<reason>); running it on the box's default code page`. The line comes after the launch, and only when the launch was not stopped.

There is no fallback after the command has started and no relaunch: a command that never shows on the console (a GUI program, one that calls `FreeConsole`, a slow starter) runs once.

The rest of the protocol, the transport of snapshots and outputs, is the remote port's design (GATE-10).

### The desktop run's design

The rules behind the desktop run (GATE-231) that a later change to the helper, the hand-off, the lock or the task keeps.

**Trust boundary.**

- The boundary is the builder's user against everyone else, not the box against the helper. Any Medium-integrity process of that user can already inject into a session-0 restricted command (its token's default DACL grants the user) and could replace a gate.exe the user can write, which the install tripwire refuses. The pid, handle and image checks guard against mistakes, stale helpers and mixed-up runs, not against the user's own processes.
- Only the builder user's SID may connect to the control pipe, and only locally. The box creates it with an explicit DACL for that SID alone (SYSTEM is refused), `PIPE_REJECT_REMOTE_CLIENTS`, `FILE_FLAG_FIRST_PIPE_INSTANCE`, one instance, a fresh 128-bit random part in the name that is not derived from the run's nonce, access narrowed to read and write (no `WRITE_DAC` or `WRITE_OWNER`), and a Medium mandatory label, so a Low-integrity process of the user cannot take the one instance.
- The helper checks the server too. The pipe's owner must be the builder's SID and its DACL exactly the entries the box writes, read with `GetSecurityInfo` on the open handle; `GetNamedPipeServerProcessId` must equal the box pid in the helper's argument, as a cross-check only, since a dead box's pid can be reused; and it connects with `SECURITY_SQOS_PRESENT | SECURITY_ANONYMOUS`, as an output file is opened.

  Without these, another identity that squatted a pipe name could impersonate the user's interactive token and choose what the helper starts.
- The read ends, the job handle, the stop event and the desktop lock never leave the box; only the two pipe write ends go out, into the helper and from there into the command. A planted job handle would keep the kill-on-close job, and the command, alive after the box dies.

**Where the command can run.**

- A desktop payload never runs in session 0. The desktop branch is its own function and never reaches the piped launch, the own-token retry or the console donor; a box that cannot get the desktop refuses. The PC's exact-version check keeps an older box, which would ignore the unknown `Desktop` key, from running the command in session 0.
- A helper's `Failed` is the command's own start failure, so it goes straight to the start marker and `could not start` (127), never through the check for a box job that refuses breakaway: that check asks about the box's own job, which says nothing of a process the helper created.
- The marker is said after adoption and before the stop look and the resume, which is as safe as saying it before the process exists: a suspended child runs nothing, and what waits in its pipes is forwarded after the marker.

**Adopting the child.**

- The child is taken by handle from the helper's handle table, never by `OpenProcess` on a pid; the pid is only a cross-check. The box copies the reported values out of the helper with `DUPLICATE_SAME_ACCESS` and closes the helper's copies itself with `DUPLICATE_CLOSE_SOURCE`, so it does not depend on the helper tidying up.
- It accepts the process only when the process id and the thread's process id both equal the reported pid, the parent is the helper and the child was created after it, the child is in the target session, its image is the application the box sent, it is not yet in the box's job, and it is suspended exactly once: `SuspendThread` returns a count of 1 and `ResumeThread` undoes it, and the final resume fails unless it returns 1, so a child suspended twice is never left suspended after the marker.
- After the assign it checks `IsProcessInJob(process, box_job)` against the box's job specifically; asking whether the process is in any job passes a child that is in some other job.
- On a failed check the box ends only what is provably the helper's, never the process behind an unverified handle, which could be any process of the user: the sweep of the helper's children also covers a helper that died between creating the command and reporting it, which would otherwise leave a suspended orphan holding the pipe write ends.

**The job.**

- The command is the first member of an empty job and nothing from session 0 ever joins it: no donor, no console keeper, nothing assigned but the child. Microsoft's documentation says the members of a job must share its session; an empty job took the session of its first member when measured, so the membership check is a hard requirement.
- The helper uses no `CREATE_BREAKAWAY_FROM_JOB` (refused), so the command nests below the Task Scheduler's job, whose most restrictive priority class wins over the command's.
- The command runs at normal priority, explicitly. A task's default priority of 7 gives its action below-normal CPU, I/O and memory priority, which a child inherits, so the helper passes `NORMAL_PRIORITY_CLASS` and the task is registered at priority 4. Otherwise the box's ceiling arithmetic, which assumes every core is demanded at normal priority, and the comparison with local runs would drift.
- The job is kill-on-close, unlike a session-0 command's. With one desktop, an orphan that outlived a dead box would hold the console while the next desktop run starts; the box's death ends the command instead. The PC's `stop` after a lost connection then finds no job and exits 0 silently.

**Output pipes and creation.**

- Pipe objects are not tied to a session; handles belong to processes. The write ends go into the helper as non-inheritable copies, so none leaks into a process the helper itself starts. The helper makes its own inheritable copies inside the spawn lock and hands exactly NUL and those two to the command through the handle list. The box's copies close once the command holds its own, the forwarders start only after the marker, and the read ends stay in the box.
- The helper creates the command by the box's own rules, through a `gate_win32` function and not a copy of them: `check_launch`, the handle list, the desktop-app policy `DISABLE_PROCESS_TREE`, `CREATE_NO_WINDOW` and `CREATE_SUSPENDED`, with `lpDesktop` set to `WinSta0\Default`.

**Environment and folder.**

- The box builds the environment block and the helper passes it on unchanged, by the code path a session-0 run uses (build servers, and the `safe.directory` entry, still needed because the token is non-elevated and the work folders are owned by Administrators). It travels as base64 of UTF-16LE, so unpaired surrogates survive.

  The helper builds its block from those bytes only through a constructor that refuses a block that does not end in two NULs or holds an empty or `=`-only entry, since `CreateProcessW` reads until the double NUL. Session-0 variables such as `SSH_*` go along.
- Every path sent to the helper must exist in the interactive logon session: the application, the working folder and the program the box resolved. Drive letters are per logon session, so a `subst` or `net use` drive made under the ssh logon does not exist for the helper, and the builder root must be on a real volume. A violation shows as a 127 after the marker.

**Reaching the command.**

- Everything goes through the box's own job handle, so the session does not matter: `gate box stop` opens the job by name from session 0, where the box created it; the kill, the ceiling's accounting, the heartbeat, the leftovers sweep (with no donor to spare), the client-gone path and the panic stopper all use it.
- The box never calls `IRunningTask::Stop` or `IRegisteredTask::Stop` after adoption, and the task is never registered with `StopExisting`: the Task Scheduler's job sits above the box's, and stopping the instance may end it. A lingering helper is ended with `TerminateProcess` only.

**The desktop lock.**

- The order is the work-folder lock, then the desktop lock, never the other way round. The desktop lock is taken after the checkout and the restores, so it is held only while the command runs, and on the run's main thread, which also releases it, since a mutex belongs to its thread; it is released once the job is empty and the output drained, before the back commit.
- The wait watches the stop event and has a deadline, and the heartbeats keep beating meanwhile, so the PC's stall check stays quiet. A lock found abandoned is a take: the dead holder's kill-on-close job is ending its command too, and a brief overlap is possible, since abandonment can come before the dead box's handles close.

**Session state.**

- Before the start the target is the active console session, owned by the builder's user, unlocked and on the `Default` input desktop. The box reads the session; the helper confirms from inside it in its hello that `OpenInputDesktop` succeeds and names `Default`, since a UAC prompt or the lock screen is a secure desktop.

  `RunEx` is given that session id, and with no user given the Task Scheduler runs the task as whoever is logged on to the session, which is why the check of the helper's token user matters. The builder logs its user on at the console after every reboot (autologon), which claude-config's provisioning of the builder sets up and keeps.
- A session change mid-run is a command failure, never a fallback. Logging off ends the session's processes, the box sees the exit and the status stands; locking, an RDP takeover and a disconnection let the command run on and fail as it fails.

**The helper dies, lies, hangs or is impersonated.**

| Case | What happens |
|---|---|
| Dies before connecting | The box's wait includes the helper's process handle, so it returns at once and refuses `desktop` with the exit status. |
| Hangs | Deadlines: connect 10 s, each message 5 s. Then `TerminateProcess`, the sweep and a refusal. |
| Lies (bad handle values, a pid that does not match, a running process) | Verification fails, the sweep runs, the box refuses. A helper's copy can never carry more access than the helper had. |
| Impersonated (another client connects first) | Pid mismatch, disconnect, keep listening. Endless reconnects run out the deadline and refuse: a denial of service that ends in a local run. |
| Squatter of another identity | The box's `CreateNamedPipe` with `FIRST_PIPE_INSTANCE` fails and it refuses before `RunEx`. A pipe the helper reaches is the helper's check, above. |
| Helper of another build | The hello carries the version text; a mismatch refuses `desktop`. |

**The task's settings.** The registered definition is part of the design, and the box reads it on each desktop run:

- instances `Parallel`, since the default `IgnoreNew` drops a run while an earlier helper is alive;
- priority 4, enabled and allowed to start on demand (`RunEx` returns `S_OK` for a disabled task, so the box checks it);
- not kept from starting nor stopped on batteries;
- no execution time limit (the box bounds the helper itself, and a limit could end the Task Scheduler's job) and no triggers;
- the builder's SID, interactive token, least privilege, which gives Medium integrity, and the helper's full path as the action with `$(Arg0)`.

A missing, disabled or different task is a refusal whose why names the fix.

### The Unix box

A macOS builder runs `gate-box` (crate `gate-box-unix`), not `gate.exe`. It speaks the box side's wire above, so the PC's client reads both alike: `gate-box --version` prints the same `gate <version>` text, and `gate-box box <verb> --stdin` reads the request as `gate box` does and parses it with `gate-wire` under `PathForm::Unix`. This section lists only where it differs.

- **Verbs.** `preflight`, `boot`, `run`, `stop`, `prune`, `inputs`, `outputs`, `drain` and `hold` are built. `run` readies the tree, runs the command under the watch and syncs the tree back (below), in the Windows box's order, the dotnet tool restore and the pnpm, bun and npm package restore (below) included.

  The run takes `Outputs`, `FailureOutputs` and `OutputsFull` and brings outputs back as the Windows box's run does ([The box side](#the-box-side)), through the `outputs` module below. Where it differs: a run with globs reads one more stdin line after the request, the PC's manifest (at most 64 MiB), after the heartbeat starts and before the layout and checkout, and a run without globs reads nothing past the request line.

  The lines come in this order: the prune's `gate: remote could not remove the old outputs archive <path>: <err>` or `gate: remote could not list the old outputs archives in <folder>: <err>` before the layout; `gate: remote could not list the outputs before the command: <err>` after the tool restore.

  After a command that exited 0 and pushed its back commits, or one that ran to its end and failed with its back commits pushed and an output glob naming a `TestResults` folder or a `FailureOutputs` glob (on a pair run after `gate: remote back commits: ...`), `gate: remote outputs: skipped <n> file(s) with names this PC cannot take: <first>[, …]` when any, then, for an archive, `gate-remote-outputs-size: <bytes>` (the pack's own count of compressed bytes) and `gate-remote-outputs: <n> <key>-<nonce>.tar.zst`, else `gate-remote-outputs: 0`.

  A PC found gone before the pack or before the trim ends the run 255 with nothing more said and nothing packed or trimmed after it; an archive already packed stays in the output folder until the 24-hour prune. The `diff-tree` step's output is read whole, never cut at the 1 MiB cap the other git steps keep.

  After those come the trim's lines; and last, on failure, `gate: remote outputs not brought back: <reason>`, which turns a passed run's 0 into 1. A failed command packs only the reports under its `TestResults` globs and the files under its `FailureOutputs` globs (held to the built-in caps, each file left said as the Windows box says it), never runs the cross build and skips the Mach-O/ELF refusal; a killed command or a failed sync-back packs nothing; a client gone keeps the command's status.

  The not-brought-back reasons the Unix box adds are the bad manifest's own text, `diff-tree printed a path that is not UTF-8`, the lines of a failed `diff-tree` step in the Unix sync-back style, and the over-budget and gone-before-sent reasons. The `diff-tree` step (named `diff-tree <leaf>` on a pair run) runs through the run's killable git steps and is recorded in `<work>.pgid`, only when a back commit was pushed for a run with outputs.

    A run can also carry `CrossBuild`, an object of `Rid` (a runtime identifier of letters, digits, `-` and `.`, not starting with `.`) and `Words` (the cross build's command, a non-empty list of non-empty texts), and only with `Outputs` globs.

    After a command that exited 0 and pushed its back commits, and before the outputs are walked, the box runs `Words` as a second command with the first's working folder, environment and `Timeout`, a fresh ceiling of its own; its output is forwarded as the command's, and `box stop` ends it as it ends the command. Just before it starts, the box says `gate: cross build for <rid>: <Words joined by spaces>` on its stamped standard error; the PC's log scan ([Log and tail](#log-and-tail)) counts no `out` or `err` line after that line, so the cross build's own error text never fails a passing command, while the gate's own lines after it still do. It says no start marker, kill tag or adjusted line of its own.

  A cross build that exits 0 says `gate: the cross build for <rid> took <s> s` and the outputs are brought back. One that ends otherwise says `gate: the cross build for <rid> failed with exit <status>; the Windows outputs were not brought back`, then `gate: remote outputs not brought back: the cross build failed`.
  The run keeps the passing command's status (0) after a cross build that exits by a code, 143 included and a trapped stop with it. A cross build the box kills for its ceiling or backstop ends the run 124 with the box's kill line and kill tag for the command, and none of the cross-failure lines.
  A cross build that ends by a signal, cannot start, or that the box cannot see to its end ends the run 1 with the same two lines, so a stopped run never reads as passed. A client gone before or during it ends the run 255.
  The PC's nothing-back record then stands for the build, so the next `--no-build` run builds first.

  The outputs of a run with a cross build are listed at the output path map: a file under `bin/<a>/<b>/<rid>/...` is judged by the output globs, and brought back, at its path with the `<rid>` folder removed, which is the path a native Windows build writes it at; a folder is entered by the globs at its mapped path too.

  A native file at a mapped file's path is left out, the mapped one being the only file at it. Only the files to be sent are checked, by their first bytes (a file the PC holds unchanged is never opened).

  A file at its own path that opens as a Mach-O or ELF program, which the PC cannot run, is left out. A mapped file that opens as one was not built for Windows: nothing is brought back, and the run says `gate: the cross build for <rid> left a non-Windows file <path>; the Windows outputs were not brought back` (the PC path, control characters escaped), then `gate: remote outputs not brought back: the cross build left a non-Windows file`, and keeps the passing command's status.

  The Windows box refuses a `CrossBuild` payload with `gate: box payload CrossBuild is a macOS builder's key; this box cannot cross-build`. A malformed one, or one without `Outputs`, is a usage line on either box (`gate: box payload CrossBuild is not an object`, `lacks Rid`, `Rid is not a text`, `lacks Words`, `Words holds an empty word`, `needs Outputs`, among them).

  The PC's router sends `CrossBuild` to a macOS builder whose `builders` entry opts the repo in (`crossBuild` in [remote.json](#remotejson)), for a routed direct `dotnet build` or a `dotnet test` without `--no-build` (the `--no-build` the router would drop counts) that builds in place, names no runtime and has output globs; never to a Windows builder, and a warm-up never carries it.
  Its `Rid` is the entry's, and its `Words` are `dotnet build <target> -c <configuration> -r <rid>` (`Debug` when the command names no configuration; no target word when the command names none), then the command's `-f`/`--framework` and `--source` words with their values as written, then its property words.
  `-s` is carried as a source for `dotnet build` alone, since in `dotnet test` it is the settings file.
  The target is the first word that is no option and no option's value: `dotnet test --blame-hang-timeout 10m tests/A.Tests` cross-builds `tests/A.Tests`, and `dotnet build --property Bar=2` cross-builds with no target. After `--self-contained`, `--sc` or `--interactive`, a `true` or `false` next word is the switch's value. Words after the dotnet command's own `--` are never read.
  A command naming `-a`, `--arch`, `--os`, `--ucr`, `--use-current-runtime`, `-r`, `--runtime`, `-o`, `--output` or `--artifacts-path`, or an option word the router does not know (`--made-up-switch`, `-bl` and `-m` among them), gets no cross build, and its route says `gate: no cross build for this command: it names <word>` after the route's other lines.

  A pair run brings back from its main tree only. `OutputsFull` with no globs is taken and does nothing. `--test-outputs-budget-bytes` reaches the run. Every other verb (`inflight`, `lfs`, `register-desktop`) says `gate: box <verb> is not built on this builder` and exits 2.

  The run checks a payload's `InputsDigest` as the Windows box does ([The box side](#the-box-side)), and refuses a `SentDigest` or `LfsDigest` as not built (`gate-remote-refused: <nonce> <tag>`, then `gate: remote <key> is not built on this builder`, status 75), so the PC runs such a run itself.

  It has no install tripwire, no desktop helper and no MSYS keeper. Its `test-seams` feature passes `gate-args`' hidden `--test-*` seams through, given before the verb; `--test-heartbeat-ms`, `--test-box-sample-ms`, `--test-box-sampler-fail` and `--test-box-free-percent` reach `box run`.
- **`boot`** reads `kern.boottime` and prints the same line, with the same format.
- **`prune`** (`prune.rs`) is the Windows box's prune ([The box side](#the-box-side)) on plain `std::fs` calls: `gate-box box prune --root <folder>` is the age prune and `--keys <key>...` the keyed prune. It reads no request and never reads stdin. The keys follow the Windows rules (1 to 50, a repeat pruned once), and a word that is no key exits 2 as there.

  `--root` must be an absolute path starting with `/` (a trailing `/` is dropped), else `gate: box prune --root is not an absolute path: "<value>"` and exit 2, before anything is opened.

  The root must pass the work-root check of `stop` (owned by the box user, not a link, with no group or other bit): the sweep works only in a root private to the box user, so only that user can swap what it looks at between its look and its act. A root the check refuses is left whole, with no folder made, no record or folder removed and no git run.

  The age prune prints the check's line (`gate: the builder's work root is not locked down: <why>`) and exits 1; the keyed prune prints `gate: remote could not prune <key>: <that line without its leading gate: >` for each key and exits 1.

  `<root>/work` and `<root>/pruned` must be plain folders, else `gate: remote could not prune: <path> is not a plain folder` and exit 1. `pruned` is made at mode 0700 when missing, and what an earlier sweep left in it is removed at the start, links removed as themselves (`gate: remote could not remove the pruned folder <path>: <error>` on stderr for one that stays). A folder that cannot be listed prints `gate: remote cannot list <path>: <error>` and exits 1.

  Per key it takes `<key>.lock` without waiting, so a key a run or another sweep holds is kept. A key whose `<key>.pgid`, `<key>.tools.pgid` or `<key>.packages.pgid` still names the group its run made, by the stale-record rules of "The run's git steps" (unknown counts as current), is kept too, as a run refuses `busy` for it; a record naming no live group goes with the key.

  A lock the age prune cannot open prints `gate: remote could not open the work folder lock <path>: <error>`, and the keyed prune that key's could-not line.

  Under the lock, an idle key (or any named key) loses its records in this order: `<key>.layout` is written as `changing`; `.warm`, `.packages`, `.tools`, `.inputs`, `.sent-build`, `.command`, `.pgid`, `.tools.pgid` and `.packages.pgid` are removed; the folder is renamed to `<root>/pruned/<key>.<pid>.<nanos>`; then `.layout`, `.index`, `.index.lock` and the temp files of the key's records (`<key>.<name>.gate-new-*`) are removed.

  The lock is let go and the renamed folder deleted. A key with no folder has its records removed. A failed rename leaves `.layout` reading `changing`, so the next run clears the folder whole.

  The renamed entry must be the folder that was looked at (device and inode), else `its work folder changed during the sweep` and it stays in `pruned` for the next sweep. A link or non-folder at `<root>/work/<key>` is refused as `<path> is not a plain folder`. An aside folder another sweep already removed counts as removed.

  The `.lock` file stays, but for a keyed prune of a key the builder never had: a lock that prune made is removed while still held (a failure says `gate: remote could not remove the work folder lock <path>: <error>`).

  A key's last use is the newest write time among its folder, the `.git/FETCH_HEAD` of the folder and of each folder directly in it (a pair keeps each tree in its own folder), and its records; the lock is left out, since nothing writes it. A key is idle strictly over 24 hours; a last use in the future is not idle.

  Keys are the names in `work` that are keys and the `<key>.<extension>` of the extensions below, the key being the part before the first `.`, listed without regard to case.

  | Name under `<root>/work` | Written by | A prune |
  |---|---|---|
  | `<key>` | the work folder (trees; a pair's trees in `<key>/<leaf>` and `<key>/<name>`) | renames aside, deletes |
  | `<key>.lock` | the work lock | keeps; the keyed prune removes one it made |
  | `<key>.pgid`, `<key>.tools.pgid`, `<key>.packages.pgid` | the git steps', the tool restore's and the package restore's process groups | read first (a live group keeps the key), then removes |
  | `<key>.command`, `<key>.tools` | the command record, the tool manifests' hash | removes before the rename |
  | `<key>.inputs` | the `inputs` verb's record of installed inputs | removes before the rename |
  | `<key>.packages` | the package restore's record of the lockfile folders whose install passed | removes before the rename |
  | `<key>.warm`, `<key>.sent-build` | not written by the Unix box (named so a later writer needs no prune change) | removes before the rename |
  | `<key>.layout` | the layout record (`single`, `pair ...`) | marks `changing`, removes after the rename |
  | `<key>.index`, `<key>.index.lock` | the back commit's copied index and git's lock on it | removes after the rename |
  | `<key>.<name>.gate-new-<pid>-<n>` | a record write cut short | removes after the rename |

  The age prune's stdout is `gate: remote pruned <key>: idle <h> h` per removed folder, then the leftover-folder lines below, and last, always, `gate: remote prune removed <n> of <m> work folders` (`n` counts the floor pass's folders too, `m` the plain folders among the keys; `0 of 0` when the root or `work` is missing). A key it cannot prune prints `gate: remote could not prune <key>: <reason>` on stderr and is kept.

  The age prune exits 0 unless the sweep ended on the root, a folder or a listing as above, and deletes no ref.

  **The free-space floor.** After its age pass the age prune reads the free bytes of the root's volume that the box user may write (`statfs`). While under 100 GiB are free it removes the work folders the age pass kept and no run holds, oldest last use first, whatever their age, one at a time with the free space read again after each, until 150 GiB are free.

  It prints `gate: remote pruned <key>: <g> GiB free` per folder (`g` the whole GiB free before that removal). On stderr it prints `gate: remote could not read the free space of <root>: <error>` (and does no floor pass) and `gate: remote prune could not reach the free-space floor: <g> GiB free` when it ends under 150 GiB, with no folder left to remove or a removal that freed nothing.

  A key the age pass could not prune is not tried again. The floor pass deletes no ref.

  **The `/tmp` sweep.** Last, the age prune removes the run temp folders a hard-killed run left in `/tmp` (not the caller's `TMPDIR`): `/tmp/gate.` and eight letters or digits, a folder (never a link) owned by the box user whose newest write, and that of each entry directly in it, is over 24 hours old. It prints `gate: remote removed the leftover run folder <path>: idle <h> h`, or `gate: remote could not remove the leftover run folder <path>: <error>` on stderr.

  **The keyed prune** removes exactly the keys given, in the order given, whatever their last use, makes a missing `work`, and prints `gate: remote pruned <key> (worktree gone)`, `gate: remote kept <key>: in use` (a held lock or a live group record) or `gate: remote could not prune <key>: <why>` on stderr, one line per key and no summary. It exits 1 when any key got a could-not line, else 0.

  Under the key's lock and with its folder out of `work` (or absent) it deletes `refs/gate/<key>`, `refs/gate-bridge/<key>` and `refs/gate-back/<key>` in each `<root>/*.git` that is a plain folder (not a link) holding `HEAD`, `objects` and `refs`, one `git update-ref -d --no-deref <ref>` per ref, so a symbolic ref goes itself, not its target.

  Git runs `/usr/bin/git` with `core.hooksPath` pointed at a path that cannot exist and `core.fsmonitor=false`, so nothing in a bare repository runs. Every bare is tried; the first failure is the key's `its refs could not be removed: <why>`, and a key kept for a held lock or a failed rename keeps its refs.

  Each bare is judged before it is touched: it must be the box user's own with no group or other write bit (the below-`work` rule of Work root, below). A flawed bare keeps its refs and fails the key with `gate: remote could not prune <key>: its refs could not be removed: <bare> is not owned by <user>` (or `<bare> is writable by others`); the other bares are pruned.

  A root that is missing is gone, and every key answers `pruned`, only when its parent folder can be read; under `/Volumes/<name>` it is unknown while that volume is not mounted. Unknown is never gone: each key gets `gate: remote could not prune <key>: the root's volume is not there` (or `the root cannot be looked at: <error>` for any other failure to look at it) and the exit is 1. A record left once the folder was aside fails the key, as on the Windows box.

  A `test-seams` build of `gate-box-unix` reads two environment variables, the one exception to "never environment variables" for seams, because the prune takes no request and its seams are not in `gate-args`' shared `BoxSeams`: `GATE_BOX_TEST_TMP_ROOT` replaces `/tmp` in the sweep above and `GATE_BOX_TEST_FREE_BYTES` replaces the `statfs` read with a byte count. The installed binary ignores both (`cfg(feature = "test-seams")`).

  **The drain barrier.** The age prune reads the builder's drain barrier ([`drain`](#the-unix-box), below) after the root check and before it opens `work`. Under a standing barrier it says `gate: remote prune skipped: the builder is drained` on stdout and exits 0, removing nothing: no age pass, no floor pass, no leftover run folders. The keyed prune says `gate: remote kept <key>: the builder is drained` on stdout for each key, and removes nothing.

  A barrier that cannot be read is `gate: remote prune failed: <why>` on stderr and exit 1. A lapsed barrier does not stop the prune. The read takes no drain lock, and a `drain` that is not the box user's 0700 folder (a link, a file, or a folder of another owner or mode) reads as no barrier.

  The Mac's bares are made with `receive.autogc false` and `gc.auto 0`, so no gc runs there and the prune holds none behind the barrier. `gate-box box prune` takes the hidden box seams (`--test-box-now` and the drain lease and lapse seams) before the verb.

  **The scheduled job.** The launchd job `dev.gate.prune-work` (`/Library/LaunchDaemons/dev.gate.prune-work.plist`, root:wheel 0644) runs `<root>/bin/gate-box box prune --root <root>` as the builder user at load and every 2 hours (`StartInterval` 7200), with low-priority I/O, background process type, the root as working directory and `HOME` set to the builder's home.

  Its output goes to `/Library/Logs/gate/prune-work.log`, which is not rotated. `install-macos.sh --system --user <name>` installs it in its System step, last after the machine settings, in this order, writing nothing to `/Library/LaunchDaemons` until every check has passed:
  1. It reads the builder's home with `dscl`; `--root` defaults to `<home>/gate-remote` under `--system` (give an absolute `--root` there, since a `~/` expands to root's home).
  2. It makes `/Library/Logs/gate` (root:wheel 0755) when missing; an existing one must be a folder owned by root that group and other cannot write. It creates the log when missing, with no-clobber, as root, then hands it to the builder at mode 0600 without following links; the log, made or found, must be a plain file of one link owned by the builder, else the step fails with a line naming the path and what is wrong. The builder's root is never `chown`ed or `chmod`ed by root.
  3. `<root>` and `<root>/bin` must be folders (not links) owned by the builder at mode 700, else the step fails naming the path. The root is made by the Gate work root step, run as the builder first.
  4. A missing `<root>/bin/gate-box` does not stop the step (a new builder is provisioned before its first `gate push`): it logs `gate-box is not in <root>/bin yet: the prune job fails until gate push installs it`.
  5. It writes the plist, then, when the job is loaded, runs `launchctl bootout system/dev.gate.prune-work`.

     A non-zero bootout exit (launchctl says 36 while the old process is still ending) never fails the step: it logs `bootout of dev.gate.prune-work said <n>; waiting for it to go` and waits up to 10 s, polling `launchctl print` every 0.5 s, for the job to go, else the step fails with `system/dev.gate.prune-work was still loaded 10 s after launchctl bootout`. Then it runs `launchctl bootstrap system <plist>`.

  Without `--system` the step logs a `would ...` line for each of these (naming `{builder user}` and `{builder home}` in the plist text) and SKIP.
- **`hold`** (`slots.rs`) is the Windows box's `hold` ([The box side](#the-box-side), `hold`) on the Unix box: `gate-box box hold --root {root} --holder "{text}" --ceiling {s} [--wait {s}] -- {command}`, with the same words, limits, lines and statuses (75 after the full lines, 124 at the ceiling, 127 for a command that cannot start, 2 for no slot count or a bad holder, 1 for `gate: hold failed: {why}`). Where it differs: the command's status is the box's own, 128 plus the signal number when a signal ended it; the slot files are `{root}/slots/` (mode 0700), `{n}.json` claims and `{n}.lock` files, and the hold's holder is an `flock` on the lock file, which the kernel frees when the hold dies; the take and every judge run under the drain lock (`under_drain_lock` in `drain.rs`); and the command runs in a process group of its own, which the ceiling ends, but there is no kill-on-close job, so a hold killed hard (SIGKILL) frees its slot and leaves its command's group running.

  A preflight or run with a `Session` under a root that has a `slots.json` takes, keeps and lets go of a builder-side slot as the Windows box's do ([The box side](#the-box-side), `preflight` and `run`), with the same refusal (the version line, `gate-remote-refused: {nonce} full`, the full line, exit 75), `gate: slots failed: {why}` (exit 1) and the unreadable-count line.

- **`drain`** (`drain.rs`) is the builder drain's barrier ([The box side](#the-box-side), `drain`) on the Unix box: `gate-box box drain --stdin` with the payload `{"Root","Action"}`, `Action` being `raise`, `status` or `lift`. The shared pure rules (the clock, the barrier parse, tickets, the decision, the status and refusal lines, the constants, and the matching that counts processes) are in `gate_wire::drain`, which the Windows box calls too; this lists where the Unix box differs.

  Its files are the Windows box's, under `{root}/drain/`: `lock`, `barrier`, `lapsed`, `lifted`, `tickets/`, `sessions/{session}` and the holder `sessions/{session}.{pid}`, an `flock` the admitted process keeps. The drain lock is the same file taken with `flock`, tried again every 20 ms for 10 s while it is busy.

  The folder is judged as every Unix box folder is: owned by the box's euid, mode 0700, not a link. A `drain` that is not that folder is renamed aside as itself and never followed (`gate: drain moved aside {path} to drain.gate-moved-{n}: it was not the box's protected folder`), where the Windows box moves aside a folder of other security. A raise on a missing root makes the root through the preflight's trusted-root path, at 0700.

  The routed command shares the box's uid, so unlike on the Windows box (where its medium integrity keeps it from writing `drain\`) it could delete the barrier or its own session record. That is accepted: it could already delete the whole work root and its lock.

  The drained test counts a session naming a job as live past its lease only while the work folder's `.pgid`, `.tools.pgid` or `.packages.pgid` record names a group still its run's, by the stale-record rules of "The run's git steps". A record that is unreadable or malformed counts as live; the `.command` record is not read.

  The processes counted are the box user's own, listed on macOS through libc (`process_scan.rs`) and judged by the shared rules: `gate: drain process {pid} prune|git` per counted process. A same-user process whose command line cannot be read counts when its image is `git`, `git-*` or `gate-box`, and paths are compared with ASCII case folded, which can only over-count.

  The box's own calls (`{root}/bin/gate-box ...` other than a prune) and the ancestors of the drain call are not counted. Processes that cannot be listed, always the case off macOS, say `gate: drain processes unknown: {why}` and the state is `waiting`. Waiting tickets do not stop `drained`, as on the Windows box.

  **The gated verbs.** `preflight`, `run` and `inputs` pass the shared admission with the payload's `Session`, `NoTicket`, `Pc` and `Repo`, and a refusal is the Windows box's (the `drain` tag, exit 75; [The box side](#the-box-side)).

  `preflight` admits after the root trust block and before it reads the layout or the warm record, so a refused preflight writes nothing under the work folder, and it says `gate: remote version {v}` first. `run` admits after the root is checked and before it takes the work lock. `inputs` admits right after the request is read, before the manifest.

  `outputs` is never refused. It renews the session, and for a dead or missing session under a standing barrier it still sends the archive and keeps it, saying `gate: remote outputs kept: the builder is drained` on stderr. A root that is not the box user's 0700 folder makes it say `gate: drain failed: {root line}` and keep the archive. The exit status and the hidden test seams (before the verb) are the Windows box's.
- **`inputs`** (`inputs.rs`) answers the Windows box's `inputs` ([The box side](#the-box-side)), step `diff` or `put`, with the same request, stdin, record format, exclude file, caps and `gate-remote-inputs:` lines; this lists where the Unix box differs. The run's `InputsDigest` check (`inputs::unmatched`, called from `run.rs`) reads the record by path as the verb does (a link at its name refused) and each copy beneath the held work folder's descriptors, as the put does, and differs as follows.

  A record that is a link, a folder or past the 128 MiB cap, or is not UTF-8, is `<work>.inputs cannot be read: <error>`; a record of another manifest's digest, or unsealed, is `<work>.inputs holds another manifest's digest`. A copy is the one installed when its size, mtime and `{dev}-{ino}` match the record, and one whose equal stamp is untrusted (a negative mtime) is hashed against the record's SHA-256 instead.

  The exclude rewrite runs before the checkout, so inputs survive `clean -fdq` and stay out of the back commit.

  The rewrite writes only while the run's work lock is still held, as the run's other record writes do: else the run says `gate: remote could not write <work>/.git/info/exclude: the work folder lock <lock> is no longer held` (or `could not be looked at: <error>`) and ends 1, with no refusal tag.

  A `<work>/.git is not a folder of its own`, `cannot ready <work>/.git/info` or `cannot write <work>/.git/info/exclude` failure refuses the run `inputs` like a failed check; a folder at the exclude name is moved aside to `exclude.gate-moved-<n>`, said, and the run goes on.

  A `Pair` is refused (`gate box: inputs for a sibling pair are not taken yet`, exit 2) and a `Step` that is neither word is `gate: box payload Step is not diff or put: <value>` (exit 2). A payload with a `SentBin` is refused after the request is read and before stdin is: `gate-remote-refused: <nonce> sent`, then `gate: remote SentBin is not built on this builder`, status 75.

  The step failures (`gate: remote inputs <step> failed: <why>`, exit 1) the Unix box adds to the Windows box's are: `the work folder lock <lock> is no longer held`, `the stage <path> is on another volume than the work folder <work>`, `its staged copy changed after the unpack`, `what is at its name after the move is not its staged copy`, `'<prefix>' is the work folder's .git`, `<path> changed before the record was sealed` and `<path> cannot be opened before the record is sealed: <error>`.

  A member spelt otherwise than the manifest, in letter case alone included, is `the archive's member '<path>' is not in the manifest`. A copy whose device and inode match a manifest file found at its own spelling is never swept, and every recorded file is opened again and its stamp checked just before the record is sealed.
- **`outputs`** (`outputs.rs`) answers the Windows box's `outputs` ([The box side](#the-box-side)) with the same payload (`Key`, `Work`, `OutputFolder`, `Name`), lines and statuses, and takes no lock; this lists where the Unix box differs. The run calls the module, so a Mac route brings outputs back.

  The Unix box has no data channel: a payload with a `Channel` is refused, after the work folder and session reads, with `gate-remote-refused: <nonce> channel` then `gate: data channel not completed: this builder has no data channel` and exit 75 (a malformed `Channel` or `Nonce` is the payload's usage line); the PC never sends one to a `macos` builder.

  The output folder is `<root>/outputs`, made 0700 beneath the judged root, or adopted only when it is private (the box user's own, no mode letting another account in) and not a link; archives are `<key>-<32 hex digits>.tar.zst` with mode 0600, and a folder that cannot be made or adopted says `cannot make the output folder <folder>: it is not the box user's own or its mode lets another account in`. A work folder not below the work root is `the work folder is not below the work root`.

  The verb opens the archive without following a link and sends only a plain file: a link says the system's ELOOP text and anything else not plain says `it is not a plain file`, both as `gate: remote could not read the outputs archive <name>: <error>` (exit 1). An output folder or root the verb cannot open is that same line; a missing one is `does not exist`. A sent archive that cannot be removed is `gate: remote could not remove the outputs archive <name>: <error>` and the status stays 0.

  The module also holds what the run calls, with the Windows box's lines: the manifest read, the listing before the command, the pack and bring-back, the 24-hour prune of plain outputs archives, the 50 GiB trim and the `gate-remote-outputs:` lines. An archive gone by the time the prune or the trim looks is not reported.

  A pack holds a shared `flock` on its own archive from its creation until the run's trim is done, and a trim removes another archive only under a non-blocking exclusive `flock`, passing over one a pack still writes, as the Windows box's share mode keeps it, and over one whose device and inode changed after it was locked.

  A work folder whose middle name is not `work` is refused as not a tree folder. A trim that can no longer open the output folder (no longer private, or the root no longer judged) says `gate: remote outputs not brought back: cannot open the output folder <folder>: <error>`.

  The walk goes beneath the work folder's descriptors one component at a time: a link is never followed or listed, only a plain file is listed (a fifo, socket or device is not), and a hard-linked file is packed. The stamp is the size and the last write in nanoseconds, compared box against box only, and a file last written before 1970 fails the pack (`<path> was last written before 1970`).

  The work root, `<root>/work` and the bare repository are judged as a run judges them, before the lock, and a refusal says the run's line and status. Both steps take `<Work>.lock` as `run` does (a held lock is `busy`, 75) and lay the work folder out as a run of one repo does, so a later run finds the layout unchanged and clears nothing. An earlier run's group record still naming a live group refuses `busy` (75) before anything is laid out, as it refuses a run.

  Every name below the work folder or a stage is reached beneath its folder's descriptor, one name at a time, a link never followed (`beneath.rs`, with `inputs.rs`'s walk). The work folder is itself reached beneath its judged root and `<root>/work`. A file below the work folder arrives by a rename over its name and leaves by an unlink, never written through.

  A folder on its way that is a link or a file refuses the put (`cannot install '<path>': ...`); the work folder's `.git` is never walked through. Files the box makes are mode 0600 and folders 0700 whatever the umask.

  The put's stage is `<root>/inputs-stage/<key>-<nonce>`, with `<key>-<nonce>.lock` beside it, held with a non-blocking `flock` for the put's whole life and removed with the stage on every way out.

  The stages folder is made 0700 or adopted only when it is the box user's own with no group or other bit (`cannot use <path>: its owner or mode is not the box's; remove it`; a link or non-folder is `<path> is a link or not a folder`); the stage and its lock file are made new, never adopted, and the stage must be on the work folder's volume (`the stage <path> is on another volume than the work folder <path>`).

  Before it makes its own, a put sweeps the stages folder: a stage whose lock file was last written over 24 hours ago and that no live put holds (the sweep takes its `flock`) is removed with that file, as is a stage with no lock file whose own last write is over 24 hours old.

  A stage it cannot look at, open or lock stays (`gate: remote kept the stage <path>: <why>`), one it cannot remove says `gate: remote could not remove the old stage <path>: <error>`, and a folder it cannot list says `gate: remote could not list <path>: <error>`, each on stderr.

  The record `<Work>.inputs` is the Windows box's, holding per file its path, size, SHA-256, the mtime in nanoseconds (`modified`) and the file's `id`, `{dev}-{ino}`. A diff trusts a copy whose size, mtime and id match and never one whose mtime is negative. A record over 128 MiB (twice the manifest's limit) is not read, and one that would be is not written (`cannot write <path>: the record would be longer than <n> bytes`); an unreadable one is removed as the Windows record is.

  A member is installed only under the manifest's exact spelling of its path: one spelt otherwise, in case alone included, is `the archive's member '<path>' is not in the manifest`. The exclude file is written to a new `exclude.gate-new-<pid>-<n>` beside it and renamed over it, and a folder at its name is moved aside to `exclude.gate-moved-<n>` as on the Windows box.

  The lines are the Windows box's: `gate-remote-inputs: need <n>`, `gate-remote-inputs: path <path>`, `gate-remote-inputs: installed <n>` and `gate: remote inputs <diff|put> failed: <why>` (exit 1).

  The record and exclude lines are `gate: remote kept the stale input <path>: <why>`, `gate: remote could not remove the stale input <path>: <error>`, `gate: remote inputs record <path> <why>; every copy is hashed`, `gate: remote removed the inputs record <path>, which cannot be read: <error>; every copy is hashed` and `gate: remote moved aside the folder <old> to <new>`.

  The 50,000-member and 4 GiB caps and the `--test-inputs-budget-bytes` seam are the Windows box's.
- **Commands' `PATH`.** The programs the box starts (the probes, `dotnet --list-sdks`, the git steps of `box run` with its back commit, and the routed command) run along the `PATH` of the box user's login shell, since a non-login ssh command lacks the toolchains' folders: `$SHELL -l -c` of `/usr/bin/printf '\0__GATE_LOGIN_{16 hex}__\0'; /usr/bin/env -0` (`/bin/zsh` when `SHELL` is unset), 10 s, nothing cached between runs.

  The same one start also gives the builder's `GATE_GODOT_ROOT` (below). `run` reads it once, just before its first git step and after the usage, unbuilt, Rosetta, root, busy, sweep, earlier-group and layout-file checks, so a run those checks end starts no login shell and says no path line.

  The git steps of `run` keep it in their environment, so a git-lfs filter is found along it, and the command is found along it; the preflight's git steps do not (they run the ssh `PATH`, with git itself found by its absolute path, below). Empty and relative entries of the login `PATH` are dropped.

  When it cannot be read the box keeps the ssh `PATH` and says `gate: remote path: could not read the login shell's PATH: <why>; using the ssh PATH` on stderr; a `PATH` with no absolute folder left is such a failure, with `<why>` `said a PATH with no absolute folder`. Each program runs in a process group of its own, which a deadline kills with `SIGKILL`; a git step's ending is under "The run's git steps" below.

  The marker is made per call from the box's process id and the time, so nothing a login profile prints can pass for it. The two programs are given by absolute path with single-quoted words and expand no variable, so the command parses alike under any login shell (fish, tcsh, a profile with `set -u`).

  The box takes the first record that starts with that marker (a NUL, the marker, a NUL, then the environment as NUL-ended `NAME=value` entries) and reads the first `PATH` and the first `GATE_GODOT_ROOT` in it; whatever the profile prints before or after is left out. Only an exported `GATE_GODOT_ROOT` is read, and one that is unset, empty or not UTF-8 is no root.
- **Work root.** Judged by owner and mode, not a DACL, by one owner-plus-mask check (`lines::owner_flaw`) at every site: `<root>` and `<root>/work` are refused when a link, not a folder, not owned by the box's user, or open to others (any group or other bit, `mode & 0o077`), saying `<root> is not owned by <user> or is open to others`.

  That is the root refusal of the Windows box (the version line, then `gate: the builder's work root is not locked down: <why>` and status 1), whose why ends with the hint to run `install-macos.sh --only "Gate work root"`. `gate push` to a Mac refuses a root open to others the same way.

  Below `<root>/work` (a key folder, a pair's tree, each bare `<root>/<name>.git`, a tree's `.git`, `.git/objects`, `.git/objects/info`, `.git/config` and `.git/HEAD`) each must be the box user's own, no link, with no group or other write bit (`mode & 0o022`). A link, a non-folder or another user's folder is `gate: remote could not create the folder <path>: it is a symbolic link` (or `it is not a folder`, `it is not owned by <user>`) and status 1.

  A bare or work folder others may write in is refused in the preflight, `stop`, `run`, `inputs` and `prune` as `gate: the builder's work root is not locked down: <path> is writable by others; remove <path> and the box makes it again` (the root's status; the PC falls back `root`), where `<root>/work` keeps the hint to run the installer's "Gate work root" step.

  In the run and `inputs`, a loose tree, `.git` or `.git/config` is a `layout` refusal (status 75), and so is a `.git/commondir` file (`<path> names another repository's folders`).

  The preflight's `ready` makes the same check on a tree's owner and modes (and `commondir`) before any git step: a tree that fails is left alone, not made ready, and the run refuses it `layout`.

  **Umask.** The box runs with umask 077. The routed command, the cross build, the tool restore, the package restore and git's checkout and clean steps run with umask 022, so checked-out tracked files are 0644 as on the PC.
- **The git program.** Every git step of the box (the preflight's, `run`'s and the back commit's) runs `/usr/bin/git`, macOS's own git, by its absolute path, with every inherited `GIT_*` variable removed and then `GIT_CONFIG_NOSYSTEM=1` set, so a builder with only a Homebrew git still uses Apple's git for the gate's steps and no system config is read.

  Each step also gets `-c protocol.allow=never -c protocol.file.allow=always -c submodule.recurse=false -c core.sharedRepository=umask` and `GIT_ALLOW_PROTOCOL=file`, with `--upload-pack` and `--receive-pack` pinned, so only local-path repositories are spoken to and no submodule is followed; the fetch and the back push also get `--no-recurse-submodules`. The checkout and clean steps run with umask 022 (Umask, under Work root).
- **`preflight`** runs every git step from `<root>/work`, with `GIT_CEILING_DIRECTORIES` set to `<root>`; a broken `<Work>/.git` fails as `gate: remote git <step> failed (status <n>)`.

  It takes no config fast path: the steps are `git init --bare`, `git init`, `git remote add` (or a `remote.origin.url` check) and the two `git config` calls, which it runs every time it readies a single tree. A new tree also removes `<Work>.warm`; when that fails it says `gate: remote could not remove <Work>.warm: <error>` and exits 1.
- **Lines.** `gate: remote created <Bare>` and `gate: remote sdks <list>` escape their text. A warm record is read only when it is a plain file holding exactly 64 lower-case hex digits and no line end, else `none`, as on the Windows box.
- **Tool probes** run on the login `PATH` under the same shared 10 s deadline and read versions the same way (`gate_wire::version_of`). `unreal` and `netfx-sdk` say `missing`, so a run that needs one never routes here.

  `godot` reads the installs under the login shell's `GATE_GODOT_ROOT`, as the Windows probe does under its own. A candidate is `<root>/<version>/Godot.app/Contents/MacOS/Godot`, run with `--version`, whose reported version must be its folder's name. The root itself may be a link; the version folder and each level below it (`Godot.app`, `Contents`, `MacOS`, the program) may not. At most the first 64 entries read are considered, then sorted.

  With no root, or none that holds an install, the line is `gate: remote tool godot missing` (the Windows probe falls back to `PATH` instead); a root that is not an absolute path or cannot be read is `gate: remote tool godot skipped: <why>`, as is a root whose installs all give no version, with the first reason met.

  A candidate left out says `cannot run <program>: <level> is a link`, `<level> is not a folder` or `<level> is not a file`; the version line otherwise names `named <folder> but reports <version>` and `Godot --version gave no version: <line>` as the Windows probe does for its console exe.

  The link exclusion is a consistency check, not a security boundary: every routed command already runs as the builder user, and the program is launched after the check, so a swap between the two runs the swapped target.
- **A pair.** A `Pair` payload with `SiblingSnapshots` is laid out as the Windows box lays it out (Routing, Sibling worktrees; [The box side](#the-box-side)): the main repo in `<Work>/<leaf>` and each sibling in `<Work>/<name>`, each from its own bare `<root>/<name>.git`, and the command runs in the main tree (or its `Subfolder`). Every bare is checked as the box user's own with no group or other write bit (Work root).

  A pair preflight says `gate: remote created <root>/<name>.git` for each missing bare, the main one first, readies no tree, and says `gate: remote new work tree` when the layout record names another layout. The record `<Work>.layout` holds `pair <leaf> <siblings sorted ignoring case>` or `single`, and `changing` while the folder is cleared; a run whose layout differs clears the whole work folder, `.tools` kept. A link at a pair tree refuses the run `layout`.

  The clear (`run.rs`, `beneath.rs`) removes `<Work>.warm` and `<Work>.inputs` first, whether or not the folder is there, then writes `changing`. It then opens `<Work>` beneath its judged root and `<root>/work`, a link at any of the three refused, and removes each child beneath that folder's descriptor, a folder with all it holds, a link or a fifo as itself and never followed; every open is `O_NOFOLLOW|O_NONBLOCK|O_CLOEXEC`.

  A `<Work>` that is a link, or cannot be opened or listed, refuses the run `layout` with `cannot list <Work>: <error>`; a child that stays says `cannot remove <path>: <error>`.

  A child folder whose device differs from its parent's, compared before it is entered, holds another volume mounted in the work folder, at that child or below it: the run is refused `layout` with `cannot remove <Work>/<child>: it holds another volume`, naming the top-level child, and nothing on that volume is touched.

  The check is the device number alone, so on Linux a bind mount from the same filesystem is not caught. A child that something else removes while the clear runs counts as removed and the clear goes on.

  A name the clear's calls take must be one component (not empty, `.`, `..` or holding `/`) and the folder opened must be an absolute path, else the call is refused. An `<Work>.inputs` that cannot be removed (a folder, say) refuses it `layout` with `cannot remove <Work>.inputs: <error>`.

  The run's git failures name the repo: `gate: remote fetch <name> failed …` (and `checkout`, `clean`, `cat-file`, `back ref reset`), `gate: remote git config core.autocrlf failed for <name> …` for the layout steps, and `gate: remote snapshot <sha> is not in the repository of <name> after the fetch`. A single-repo run's lines are unchanged.
- **Layout check.** One check, shared by `preflight`, `run` and `stop`, requires a `Work` shaped `<root>/work/<key>`, else a usage line and exit 2. A `:` in `<root>` is the same usage error, since git splits `GIT_CEILING_DIRECTORIES` at every `:` with no escape: `gate: box payload Work must be <root>/work/<key> with no ':' in <root>, which GIT_CEILING_DIRECTORIES cannot hold: <Work>`; nothing is made.
- **Record writes.** A record the box rewrites (the alternates file `preflight` writes, a run's group record) is replaced whole, so a reader finds the old text or the new and never a part: a new file `<name>.gate-new-<pid>-<n>` beside it, made `O_CREAT|O_EXCL|O_NOFOLLOW` with mode 0600, written and synced, renamed over the record, then the folder synced (with `fsync` where the volume refuses the full sync, as `F_FULLFSYNC` on SMB or NFS).

  A leftover temp of this process's pid is replaced. Only a missing path or a plain file is replaced, else `gate: remote could not write <path>: it is not a plain file`. A folder sync that fails after the rename leaves the new text in place and fails the preflight with `gate: remote <path> was written but its folder could not be synced: <error>`.
- **The work lock.** The work-folder lock `<Work>.lock` is an exclusive, non-blocking `flock` on a plain file made with mode 0600 and opened `O_NOFOLLOW`; a lock another holds is busy, any other failure an error. The locked file is checked, by device and inode, to be still what the name names, else it is let go and locked again, up to 3 more times. A lock file with more than one hard link is refused: `<path> has <n> hard links, so another name shares its lock: remove <path>`.

  A lock file another account owns is refused `layout` (status 75) with `<lock> is not owned by <user>`, whether or not the box can open it, so the PC runs the command itself. The box user's own lock open to others (a group or other bit) is set to 0600 on its descriptor before it is locked.
- **`stop`** needs a `Work` laid out as above. It judges the root as the preflight does and makes nothing: an untrusted root is the root line with status 1, a linked or foreign `<root>/work` the folder line with status 1, and a missing root or `<root>/work` exits 0 with no output.

  A run records each git step's process group in `<Work>.pgid` (the job name, the decimal group, the decimal session id and the leader's start as `<sec>.<usec>` with six digits, one per line) a tool restore's in `<Work>.tools.pgid` and a package restore's in `<Work>.packages.pgid`.

  A record that does not name the payload's job (`<Job>-tools` for the second, `<Job>-packages` for the third) stops nothing, and neither does a record that is stale (the stale-record rules in "The run's git steps" below): `stop` says nothing, exits 0 and leaves the record, since only a run removes one, under the work lock.

    The group gets `SIGTERM` with `SIGCONT` right after it (so a stopped group takes the TERM), then `SIGKILL` after 10 s, and 2 s more to end; the record is read again before each signal, and a group it no longer names is never signalled.

    A signal is sent only when the record carries a start and the live process whose pid is the recorded group has exactly that start (a `proc_pidinfo(PROC_PIDTBSDINFO)` read, `pid_info.rs`), checked before each send and in the escalation's wait. A reused number reads as gone and never draws `SIGKILL`; a zombie (exited, unreaped) leader still reads with its start. A start that differs at the `SIGTERM` check sends nothing, says no stopped-job line and exits 0.

    A record with no start that is still judged live is read again every poll for up to 10 s, then `gate: remote could not stop job <job>: its record names no leader start` with status 1.

    A leader gone with processes left in its group or session says `gate: remote could not stop job <job>: its leader <pid> is gone; the processes left in its group are not signalled` with status 1. Off macOS a record with a start says `gate: remote could not stop job <job>: its leader <group> could not be looked at; its group is not signalled` with status 1.

  A record whose group is 1 or less, whose session is 0 or less, or that holds a sign or another non-digit, says `gate: remote could not stop job <name>: <record> names no process group` with status 1; the box's own group is refused.

  Once a group has ended, `stop` follows the record to a later current group of the same job, at most 3 groups; past that it says `gate: remote could not stop job <name>: its process group <G> is still running after 3 of its groups were stopped` with status 1. When the record moves to another job's, or goes, after this job's group had its `SIGTERM`, `stop` says `gate: remote stopped job <name>` and exits 0 with no `SIGKILL` to the old group.

  A signal refused with `EPERM` while the record still names the group (only its unreaped leader is left) counts as sent, and `stop` waits for the runner to reap it. A group the record stops naming before `stop` signals it is not counted.

  A record that cannot be read again before a signal fails the stop with `gate: remote could not stop job <name>: cannot read <record>: <error>` (status 1), and nothing more is signalled. The lines are the Windows box's `gate: remote stopped job <name>` and `gate: remote could not stop job <name>: <why>`.
- **The command.** The routed command starts from the payload's word list, exec'd directly with no shell, the program found along the login `PATH`, with stdin null and both streams forwarded line by line. The order is the start marker `gate-remote-started: <nonce>` on stderr, bare, as the Windows box says it, a client-gone probe, `<Work>.command` written, a second probe, the spawn; a client gone at a probe starts nothing and gives 255.
  - **Environment.** The box's own, then `PATH` set to the login shell's, then the payload's `Env`, then `GODOT_PATH` (Godot, below), then the build-server settings (every case variant of each name removed first; the list is `build_server_settings` in `gate-wire`, which the Windows gate maps over unchanged), then `GATE_BOX_JOB=<job>`. In the child before the exec, `SIGTERM`, `SIGINT`, `SIGHUP` and `SIGQUIT` are back to default and the signal mask is cleared.

    `TMPDIR` is the run's temp folder unless the payload's `Env` sets it (exact name, case-sensitive), in which case the caller's value is used as given and no folder is made.

    The folder is `/tmp/gate.XXXXXXXX/` (19 bytes), made by `mkdtemp` with mode 0700, then given the box user's own effective group (not the group of `/tmp`), and private to the box's user, so the Unix-socket named pipes .NET makes under `TMPDIR` fit the 104-byte socket path limit that the box's own long `TMPDIR` would exceed.

    The group matters on macOS, where a new file takes its folder's group and `chmod` drops the setgid bit when that group is not one of the caller's, so a command's `chmod 02700` in its `TMPDIR` would read back 0700. The `dotnet tool restore` and the package restore get the same folder (the box's own `TMPDIR` when the caller sent one); the git steps keep the box's own `TMPDIR`.

    The folder is removed, links not followed, at every end of a run that took the work lock (pass, fail, ceiling or stop kill, back-commit failure, failed tool restore, failed package restore), before the lock is let go. A folder that cannot be made, or whose group cannot be set (it is then removed), refuses the run with the `layout` tag and `gate: remote could not make the run's temp folder in /tmp: <why>`, status 75, so the PC runs the command itself.

    The box removes only the folder it made: it records the folder's device and inode after the group change, and when something else sits at that name at the run's end (the command removed its folder and another account made one of that name), it leaves it and says `gate: remote did not remove <path>: it is no longer the run's temp folder`.

    A removal that fails says `gate: remote could not remove the run's temp folder <path>: <why>` on stderr. Neither changes the run's status. A run killed hard leaves its folder in `/tmp` until a reboot or the Unix box's prune (GATE-690).
  - **Godot.** A payload without `GodotVersion` runs every word as written and leaves `GODOT_PATH` as the environment has it. With one, the install is `<root>/<version>/Godot.app/Contents/MacOS/Godot` under the login shell's `GATE_GODOT_ROOT`, found by the probe's checks (no link below the version folder). A Godot console word then runs that program, and `GODOT_PATH` is set to it after the payload's `Env`; any other word runs as written.

    When the run resolves no Godot (no root, the root not an absolute path, or no such install), `GODOT_PATH` is removed, an inherited value included, and a Godot console word fails 127 with `gate: remote could not start <word>: <error>`, never falling back to the `PATH`. A version with no root fails 127 here where the Windows box falls back to the `PATH` (GATE-699 owns the difference). The payload's `Env` setting `GODOT_PATH` is refused as on the Windows box.
  - **`<Work>.command`.** The record is `{UTC start, round-trip format} {job}`, mode 0600, written before the spawn. It is kept when a signal ended the command, the box ended it (client gone), the client is gone at the end, the runner failed or left the group, the lock was lost, or a leftover process still held the output; it is removed after a clean exit and for a command that never started, each removal only while the work lock is still held.

    A failed write says `gate: remote could not write <Work>.command: <error>` with status 1; a failed removal says `gate: remote could not remove <Work>.command: <error>` and the run's status stands. A `<Work>.command` that is not a plain file (a folder, a link) refuses `layout` (75) before the start marker, with the layout line `<Work>.command is a link or not a file`.
  - **Heartbeat.** From before the checkout to the run's end the box says `gate-remote-heartbeat: <cpu ms>` on stderr every 5 s, with the command group's CPU, each process counted once and mach ticks converted by the timebase. It is said under the output lock and dropped once the client is gone. A heartbeat that cannot start says `gate: remote heartbeat stopped: cannot start the heartbeat: <error>` and the run goes on.

    The figure folds by reaper: each live process counts as its own CPU plus that of the children it reaped (`proc_pid_rusage`, which still reads a zombie), read in start order. A process that vanishes is dropped when a live member of the group will report it in its children, and added to a running total when its parent chain leaves the group (launchd for an orphan, the box for the leader).

    The figure never falls, and only the processes alive at the last tick are held. A member that left the group (`setsid`, `setpgid`) keeps counting, read by pid, until it ends.

    A process vanishes only when its read says it no longer exists; one whose read fails another way (a setuid program the box's user cannot read for a while) keeps its last figure until it reads again.

    What it cannot see: an orphan's CPU after its last read (all of it when never read); a child whose parent ended since its last read and which launchd then reaped, which is dropped; a zombie first seen with no parent known, also dropped; and a member never readable, which drops to its parent. Past the 4 096-process listing cap a child can be folded and later counted again in a parent admitted after it.
  - **Ceiling and backstop.** The box's watch runs `gate-watchdog`'s clocks on the machine's CPU load, read from `host_statistics(HOST_CPU_LOAD_INFO)` once per sample and kept as 64-bit totals of the counters' wrapping differences, so a wrap of the 32-bit counters never reads as a busy sample.

    The cores are `available_parallelism`, unweighted. The ceiling and the heartbeat share one CPU source: the pump's group CPU when the run has a pump, else the ceiling's own reading of the command group. The first read is at the watch's start.

    A command past its ceiling in load-adjusted time is ended through the runner. The run then says, in one write, an optional note (`gate: note: the box's {ceiling|backstop} kill could not end job <job>: <why>` when the group is still there, else `gate: note: after the box's {ceiling|backstop} kill: <why>`), the kill line, `gate-remote-killed: <nonce> timed-out|backstop` and `gate-remote-adjusted: <nonce> <s.sss>`, and exits 124 with the records kept.

    The kill lines are the Windows box's: `gate: TIMED OUT[ on <host>] after <t> s of load-adjusted time (wall <w> s, machine free <p>% on average); killed <words> and its children`, and for a command past 5 x its ceiling of wall time `gate: BACKSTOP[ on <host>]: <w> s of wall time (5 x <t> s ceiling, load-adjusted <a> s, machine free <p>% on average); killed <words> and its children`.

    A command that exits, or is ended by a signal, is followed by its adjusted line.

    A failed read of the group's CPU or of the machine's load (off macOS the group's CPU is read first and fails, `a process group's CPU is not read on <os>`; on a Mac a failed load read says `cannot read the machine's CPU load: kern_return <n>`) marks the sampler failed and the run goes on by wall time: only the backstop can fire, its middle clause reads `machine load unknown: the sampler failed`, and no adjusted line is said.

    The run says `gate: sampler failed: <msg>; watching by wall time only` with its end lines, the first among them, never when the failure happens.

    With the client gone the status is 255 and nothing is said. A signal still never gives 124. `box run` runs the command under this watch; `--test-box-sampler-fail` and `--test-box-free-percent` choose the sampler, the first over the second, else the machine's load, and `--test-heartbeat-ms` and `--test-box-sample-ms` set the periods.
  - **Output lock.** A say from the main thread that cannot take the output lock within 30 s (a pump or reader stuck writing to a stalled client) marks the client gone and writes nothing.
- **Stamped lines.** The same rule as the Windows box's ([The box side](#the-box-side), `run`): in `box run` every line but the protocol lines and the heartbeat is stamped `<stamp> <tag> <text>`, the stamp taken under the output lock.

  The command's streams are `out` and `err`; the box's own lines, git's and the tool restore's forwarded output, the back commit's lines and the process lines are `gat`. The start marker, the refusal and kill tags, the adjusted line, the outputs line and the heartbeat stay bare and byte-exact, and the other verbs are unchanged.

  The box writes a root line for itself, then a spawn line and an exit line for the leader. On macOS a sampler thread lists the command's process group every 10 ms for the spawn lines of the others (ppid, start time, image) and watches each new pid with an `EVFILT_PROC` kqueue knote for its exit code or signal.

  A process shorter than a tick, or one that leaves the group, may be missing, and the box says so once per run (`process lines: the group is sampled every 10 ms; ...`). A spawn line on macOS can name the image before the process has run `exec` (GATE-733). On any other Unix only the leader is listed, and the box says that once.

  The process lines go through a writer thread of their own, so neither the sampler nor the runner waits on the output lock; the writer is given 2 s to say what it holds when the lines end, and nothing is written after the box's final lines. On macOS a process still in the group when the sampler stops gets a `left` line.
- **Statuses.** Exited `c` gives `c`; killed by signal `s` gives `128+s` (a signal outside 1..126 gives 1); a client gone at a probe or at the end gives 255; the box's ceiling or backstop kill gives 124. A command that cannot start gives 127 with `gate: remote could not start <program>: <error>`, said after the start marker. A runner failure gives 1 with `gate: the gate failed while running the command: <why>`. The rest is as on the Windows box ([The box side](#the-box-side)).
  - **Rosetta.** Before the start marker, a `gate-box` translated by Rosetta (`sysctl.proc_translated` is 1) cannot read the CPU times of the run, so it refuses with status 75, `gate-remote-refused: <nonce> install` and `gate: remote gate-box runs under Rosetta on this builder, whose CPU times it cannot read: install the arm64 build`.

    A flag that cannot be read says `gate: remote could not read sysctl.proc_translated: <error>; taking the box as native` and goes on; off macOS a missing flag reads as native, silently.
- **The back commit.** (A pair's is below.) After a command that ended on its own, an exit status or a signal the box did not send, and while the work lock is held, the box commits the tree the command left and pushes it to the bare repository's `refs/gate-back/<key>`.

  None is made after a ceiling or backstop kill (124), a client gone (255), a command that never started or one the box failed to run. A `box stop` while the client is still connected is a signal the box did not send, so it is followed by a back commit.

  The work folder's `.git/index` is copied to `<Work>.index` only while the work lock is held.

  `<Work>/.git` must be a plain folder and `<Work>/.git/index` a plain file; the copy is made fresh (an earlier plain file there removed first), never through a link, with mode 0600, and keeps the index's modification time, so git's racy-git check still compares the content of entries written in the index's own second (Apple Git compares stat times only to the second, and a copy stamped with the time it was made would hide a same-size edit from `add -A`).

  `add -A` and `write-tree` run on that copy with `GIT_INDEX_FILE`, and the copy is removed after; a removal that fails says `gate: remote could not remove <Work>.index: <error>` and fails nothing. Before `write-tree` the copy must still be there and the lock still held, and the lock is checked again before `commit-tree` and `push`. A copy left by a run that lost its lock stays in place.

  A tree equal to `HEAD^{tree}` makes no commit and pushes nothing. Else `commit-tree -p HEAD` writes a commit with the message `gate-remote back` and the payload's identity, and `push -q` sends it, to its end though the client goes. The steps are the run's git steps below, with its attribute and LFS pins, their groups recorded in `<Work>.pgid`.

  A back commit that fails says `gate: remote sync-back failed: <why>` and the status becomes 1 when the command exited 0, else the command's own; a client gone during it gives 255. The reasons of the copy and the lock are:
  - `write-tree failed: the copied index <Index> is gone`
  - `<step> failed: the work folder lock <Lock> is no longer held`, for `write-tree`, `commit-tree` or `push`
  - `cannot copy <Work>/.git/index to <Index>: <Index> is a link or not a file`
  - `cannot copy <Work>/.git/index to <Index>: <Work>/.git is a link or not a folder`
  - `cannot copy <Work>/.git/index to <Index>: <Work>/.git/index is not a plain file`, for a FIFO or other non-file: the source is opened without following a link and without blocking, then checked to be a plain file, so a FIFO cannot hang the copy
  - `cannot copy <Work>/.git/index to <Index>: <Work>/.git/index is missing or a link`, for a missing or linked index; any other failure to open or read it says the OS error text

  On a pair the back commits are all or none: every repo is committed, the main one first, then every back ref is pushed. A push that fails while the run still holds its lock deletes every back ref the run pushed; once the lock is lost, nothing is deleted, since another run may hold it. The step names carry the repo's name.

  A pair run prints `gate: remote back commits: repo <sha>, lib <sha>` (`<name> none` for an unchanged repo) once all are pushed. A failed one prints `gate: remote back commits: failed` and then `gate: remote sync-back failed: push lib failed…` last, with status 1 for a command that exited 0.

  A pair run that ends before its back commit says `gate: remote back commits: failed` last, so the PC applies nothing: after the lock whenever it ends without a refusal (a box kill among them), and before the lock when it fails with status 1 (the lock cannot be opened, or the root check fails). A refusal (75) says no such line. A single-repo run says no back commits line.
- **The tool restore.** After the place checks and with the heartbeat on, before the command, `run` runs `dotnet tool restore` in the place folder when the dotnet tool manifests differ from the record `<Work>.tools`.

  The manifests are found by walking from the place's physical folder (links resolved, as `dotnet` reads it) up to the work folder's (on a pair, up to the main tree, and the tools-resolve error line names the main tree), each folder contributing its `.config/dotnet-tools.json`, else its `dotnet-tools.json`; no manifest, or a record equal to their hash, runs nothing.

  The record is the SHA-256, as lower-case hex, over each manifest's path relative to the work folder and its bytes, in walk order; it is the box's own, so its text need not match the Windows box's. It is written, whole by atomic replace, once the restore exits 0.

  The restore runs along the login `PATH` with no deadline, as the leader of a process group of its own recorded in `<Work>.tools.pgid` (job `<Job>-tools`), so `box stop` ends it; it starts with the inherited signal state reset, as the command does, and both its streams are forwarded like the command's. A live group in `<Work>.tools.pgid` left by an earlier run refuses the next run `busy` (75), as one in `<Work>.pgid` does; a stale one is removed.

  It says `gate: dotnet tool restore took <s> s`. A nonzero exit says `gate: remote tool restore failed (status <n>)`, then the `tools` refusal tag and `gate: remote work folder <key> could not restore its dotnet tools`, and the run ends 75, so the PC runs the command itself. A restore that never started (no `dotnet`, its group record not readied, or the lock lost first) says `gate: remote could not start the dotnet tool restore: <error>`, then the same three lines, status 75.

  A restore ended by a signal, a `box stop` among them, says `gate: remote tool restore failed (signal <n>)` and ends 1 with no tag.

  Status 1 also follows `gate: remote could not read the tool manifest <path>: <error>`, `gate: remote could not place the tool manifest <path> under the work folder <work>`, `gate: remote could not write <Work>.tools: <why>`, `gate: remote could not resolve the work folder <work> or the folder <place>: <error>`, and `gate: remote could not see the dotnet tool restore to its end: <why>`; an unreadable record only notes `gate: remote could not read <Work>.tools: <error>; restoring the tools` and restores.

  A client gone during it ends 255.
- **The package restore.** After the tool restore and before the command, `run` installs the packages of a command whose first word's file stem is `pnpm`, `bun`, `bunx`, `npm` or `npx` (any case), by the Windows box's rules ([The box side](#the-box-side)): the same start-folder reading, nearest lockfile, hashes, one-manager-per-folder rule, skips and lines, in the same order.

  The rules live in `packages.rs`, pure and built on every target; the install runs on a Unix machine only. Where it differs from the Windows box's:

  It handles the one command of the request (the Unix box builds no warm-up), runs `pnpm install --frozen-lockfile`, `bun install --frozen-lockfile` or `npm ci --ignore-scripts --no-audit --no-fund` along the login `PATH` with the inherited signal state reset, `CI=true` and `TMPDIR` the run's temp folder, with umask 022 and no time limit, and forwards its output like the tool restore's.

  It has no restricted token and no job: it is the leader of a process group of its own recorded in `<Work>.packages.pgid` (job `<Job>-packages`), so `box stop` ends it (`gate: remote stopped job <Job>-packages`). A live group in `<Work>.packages.pgid` left by an earlier run refuses the next run `busy` (75), and a stale one is removed.

  The record `<Work>.packages` holds the same `{key}\t{hash}` lines (keys `.`, `web`, `bun:web`, `npm:web`, with `/` between folder names), sorted and compared as bytes. It is cleared with `<Work>.warm` and `<Work>.tools` by an interrupted command's record and kept across a layout change. The install is skipped while the folder's line matches and `node_modules` is a real folder.

  A failed or stopped install leaves no line for its folder: its own line is dropped with the other managers' lines for the folder, and the record is written before the install runs, so the next run installs again.

  A `node_modules` that is a link, a file or a fifo is never installed through: the run says `gate: remote <name> install skipped: <place>/node_modules is reached through a link` (at the root, `node_modules is reached through a link`) and the command runs. One that cannot be looked at fails the run with status 1 and `gate: remote could not read the folder <path>: <error>`, and so does a folder on the lockfile walk that cannot be looked at (it does not read as a link).

  A lockfile folder whose relative name is not UTF-8 or holds a control character is skipped with `gate: remote <name> install skipped: <folder> has a name the packages record cannot hold`, and the command runs.

  The installs leave their caches in the builder's home (`~/.npm`, `~/Library/pnpm/store`, `~/.bun/install/cache`) at mode 0755 and 0644, and every standard macOS account is in the home's group `staff`. The installer's first step, `Private home`, makes the home mode 0700 so other accounts cannot read them, and refuses a home that is a symlink or owned by another account.

  Package files and lockfiles are read whole up to 256 MiB, and one with more than one name is refused as on the Windows box: `gate: remote could not read the package file <path>: it has <n> names`, the same for `the lockfile`, or `...: it is longer than 268435456 bytes`, with status 1.

  A link at a lockfile's name stops the walk and fails the run on the read (`gate: remote could not read the lockfile <path>: <error>`), as on Windows; a start folder that is a file walks on up.

  A `check-ignore` ended by a signal, a `box stop` among them, ends the run with status 1 and `gate: remote check-ignore failed: git ended with signal <n>`.

  A manager missing from the login `PATH`, or an install that fails, refuses the run 75 with the `tools` tag and `gate: remote work folder <key> could not restore its <name> packages`, the failed install first saying `gate: remote package restore failed (status <n>)`; one ended by a signal says `(signal <n>)` and ends 1 with no tag.

  The other lines are the Windows box's, with `could not start the <name> install: <why>` and `could not see the <name> install to its end: <why>` for the Unix install's own failures; a client gone during it ends 255.

- **The run's git steps.** Each step runs as the leader of a process group of its own. Its output is read by one thread per pipe, which writes each line to the client itself, one write per line, so the thread that waits on git never writes and a stalled client cannot delay a step's bound.
  - **Bound by class.** Fetch, checkout, clean, `lfs checkout` and `add`: 30 min; `push`: long; `commit-tree` and the rest: 5 min. A step past its bound gets `SIGTERM`, then `SIGKILL` after 2 s, to the group.
  - **Ending.** The box learns of the leader's exit without reaping it, judges the group ended only when the leader has exited and the group lists no other process, ends the group's leftovers (`SIGTERM` with `SIGCONT` right after it, `SIGKILL` after 2 s), removes the record, then reaps, so no signal reaches the group after the reap. The step keeps git's status.

    The drain then waits for the readers, bounded at 30 s from its start. A reader still blocked in a read is a leftover process's and is left behind, with `gate: remote git's output is still held open by a leftover process; stopped reading` on stderr, said only for a step whose output is forwarded; a reader still in a write is held by a client that stopped reading, which counts as gone.
  - **Client gone.** No line is written once the client is gone. A failed write to stdout or stderr marks the client gone, and so does a `POLLHUP` probe of both before each step and during every wait. A gone client ends the step's group and the run with status 255 and `gate: remote the client closed the connection during git <step>`.
  - **The record.** The group is written to `<Work>.pgid` from the child before git runs, atomically at 0600, as `<job>\n<pgid>\n<sid>\n` with the session id from `getsid(0)`, and removed by the runner once the group has ended, so `box stop` ends a running step.

    Right after the spawn the parent rewrites the record, atomically, as `<job>\n<pgid>\n<sid>\n<sec>.<usec>\n`, the fourth line being the leader's start (microseconds as six digits) from `proc_pidinfo`. A fifth line, or a fourth that is not digits, a dot and six digits, makes the record malformed; the two- and three-line forms are still read and carry no start.

    When the fourth line cannot be written the run says `gate: remote job record <record> keeps no leader start: <why>` (stamped `gat`), with `<why>` one of `the start of its leader <pid> could not be read`, `the work folder lock <Lock> is no longer held`, `the work folder lock <Lock> could not be looked at: <error>`, `the session could not be read: <error>` and `it could not be rewritten: <error>`.

    A step whose member stays past the 10 s wait after `SIGKILL` fails as `gate: remote <step> failed: its process group <G> was still there 10 s after SIGKILL` and keeps the record, so the next run refuses `busy`.

    A step whose exit cannot be read or whose group fails to end fails as `gate: remote <step> failed: its exit could not be read: <error>` and keeps the record: the leader is not reaped and the next run refuses `busy`. A run whose work lock is no longer held fails the step as `gate: remote <step> failed: the work folder lock <Lock> is no longer held` with status 1 and leaves the record.
  - **Stale-record rules.** A run that finds `<Work>.pgid` removes it under the work lock when stale, else refuses `busy` (75, before any start marker), never signalling the group and never sweeping git's lock files.

    A record is stale when `killpg(group, 0)` gives `ESRCH` or `EPERM`; or its mtime is before the boot time; or a process has the group's id but is another user's or started more than 1 s after the record's mtime (a reused pid). The mtime checks (against the boot and the 1 s slack) apply only to a record without a start.

    For a record with a start, the record is current when the leader with that start is there, or when the leader is gone and a member is in the recorded session (or the members cannot be read), and stale otherwise; a pid with another start is a reused number. With that leader gone, the record is current only when a live member of the group is in the recorded session (`getsid`); a two-line record, which names no session, is stale then.

    A leaderless group whose listed members all read `ESRCH` is listed again, up to 3 passes 50 ms apart, and counts as current unless `killpg(group, 0)` gives `ESRCH` or a pass lists no member. A malformed record is removed under the work lock. Anything else the box cannot tell counts as current.

## Install

- `pwsh tools/install.ps1` builds a Release `gate.exe` and the desktop helper with `cargo build --release -p gate -p gate-desktop` (default features, so no `test-seams`) through the repo's `tools\gate.ps1` on a heavy slot, and installs them to `%LOCALAPPDATA%\gate\bin\gate.exe` and `gate-desktop-helper.exe` beside it, one build so both carry the same build id.

  Installed gates never run from `target/`, which build cleaners empty and a rebuild overwrites while gates hold it open. It refuses an unset or empty `LOCALAPPDATA`.
- Both exes link the C runtime statically (`.cargo/config.toml`, `+crt-static` for MSVC targets) and carry `DependentLoadFlags = 0x800` (`LOAD_LIBRARY_SEARCH_SYSTEM32`, set by each crate's `build.rs`), so their static imports load from System32 only and the install folder needs no VC runtime DLLs and loads none planted beside the exe.

  A build for an MSVC target with `RUSTFLAGS` or `CARGO_ENCODED_RUSTFLAGS` set fails with `gate's exes must link the C runtime statically (.cargo/config.toml); RUSTFLAGS or CARGO_ENCODED_RUSTFLAGS overrides it, so unset them for this build`, which `tools/install.ps1`'s build meets too.
- When the built exe has the same SHA-256 as the installed one, the copy is skipped and the script prints `install: gate <version> already installed at <path>`.

  The helper is compared the same way, and the success lines name it: `install: installed gate <version> at <path> with its desktop helper at <helper path>`, `install: gate <version> already installed at <path> with its desktop helper at <helper path>`, and, when only the helper changed, `install: gate <version> already installed at <path>; installed its desktop helper at <helper path>`.
- Otherwise the new exe is first copied to a temporary name in the folder; then a copy already there, running or not, is moved aside to `gate.exe.<stamp>.<pid>.old` (which works while gates run it), and the temporary file is renamed into place, so `gate.exe` is missing only between two renames.

  A failed copy leaves the installed exe untouched; a failed final rename moves the old exe back, and the failure line says whether that worked. The script prints the version it replaced and the version it installed.

  The helper goes first, with the same temp copy, move-aside and rename steps, and its aside copies are `gate-desktop-helper.exe.<stamp>.<pid>.old`, deleted like gate.exe's. A failure afterwards, in gate.exe's copy, rename or verification, puts the previous helper back, so the previous pair stays; a first install's helper is removed.

  A helper that cannot be put back says `install: the desktop helper at <dest> could not be put back as it was: <message>` (`; the previous desktop helper is at <aside>` when one was moved aside), `install: another desktop helper was placed at <dest> meanwhile ...` when another process placed one, and `install: the previous desktop helper was put back beside a gate.exe another process placed; they may not match`.

  A helper built with its test seams is refused before anything is changed: `install: the desktop helper at '<path>' was built with its test seams; build it without features`.
- The script holds `push.lock` in the install folder, open and unshared, from the confirmed Install step to its end, the same lock `gate push` takes on a builder and on this PC ([`gate push`](#gate-push)). While a push holds it the script stops with `install: another gate push is installing; nothing was changed` and exit 1.

  The script needs pwsh 7.2 or later (`#Requires -Version 7.2`); `install: could not prepare the install: <message>` says its small C# type for the file checks could not be added.

  A `push.lock` that is a link, a folder, a junction or a second name of another file gives `install: <dir>\push.lock is a link; nothing was changed`, a folder it cannot make `install: could not create <dir>: <message>`, and any other refusal `install: could not lock <dir>: <message>`. Declined (`-Confirm` "No") and `-WhatIf` create no folder and no lock.
- The installed copy is verified when it answers `--version` with `gate <version>` (the script's version reads are bounded at 30 s).

  A copy that does not answer is taken out and the previous gate.exe put back: the script fails with `install: the installed gate.exe does not run; the previous gate.exe is back in place` (`; the previous gate.exe is at <aside>` when the move back fails, `; the copy was removed and no earlier gate.exe existed` on a first install, `; the copy at <path> could not be removed: <message>` when that fails).
- The move into place and the move back run without force over a missing gate.exe and act only while gate.exe is missing or the install's own file (the temp copy's file id, read from the handle that wrote it; a file gone after the last id read counts as missing); one another process placed is left as it is, with `; another gate.exe was placed meanwhile and was left as it is` and, on a replacing install, `, the previous gate.exe is at <aside>`.
- Aside copies no process holds are deleted only once the installed gate is verified (also when the exe is already installed and answers), but this install's own, which stays as the last good gate; one a running gate still holds stays until a later install. Every aside copy is kept, silently, when the install ends unverified. One that cannot be deleted prints `install: could not delete <path>: <message>` on stderr.
- It adds `%LOCALAPPDATA%\gate\bin` to the user's PATH when missing, so callers run `gate`: the raw registry value of the user environment is read and written, keeping its kind and its unexpanded `%NAME%` entries; the change is broadcast (`WM_SETTINGCHANGE`, as `setx` does) so programs started from then on see it, and the script says to open a new terminal. It never touches the state root's files.
- `-WhatIf` lists every step a real run would take without taking it, the aside deletes included.

  For the exe already in place it prints `install: gate <version> already installed at <path>` and a Delete line for each aside copy a real run would delete, when the installed gate answers `--version`. The helper is listed too.

  `-Exe` (install a prebuilt exe, skipping the build), `-HelperExe` (the prebuilt helper; each is given only with the other, else `install: -Exe and -HelperExe are given together or not at all`, and a missing or unreadable one is `install: no desktop helper at '<path>'` or `install: could not read the desktop helper at '<path>': <message>`), `-EnvironmentKey` (the registry key the PATH entry lives in) and `-PrintBuild` (prints the build's cargo arguments on one line and exits 0) exist for the suite, which runs the script against a temporary `LOCALAPPDATA` and registry key.
- The script records which commit the installed gate was built from in `%LOCALAPPDATA%\gate\bin\source.json`, one JSON object and one LF in UTF-8 without a byte order mark: `{"repoRoot":"<abs path>","commit":"<40 hex>","buildId":"<id>","dirty":true|false}`. `commit`, `buildId` and `dirty` come from one read of HEAD, the id being the first 12 hex of the full sha.

  The record is written or removed only for an install that happened under `push.lock` (or found the files already installed), and only once the installed gate answers `--version`, using that answer, never a second call. A gate that does not answer gets the record removed, with `install: the installed gate does not answer --version; removed <path>`.

  A leftover `source.json.<pid>.tmp` from a killed run is removed first; the write goes through a temp file and a move. A write or removal that fails leaves the gate installed and the PATH entry added, prints `install: could not write <path>: <message>` or `install: could not remove <path>: <message>` and exits 1.

  `-Source <folder>` (the suite's; given only with `-Exe`, else `install: -Source is given only with -Exe`) names the work tree the record describes in place of the script's own. It must be the root of a git work tree (a junction to one or a trailing separator is accepted); a subfolder or a non-repo is refused with `install: -Source <folder> is not a git work tree`, a repo with no commits with `install: -Source <folder> has no commits yet`, each exit 1 with nothing changed.

  `-WhatIf` lists `What if: Performing the operation "Write" on target "<bin>\source.json".`, or `"Delete"` when the files are already installed and the installed gate does not answer, and writes and deletes nothing. The script sets `$PSNativeCommandUseErrorActionPreference = $false`, so a caller profile's `$true` does not turn git's exit 1 into a throw.
- Every install deletes a leftover `remote-bridge.ps1` file in the install folder, which no gate starts. When it cannot, it prints `install: cannot remove the old remote-bridge.ps1 (<reason>); it is unused` on stderr and carries on with exit 0; a folder at that name is left alone.
- The script installs locally only. `gate push` copies the installed gate.exe and the desktop helper to each builder in `remote.json` (`host`, then `hosts`), into the builder's `C:\Program Files\gate\bin` with the same rename-aside, and registers the desktop task there on every push, so a machine with builders runs `pwsh tools/install.ps1` and then `gate push` ([`gate push`](#gate-push)).

  It differs in order: the script places the helper first, the push places gate.exe first and the helper after it. It pushes the installed files, never ones from `target/`, and never installs here.
- Install is a script, not a `gate install` subcommand.

### Build id

`gate --version` prints `gate <package version>+<build id>`, and the box preflight's `gate: remote version` line says the same text. The id is the text between the first `+` and the next `+` or the end: the 12-hex head commit, or `<12-hex>-dirty.<8-hex>` for a build of changed tracked files, the 8 hex being the first 8 of the SHA-256 of `git diff HEAD --binary`. Untracked files do not count.

A build with the `test-seams` feature (the suite's) ends its version text with `+seams` after the id or after `dev`: `gate 0.1.0+dev+seams`, `0.1.0+<12hex>+seams`, `0.1.0+<12hex>-dirty.<8hex>+seams`. Everything that shows or compares the version, `gate --version` and the box's `gate: remote version` line included, shows it, so a seams build never equals the clean build installed beside it.

`tools/install.ps1` works the id out and sets `GATE_BUILD_ID` for its release build only, running git with `--no-optional-locks`. An empty id removes any `GATE_BUILD_ID` the caller had set, so the build never takes an outer one. The build runs with the launcher's `-Local`, so it is never routed to a builder.

A build with no id (not made by the install script, or git failing) says `+dev`, and compares equal only to another `+dev`. When git cannot say, the script prints `install: git could not say which commit this is or what changed in it; building a gate whose version ends +dev`.

The remote client compares the builder's whole version text with its own ([Routing](#routing)), so another build of one package version falls back with reason `version`; `gate push` still decides "already installed" by the exe's SHA-256, and its lines name the whole version text.

## Cutover

The PowerShell gate and `gate.exe` share slot mutexes and never share files.

The default root's mutex names are the PowerShell gate's, so a PowerShell gate still running and a new `gate.exe` gate take turns on one set of slots and the machine is never over-subscribed; each generation writes its claims, tickets and journal in its own folder and format, and once the drain is over ops-tower reads `gate.exe`'s, plus the PowerShell claims of a slot whose mutex is open. Moving a machine to `gate.exe` is a drained cutover:

1. Before it runs, ops-tower's Gates tab already writes the default root's `slot-counts.json` (`%LOCALAPPDATA%\gate\v1\`) on every edit, through `gate-state`'s writer, since `gate.exe` never reads the PowerShell gate's `%LOCALAPPDATA%\gate\slot-counts.json` ([Counts file](#counts-file)).

   `gate cutover` copies the PowerShell gate's slot counts into the default root's `slot-counts.json`, so both generations count the same slots, then writes `%LOCALAPPDATA%\gate\cutover.flag`, holding its write time in RFC 3339 UTC (the launcher never reads it). The launcher in each repo hands every call to the installed `gate.exe` through the compatibility layer, flag or no flag ([The launcher's hand-over](#the-launchers-hand-over)).
2. It then waits until every PowerShell gate's claims and tickets in `%LOCALAPPDATA%\gate\` are gone or stale, naming on stderr what it still waits on, and exits 0 once none is left. The drain of the PowerShell gates (`powershell_gates`) reads claims and tickets only and probes no mutex; a pid whose start cannot be read counts as live.

   Under the default root a slot a live PowerShell claim holds counts once.

   It probes slots `0..remote_count` and every index a v1 claim names, reading tickets, then claims, then mutexes, and a process whose start cannot be read counts as live.

   What it cannot read (a remote-slot claim, a ticket of any kind, an out-of-range index or arrival, a `slots` or `waiting` folder that exists but cannot be listed) it reports by the root's real path in `unreadable`; a v1 ticket whose open is refused is going away, not unreadable.

   It ends only when every scan across a 5 s confirmation window finds no gate, because the PowerShell gate deletes its ticket when it takes a slot and writes its claim up to about 2 s later, and a single empty scan could fall in that gap. The lines are in [`gate cutover`](#gate-cutover).

   It ends by printing the flag's path and saying that deleting it goes back to the PowerShell gate. That line is stale: the launcher never reads the flag and the PowerShell gate is gone, so the take-back is reinstalling the previous commit (`tools/install.ps1` from it).

   A `gate cutover` stopped part way is run again: the rerun rewrites the flag and goes back to waiting, and it copies the slot counts only when the default root's `slot-counts.json` is absent, so a Gates-tab edit made since is kept.

   The copy never replaces: the file is written beside the target and hard-linked into place, so one that appears meanwhile wins, and a target that exists but cannot be read is kept too.

   Callers that start the PowerShell gate without the launcher keep starting PowerShell gates after the flag is set. The drain names on stderr each PowerShell gate whose earliest claim or ticket `since` is later than the flag's write time (a late arrival), with its command line, so a bypassing caller is visible rather than holding the drain open silently; each pid is named once. The cutover is not run until claude-config lists the callers it moved.

### The launcher's hand-over

The launcher is this repo's `tools/gate.ps1` (`#requires -Version 7.3`), a slim pass-through to `gate.exe`; the black-box cases run it as checked in. It reads no flag.

- It hands over to the first `gate.exe` that exists:
  - A non-empty `GATE_BOX_EXE` is the only candidate. It must be a fully qualified existing file; otherwise the launcher says `gate: GATE_BOX_EXE names <value>, which is not a gate.exe here` on stderr and exits 127.
  - Else `<root>\gate\bin\gate.exe`, where `<root>` is `GATE_LAUNCHER_PROGRAM_FILES` when set and non-empty, else `ProgramFiles`. The override exists for tests, since Windows resets `ProgramFiles` in every process.
  - Else `%LOCALAPPDATA%\gate\bin\gate.exe`.
- It waits for gate.exe and exits with its whole status. It says nothing of its own: gate.exe's lines are the whole output. A Ctrl+C reaches both; the launcher waits for gate.exe to end and exits with its status.
- The words go through as the caller gave them, `-StopTree` included (the compatibility layer parses it), and the subcommands (`run`, `stop-tree`, `push`, ...) go as they came. An in-session call whose `--` PowerShell's parser ate gets it put back before the command, for both the compatibility spelling (`-Log ...`) and the `run --log ...` spelling, with split `-p:` colon words rejoined.
- With no `gate.exe` found, the launcher says `gate: no gate.exe found; running the command without slots or watchdog` on stderr and runs the command directly, with the command's own status:
  - With a `--`, the command is the words after it; only `-Cd`/`--cd` and `-Env`/`--env` are read before it. Colon words are rejoined only when no `--` was found.
  - `-Env` variables are restored afterwards. A `GATE_` name is refused with `gate: -Env cannot set GATE_ names: <name>` and exit 2.
  - A `-Cd` folder that cannot be entered gives `gate: cannot enter the -Cd folder <folder>: <reason>` and exit 2.
  - A command that is not found or throws gives `gate: <message>` and exit 1.
  - `-StopTree <pid>`, `-StopTree:<pid>` and `stop-tree <pid>` give `gate: no gate.exe found; cannot stop the tree of <pid>` and exit 1.
  - With no command, `gate: missing the command: put it after --, e.g. -- dotnet test` and four usage lines on stderr, exit 2.

Callers that start the PowerShell gate without the launcher (inventory section 4) are moved by the caller sweep.

During the drain a PowerShell gate neither reads `gate.exe`'s tickets nor signals its wake events: a PowerShell critical request can be overtaken by `gate.exe` gates that arrive after it (it still waits for every slot), and a `gate.exe` waiter learns of a PowerShell gate's release from its fallback poll.

ops-tower follows the flag: it reads the PowerShell gate's files until the flag exists and then `v1\`, plus the PowerShell claims of a slot whose mutex is open, and its waiters come from `gate.exe`'s tickets ([Common rules](#common-rules)). The hooks and scripts that read gate command lines or output (inventory section 4) change in the same drain.

The caller sweep keeps each repo's `tools/gate.ps1` as the slim pass-through ([The launcher's hand-over](#the-launchers-hand-over)), so a clone on a machine without `gate.exe` still runs its commands, and switches that repo's docs and scripts to `tools/gate.ps1 run` where they call the gate directly.

Every caller runs the gate through its repo's `tools/gate.ps1`: `pwsh -NoProfile -File tools/gate.ps1 run --log <path> --timeout <s> --slot <kind> -- <command>` (`--stall`, `--tail`, `--no-markers`, `--local`; `tools/gate.ps1 stop-tree <pid>`). Nothing calls `gate.exe` directly or by a path. `tools/install.ps1` builds with a plain `cargo build`, not through a gate, so a broken installed gate can always be replaced.

A caller that runs `gate ...` on PATH gets "command not found" on a machine without it, and each repo's docs name the install script; a caller that runs the launcher gets the launcher's direct run there, not `gate.exe`'s.

The compatibility layer is deleted once no caller uses `-Log`, `-TimeoutSeconds` or `-Slot`. The guard launcher stays.

## Crate layout

A virtual Cargo workspace (`resolver = "3"`) with every crate under `crates/`, listed by path. The root copies ops-tower's `[workspace.lints.clippy]` table, its `[profile.test-release]` and its `rustfmt.toml`, so moved code lints and formats as it did.

| Crate | Owns | Builds on Linux |
|---|---|---|
| `gate-state` | Every file format in the state root (claim, ticket, counts, journal, archive, `remote.json`), their readers and writers, path and object-name rules, journal history and `normalize_command`, the dashboard state ops-tower shows. Moved from ops-tower with its history (GATE-4). | yes; a pure crate |
| `proc-table` | The process snapshot model (`ProcessInfo`, `ProcessTable`, `ProcessTree`) and its Windows reader. Moved with `gate-state` (GATE-4). | yes; its `win32` module is Windows-only |
| `gate-win32` | The Win32 layer: job objects, process creation, named mutexes and events, priority, console, the registration of the desktop helper's Task Scheduler task and the start of a registered one in a given session (`task.rs`, through COM), the WSL VM's CPU reader, the samples the watchdog consumes. | no |
| `gate-desktop` | The desktop helper exe and the pipe protocol the box speaks with it and its exit codes. | no |
| `gate-box-unix` | The Unix box, bin `gate-box` ([The Unix box](#the-unix-box)): `box preflight`, `boot`, `run`, `stop`, `inputs`, `outputs`, `prune`, `drain` and `hold` for a macOS builder, with the builder-side slots (`slots.rs`), the process helpers, the CPU source (`cpu.rs`), the heartbeat (`heartbeat.rs`), the backstop watch (through `gate-watchdog`) and the back commit (`back.rs`). Its verbs and helpers are `cfg(unix)`; on another target the bin says `gate-box: runs only on a Unix builder` and exits 1. | yes; the verbs are Unix only |
| `gate-wire` | The box request payload both the PC client and every box parse (`PathForm::Windows` or `Unix`), and the gate's version text `build_version` with the version parse the tool probes share (`version_of`, `first_version`), the work-root refusal and busy lines (`root_unsafe_line`, `root_busy_line`), the tool probe's result and line (`Probed`, `tool_line`) and the sdks list (`sdk_list`). | yes; a pure crate |
| `gate-inputs` | What the Windows box and the Unix box must judge alike about a routed run's inputs and outputs: the path-name rules (`names`), the stream SHA-256 (`hash`) and the manifest (`Manifest`, `Held`, `Stamp`, `read_sent`, and `refused_path`, the check of the paths a sent manifest names) and the archive member reader (`archive`: `read_members`, its limits and refusals, and the `Place` trait each box writes through), the archive writer's core (`pack`: `Packer`, `Packed`, `window_log`), the output globs (`glob`: `Globs`, `regex_upto`) and the walk's result types (`walk`: `Found`, `Walked`, `relative_path`); `manifest` also holds `Listing`, `Chosen`, `select` and `hash_possible_copies`. No OS calls. | yes; a pure crate |
| `gate-watchdog` | Free-share, load-adjusted clock, stall, ceiling and backstop arithmetic over samples with the kill lines, and the box heartbeat, on an injected clock; the cores-hint verdict. | yes; a pure crate |
| `gate-args` | The gate's own argv: the subcommand CLI, the compatibility layer, the C-runtime and cmd.exe quoting, the `%VAR%` refusals and the PATHEXT resolution rules over an injected environment and file lookup. | yes; a pure crate |
| `gate-router` | The routing rules (T2-T4) over strings: what routes, the path rewrite, the `--no-build` drop (`test` and `run` only), the wrapped dotnet form, the builder warm-up's file, hash and judge (`warm.rs`), the caller's variables a routed run does not send, the repo-declared prefixes that add to them (`env.rs`) and the line it says for them (`watch.rs`), the repo's `.gate/local.toml` patterns that keep a command here (`local.rs`), and the one-key list reader the declaration files share (`list.rs`). | yes; a pure crate |
| `gate` | The `gate` binary: the subcommands, the wait loop, the run, the journal writes, the remote client and `gate box`. | no |

Rules:

- No pure crate's API exposes `windows` or `windows-sys` types; Windows-only code sits in `cfg(windows)` modules. `proc-table` is not a pure crate: its `win32` module's API carries `windows-sys` handle and access-right types.
- `gate-state` and `proc-table` are also ops-tower's dependencies, by one pinned git rev, so their types unify there. `gate-state` depends on `gate-router` (for `remote.json`'s `builders` and the pool id), so ops-tower's build brings `gate-router` and its `toml` with it.
- The pure crates are kept buildable off Windows, but nothing builds them there: the repo has no CI.

## Black-box suite harness

A Rust integration suite drives the built `gate.exe` through its CLI and reads only what it writes: exit codes, stdout, stderr, the log, the journal, claims and tickets.

- **Own state root per case.** Every case creates a fresh folder under the target directory's temporary folder. The folder is the case's working folder, holding its logs and its `LOCALAPPDATA`, and its `state` subfolder is the `--state-dir`, so cases never touch the machine's real slots, journal or counts, or each other's. Cases run side by side.
- **Load.** Watchdog-timing cases depend on the scheduler. The gate under test runs at Normal priority, and its command runs at BelowNormal. Under an all-core load, the gate was measured runnable but not running for up to 13.7 s, and a BelowNormal command for up to 31.8 s with no CPU.

  So a case never races a fixed sleep against the watcher: its command sleeps past the 600 s hang guard, and only the behaviour under test ends it. A stall window a case relies on is far past those gaps, or the case is ignored and run with `--ignored` on a critical slot (the repo's nextup profile lists them).

  The load rule: a case never asserts that something did or did not happen within a wall-clock window a loaded PC can cross. A case that expects a kill runs a command that sleeps until killed (`support::UNTIL_KILLED`), so a starved gate is late, never wrong.

  A case that cannot be written without a timer is a machine-alone case, `#[ignore = "needs the machine alone: run with -Slot critical and --ignored"]`, listed in the nextup profile and run on a critical gate with `--run-ignored only -Reason "<why>"`. Widening a window until it passes on one PC is not a fix: it fails again on a busier one.

  A case machine-alone for another reason keeps the `#[ignore]` reason naming that reason, and the nextup profile lists it with how it runs.

  The MSYS keeper's `boxside::msys::tests::a_start_up_killed_beside_the_keeper_leaves_the_next_sh_working` is `#[ignore = "needs no other MSYS process alive (a direct ssh session, not a routed run): run on a builder"]`: it needs no MSYS process of the user alive, so it runs with `--run-ignored only` over plain ssh on an idle builder and never as a routed run, whose own box keeps an MSYS keeper up and would pass it with the keeper removed.
- **Git home.** Every git the suite starts, in a case or a harness helper, reads the suite's own git home, `<workspace>\target\tmp\git-home`, not the developer's `~/.gitconfig` and `~/.config/git`: `HOME` and `XDG_CONFIG_HOME` point at it (`support::git_home_for`), the inherited `GIT_CONFIG_*` variables are removed as the gate removes them, and its `.gitconfig` and `git\attributes` are written once per test process.

  The config pins line endings, longpaths, `untrackedCache`, and auto-maintenance and gc off, so a case's git behaves the same on every PC. A case that tests the gate's own git environment (`routing`, `syncback_conversions`' `isolated`) sets its own values after `prepare`. Auto-maintenance stays on in the product; only the suite turns it off.
- **Unit-test scratch folders.** The unit tests of `gate`, `gate-state` and `gate-win32` write under `target\tmp\unit-<crate>`, through each crate's `#[cfg(test)]` `test_support::temp_dir()`, not `%TEMP%`.

  One unit test keeps `%TEMP%`: the one in `remote/mod.rs` that needs a path outside any checkout, since everything under `target` is inside the worktree. Two suite cases in `tests/suite/durations.rs` also keep it: one runs from a folder outside any git repository, and the other only builds a path under the system temp root.
- **Why the files stay under `target`.** `%TEMP%` lies outside the excluded tree and is scanned, which slows every file create and git start there; the suite keeps its files under `target`, which is excluded.
- **Timing the suite.** `cargo nextest run --profile timing` (the `timing` profile in `.config/nextest.toml`; the default profile differs from nextest's only by ending a case still running after 15 periods of 60 s) writes a junit report of every case's time to `target\nextest\timing\junit.xml`.

  To count git processes, set `GIT_TRACE2_EVENT=<folder>`: each git writes a trace file there, at near-zero cost. A process tracer built on WMI is too heavy to time with, since it slows the very starts it counts.
- **What a routed case costs.** A routed case's time is about its git process count divided by the PC's git start rate, so 16 test threads queue on git starts, not on CPU. A change that makes the suite faster removes git processes or makes each start cheaper; adding threads does not.
- **Test seams.**

  Hidden `--test-*` flags, compiled only with the `test-seams` cargo feature, set the timing knobs: `--test-sample-ms` (sample period), `--test-heartbeat-ms` (wait heartbeat; on `gate box` the command heartbeat's period), `--test-poll-ms` (fallback poll), `--test-stall-ms` (stall window below a second) and `--test-stall-from-output` (the local watchdog's stall window runs only from the command's first output byte, so a quiet start is no stall; `--test-print-request` shows it as `testSeams.stallFromOutput`) and `--test-route-hint-floor-ms` (the unpaused wall time under which a passed light run asks no route hint, in place of 30 s) and `--test-load-window-ms` (the length of each window of an `--alone` run's load snapshot, in place of 3 s; `--test-print-request` shows it as `testSeams.loadWindowMs`, after `stallMs`).

  Each takes whole milliseconds above 0 on `gate run`. The sample and stall knobs drive the watchdog.

  Two seams of `gate box` shape the command's console: `--test-box-console fail-before` makes the UTF-8 console fail before the command starts, and `--test-box-park-launch <path>` parks the launch until `<path>` exists ([The box side](#the-box-side)). `--test-box-token refused` refuses the start with the restricted token, for the fallback to the box's own token. `--test-outputs-budget-bytes <n>` replaces the 50 GiB outputs budget of the box's trim.

  Two more seams make a run end by Ctrl+C without a console: `--test-interrupt-after-samples <n>` raises the interrupt after the n-th sample, and `--test-interrupt-queued` raises it when the waiting line is printed, so once no slot is free for the gate, and at the wait for a routed run's work folder. `--test-print-request` shows them as `testSeams.interruptAfterSamples` and `testSeams.interruptQueued`.

  `--test-interrupt-queued-after-fallback` (a switch) raises it once the gate has said it waits for its own slot after a `full` fallback from the builder, the wait that counts from the run's queue line, so a case can pin how a Ctrl+C there counts that wait; `--test-print-request` shows it as `testSeams.interruptAfterFallback`, after `interruptQueued`.

  On `cutover` the seams are `--test-poll-ms`, `--test-heartbeat-ms`, `--test-confirm-ms` (the confirmation window) and the switch `--test-cutover-interrupt-after-list`; `--test-print-request` shows them as `testSeams.pollMs`, `heartbeatMs`, `confirmMs` and `interruptAfterList` ([`gate cutover`](#gate-cutover)).

  On `drain` the seams are `--test-poll-ms`, `--test-heartbeat-ms`, `--test-drain-raise-ms` (the 5 minute re-raise), the switches `--test-drain-interrupt-after-list` and `--test-drain-interrupt-before-raise`, `--test-remote-transport` and `--test-remote-ssh` (which also needs `--test-remote-transport`); `drain` has no `--test-print-request`, and `--test-drain-poll-ms` stays `gate run`'s ([`gate drain`](#gate-drain)).

  On `prune` the seams are `--test-remote-transport`, `--test-remote-ssh` (which also needs `--test-remote-transport`) and `--test-prune-gone-age-ms <n>`; `prune` has no `--test-print-request` ([`gate prune`](#gate-prune)).

  On `push` the seams are `--test-push-ssh <path>` (the ssh program to run instead of the one found along `PATH`, the suite's `fake-ssh`) and `--test-push-bound-ms <n>` (each builder's bound); `push` has no `--test-print-request` ([`gate push`](#gate-push)).

  On `check` the seams are `--test-probe-limit-ms <n>` and `--test-sdk-limit-ms <n>` (each refused with `--builder`), `--test-probe-drain-ms <n>` (also bounding the drain of the builder check's call with `--builder`) and, each needing `--builder`, `--test-remote-transport`, `--test-remote-ssh` (which also needs `--test-remote-transport`), `--test-remote-timeout-ms` and `--test-tailscale-status <file>` ([`gate check`](#gate-check)).

  `--test-print-request`, on `run`, `stop-tree`, `cutover` and the compatibility form, prints the parsed request as one JSON line on stdout and exits 0, so the suite sees what a command line asked for; it is the one hidden switch the compatibility layer accepts. On `run`, `"timeout"` is the number given, or the floor of `auto:<n>`, and `"timeoutAuto": true|false` follows it, always present.

  `--test-sampler-fail` (a switch) makes every sample fail with `--test-sampler-fail makes every sample fail (test seam only)`, so the run says `gate: sampler failed: ...; watching by wall time only` and only the backstop watches; `--test-print-request` shows it as `testSeams.samplerFail`, after `interruptAfterFallback`.

  `--test-diagnostics-budget-ms <n>` (whole milliseconds above 0) replaces the 2000 ms diagnostics gather budget, so a case can make every block it gets cut; `--test-print-request` does not show it.

  `--test-diagnostics-fail` (a switch) makes the diagnostics gathering of a kill fail, so the log carries `gate: diagnostics failed: <message>` in place of the block; `--test-print-request` shows it as `testSeams.diagnosticsFail`, after `samplerFail`.

  `--test-remote-client <path>` replaces the remote client with a scripted one that runs in the gate's own process: the file's steps, one a line, are `out`, `err`, `record`, `wait`, `interrupt`, `killed`, `box-killed timed-out|backstop` (sets the box kill's kind, as the real client reads it from the tag), `status`, `adjusted <seconds>` (the load-adjusted seconds the builder counted), `refuse` (any fallback reason, `cold` included) and `fail-start`, the drain steps `draining {calls} {place|none} {raised} {lapses}` (the next `{calls}` calls answer draining with that place and barrier), `draining-at {call} {place|none} {raised} {lapses}` (the numbered call alone) and `wait-at {call} {path}` (waits at the start of that call for `{path}`, before any drain refusal; a drain poll performs `wait {path}` steps too), and the steps that set how a warm-up ended: `warmed`, `warm-failed commands|refused <reason>` and `warm-deleted`.

  `on <alias>` starts a block whose steps run only for a call to that builder; the steps before the first `on` line run for every builder without a block of its own, and a file with no `on` line runs every step for every call. An `on` with no alias or more than one, or an alias in two `on` lines, is a parse error with its line number.

  The `record` step's JSON carries the call's `warmth`: `null` for a call that asks nothing of the warm-up, `{"consented": <hash>}` for a routed run of a repo whose warm-up is set up (consent and a warm-up file that parsed, with the file's hash), `{"warmUp": {"commands": …, "hash": …}}` for a warm-up.

  `--test-remote-wait-ms <n>` (whole milliseconds above 0) replaces the 60 s budget of the waits for a remote slot, which all the builders of a run share. `--test-print-request` shows them as `testSeams.remoteClient` and `testSeams.remoteWaitMs`.

  `--test-git-limit-ms <n>` (whole milliseconds above 0, on `gate run` only) replaces the 60 s limit of each git call of the routing decision, of the remote client and of the `--test-remote-snapshot` client; `--test-print-request` shows it as `testSeams.gitLimitMs`, after `remoteWaitMs`.

  `--test-key-wait-ms <n>` (whole milliseconds above 0, on `gate run` only) replaces the 90 s bound of a routed run's wait for its work folder's key, the 2 s brief retry of a passing build here taking that key to write its built-here record, and that build's bounded wait for it to remove the outputs-from record, and the switch `--test-interrupt-before-key` raises the interrupt before the key is taken; `--test-heartbeat-ms` sets the wait's step. `--test-print-request` shows them as `testSeams.keyWaitMs` and `testSeams.interruptBeforeKey`.

  `--test-drain-poll-ms <n>` (whole milliseconds above 0, on `gate run` only) replaces the 5 s poll of a routed run's wait behind the builder drain and for a slot on a builder that refused it `full`; `--test-heartbeat-ms` sets the still-waiting line, and `--test-poll-ms`, which the slot wait reads, is not reused. `--test-print-request` shows it as `testSeams.drainPollMs`, after `pollMs`.

  `--test-interrupt-before-later` (a switch) raises the interrupt once a run that fell back has built the next builder it may try, before it tries it, so a case can pin a Ctrl+C during the walk over the later builders; `--test-print-request` shows it as `testSeams.interruptBeforeLater`, after `interruptBeforeKey`.

  `--test-git-drain-ms <n>` (whole milliseconds above 0, on `gate run` only) replaces the 2 s the readers of each git call of the routing decision, of the remote client and of the `--test-remote-snapshot` client, and of this PC's `dotnet --list-sdks` read, get to end once the program has exited (a call that places a folder gives its readers the larger of this and its git limit).

  `--test-print-request` shows it as `testSeams.gitDrainMs`, after `gitLimitMs`. The suite's routing, remote client, `warm` and `cores` cases pass both seams with very long values (`GIT_HANG_GUARDS`), so a starved gate does not fail a call that a loaded machine only delays.

  `--test-beside-claim-wait <file>` (on `gate run` only) makes a local light gate that took its slot holding the beside mutex wait, after it takes the slot and before it writes its claim and journals its `take`, until `<file>` exists, so a case can hold a gate between its take and its claim.

  `--test-built-here-key-reached <file>` (on `gate run` only) makes a passing build here write `<file>` once, after the first slice of its brief take of the work folder's key times out, so a case holding the key lets it go only once the take is retrying.

  `--test-key-waits <file>` (on `gate run` only) makes each take of that key that waits (the bounded wait for the outputs-from removal, and the nothing-back add's wait that a drain marker holds off) append one line `wait` to `<file>` as it begins, so a case counts the waits a passing build makes.

  Before it waits it writes the empty file `<file>.reached`, so a case can tell the gate is at the seam; when that cannot be written the gate says `gate: cannot write the seam's arrival <path>: <error>` and waits on. `--test-print-request` does not show the seam.

  `--test-remote-snapshot` (a switch, on `gate run` only) sends the run to the Rust remote client's snapshot alone ([The remote client](#routing)); `--test-print-request` shows it as `testSeams.remoteSnapshot`, after `gitDrainMs`.

  `--test-remote-transport local:<folder>` (on `gate run` only) sends the run to the Rust remote client with a local folder standing in for the builder; `local-ssh:<folder>` does the same but pushes and fetches to `<host>:<folder>/<repo>.git` with the `--test-remote-ssh` program as git's ssh (which it needs), so git's own ssh calls reach the stand-in.

  After either prefix the folder may be a map, `{alias}={folder}|{alias}={folder}…`, giving each builder its own folder (a case that routes one run across several builders); plain `local:<folder>` is unchanged and serves every builder.
  A part is a map when it holds `|`, or starts with `{alias}=` for an alias `remote.json` could name (gate-router's host rule: letters, digits, `.`, `_`, `-` and one `@`), so a drive path stays a plain folder. Aliases match case-insensitively. A malformed map (an empty folder, an alias named twice) is refused with the same `it is not local:<folder>` line as any other bad form.
  A map that lacks a folder for an alias the run's `remote.json` names (each `builders` entry and `host`) ends the run before any call with `gate: --test-remote-transport names no folder for builder {alias}`; a missing or unreadable `remote.json` checks nothing, and extra entries pass.
  `gate check --builder` takes the same forms.

  `--test-remote-ssh <exe>` replaces ssh with a program (the test bin `fake-ssh`), found as the gate finds its own programs ([Command launching](#command-launching)): a name with a folder part against the working folder, a bare name along PATH only. One it cannot find ends the run before any call with `gate: cannot use the remote transport '<transport>': cannot find --test-remote-ssh <word>: <why>` and status 1, after the transport's own form is checked.

  `--test-remote-retry-pause-ms <n>` (whole milliseconds above 0) replaces the 2 s pause before the pre-banner retry; it and `--test-remote-ssh` need the transport, and `--test-remote-snapshot` cannot be given with it. `--test-print-request` shows them as `testSeams.remoteTransport`, `testSeams.remoteSsh` and `testSeams.remoteRetryPauseMs`, after `remoteSnapshot`.

  `--test-local-sdks <comma list|none>` (needs `--test-remote-transport`) replaces the `dotnet --list-sdks` read of the parity check with a list of versions separated by commas, or `none` for no SDK.

  `--test-dotnet-limit-ms <n>` (whole milliseconds above 0; refused given twice) replaces the 30 s bound of the parity check's `dotnet --list-sdks` read. `gate box` takes the same seam before the verb for the preflight's read ([The box side](#the-box-side)).

  `--test-restore-limit-ms <n>` (whole milliseconds above 0, on `gate run` only) replaces the 300 s bound of the `dotnet restore` a routed build runs here ([Routing](#routing), The restore here); what is left of the routed command's ceiling still bounds it, and `--test-print-request` does not show it.

  `--test-remote-bound-ms <n>` (whole milliseconds above 0) replaces the 30 s bound of the client's `stop` and `boot` calls.

  `--test-remote-interrupt-at <preflight|inputs|lfs|run|outputs|install|sdks|stop|manifest|apply>` makes the client raise the console interrupt itself after that call's first output byte (for `inputs` or `lfs`, the first of either call); `install` raises it before the second file's move, so an install of one file is never interrupted, and `sdks` just before the parity check reads this PC's SDK list with `dotnet --list-sdks`; `manifest` just before it reads the snapshot's `.gate/toolchain.toml`; `apply` once a pair's sync-back has checked every patch and before it applies the first (no output byte is waited for); `stop` raises it at the run call's first byte and again while the stop call runs.

  `--test-remote-backstop-ms <n>` (whole milliseconds above 0) replaces the run call's backstop of 5 x `--timeout` + 60 s for the client, counted from the run call's start.

  `--test-remote-snapshot-age-ms <n>` (whole milliseconds above 0, on `gate run` and `gate warm`) puts each attempt's [snapshot time](GLOSSARY.md#snapshot-time) `n` ms before the moment the snapshot begins, so a case can have the install stamp its files well before the moves and keep their hashes.

  Refused without the transport with `gate: --test-remote-snapshot-age-ms needs --test-remote-transport`, and a value that is not a whole number above 0 with `gate: --test-remote-snapshot-age-ms must be a whole number above 0, got '<v>'`. `--test-print-request` does not show it.

  `--test-remote-tamper <point>:<step>` makes the client change its own folders at one point of its run, so a case can change a folder after the route checked it. It exists only in a `test-seams` build and needs `--test-remote-transport`.

  The values are `before-hold:case` (the first sibling's folder renamed to upper case), `before-hold:replace` (the first sibling replaced by a new folder holding an empty `.git`), `before-hold:replace-root` (the worktree root replaced the same way) and `after-first-snapshot:worktree` (a `core.worktree` written into the first sibling's git config, naming the folder beside it with `-canary` after its name).

  The first three are made before the client holds the run's folders, the last once the main repository's snapshot is taken (`remote/tamper.rs`).

  Refused without the transport with `gate: --test-remote-tamper needs --test-remote-transport`, and a value outside those four with `gate: --test-remote-tamper must be before-hold:case, before-hold:replace, before-hold:replace-root or after-first-snapshot:worktree, got '<v>'`. `--test-print-request` does not show it.

  `--test-remote-channel-fault <kind>` makes the PC's side of an outputs pull's [data channel](GLOSSARY.md) take one fault once it dialled the box. It exists only in a `test-seams` build and needs `--test-remote-transport`, else `gate: --test-remote-channel-fault needs --test-remote-transport`.

  The kinds are `box-proof` (the box's proof read is taken for a mismatch: the PC sends nothing and drops, with `the box's proof does not match`), `truncate` (stops after half the size and drops without an ack, `the test seam stopped the pull after <got> of <size> bytes`), `alter` (flips the first byte it reads, so the hash and the box's ack check fail) and `no-ack` (reads everything and drops before the ack, `the test seam dropped the channel before the PC's acknowledgement`).

  Each ends with the box's `channel` refusal and the ssh pull installing the archive whole.

  A kind outside the four is refused like the other call names.

  `--test-remote-timeout-ms <n>` (whole milliseconds above 0) applies only under `--test-remote-watch-call` naming a call other than `run`, as that call's transfer ceiling; given with `--test-remote-transport` without such a call it is refused with `gate: --test-remote-timeout-ms needs --test-remote-watch-call naming a call other than run`.

  `--test-remote-watch-call <preflight|push|inputs-diff|inputs|lfs-diff|lfs|run|ls-remote|fetch|outputs>` makes `--test-stall-ms` and `--test-remote-timeout-ms` apply to that one call alone, its limit counted from the call's own start, every other call keeping the run's own window and limits. For `run` only the stall seam applies (the backstop has its own seam); for any other call `--test-remote-timeout-ms` is that call's transfer ceiling.

  With `ls-remote`, `--test-remote-timeout-ms` is that call's deadline and `--test-stall-ms` alone leaves it on the git limit. With `fetch`, the stall and ceiling seams apply to the fetch alone; with `outputs`, to the outputs fetch alone; with `inputs-diff`, to the inputs' diff call alone, and with `inputs`, to the inputs' put call alone, whose stall window counts the later of the last byte fed and the last byte of output while the archive is being fed.

  `lfs-diff` and `lfs` name the LFS diff and put calls the same way.

  A value outside a seam's calls says `gate: --test-remote-watch-call must be preflight, push, inputs-diff, inputs, lfs-diff, lfs, run, ls-remote, fetch or outputs, got '<v>'`, and `--test-remote-interrupt-at` says `must be preflight, inputs, lfs, run, outputs, install, sdks, stop, manifest or apply`.

  The five (with `--test-remote-backstop-ms`) need the transport; `--test-print-request` shows them as `testSeams.remoteBoundMs`, `remoteInterruptAt`, `remoteWatchCall`, `remoteTimeoutMs` and `remoteBackstopMs`, after `remoteRetryPauseMs`.

  `--test-outputs-progress-ms <n>` (whole milliseconds above 0, on `gate run` only) replaces the 30 s between an outputs pull's progress lines, counted from the pull's start. It is refused without the transport with `gate: --test-outputs-progress-ms needs --test-remote-transport`.

  `--test-remote-hint-from-pull` (a switch on `gate run` only) makes the routed hang hint not due before the outputs pull starts: its due time and its `still running after` figure count from the pull's start, and the run call says no hint. It is refused without the transport with `gate: --test-remote-hint-from-pull needs --test-remote-transport`, and `--test-print-request` does not show it. Without it nothing changes.

  `--test-remote-fetch-cap-bytes <n>` (a whole number of bytes above 0, default 4 GiB) replaces the cap on what the sync-back fetch's job may write. Refused without the transport with `gate: --test-remote-fetch-cap-bytes needs --test-remote-transport`, and a value that is not a whole number above 0 with `gate: --test-remote-fetch-cap-bytes must be a whole number above 0, got '<v>'`. `--test-print-request` does not show it.

  `--test-remote-apply-cap-bytes <n>` (a whole number of bytes above 0, default 4 GiB) replaces the cap on what a back commit's files may write into the worktree. It needs `--test-remote-transport` and is given once, and `--test-print-request` does not show it.

  `--test-remote-outputs-budget-bytes <n>` (a whole number of bytes above 0, default the 50 GiB outputs budget) lowers the budget behind both the outputs fetch cap and the install's unpack limit.

  Refused without the transport with `gate: --test-remote-outputs-budget-bytes needs --test-remote-transport`, and a value that is not a whole number above 0 with `gate: --test-remote-outputs-budget-bytes must be a whole number above 0, got '<v>'`. `--test-print-request` shows it as `testSeams.remoteOutputsBudgetBytes` (null unless given), after `remoteTimeoutMs`; the fetch-cap and apply-cap seams are not shown.

  `--test-remote-inputs-budget-bytes <n>` (a whole number of bytes above 0, default the 4 GiB one put of inputs may total) lowers that budget: a set the diff names past it is not sent, and the run falls back `inputs`. It is refused without the transport with `gate: --test-remote-inputs-budget-bytes needs --test-remote-transport`, and `--test-print-request` shows it as `testSeams.remoteInputsBudgetBytes` (null unless given), after `remoteOutputsBudgetBytes`.

  `--test-remote-lfs-budget-bytes <n>` (a whole number of bytes above 0, default the 16 GiB one put of LFS objects may total) lowers that budget: the objects the diff names past it are not sent, and the run falls back `lfs`. It is refused without the transport with `gate: --test-remote-lfs-budget-bytes needs --test-remote-transport`, and `--test-print-request` shows it as `testSeams.remoteLfsBudgetBytes` (null unless given).

  `--test-tailscale-status <file>` (on `run`, `warm` and `check --builder`; a test-seams build also reads `GATE_TEST_TAILSCALE_STATUS`) names the file the Tailscale path is read from in place of running `tailscale status --json`, a file that does not exist standing for a PC without Tailscale. On `run`, `--test-tailscale-delay-ms <n>` makes the read sleep before it reads, and `--test-tailscale-wait-ms <n>` makes the release line wait up to that long for it, in place of not waiting at all. `--test-tailscale-hold-start-ms <n>` holds the read's launch guard for n ms between creating its program and putting it in its job.

  `--test-remote-log-cap-bytes <n>` (a whole number of bytes above 0, default 256 MiB) lowers the log cap, the most bytes of one call's output the client writes to the gate's log.

  Refused without the transport with `gate: --test-remote-log-cap-bytes needs --test-remote-transport`, a value that is not a whole number above 0 with `gate: --test-remote-log-cap-bytes must be a whole number above 0, got '<v>'`, and given twice with `gate: --test-remote-log-cap-bytes is given twice`. `--test-print-request` does not show it.

  `--test-remote-script` and `--test-bridge-pwsh` are unknown options in every build (`gate: unknown option --test-remote-script`, and likewise for `--test-bridge-pwsh`), and `--test-print-request` shows no `testSeams.remoteScript` or `testSeams.bridgePwsh`.

  `--test-own-start-fails` (a switch, on `gate run` only) makes both reads of the gate's own start fail, so the run ends with the refusal line of [Output lines](#output-lines), and the quarantine's start read in a sync-back too; `--test-print-request` shows it as `testSeams.ownStartFails`, after `bridgePwsh`.

  `--test-parent-job-trusted` (a switch, on `gate run` and in the compatibility layer) skips the binding of the slot marker to the parent's job, so a case can run under a marker that names the test process; `--test-print-request` shows it as `testSeams.parentJobTrusted`, after `ownStartFails`.

  `--test-job-breakaway` (a switch, on `gate run` only) makes the command's job allow a process in it to create one outside it with `CREATE_BREAKAWAY_FROM_JOB`, the job staying not kill-on-close, so a case stands a process that left the job in for a packaged app; `--test-print-request` shows it as `testSeams.jobBreakaway`, after `parentJobTrusted`, the last key.

  `--test-prune-sweep` (a switch, on `gate run`) lets a routed run whose builder call ended start the background prune sweep, which a `test-seams` build starts only then, handing the child the run's `--test-remote-transport` and `--test-remote-ssh` ([`gate prune`](#gate-prune), The triggers).

  An installed gate has none of them, and without the feature each is an unknown option.
- **Spawn and wait helpers.** The suite's `start` launches the gate and returns a `Running` that can wait for a stderr line, say whether the gate is still running, and finish; dropping a `Running` whose program still runs first runs `gate stop-tree` on it, with its `--state-dir` and environment, under a 60 s guard, saying any failure on stderr and never panicking, so a case that panics also ends its gate's command and what that started; then it kills the pid.

  `Running::kill` kills the pid alone and leaves the command running, for the abandoned-claim cases. Output readers are joined with a deadline, so a grandchild holding the pipes cannot hang the suite. A run's `Outcome` carries `raw_stdout`, the bytes as the gate wrote them, beside the decoded text.
- **Holding and killing a gate.** A case holds a run with a real command, `fake-tool wait <go>`, and lets it go by creating `<go>`. A case that kills a gate lets its command go and waits until the command's log is closed (`support::wait_until_closed`), since the job has no kill-on-close. `support::run_words(log, extra, command)` builds the words of a light run.
- **Box payloads name their own job.** `support::box_payload` gives each case's `gate box run` payload a `Job` name and nonces whose unique part is `support::job_unique`: a hash of the suite process's full pid, the suite's start time and the case's number, since the box's 12-hex job nonce ends in the run's number and leaves 8 digits, too few to hold the pid and case number whole.

  Two cases, or a process that reuses an earlier one's pid or shares its low 16 bits, never share a job whose stop event an earlier box may still hold.
- **Ordering, never elapsed time.** A case asserts the order of events in the journal and the log (a waiter's `take` after the holder's `release`, a heartbeat line before the inner gate's take, a kill line before the exit), never a wall-clock bound a busy machine could cross. A case's own deadline only guards against a hang and is generous.
- **Injected clock.** The watchdog arithmetic is unit-tested in `gate-watchdog` with an injected clock and scripted samples, not through the binary.
- **Fake process trees.** A small helper executable, `fake-tool`, is built only with the `test-seams` feature.

  It runs its steps in order:

  - `argv`, `pid` (prints `pid <its own pid>`), `env <name>` (prints the variable's value) and `exit <n>` (any 32-bit status).
  - `session` (prints `session <its session id>`), `priority` (prints `priority 0x<its priority class>`) and `cwd` (prints `cwd <its working folder>`), one line each on stdout.
  - `out <text>`, `err <text>`, `bytes <hex>` (writes those bytes to stdout as they are) and `lines <n> <text>` (prints `<text>1` to `<text><n>`).
  - `noise <path> <bytes>` (writes a file of that many incompressible bytes, the same on every run, its folders made when missing).
  - `sleep <ms>` and `wait <path>` (until the path exists, giving up after 600 s with exit 1).
  - `burn <stop-path>` (prints `burning`, then spins on the CPU until the file exists), `burn-threads <n> <ms>` (spins on `n` threads for `ms` of wall time, then goes on), `spin <n> <stop-path>` (spins on `n` threads, `n` at least 1, until the file exists).

    There are also `spawn-away <pid-path> <n> <stop-path>` (starts `spin <n> <stop-path>`, or, when `n` is 0, `sleep 600000`, which never looks at the stop path and uses no CPU, outside its job with `CREATE_BREAKAWAY_FROM_JOB`, which the job must allow, writes `<pid> <start>` and a newline to `<pid-path>`, prints `spawned <pid>` and goes on) and `chatter <path>` (prints numbered lines `chatter 1`, `chatter 2`, ... about every 50 ms until the file exists).
  - `spawn-wait <path>` starts `fake-tool wait <path>` with the caller's handles, prints `spawned <pid>` and does not wait.
  - `spawn-orphan <path>` runs `fake-tool spawn-wait <path>` as a child, waits for it to end, then continues with its next step, so the waiting helper is left without a parent and its `spawned <pid>` line is the up signal.
  - `spawn-pid <pidfile> <release-path> <program> [words]` starts the program with those words and the caller's handles, writes its pid to the pidfile, prints `spawned <pid>` and holds the program's handle until the release path exists, so the pid cannot be reused while a case opens it as the parent of a `gate burn`. It ends the steps: nothing after it runs.
  - `spawn-pid-err <err-path> <pidfile> <release-path> <program> [words]` is `spawn-pid` with the program's stderr sent to a file created at `<err-path>`, so a case reads what the program says while it runs (the nesting cases read a covered gate's stderr there, since its log takes its own lines only at its end and the outer gate's pipe is gone with the parent).

  It is copied under the names the gate classifies (`VBCSCompiler.exe`, `dotnet.exe`, `MSBuild.exe`).

  A second helper, `fake-ssh`, stands in for ssh in the remote client's cases (`--test-remote-ssh`). Its `ok` step puts the words of the variable `GATE_FAKE_SSH_BOX_SEAMS` before the verb of a `gate box run` call, so a case gives the box its seams (it reaches a `gate box lfs` call too); the variable `GATE_FAKE_SSH_PREFLIGHT_SEAMS` does the same for a `gate box preflight` call.

  The variable `GATE_FAKE_SSH_ROOT_WALK_STOP`, a folder named as one path (never split), puts `--test-box-root-walk-stop <folder>` before a `gate box preflight` call's verb, after those seams, so the preflight's walk above the work root stops there.

  It reads a box call from the last three words of the line, `box <verb> --stdin`, and the box it runs inherits its stdin, so the request the client feeds reaches the box; a box call whose last word is not `--stdin` is refused by name.

  Its steps are `ok`, `hold`, `version`, `boot <time>` (as `version`, for the box's boot line), `no-boot` (drops the box's boot line), `drop` (exits 255 at once, writing nothing), `exit`, `fail-banner`, `fail-other`, `out-then-drop`, `fail-host <host>` (writes `ssh: connect to host <host> port 22: Connection refused` and exits 255).

  An `on {alias}` line starts a block of steps that answer only calls to that host, numbered by that host's own calls; the steps before the first `on` line answer the hosts with no block, and a file with no `on` line keeps one order for every call.
  An `on` with no alias, with more than one alias, or for an alias that already has a block is refused with its line number: `line N: on takes a builder's alias`, `line N: on takes one builder's alias, got '<a b>'`, `line N: <alias> already has its own steps`.

  They also include `ok-then-out <line>` (as `ok`, then writes the line on stdout), `say-exit <n> <line>` (writes the line on stderr and exits `<n>`, as a call that fails with a line of its own does).

  They also include `fed-hold <path>` (reads its whole stdin and keeps it as `<steps file>.input`, then waits for `<path>`, then runs the call as `ok` does fed what it read, so the call's input has all been sent before the call does anything), `flood <n>`, `say-hold <word> <path>` `progress-ok <n>`, `truncate <n>` (passes `<n>` stdout bytes, writes `Connection timed out during banner exchange` on stderr and exits 255), `garbage <n>`, `bytes-hold <n> <path>` and `bytes-exit <n> <status>` (runs nothing, writes `<n>` bytes of `x` raw on stdout and exits `<status>`).

  A line that starts `pwsh ` and holds `-EncodedCommand` is a `gate push` call (`push` in its call log): `ok` runs it with the local `pwsh`, `LOCALAPPDATA` set to `<GATE_FAKE_SSH_BUILDERS>\<host>`, so each builder is a folder the case owns. Started with `--version` alone, `fake-ssh` answers `gate 0.0.0-old`, the stand-in for an older gate on a builder.

  Beside the `.log` it writes `<steps file>.words`, one line a call (the verb, then the words before the host, ssh's options), and `<steps file>.input`, the stdin of the last push call.

  `fake-tool` spawns children, burns CPU, prints its argv, writes output or sleeps on command, so build-server, orphan and kill-tree cases need no real toolchain.

  A third helper, `fake-desktop-helper`, is the real desktop helper's code (`gate_desktop::run`) built beside `gate.exe`, so a box case can put it where the box looks for the installed one. It is built only with `--features test-seams` (`required-features`), and `gate`'s `test-seams` feature turns on `gate-desktop/test-seams` for it. With no argument it exits 10 (`HelperExit::BadArgument`).

  The helper has five seams, environment variables that exist only in a `test-seams` build of it (unlike the `--test-*` flags). `GATE_DESKTOP_TEST_RECORD`, set to a file's path, has it add the pid of each child it creates to that file, one line each.

  The box writes the line `stand-in <pid> <created>` (the stand-in's pid and creation time) there first, before it resumes the stand-in, and starts the stand-in in no job of its own, so a child the box failed to end outlives the run and a case can read the record.

  `GATE_DESKTOP_TEST_LIMIT_MS` sets each of its waits (connect, message, done) to that many whole milliseconds, raising or lowering each. `GATE_DESKTOP_TEST_PIPE_OWNER=user` makes it take a control pipe its own user owns in place of Administrators. A value of either that it cannot read (a limit that is not a whole number of milliseconds, an owner other than `user`) ends the helper at once with its bad-argument exit, 10.

  The box sets the stand-in's limit to twice its own `--test-box-desktop-ms` limit, so the box's limits pass before the stand-in's.

  `GATE_DESKTOP_TEST_INPUT_DESKTOP` names the input desktop the helper reports in its hello in place of the one it reads, and the helper still judges that name against `Default`; unlike the fault seam, an empty value names the empty desktop.

  The suite's desktop cases set it to `Default` through the box's environment, which the stand-in inherits, so they run and pass in a builder's session 0 as at the console. `boxside::desktop::the_real_helper_checks_the_input_desktop` leaves it unset: on the `Default` input desktop the run is handed off, and anywhere else it is refused with exit 75 and `gate: not routed: the builder has no desktop session: the desktop helper is not on the console's Default desktop`.

  `GATE_DESKTOP_TEST_FAULT` names the step the helper breaks; absent, empty or unknown is no fault:
  - `exit-before-hello` exits 13 before saying hello, `hang` answers nothing and waits until the pipe goes away (13) or its own done limit passes (18), the longest limit it keeps, so a box that keeps its message limit gives up first, and `wrong-version` says the build `0.0.0+fault`.
  - `lie-handle` reports the child's thread handle value as `process` (the real pid and thread), and `exit-after-create` creates its child, leaves it suspended and exits 19.
- **No fixture from the checkout path.** No routing pattern, expected path or assertion is derived from where the repo is checked out; a case builds its own repository and routing file in its root. `checkout::repo_file` is the one route to a checked-in file (the install script), never for a path or pattern a case asserts on.
- **Launcher cases** (GATE-41): `crates/gate/tests/suite/launcher.rs` has 20 cases. They run the checked-in `tools/gate.ps1`. A case's `LOCALAPPDATA` is its `appdata` folder, whose `gate\v1` is the default root with the real slot names, so every such case also sets `GATE_STATE_DIR` to a non-default folder, and its `GATE_LAUNCHER_PROGRAM_FILES` is an empty folder of its own, so the machine's installed gate is never found.

  They compare the hand-over with the same words run directly; the no-gate cases pin the command's own status and the launcher's lines. A new suite module that calls `repo_file` updates the callers pin in `checkout.rs`.
- **Cutover cases** (GATE-9): `crates/gate/tests/suite/cutover.rs` has 15 cases: the counts copy, the flag, the refused words and the printed request, and six over the drain. A live PowerShell gate is a `fake-tool wait` child whose claim or ticket, written by `support::write_powershell_claim` or `write_powershell_ticket` under `support::powershell_folder`, carries the child's real pid and start time; a stale one names a pid that has ended or a start that differs from the running process's.

  The six cases wait on a live claim and name it, wait on a live ticket and name its command line, ignore stale claims and tickets, name a late arrival, resume after an interrupt, and run the launcher's hand-over after a finished cutover.

  They use the launcher cases' helpers (`install_gate`, `run_launcher`, `run_direct`). Their waits are ordering-only: `Running::wait_for_stderr_while_running` returns the stderr once the line is there and the gate has not exited, and no case asserts how long anything took.
- **A packaged app the command starts.** `stall::a_stall_kill_ends_a_packaged_app_the_command_started` runs only where the first `pwsh` on PATH is the Store's (WindowsApps) alias, and skips elsewhere. To tell whether a process is in the gate's job, `IsProcessInJob(h, NULL)` and the job's `TotalProcesses` answer in one run; `winget --version` under `cmd` is the packaged reproducer on Windows 11 and Server 2025.
- **Running it.** The suite is a test target with `required-features = ["test-seams"]`, run through the gate with `cargo test --workspace --profile test-release --all-features`. Without the feature (no `--all-features`) cargo skips the suite silently.

## Cores hint

When a run that passed did not fit its slot, the gate says how many cores the command kept busy, so the caller learns whether heavy or light was right (GATE-6). For example: `gate: used 7.4 cores on average (peak 15, 0.6 free) on a light slot; work that keeps more than 2 busy belongs on heavy`.

- The line is said on stdout, just before the passed line, only for a passed run that took its own light or heavy slot or ran on no slot, ran 10 s or more of unpaused wall time, and had no gate under it take a slot of its own. A critical run is never judged, and a covered or lent run takes no slot.
- A commit run ([Commit run](#critical-drain-and-nesting)) is judged the same way and says the hooks' cores: `gate: the commit's hooks used <avg> cores on average (peak <peak>, <free> free) on a light slot; hooks that keep more than 2 busy belong on --slot heavy`, or `... on a heavy slot; hooks that keep under 1.5 busy fit on --slot light`. It gets no route hint.
- `avg` is the job's CPU (user plus kernel time of every process ever in the job, exited ones included, with a final read at exit) plus the CPU of the escaped tree under the job's members ([Watchdog](#watchdog); read per sample, the final stretch counted with the last sample's processes that still run), over the run's unpaused wall time.

  The CPU of paused samples is left out.

  `peak` is the highest own CPU over one sampler interval, paused samples skipped. `free` is the mean over the unpaused samples of the cores others left free. `avg` and `free` show one decimal; `peak` is rounded to a whole number, ties to even.
- A light run averaging over 2 cores is told heavy would fit. A heavy run averaging under 1.5 cores with at least 4.0 free on average is told light would fit. The peak is reported but does not change the verdict.
- A run on no slot averaging 1.5 cores or more is told `gate: used <avg> cores on average (peak <peak>) on no slot; work that keeps 1.5 or more busy belongs on light or heavy`, whatever was free; the line has no free cores. The conditions above are otherwise unchanged.
- Whether a gate under the run took a slot of its own is read from the journal (`nested_took_slot`): a gate under `Nesting::Under` or `Nesting::Beside` writes `parentPid` on its `take`, and the parent drops its hint and cores only when that `take` lists a slot of a local pool or took the machine (a critical `take`); a routed gate's `take` of a remote slot does not, for any parent.
- A nested no-slot gate takes nothing, so it leaves its parent's hint and cores. When the CPU at the end or the journal cannot be read, the hint is left out with a log-only note ([Output lines](#output-lines)).
- A routed run is never judged: it says no cores hint and its `release` journals no cores. The hint answers which slot fits a command on this PC, and a routed heavy run does its work on the builder, so "light fits" would bring the run back onto the PC, where light never routes.
- The route hint tells the caller of a light run that it could have gone to a builder. It is said on stdout after any cores line and before the passed line: `gate: ran <n> s on a light slot; on heavy, remote.json would send this command to a builder`, `<n>` the whole unpaused seconds, rounded down.

  Only a passed run that took its own light slot and ran here for 30 s or more of unpaused wall time is asked (about twice the routing overhead, so the push costs well under half the run), and only once it has passed, never before. The decision is asked again as heavy with every other input as the run's own: it reads the routing file and git as the decision does, starts no warm-up and says none of the decision's lines.

  The hint still applies only to a light run that ran here: a light run the critical barrier sent to a builder ([Routing](#routing)) is a routed run and gets none, and one that fell back and ran here is judged like any light run.

  It is silent on any other slot, on any failure, under the floor, with `--local`, under a covering or lending parent, inside a git hook of a repo `hooks` does not name, with a command word that is not Unicode, and when the heavy decision keeps the command local. The hint runs while the run still holds its slot, so each git call it makes is limited to 5 s, and one that does not answer in time leaves the run silent. Act on it as on the cores hint: move the command to heavy.
- The journal's `release` line records `avgCores`, `peakCores` and `freeCores` ([Journal](#journal)), the history `--slot auto` will read (GATE-8). `--slot auto` runs a command with too little history on heavy and says it guessed; it never chooses critical or none.
- The gate learns durations and says the expected-time line ([Output lines](#output-lines)); `--slot auto` is built after the local cutover.

## Open questions

What no ruling settles yet, with the issue that owns it:

- **The box protocol and `remote.json`'s next format** (GATE-10): verbs, payload and transport between PC and builder, whether ssh keeps cmd.exe as the builder's shell, and the routing file's version and matcher.
- **A prediction a run has passed** (GATE-14): what a holder past its p90 reports in wait lines, and whether the critical gate re-prices its shadow end and closes or shrinks the lent window when a blocker overruns.
- **Which members the run's end spares** (GATE-144): the run's end spares every job member whose image is `gate.exe`, whatever gate it is; GATE-144 tracks hardening that judgement.
- **The surface of `gate status`** (no issue yet). Install stays a script (`tools/install.ps1`) until a reason for a `gate install` subcommand appears.
- **The Windows facilities to spike before their slice** (GATE-5): other users' command lines (B1), the desktop-app breakaway policy (X1) and nested jobs, each of which changes kill behaviour if it silently fails.
