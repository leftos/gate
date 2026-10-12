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

The CLI is subcommands with GNU-style flags: `--name value` or `--name=value`, and `--` ends the gate's options. The first word picks the parser. `--version`, `--help` and `help` are matched first; then a subcommand name selects the CLI below, and any other first word starting with `-` selects the [compatibility layer](#compatibility-layer). No arguments at all print the usage on stderr and exit 2.

| Subcommand | Purpose |
|---|---|
| `gate run` | Run one command under a slot and the watchdog. |
| `gate stop-tree` | Stop a process tree, reaching the gate jobs inside it. |
| `gate cutover` | Switch this machine from the PowerShell gate to `gate.exe`, draining it first. |
| `gate push` | Copy the installed `gate.exe` to every builder `remote.json` names. |
| `gate box` | Hidden: the builder side of a routed run. |
| `gate --version` | Print the version and exit 0. |
| `gate --help`, `gate help` | Print the usage on stdout and exit 0. |

An unknown subcommand, an unknown flag or a missing value prints what is wrong on its own `gate: ` line, then the usage, and exits 2 before any slot is queued (kept, C1). Every problem found is printed, one per line: invalid values in the order the words came, then the missing inputs in the order slot, timeout, log, command. A state-root failure ([State root](#state-root)) exits 2 with its one line and no usage.

Rules both parsers share:

- An option given twice is a usage error.
- A value option takes the next word unless that word is `--`; an empty value counts as missing.
- A whole number is ASCII digits only: no sign, space or fraction. Timeout, stall, tail and the `--test-*-ms` values are at most 2147483647.
- `--slot` values compare case-insensitively.
- `--slot auto` and `--timeout auto` are refused as invalid values until GATE-8 and GATE-16 build them.
- A gate option that is not valid Unicode is a usage error; the words after `--` are passed through as the caller gave them.
- A word before `--` that is not an option is a usage error (`gate: unexpected argument <word>`).

A valid `gate stop-tree` stops the tree ([`gate stop-tree`](#gate-stop-tree)).

A valid `gate run` starts the command: it refuses a batch word (exit 2), refuses a `--log` inside the state root (exit 2, `gate: the log <path> is inside the gate's state folder <root>`; the paths are compared canonical, through links and junctions, after canonicalising the nearest existing folder), takes the log's mutex, prepares the log, queues, takes a slot, starts the command in the job `Local\gate[-<h>]-job-<gate pid>`, waits for it, reaches the verdict and releases the slot.

The watchdog samples the command and kills its job on a stall, at the ceiling and at the backstop.

### `gate run`

`gate run --slot heavy|light|critical --timeout <seconds> --log <path> [options] -- <command> [args...]`

`--slot`, `--timeout`, `--log` and the command are required and have no default, because a gate with no ceiling holds its caller for as long as the command hangs (kept, C1). Each missing or unreadable input is named on its own line before the usage.

| Flag | PowerShell option | Meaning | Source |
|---|---|---|---|
| `--slot heavy\|light\|critical` | `-Slot` | The slot kind ([Slots](#slots)). | ruling |
| `--timeout <seconds>` | `-TimeoutSeconds` | The ceiling in load-adjusted seconds, a whole number above 0 (W3). | ruling |
| `--log <path>` | `-Log` | The command's log; a relative path is read against the working directory once, at start (P5). | ruling |
| `--stall <seconds>` | `-StallSeconds` | The stall window, default 120 (W2); the gate passes it as given, and the remote client raises a value under 15 to 15 on a routed run (F2). | kept (C1, F2) |
| `--tail <lines>` | `-Tail` | Lines of the log printed at the end, default 20 (V3). | kept (C1, V3) |
| `--no-markers` | `-NoMarkers` | The command's status is the verdict; the marker scan is off (V2). | ruling |
| `--local` | `-Local` | Never route this run to a builder (F1). | kept (F1) |
| `--state-dir <path>` | none | Use another [state root](#state-root). | ruling |

The words after `--` reach the command exactly as the caller passed them ([Command launching](#command-launching)).

### `gate stop-tree`

`gate stop-tree <pid>` takes one process snapshot (pids, parents and start times, no command lines) and builds the tree: the pid plus its descendants, a child counting only when its creation time is readable and at or after its parent's, so a recycled pid adopts no one (a child whose creation time is unreadable is left out with its subtree).

It terminates the job of every gate in the tree first, then the tree's processes deepest first, skipping itself and any process whose creation time changed since the snapshot (the pid was reused) (kept, X6). The members of a terminated gate job share a 2 s grace to exit on their own, and only the processes stop-tree had to terminate itself are counted.

A job or process it stops ends with status 124. Gate jobs are found under the resolved [state root](#state-root), named as the job of each gate pid in the tree, so `--state-dir` and `GATE_STATE_DIR` are honoured.

A failed terminate is a failure only if the process is still alive 1 s later. It prints `gate: stopped <n> gate job(s) and <m> process(es) under <pid>` on stdout and each failure as `gate: could not stop: <reason>` on stderr, and exits 0 when nothing failed and 1 when anything did.

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
- `push: cannot read <path>: <error>`, when the installed exe cannot be read.

`<v>`, `<old>` and `<new>` are the text after `gate ` in `gate.exe --version`, as `tools/install.ps1` reads it; the local one is read once, before the first builder. Per builder, on stdout:

1. `push: <host>: sending gate <v>`, before the transfer.
2. Then one of `push: <host>: gate <v> already installed` (an identical SHA-256: nothing was replaced), or `push: <host>: replaced gate <old>` followed by `push: <host>: installed gate <new>` (`replaced` only when a gate was there before), or on stderr `push: <host> failed: <reason>`.

`push: <host>: could not delete <path>: <message>` on stderr names an aside or stale temporary copy the builder could not delete; it never fails the push.

A failure reason is one of: `timed out after <n> s`, `no answer`, ssh's last stderr line, `cannot feed the call: <error>`, the builder script's own (`the copy arrived damaged`; `the installed copy does not match` with `; the previous gate.exe is back in place` or `; the previous gate.exe is at <aside>`; `<dir> is a link`; the failing step's message). Builder text is escaped as the remote client escapes it and cut to 4096 bytes.

Each builder is one ssh call, run in a kill-on-close job with stdin piped and bounded at 300 s.

A timeout stops only the local ssh, so the builder's install may still finish. The call is `pwsh -NoProfile -NonInteractive -OutputFormat Text -EncodedCommand <script>` through the transport's ssh words ([Routing](#routing-and-the-remoteps1-bridge)), and the exe goes on stdin as base64 text. The builder needs pwsh 7.1 or later. The script carries this PC's SHA-256 of the exe and refuses bytes that differ, reports on `gate-push: ` lines that this side maps to the `push:` lines, and exits 1 after a failure. Concurrent pushes to one builder are not locked.

On the builder the script installs as `tools/install.ps1` does ([Install](#install)), in its own `%LOCALAPPDATA%\gate\bin`, which it creates when missing and refuses when it is a link: it deletes the aside copies (`gate.exe.*.old`) and the stale `gate.exe.<pid>.tmp` copies no process holds; says the same version when `gate.exe` already has the same SHA-256; else writes the exe to `gate.exe.<pid>.tmp` (created new), moves the old one aside to `gate.exe.<stamp>.<pid>.old`, moves the temporary file into place, checks the SHA-256 of the installed copy and moves the aside copy back when anything failed.

Each version read on the builder is bounded at 30 s.

Test-seams builds accept hidden options on `push`: `--test-push-ssh <path>` (the program each builder's call runs in place of the ssh found along `PATH`) and `--test-push-bound-ms <n>` (the 300 s bound, in milliseconds above 0). `gate push` has no `--test-print-request`.

### `gate box`

Hidden from the usage and from `--help`. It is the builder side of a routed run, run over ssh on the builder ([The box side](#the-box-side)).

`gate box preflight|run|stop|boot|outputs <payload>` takes exactly one verb and one non-empty payload word, and no `--state-dir`: every path comes from the payload. Any other shape (no verb, no payload, an empty payload, an unknown verb, an extra word) prints `usage: gate box preflight|run|stop|boot|outputs <base64 JSON payload>` alone on stderr and exits 2.

### Later subcommands

Further modes arrive as subcommands of the same binary, each designed with its issue: `gate expect -- <command>` (the expected duration of a command, GATE-13), `gate status` and `gate drain`. New options, such as `--timeout auto` (GATE-16) and `--slot auto` (GATE-8), exist only in the new CLI, never in the compatibility layer.

### Compatibility layer

`gate.exe` also accepts the PowerShell gate's argument form, so the cutover breaks no caller:

`gate -Log <path> -TimeoutSeconds <n> -Slot heavy|light|critical [-StallSeconds <n>] [-Tail <n>] [-NoMarkers] [-Local] -- <command>` and `gate -StopTree <pid>`.

- It accepts exactly the options `-Log`, `-TimeoutSeconds`, `-Slot`, `-StallSeconds`, `-Tail`, `-NoMarkers`, `-Local` and `-StopTree`, compared case-insensitively, and maps each to the flag in the table above (`-StopTree <pid>` to `gate stop-tree <pid>`).

  Any other option, or a new-CLI flag in the old form (`-StateDir`, `--state-dir`, `--slot`), is a usage error (exit 2). The one exception is the hidden `--test-print-request` of a `test-seams` build ([Black-box suite harness](#black-box-suite-harness)).
- `--` is required before the command; the command never starts at the first word that is not an option.
- `-StopTree <pid>` (or `-StopTree:<pid>`) must be the only option.
- A value option never takes one of the eight option names as its value: `-Log -NoMarkers` is `-Log` with no value, then `-NoMarkers`.
- The colon form `-Name:value` in one word is read as that option with that value (C2). A switch's colon form is read by its value, compared case-insensitively: `-NoMarkers:true`, `-Local:$true` set it, `-Local:false`, `-Local:$false` clear it, and any other value is a usage error. This fixes the PowerShell gate's reading of every `-Switch:<value>` as true (C2, CC-194).
- Values are validated as in the new CLI; the messages may name either spelling.
- The layer is removed once no caller uses it. Until then, every reader of command-line text (ops-tower's activity join, the hooks that inspect a gated command) matches both spellings.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | The command exited 0 and its log holds no failure marker. |
| `N` | The command's own non-zero status, passed through whole: the gate exits with the command's 32-bit exit code (300 stays 300; 0xFFFFFFFF reads as -1 where a caller reads it signed), and the FAILED line and the journal show it signed (a routed run whose connection was lost reports 255, RC7). |
| 1 | The command exited 0 but its log holds a failure marker (never with `--no-markers`, V2); or the gate itself failed after the command started (X4); or it failed before queueing (the working directory, the command's environment, the log's folder or an earlier log that cannot be removed); or a routed run passed but its changes or outputs could not be brought back (RC9, D1). |
| 2 | Usage: a missing, unknown or invalid input; a `.cmd` or `.bat` word the gate refuses (C5); a `--log` inside the state root; a critical gate under a heavy or light slot marker (K7). Nothing is queued. `gate box` also uses 2 for a refused payload and for a refused subfolder or batch word ([The box side](#the-box-side)). |
| 124 | The watchdog killed the command: STALLED, TIMED OUT or BACKSTOP (X2). |
| 127 | The command could not be started (C7); the slot is released at once. `gate box run` uses it for the same failure. |
| 75 | A builder refused a routed run (`gate box`, or the bridged remote client): busy (`gate box run` finding its work folder's lock held), unreachable, parity, rebooted, tools, version. The PC side reads the refusal and falls back to a local run, so a caller sees 75 only as a command's own status, or as a refusal that carried no reason (T5). |

The codes are kept (V4), with 75 added for refusals. 124 is the status coreutils `timeout` uses, so no caller mistakes a kill for the command's own status; a command that itself exits 124 is told apart in the journal's `verdict`.

## Output lines

Every line the gate says is written and flushed as it is said, never held until exit, so a running or killed gate's lines are already visible. Every line the gate writes for itself starts `gate: ` and goes to stderr and to the end of the log (V1), except the passed line and the stop-tree summary, which go to stdout (V3, X6). Three forms are the published interface that callers may parse; they keep their wording, and a change to them is a breaking change:

| Line | Where | Meaning |
|---|---|---|
| `gate: STALLED: ...`, `gate: TIMED OUT ...`, `gate: BACKSTOP: ...` | at line start in the log and on stderr | The watchdog killed the command; the exit is 124. Match `^gate: (STALLED\|TIMED OUT\|BACKSTOP)`. A gate wrapping a gate reports the inner kill through its own marker scan (V2). |
| `gate: waiting for the machine (critical)` | stderr | A critical gate is queued and waiting for the machine (K5); the stable part is the prefix, and the status after it is prose. A critical gate that takes the machine at once says nothing. |
| `Full output: <path>` | the end of the verdict line: `gate: passed in ... Full output: <path>` on stdout, `gate: FAILED (status <n>). Full output: <path>` on stderr | Where the whole log is (V3). |

Every other `gate: ` line is prose: it informs people and agents and may change in any release. That includes the passed and FAILED lines apart from their `Full output:` tail, the wait lines, the heartbeat lines, the diagnostics block and the stop-tree summary.

The lines of a run that starts its command:

- Passed, on stdout: `gate: passed in <w> s (load-adjusted <a> s, ceiling <n> s). Full output: <log>`, `<w>` and `<a>` in whole seconds rounded half to even. Once the sampler failed there is no load-adjusted time, and the line reads `gate: passed in <w> s (load-adjusted unknown: the sampler failed, ceiling <n> s). Full output: <log>`.
- The cores hint, on stdout just before the passed line, when its conditions hold ([Cores hint](#cores-hint)): `gate: used <avg> cores on average (peak <peak>, <free> free) on a light slot; work that keeps more than 2 busy belongs on heavy` or `... on a heavy slot; work that keeps under 1.5 busy fits on light`. Two notes go to the log only, and the hint is left out: `gate: cannot read the command's CPU at its end: <error>; its cores are unknown` and `gate: cannot tell whether a gate under this one took a slot of its own: <error>; its cores are unknown`.
- Failed, on stderr: `gate: FAILED (status <n>). Full output: <log>`. A command that exited 0 with a marker in its log first says `gate: command exited 0 but its log reports a failure -> <log>` on stderr, and its status is 1.
- On a non-zero status, up to 40 `<line number>:<line>` marker lines on stdout, before the tail.
- A start failure, on stderr and in the log: `gate: could not start <word> (<resolved full path>): <system message>`, exit 127. The parenthesis is left out when the resolved path is the word itself, and a program found nowhere gets the message of error 2 with no parenthesis. `gate: cannot read the system folder, which holds cmd.exe: <error>` and `gate: cannot make <path> a full path: <error>` are start failures too.
- A gate failure after the slot is taken: `gate: the gate failed while running the command: <message>`, exit 1. The job is terminated with 124, with its follow-up lines `gate: and could not terminate the command's job: <error>`, `gate: the command was still running 5 s after its job was terminated` or `gate: cannot wait for the command after terminating its job: <error>`.
- Failures before queueing, exit 1: `gate: cannot read the working directory: <error>`, `gate: cannot build the command's environment: <error>`, `gate: cannot create the log's folder <dir>: <error>` and `gate: cannot remove the earlier <path>: <error>`.
- A gate that cannot read its own start, exit 1, before it writes any ticket, claim, slot marker, pause marker, wake event, covered marker or journal line: `gate: cannot read this gate's own start: <error>; not running <command>`.

  The gate reads its start a second way (`process_start_of` of its own pid, the source the sweepers judge liveness by) when the first read fails, and `<error>` is the second read's error, or `<first error>; then the process has no start` when that read finds no process start. A gate therefore always holds its real start.
- A log in use, exit 1, before queueing and with no journal line: `gate: the log <path> is in use by another gate`, when another gate holds the log's mutex ([Kernel object names](#kernel-object-names)). A mutex a killed gate left is taken silently. `gate: cannot take the mutex <name> of the log <path>: <error>`, exit 1, when the mutex cannot be opened or tried.
- Routing, on stderr and in the log, with the command running here: `gate: ignored <path>: <reason>; running locally` when `remote.json` is there but unusable (not a file, unreadable or refused), and `gate: not routed: <reason>` when a heavy gate that would route stays local.

  The second line is said for `--local` (`gate: not routed: --local`), a word outside the worktree (`gate: not routed: <word> is outside the worktree`), a folder that holds nothing the snapshot sends (`gate: not routed: <sub> holds nothing the snapshot sends`), a word of the command that is not valid Unicode (`gate: not routed: a word of the command is not valid Unicode`), or a fact that could not be read (`gate: not routed: cannot run git: <error>`, or the working directory or git's output).

  A gate that is not heavy, has a covering parent, matches no pattern or has no `remote.json` says nothing.
- A routed run, on stderr and in the log, after any `gate: dropped --no-build: the last build may have run on the other machine` and `gate: outputs: none brought back for a --no-build command` lines. While it waits for a remote slot: `gate: waiting for a remote slot on <host> (<n> busy)` and `gate: still waiting for a remote slot on <host> (<s> s)`, as a local wait says them.

  A run the builder does not take says why and goes on here: `gate: remote <host> <reason>; running here`, with `<reason>` one of `full` (no remote slot within 60 s), `busy`, `unreachable`, `parity`, `rebooted`, `tools` or `version`, and `gate: remote <host> client could not start: <detail>; running here` when the remote client cannot start.

  A remote part the gate cannot start says the second line before it takes a remote slot when a part is missing, else after it ([The bridge](#routing-and-the-remoteps1-bridge)).

  A Ctrl+C while waiting for the remote slot says `gate: interrupted while waiting for a remote slot`. One during the client's call, or raised before it, ends the gate with `gate: interrupted before the command started` and no fallback, even when the client refused the run; the remote slot is released.

  A routed run that passed says `gate: passed in <w> s on <host> (wall clock, ceiling <n> s). Full output: <log>`, with no load-adjusted time. The client's status is the gate's own, passed through as `N`.
- A critical gate, on stderr and in the log: the `waiting for the machine (critical)`, `still waiting for the machine (critical)`, `holds the machine (critical)` and `interrupted while waiting for the machine` lines, in [Critical drain and nesting](#critical-drain-and-nesting).
- Nesting, on stderr and in the log, from the slot marker in the gate's environment: the `running under` (including the lend's `while a critical request drains`), `ignored GATE_SLOT_HELD` and `does not cover` lines, the barrier's `waiting for a heavy slot behind the critical request` and `waiting for a light slot beside the critical request` lines, and the refusal of a critical gate, all in [Critical drain and nesting](#critical-drain-and-nesting).
- The gate prints no `queued for` or `took ... slot` line, for a local or a routed run: the queue and the take are in the journal, and a wait longer than 30 s says `gate: still waiting for a <kind> slot (<s> s)`.
- Notes that never fail the gate: `gate: cannot remove <log>.err: <error>`, `gate: cannot remove the log <path> of the attempt: <error>` (a routed run that fell back could not delete the log the client wrote), `gate: cannot append <log>.err to the log: <error>` and `gate: cannot read this gate's priority class, so the command runs below normal: <error>`.

## Slots

### Pools and names

A slot is a named mutex in the logon session's `Local\` namespace, held by the gate for the command's whole run (kept, S1, S3). A gate killed while holding a slot is the crash recovery: the next gate takes the slot over, where a semaphore's count would be lost for good. It sees the takeover in either of two ways, the mutex coming back abandoned or the slot's claim naming a gate that is no longer alive ([Abandoned slots](#abandoned-slots)).

| Pool | Mutex names (default root) | Who takes it |
|---|---|---|
| heavy | `Local\gate-slot-<i>` | `--slot heavy`; a critical gate takes every heavy slot. |
| light | `Local\gate-light-slot-<i>` | `--slot light`; a critical gate takes every unreserved light slot (K2). |
| remote | `Local\gate-remote-slot-<i>` | a routed heavy run, for the length of its remote attempt (T5). |
| critical | `Local\gate-critical` | a critical gate, for its whole run, so two critical requests run one at a time in arrival order (K1). |

The pools never wait on each other, so a serial test run does not queue behind builds (S1). A gate takes and releases its mutexes on one OS thread, since Windows mutex ownership is per thread.

### Counts

The default count of a pool is `max(1, floor((logical processors - 1) / threads))`, with threads 4 for heavy and 2 for light (kept, S2). `slot-counts.json` in the state root overrides either count with a whole number from 1 to 64. It is re-read on every pass of the wait loop, so a count changed while gates wait applies to them, and a gate holding a slot the new count leaves out keeps it.

A missing file or key means the default; an unreadable or invalid value is ignored for that pool with one line, `gate: ignored <path>: <reason>; using <n> <kind> slots`, said once per wait and only for the waiting gate's own kind (S2). A processor count the system will not give is taken as 1, with `gate: cannot read the processor count: <error>; counting 1`, said once. The remote pool's size comes from `remote.json` (T1).

### Taking a slot

1. A gate that needs a slot creates its wake event, then writes its arrival [ticket](#ticket), both before its first try, so no wake is lost. A gate a parent's slot covers takes no slot and writes no ticket (N1). A gate its light parent lends the slot while a critical request drains (N2) writes no ticket when it is lent before it queues; one lent as it waits has queued with a ticket, which it deletes when lent, waking the waiters.
2. Waiters of one kind take slots in ticket `arrived` order, ties broken by pid. A gate tries a slot only when fewer live tickets of its kind are ahead of it than the pool has free slots (indices whose mutex no handle holds, a probe that fails counting as not free), not counting a ticket whose gate already holds a live claim (it holds a slot and only failed to delete its ticket), so a try never takes the slot an earlier waiter would get.

   It then tries indices 0 up to the count in order and takes the first free one. A live critical ticket that arrived before the gate (by `arrived`, then pid; the barrier, Q3) narrows that: a heavy gate tries no slot, and a light gate tries only the reserved light slots (K2), and none once as many light gates run beside the critical request as there are reserved slots (K3). See [Critical drain and nesting](#critical-drain-and-nesting).
3. A take deletes the gate's own ticket, journals `take`, writes the claim and wakes the waiters, so the next in line tries at once. A release deletes the claim, journals `release`, lets the mutex go and wakes the waiters. A critical gate takes the machine instead ([Critical drain and nesting](#critical-drain-and-nesting)): its `take` and `release` list every slot it holds, and it writes one claim, `gate-critical.json`, and none for the slots.
4. A waiter sleeps on its own wake event, `Local\gate-wake-<pid>-<start>` (auto-reset). A gate that takes or releases a slot, or deletes a dead gate's ticket, signals the wake event of every live ticket in its state root. An auto-reset event stays signalled until its waiter wakes, so no wakeup is lost between a waiter's check and its wait. A wake that cannot be opened or set is not said: the fallback poll covers it.
5. Each wait has a fallback poll as its timeout (2 s, kept from S4's interval). The poll re-reads the counts and the tickets, which deletes a dead waiter's ticket so it cannot hold the queue (Q1), and picks up a count changed by hand.
6. A waiter prints `gate: waiting for a <kind> slot (<n> busy)` once, on the first pass whose own [probe by open](#probe-by-open) finds none of the pool's slots free, where `<n>` is how many slots that same probe reports held. A gate that waits only behind earlier tickets while a slot is free for it does not print it. A gate held back by a critical request prints the barrier line instead, on its first pass ([Critical drain and nesting](#critical-drain-and-nesting)).

   It prints `gate: still waiting for a <kind> slot (<s> s)` every 30 s from its queueing, on stderr and in the log, so a gate around it does not read the wait as a stall (kept, S4). Its watchdog starts only once it holds a slot. `waitedSeconds` in its `take` counts from its `queue` line; the local `take` of a routed run that fell back counts from the `fallback` line, as the PowerShell gate does.
7. A problem a pass meets is said as a prose line, once per name or path per wait: `gate: could not take <name>: <error>`, `gate: cannot probe <name>: <error>`, `gate: cannot create the wake event <name>: <error>` (the gate then waits on the poll alone), `gate: cannot wait on the wake event <name>: <error>`, `gate: cannot write the ticket <path>: <error>`, `gate: cannot write the claim <path>: <error>`.

   More: `gate: cannot read the tickets in <dir>: <error>`, `gate: cannot read the claims in <dir>: <error>`, `gate: ignored the ticket <path>: <reason>`, `gate: cannot check the gate of <path>: <error>` and `gate: cannot delete the ticket|the claim|the dead gate's ticket <path>: <error>`.

   A gate under a parent's slot says `gate: cannot write the covered marker <path>: <error>` and `gate: cannot delete the covered marker <path>: <error>`, and the survey says `gate: cannot read the covered markers in <dir>: <error>` and `gate: cannot delete the dead gate's covered marker <path>: <error>`, each of the last two once; the run goes on after each.

Ordering by ticket replaces the PowerShell gate's free-for-all among waiters of one kind (Q2), and the wake events replace its 2 s polling as the way a waiter learns of a release.

### Abandoned slots

A slot counts as taken from a gate that died holding it in either of two cases: its mutex comes back abandoned (S3), or the take is clean but the slot's claim names a pid and start that are no longer alive.

The gate says `gate: took <name>, which a killed gate had left held` on stderr and in the log, and its journal `take` line carries `abandoned`, the list of slot names it took that way, so the journal counts how often gates die holding a slot (GATE-30). An abandoned journal mutex counts as owned and is said the same way, and the line appended under it carries `abandoned` with the journal mutex's name, whatever its event.

The second case exists because a killed gate leaves its claim and Windows frees a mutex with its last handle: the abandoned result is seen only when another handle to the name was open at the kill ([Probe by open](#probe-by-open)). The dead-claim check belongs to the slot loop, and runs on a clean take of a slot only.

A claim that is missing or unreadable is no takeover: the gate overwrites it with its own. A claim is dead by the [liveness rule](#claim).

### Probe by open

A slot is held while any handle to its name exists: a reader opens the name with `SYNCHRONIZE` access and closes it at once (kept, S5). ops-tower uses the probe to show a slot held with no claim, the critical gate uses it to count light gates beside it (K3), and a waiter uses it for the busy count of its first wait line.

A name the prober may not open (an elevated gate made it with an access list this process is refused) counts as held, since the refusal proves the name exists; any other failure is the prober's error.

A gate that is trying a slot holds a handle for an instant, so a probe can read a transient "held"; only waiters with a free slot for them try ([Taking a slot](#taking-a-slot)), which keeps such reads rare. The same rule bounds crash recovery: a mutex comes back abandoned only if another handle to its name was open when its owner was killed, so a take cannot rely on the abandoned result alone ([Abandoned slots](#abandoned-slots)).

## State root

One folder holds every file the gate writes: `%LOCALAPPDATA%\gate\v1\` by default, beside the install folder `%LOCALAPPDATA%\gate\bin\` and the PowerShell gate's own files in `%LOCALAPPDATA%\gate\`, so the two generations' files never mix.

`--state-dir <path>` overrides it, else the `GATE_STATE_DIR` environment variable; the flag wins over the variable. An empty `GATE_STATE_DIR` or `LOCALAPPDATA` counts as unset. A relative root is made absolute against the working directory, and the root need not exist: nothing is created under it by the CLI alone.

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
| `waiting\<pid>-<start>.json` | the waiting gate | other gates, ops-tower | [Ticket](#ticket). |
| `covered\<pid>-<start>.json` | a gate running under a parent gate's slot | other gates (the survey), ops-tower | [Covered marker](#covered-marker). |
| `paused\<parent pid>-<parent start>-<pid>-<start>.pause` | a gate waiting behind a critical request under a parent gate's slot | its parent gate's watchdog | Empty file; [Critical drain and nesting](#critical-drain-and-nesting) (kept, N3). |
| `events.jsonl`, `events.1.jsonl` | every gate, under the journal mutex | ops-tower, the durations summary, people | [Journal](#journal). |
| `archive\events-<yyyyMMddTHHmmssZ>.jsonl.gz` | the gate whose append rolled the journal | the durations summary | [Archive](#archive). |
| `durations.json`, `backfill\<pid>.json` and the backfill bar record | gates | gates | Reserved for learned durations (GATE-7; [design/gate-durations.md](design/gate-durations.md)). |
| `remote\<key>.outputs-pending` | the bridged remote client, and the Rust remote client (dormant) | the same | Empty file. Written by `remote.ps1` while the bridge stands (D1). The Rust client writes it before a routed run's run call when the run has output globs, removes it only after an accepted `gate-remote-outputs: 0`, and sends `OutputsFull` while it stands ([The remote client's run](#routing-and-the-remoteps1-bridge)). |
`<start>` everywhere in a name is the process start time in Unix epoch milliseconds. Claims, tickets and counts are written to a temporary sibling (`<file>.<pid>.<n>.tmp`, where `<n>` is a per-process counter so two threads of one process never share one) and moved over the target, so a reader never sees a torn file (kept, L1, Q1).

### Kernel object names

| Object | Default root | Non-default root |
|---|---|---|
| heavy slot | `Local\gate-slot-<i>` | `Local\gate-<h>-slot-<i>` |
| light slot | `Local\gate-light-slot-<i>` | `Local\gate-<h>-light-slot-<i>` |
| remote slot | `Local\gate-remote-slot-<i>` | `Local\gate-<h>-remote-slot-<i>` |
| critical | `Local\gate-critical` | `Local\gate-<h>-critical` |
| journal mutex | `Local\gate-journal` | `Local\gate-<h>-journal` |
| waiter's wake event | `Local\gate-wake-<pid>-<start>` | `Local\gate-<h>-wake-<pid>-<start>` |
| command's job | `Local\gate-job-<gate pid>` | `Local\gate-<h>-job-<gate pid>` |
| box command's job | `Local\gate-remote-<key>-<nonce>` | the same; the name is not root-dependent |
| box command's stop event | `Local\gate-remote-<key>-<nonce>-stopped` | the same; the name is not root-dependent |
| log's mutex | `Local\gate-log-<l>` | `Local\gate-<h>-log-<l>` |
| routed lock | `Local\gate-routed-<k>` | `Local\gate-<h>-routed-<k>` |

The critical mutex is taken by a critical gate once no earlier live critical ticket exists, and held for its whole run. An abandoned one counts as taken ([Abandoned slots](#abandoned-slots)).

The box command's job name comes from the payload's `Job` (`<key>` is the payload's `Key`, `<nonce>` is 12 hex digits); `gate box` only checks its form and that it ends in the payload's key, and never derives it.

`<l>` is the log hash: 16 lower-case hex digits, the first 8 bytes of the SHA-256 of the lower-cased, normalised absolute log path. Each run takes its log's mutex without waiting, before it readies the log, and holds it until after the slot release and the final append to the log, so two gates never write one log; a second gate fails at once ([Output lines](#output-lines)).

`<k>` is the key hash: 16 lower-case hex digits, the first 8 bytes of the SHA-256 of the lower-cased work-folder key. The Rust remote client takes the routed lock without waiting, before the remote slot, and holds it for the whole remote attempt ([Routing](#routing-and-the-remoteps1-bridge)).

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

  In `V1` a slot's row also reads the claims in the PowerShell root's `slots\` folder (`%LOCALAPPDATA%\gate\slots\`) while the slot's mutex is open and no live `v1\` claim holds it: a PowerShell gate holding the slot shows `busy` with that claim, its processes and last line. A claim whose pid is absent from the process snapshot but whose `processStart` is later than every process's creation time in the snapshot counts as live, only while the slot's mutex is open. A claim file whose read fails with access denied (delete-pending) or not found (deleted after the listing) is skipped, never a stale claim; the cutover drain reads claims the same way.

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
| `kind` | string | `heavy`, `light`, `critical` or `remote` |
| `index` | number | slot index; 0 for `gate-critical.json` |
| `pid` | number | the gate's pid |
| `processStart` | string | the gate's start time, ISO UTC |
| `started` | string | when it took the slot, ISO UTC |
| `cwd` | string | the gate's working directory |
| `command` | string | the command's raw words (the program unresolved), each quoted as for CreateProcess (C4) even for a batch target and joined by spaces, as the PowerShell gate's `Format-CommandText` writes them, such as `cmd /c echo "a b" "x\"y"` |
| `log` | string | the log's full path |
| `timeoutSeconds` | number | the ceiling; the writer refuses a value JSON cannot carry (not finite) |
| `remote`, `host`, `key` | bool, string, string | a remote slot's claim only (L1) |
| `expected` | object | reserved, optional ([Reserved fields](#reserved-fields)); `null` reads as absent, any other non-object is unreadable |

The file name `<name>` is `gate-slot-<i>`, `gate-light-slot-<i>`, `gate-remote-slot-<i>` or `gate-critical`, the same whatever the root: a non-default root's claims sit in that root's own `slots\` folder, so the file name carries no root hash.

A claim is live while its `pid` runs with that `processStart` (L2), the start compared in whole milliseconds. A killed gate leaves its file; the next holder overwrites it and readers treat it as stale. A gate that cannot write its claim says so once and runs on (L1).

The liveness rule is the same for claims and [tickets](#ticket). A pid with no running process, or one that has exited, is dead; exited is judged by a zero-time wait on the process, never by its exit code, since a gate whose command exited 259 (`STILL_ACTIVE`) passes that code through. Only a process that refuses the wait's `SYNCHRONIZE` right while granting the query right is judged by its exit code (none does on a standard machine, elevated gates included). A pid the checking gate may not query is dead too, since it is another user's process reusing a dead gate's pid. Any other failure to check counts as live, said once per path, since a gate deletes nothing it cannot judge. A gate never judges its own ticket or claim (same pid and start).

### Ticket

`waiting\<pid>-<start>.json`, written before a gate's first try, deleted once it holds its slot or ends (kept, Q1, with the [ordering rule](#taking-a-slot)).

| Field | Type | Meaning |
|---|---|---|
| `v` | number | 1 |
| `kind` | string | `heavy`, `light`, `critical` |
| `arrived` | number | first arrival, epoch ms; a routed run that falls back keeps its first arrival (K6) |
| `pid` | number | the gate's pid |
| `startTicks` | number | the gate's process start, epoch ms (the field keeps its name) |
| `holds` | bool | true only for a critical gate that holds the machine: it rewrites its ticket with `true` once every slot is held and keeps the ticket until it ends |
| `remote` | bool | present and true only while queued for the remote pool |
| `expected` | object | reserved, optional; `null` reads as absent |

Waiters of one kind queue by (`arrived`, `pid`). The ticket reader lists only `*.json` files in `waiting\` (a `*.tmp` file is skipped), and leaves out a ticket deleted between the listing and the read, since a gate that took its slot deletes it.

A ticket whose gate is dead by the [liveness rule](#claim) is ignored and deleted by the first gate that finds it, which then wakes the other waiters; the reader itself deletes nothing. An unreadable ticket is ignored but left in place, said once. ops-tower learns that a gate is waiting from these tickets, read through `gate-state`; it does not scan process command lines.

### Covered marker

`covered\<pid>-<start>.json`, written by a gate running under a parent gate's slot ([Critical drain and nesting](#critical-drain-and-nesting)) right after its `covered` journal line, so ops-tower sees a gate that takes no slot and writes no ticket or claim.

| Field | Type | Meaning |
|---|---|---|
| `v` | number | 1 |
| `pid` | number | the covered gate's pid |
| `processStart` | number | the covered gate's process start, epoch ms |
| `parentPid` | number | the pid of the parent gate whose slot covers it |
| `command` | string | the command text, as the claim's [`command`](#claim) |
| `cwd` | string | the gate's working directory |
| `since` | number | when the marker was written, epoch ms |

The gate deletes the marker when it releases, after a panic and on drop. It writes none when it cannot read its working folder, the condition that also leaves out the `covered` journal line. A gate whose own start cannot be read never gets this far: it refuses to run (see Output lines).

A marker whose gate is dead by the [liveness rule](#claim) is deleted by the next gate's survey, the pass that deletes dead tickets; the reader itself deletes nothing. The reader (`gate_state::v1::read_covered`) returns the good markers in `covered\`, in file-name order, whose name `<pid>-<start>.json` matches the pid and start in the body; a file that is unreadable, misnamed or gone between the listing and the read is skipped without a word, and a missing folder is empty.

### Counts file

`slot-counts.json`: `{"v": 1, "heavy": <n>, "light": <n>}`, both keys optional, each a whole number from 1 to 64 (kept, S2). People and ops-tower write it; `gate.exe` only reads it.

ops-tower's Gates tab writes through `gate-state` (`write_slot_counts`, `apply_slot_count`) on every edit: the PowerShell gate's `%LOCALAPPDATA%\gate\slot-counts.json` first, then `gate.exe`'s `%LOCALAPPDATA%\gate\v1\slot-counts.json` as `{"v":1,"heavy":<n>,"light":<n>}`, creating `v1\`, whether or not the cutover flag exists. A failure on the first file leaves the second as it was. Counts that are empty delete both files.

A missing file, or a missing key, gives the default silently. A file unreadable as a whole (including one without `v`, reason `missing key "v"`) gives both pools their default, each with its own ignore line. A value that is not a whole number from 1 to 64 gives that pool alone its default, with the reason `"<kind>" must be a whole number from 1 to 64`. Every line is `gate: ignored <path>: <reason>; using <n> <kind> slots`, built in one place for the `v1` and the PowerShell-era readers.

### Journal

`events.jsonl` holds one JSON object per line, appended under the journal mutex (`Local\gate-journal`, [Kernel object names](#kernel-object-names)) with a 2 s wait (kept, J1). The events are `queue`, `take`, `release`, `abandon`, `covered`, `parent` and `fallback`. `gate.exe` writes `covered` for a gate running under its parent's slot; `gate.exe` writes `parent` for a gate lent its light parent's slot while a critical request drains, once the command's environment is built.

Every line starts with `v`, `time`, `event`, `pid`, `processStart`, `kind`, `cwd` and `command` (the same text as the claim's [`command`](#claim)), in that order, then the event's own fields, as the PowerShell gate writes them (J1); a routed run adds `remote` (true), `host` (the ssh destination) and `key` (the remote work folder's key) after the event's own fields and before `abandoned`, on its `queue`, `take`, `release`, `abandon` and `fallback` lines, and on none after the `fallback`.

The journal keeps ISO 8601 UTC strings for `time` and `processStart` (7 fractional digits, ending in `Z`); it never held .NET ticks, so the move of claims, tickets and markers to epoch milliseconds does not touch it. `gate.exe` takes `processStart` from `GetProcessTimes`, in whole microseconds, so its 7th fractional digit is 0. Seconds fields (`heldSeconds`, `waitedSeconds`, `wallSeconds`, `adjustedSeconds`) are rounded to 3 decimals.

`v` is a line-level rule, unlike the other formats: the writer always writes `"v":1` first; a reader skips a line whose `v` is present and not 1, and reads a line with no `v` as the PowerShell gate's format until the cutover's drain (GATE-9) makes `v` required.

The event's own fields:

| Event | Fields after the common ones |
|---|---|
| `queue` | `log` (the log's full path), `notRouted` (written right after `log`, only when a heavy gate's caller kept it local with `--local`, with the value `--local`; other reasons for a local run write none) |
| `take` | `slots` (the slot names taken; a critical gate lists every heavy and unreserved light slot it holds), `waitedSeconds`, optionally `parentPid`, and for a critical gate `counts`, `otherHeld`, then `otherClaims` or `snapshotError` |
| `release` | `slots` (the same list), `heldSeconds`, `status` (null when the gate failed before its command had one), `wallSeconds`, `adjustedSeconds`, optionally `avgCores`, `peakCores`, `freeCores`, `verdict` |
| `abandon` | `waitedSeconds`: the gate ended while still queued (Ctrl+C while waiting for a slot) |
| `parent`, `covered` | `parentPid`; a lent gate writes `parent` after its `queue` when it was lent the slot as it waited, and alone, with no `queue`, when it was lent it before it queued |
| `fallback` | `reason`, then `remote`, `host`, `key`: it ends the routed attempt, so a remote `take` that falls back gets no `release`; a `full` fallback, and a `client` fallback with no client built, follow no remote `take` |

A release with no run facts writes `wallSeconds: null`, `adjustedSeconds: null` and `verdict: "unknown"`.

Fields beyond J1's:

- any event: `abandoned`, the last field, a list of names present only when there were any ([Abandoned slots](#abandoned-slots); GATE-30). On a `take` it is the slots taken from a gate that died holding them, followed by the journal mutex's name when that came back abandoned; on any other event it is just the journal mutex's name.
- `take`: a critical gate's `take` carries, after `waitedSeconds` and in this order, `counts` (`{"heavy": <n>, "light": <n>}`, the counts of the pass that took the machine), `otherHeld` (the same shape: the slots within the counts that other gates hold), then `otherClaims` or `snapshotError`; `abandoned` stays last (K5, GATE-38).

  `otherClaims` is a list of `{pid, kind, index, command, file}`, one per live claim of another gate in this root's `slots\` folder, sorted by file name: only the `gate-*.json` files, the remote pool's claims and this gate's own left out, `file` the claim's file name. `snapshotError` replaces it only when the claims folder cannot be listed, and holds the reason.
- `take`: `parentPid`, an integer after `waitedSeconds` (and a critical take's snapshot fields) and before the remote fields and `abandoned`, written by a gate under a parent whose slot did not cover it ([Cores hint](#cores-hint)). `gate-state`'s reader accepts it and does not treat the run as on the parent's slot.
- `release`: `avgCores`, `peakCores`, `freeCores`, optional numbers of one decimal after `adjustedSeconds` and before `verdict` ([Cores hint](#cores-hint)). They are omitted when there is no unpaused sample, the sampler failed, the read of the command's CPU at its end failed, a gate under the run took a slot of its own, the journal could not be read to tell, or the run was routed. Covered and lent runs write no `release`.
- `release`: `wallSeconds` (the command's wall time less the seconds paused behind a critical request; a routed run's whole remote wall time), `adjustedSeconds` (load-adjusted seconds, 3 decimals, a number for every run that started; null for a routed run and for a run whose sampler failed) and `verdict`, one of `passed`, `failed`, `log-failure`, `stalled`, `timed-out`, `backstop`, `unknown` (kept, J5).

  A routed run is a kill only when the gate killed it; a command that exits 124 on its own is `failed`.
- `fallback`: `reason`, one of `full` (no remote slot within the 60 s wait), `busy`, `unreachable`, `parity`, `rebooted`, `tools` (T5), `version` ([The box side](#the-box-side)) and `client` (the remote client could not start, or none is built). The `take` after it is the run's local one, untagged, its `waitedSeconds` counted from the `fallback`.
- a routed run's `take`, `release` and `abandon` carry the tag; its `release` has `adjustedSeconds` null and a `wallSeconds` of the whole remote call.

The verdict comes from the run's end: a kill line matching `^gate: (STALLED|TIMED OUT|BACKSTOP)\b` gives `stalled`, `timed-out` or `backstop`; otherwise a command status of 0 with a non-zero gate status is `log-failure`, a status of 0 is `passed`, anything else `failed`; `unknown` when there are no run facts.

The reader starts a run from a lone `parent` line that names a parent: state `parent` and `parentPid`, no queued or started time, and the status and held time of the parent's run. A `parent` line naming no parent is skipped. A lent gate whose command's environment cannot be built writes no `parent` line: one that had queued writes a `release` with `"slots":[]` and `"status":1`, and one lent before it queued writes nothing.

A lone `covered` line is its own state, `covered` (`RunState::Covered`), distinct from a run that lent its slot: the run has `parentPid`, no queued or started time, the kind and first time of the `covered` line, and the status and verdict of the parent's released run. A covered run is never counted in the summary.

The history that ops-tower serves (`HistoryRun`, `GET /api/gate/history`) carries `verdict`, `wallSeconds` and `adjustedSeconds`, null when a line has none, and `abandoned`: every name any line of the run lists (`parent` and `covered` lines included), in line order without repeats, always present and empty when there are none.

The history lists the 50 newest runs that are not covered, and at most the 50 newest covered runs newer than the oldest of them: at most 100 runs, newest first by the time of each run's first journal line (its queue line, or the `parent` or `covered` line of a gate that never queued), then by pid and process start. A run fallen back onto or lent its parent's slot counts toward the 50; a covered run does not, so a parent with many nested covered gates never crowds other runs out. The 7-day prune keeps every run the history lists, and the parent run of each listed covered or fallen-back run, whose row takes its status, held time and verdict from it.

It also carries `pid` (a number), `processStart` (the ISO string from the journal) and `running` (a bool, true while the run's gate is live by the same pid-and-start check as the tickets', for every run): a covered run's `state` stays `covered` while it runs.

It also carries, after `slot`, `slots` (the slot names the run took, such as `["Local\\gate-slot-0"]`, `[]` when none) and `machine` (null, or for a critical gate's `take` `{counts, otherHeld, otherClaims}` or `{counts, otherHeld, snapshotError}`). A `fallback` line clears `slot`, `slots` and `machine`. ops-tower reads the journal of the generation the cutover flag selects ([Common rules](#common-rules)).

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

### Reserved fields

Readers accept this optional field now; its issue writes it:

- `expected`: `{"p50": <s>, "p90": <s>, "n": <runs>, "key": "<key>"}` in claims and tickets, present when the gate has history for the command (GATE-15).

### remote.json

`remote.json` keeps the PowerShell gate's format (T1), read by `gate-router` as [Routing](#routing-and-the-remoteps1-bridge) describes: `host`, `slots`, `repos` and the optional per-repo `outputs`, read afresh by each routed gate and ignored as a whole with one line when unusable. A host of one letter is refused (`host must not be a single letter, which a git host:path URL reads as a drive`).

The optional `hosts` is an array of strings naming further builders, which only `gate push` reaches; a routed run goes to `host` alone. Each entry passes the checks `host` does, with the same reasons worded `hosts entries ...`. The push order is `host`, then `hosts` in file order, an entry equal to an earlier one in any letter case skipped. A `hosts` that is not an array of strings, or holds a bad entry, never affects routing: `gate push` alone refuses the file with `push: ignored <path>: <reason>`.

ops-tower reads only `slots`.

 It carries no `v` while the bridge stands; its format moves with the remote port (GATE-10).

## Critical drain and nesting

All kept: what a critical gate asks for and the order of its drain (K1), the reserved light slots (K2), light gates beside a critical gate (K3), priorities (K4), the take line and its snapshot (K5), a routed run falling back in its first place (K6), the refusal of a critical gate under a heavy or light marker (K7), nesting under a slot marker (N1), lending while a critical request drains (N2) and pause markers (N3).

Changes from the PowerShell forms:

- The slot marker is `GATE_SLOT_HELD=<kind>:<pid>:<start>` with `<start>` in epoch ms. Under a non-default root it gains the root hash, `<kind>:<pid>:<start>:<h>`, and a gate honours a marker only when its hash matches its own root (no hash for the default root). A test gate running under a real gate's marker therefore takes its own slots.
- The pause folder lives in the state root, so a test root isolates it.
- Light gates beside a critical gate and the critical gate's own waits use the wake events and the queue order of [Taking a slot](#taking-a-slot).

What `gate.exe` does of it today is part A, nesting under the slot marker (N1, K7), part B1, the critical request's own drain and take (K1, K2, K5), the barrier (Q3), the light gates beside a critical request (K3), the lend (N2) and the pause markers' writer and reader (N3), all below. The watchdog reads the markers on every sample.

- A gate reads `GATE_SLOT_HELD` once, before routing and before the log mutex (`nesting.rs`). A live parent whose slot covers the gate's kind (heavy covers heavy and light, light covers light, critical covers all three) gives `gate: running under the <held kind> slot of gate <pid>`: the gate takes no slot, writes no ticket or claim, and journals one `covered` line (identity fields, then `parentPid`; no `queue`, `take` or `release`).

  It then keeps a [covered marker](#covered-marker) until it releases, so ops-tower sees it run.

  Its command gets the parent's marker verbatim, and a heavy gate covered by any parent is never routed.
- Under a critical parent the command runs at High priority and the watchdog measures it as a critical gate's.
- A parent is live by the three-way liveness dead-gate cleanup uses (`slots::Liveness`): gone means no such process, a start mismatch or access denied; any other error reading its start counts as running, with no extra line.
- A malformed marker says `gate: ignored GATE_SLOT_HELD='<text>', which is not <kind>:<pid>:<start>; taking a <kind> slot`, with every character outside `[A-Za-z0-9:]` in `<text>` shown as `?` and at most 64 characters, then `...`.

  A marker whose parent is gone says `gate: ignored GATE_SLOT_HELD='<marker>': gate <pid> is no longer running; taking a <kind> slot`. A marker hashed for another root, or unhashed under a non-default root, is ignored silently. Each of these gates then takes its own slot.
- A critical gate under a heavy or light marker is refused (K7) on stderr only, exit 2, before the log mutex, with nothing queued or journalled: `gate: --slot critical cannot run under the <held kind> slot of gate <pid>: it would wait for that slot for good; run the critical command outside that gate` (`-Slot critical` through the PowerShell spelling).
- A heavy gate under a light marker takes its own slot and says `gate: the <held kind> slot of gate <pid> does not cover a <kind> gate; taking a <kind> slot` just before its local take (after a Local route or a fallback, before its wait lines); a gate routed to the builder never says it.
- **One request at a time.** A critical gate queues a ticket of kind `critical` and runs after every earlier live critical ticket, in arrival order, each through the critical mutex ([Kernel object names](#kernel-object-names)). An abandoned critical mutex counts as taken, with the `took <name>, which a killed gate had left held` line.
- **The drain.** With the mutex held, the gate waits for every earlier live heavy ticket, and for every earlier light ticket when the light count less the reserved light slots is above 0. Remote tickets are ignored, and so is a ticket whose gate already holds a live claim and only failed to delete its ticket.
- **The take.** It then takes every heavy slot and every unreserved light slot, re-reading the counts on each pass. The reserved light slots are the `min(2, light count)` of the highest indices, and a critical gate never takes them.

  A slot a killed gate left is a takeover, journalled in `abandoned`, and the gate deletes that gate's claim, since it writes none for the slot.
- **The hold.** Once every slot is held the gate rewrites its ticket with `holds: true`, keeps it until it ends, and writes the one claim `slots\gate-critical.json`. Its `take` and `release` list the slots held ([Journal](#journal)).
- **The barrier.** A gate that finds a live critical ticket that arrived before its own (by `arrived`, then pid; its own ticket never counts) is held back by the earliest such request, whether or not that request holds the machine yet. Gates that arrived before the request run first and are waited for. A heavy gate tries no slot. A light gate tries only the reserved light slots, and none once as many light gates run beside the request as there are reserved slots.

  Beside a request that still waits for the machine, the light gates are the reserved slots some gate has open. Beside one that holds it, they are those and every other gate's live light claim on a slot below the reserved ones or past the count. A count raised during the critical run lets no more light gates in. A remote wait has no barrier.
- **The lend (N2).**

  A heavy gate under a light parent (the one nesting case where the parent's slot does not cover the gate) is lent the parent's slot, takes no slot of its own and says the lend line, while a critical request is pending (a live critical ticket of another gate, or else the critical mutex open), no critical request holds the machine (no live critical ticket with `holds`), and the parent's light claim is not on a reserved light slot or past the light count (an unreadable or missing claim lends).

  The rule is checked before the gate queues, when the gate is lent without routing, queue line or ticket, and on every pass of its wait once it has queued, when it deletes its ticket and wakes the waiters. Its command gets the parent's marker. It journals `parent` with `parentPid` and no `take` or `release`; a gate lent as it waited has its `queue` before the `parent`. Once the critical gate holds the machine nothing is lent, and the child waits behind it like any gate.
- **The pause marker (N3).** A gate waiting for a slot under a parent gate's marker that does not cover it keeps the empty file `paused\<parent pid>-<parent start>-<pid>-<start>.pause` in the state root. It writes the file on every pass of the wait while a live critical ticket that arrived before its own exists (the barrier), setting its last-write time explicitly each time, so the file's age shows the gate is alive.

  It deletes the file when that stops and however the wait ends: take, lend, timeout, Ctrl+C or a panic. A write or delete that fails is said once per wait, each, and the wait goes on.
- **Reading the pause markers.** `PauseReader` (`pause.rs`) answers on each watchdog sample whether a gate under this one waits behind a critical request. No `paused` folder means no pause.

  A marker counts when its name parses, names a parent the gate reads for (below), its child gate still runs with the start in the name (a start that cannot be read counts as not running), it was written at most 30 s ago (exactly 30 s is live) and the child's ticket exists. A marker gone while it is read is skipped. On the first marker that counts, the answer is whether a critical request is pending (a live critical ticket, else the critical mutex open).

  A folder, the waiting tickets or the critical mutex that cannot be read is an error the caller says once, and the parent pauses nothing: the watchdog says it once per watch, counts that sample as not paused, and reads again on the next.

  A gate reads for itself (its own pid and start) and, when its parent's slot covers it or the parent lent it that slot, also for that parent, since the gates under its command name their markers after the parent whose marker the command gets. A marker named after that parent counts only when its waiting gate runs in this gate's job: a sibling the parent started beside this gate names it too, and must not keep a hung gate from being stalled.
- **Lines**, on stderr and in the log:
  - `gate: waiting for the machine (critical): <h> heavy and <l> light slots held (<r> light slot|light slots reserved for light gates beside it), <n> earlier waiters`, the first time a pass falls short; a critical gate that takes the machine on its first pass says nothing.
  - `gate: still waiting for the machine (critical): <status> (<s> s)`, with the same status, on each heartbeat.
  - `gate: holds the machine (critical) at <yyyy-MM-ddTHH:mm:ss.fffffffZ> after <s.d> s: counts heavy <H>, light <L>; other gates hold <h> heavy and <l> light slots (<r> ... reserved for light gates beside it); other live claims: <pid> <kind> <index> <command>, ...`, the claims ending as `none` or `unknown (<reason>)` when they could not be listed.
  - `gate: waiting for a heavy slot behind the critical request of gate <pid>`, from a heavy gate the barrier holds back, on its first pass whether or not a slot is free, with `<pid>` the earliest request's gate; then `gate: still waiting for a heavy slot (<s> s)` on each heartbeat.
  - `gate: waiting for a light slot beside the critical request of gate <pid>`, from a light gate the barrier holds back, likewise, then `gate: still waiting for a light slot beside the critical request of gate <pid> (<s> s)`.
  - `gate: running under the <held kind> slot of gate <pid> while a critical request drains; taking no slot`, from a lent gate, said once its command's environment is built.
  - `gate: interrupted while waiting for the machine`, on Ctrl+C: the journal gets `abandon`, everything taken is released and the exit is 0xC000013A.
  - `gate: cannot read the claims in <dir>: <error>`, said once, for an entry of the claims folder that cannot be read; the entry is skipped.
  - `gate: could not write the pause marker <path> (<reason>)` and `gate: could not delete the pause marker <path> (<reason>)`, each said once a wait.
  - `gate: could not read the pause markers in <folder> (<reason>); a gate under this one pauses nothing`, and the same with `the waiting tickets in <folder>` or `the critical mutex <name>`, from the watchdog, once per watch; likewise `gate: could not read the processes in this gate's job (<reason>); a gate under this one pauses nothing`.

## Watchdog

All kept, with each knob's default unchanged:

- The sampler (W1): every 5 s while the command runs, it reads, in order, the pause state, the log and `.err` sizes, the job's pids, one process snapshot and the build servers, then the WSL VM, the system times and the job's accounting; nothing goes through WMI.

  Each sample carries two stamps: `began`, taken before its reads, and `at`, taken after them, next to the system times and the job's accounting. The `interval`, the load-adjusted clock, the paused time, the free share and the exit use `at`; the PowerShell gate stamps once, before its reads.
- Stall (W2): no output, no CPU (at least 1% of one core over the window), no new process in the job, no build-server progress, no WSL VM CPU and no nested gate paused behind a critical request, for `--stall` seconds: `gate: STALLED: ...`.

  The quiet time runs from the last progress sample's `at` to the latest sample's `began`, not to the moment the watcher checks: a watcher starved of CPU wakes late, and the time it spends in its reads, and after them up to the check, is not the command's quiet time, so the next sample decides.

  A quiet command is therefore killed up to one sample period after its window, plus the read time of the last progress sample and of the latest sample, never before; the PowerShell gate measures to the check and can kill earlier. The ceiling and the backstop read the check's own time.

  Progress as the watchdog crate counts it resets the stall clock when the sum of the log and `.err` sizes changed (grew or shrank), the job's CPU since the last progress reached 1% of one core over the stall window, a pid appeared that the last sample did not have, or a nested gate is paused, or a build server's CPU counts as progress (see Build servers below), or, for a `wsl` command, the WSL VM's CPU counts as progress (see WSL below).
- The load-adjusted clock and the ceiling (W3): each sample adds `interval * free`, the interval running between the samples' `at` stamps, to the clock `--timeout` runs on; a sample taken while a nested gate is paused adds to the paused time instead; `gate: TIMED OUT after ...`.

  The free share is 1 for a High-priority command; otherwise `busy` is the share of the interval's system time that was not idle, the cores others held are `max(0, busy * cores - own cores)` with `own cores` the job's CPU gained over the interval, and `free = clamp((cores - others) / demand, 0.05, 1)`. `demand` is every core, or `min(cores, 2)` for a light gate.
- The backstop (W4): wall time less paused time reaching 5 times the ceiling, whatever the load: `gate: BACKSTOP: ...`. One sample's precedence is STALLED, TIMED OUT, BACKSTOP.
- Sampler failure (W5): `gate: sampler failed: <message>; watching by wall time only`, then the backstop alone watches. The pause markers are still read, and a paused stretch, that of the failing sample included and running to the failure's own fresh stamp, is left out of the backstop's wall time.
- WSL (W6): for a command whose first word is `wsl` or `wsl.exe` (a path allowed, case-insensitive), the VM's CPU since the last progress reaching a tenth of one core over the stall window counts as progress. The VM's CPU is not added to the command's own CPU, since WSL's relay processes escape the job and the VM is the only signal.

  The reader is `gate_win32::wsl_vm_cpu` (`times.rs`): the sum of kernel and user time over every process named `vmmemWSL`, read in-process through `NtQuerySystemInformation(SystemProcessInformation)`, never through a child process (an unelevated `OpenProcess` on the VM process is denied, and so is HCS). Plain `vmmem` is not read, since it can be any Hyper-V VM.

  The undocumented entry layout is checked: a matched entry whose create time is zero or more than a day past now, or whose times are negative, is an error.

  No `vmmemWSL` (a cold start) is silent: there is nothing to count, and the next reading is a fresh baseline, as it is when the VM restarts between samples (its CPU fell).

  A read that fails is said once per run, by the first sample whose read fails, on stderr and in the log: `gate: WSL VM CPU unavailable (<message>); retrying each sample, and until a read succeeds this wsl command counts only output and job CPU as progress`. A failed read at the watch's start alone says nothing, and every sample retries.
- A command that exits on its own before the kill keeps its status (W7).
- Build servers: classification (B1), their priority (B2), and `MSBUILDDISABLENODEREUSE=1` and `DOTNET_CLI_USE_MSBUILD_SERVER=0` so worker nodes run in the command's job (B3).

  Every sample reads the build servers outside the command's job: `VBCSCompiler.exe`, `dotnet.exe` whose command line names `VBCSCompiler.dll`, and `MSBuild.dll` with `nodemode`. It takes one ToolHelp snapshot, which carries no command lines, and each process's times; a command line is read once per (pid, start) through `proc_table::command_line_of(pid)` (OpenProcess and ReadProcessMemory), never through a child process. A process the gate cannot read is skipped.

  A server started at or after the command is new. One started before it counts only when it is a compiler server, and then it is shared. The job holds a build client when it has an `MSBuild.exe` or a `dotnet.exe` without `--no-build`.

  With a build client in the job, a rising new or shared server is progress, and its CPU counts as the gate's own in the free share. A real gate therefore counts another session's compiler server as shared progress while its job holds a build client, as the PowerShell gate does.

  The watch hands the pure crate raw facts: the per-pid cumulative CPU of the new and the shared servers, the build-client flag and the VM's CPU as an option (`None` for a command that is not a `wsl` one, or with no VM). The classifiers are in `gate-watchdog`'s `servers.rs`; the reader is the binary's `servers.rs`.

  Priority (B2): after every sample, the watch moves the priority class of the build servers that sample classified, with the same classification as above. An ordinary gate lowers each classified server outside its job from Normal, AboveNormal, High or RealTime to BelowNormal; there is no restore after the run. Every classified server outside the job is lowered, not only those that count as progress, so a warm `MSBuild` node already running is lowered too.

  A High gate (a critical gate, or one under a critical parent) instead raises the servers outside other live gates' jobs, and the servers of its own job, to High. An ordinary gate leaves its own job's servers as they started. A High gate's guard does not count the critical claim (`gate-critical.json`) or the remote-slot claims as other gates, so a gate covered by a critical parent raises its own job's build servers to High.

  A server whose priority class cannot be read (the gate opens it with `PROCESS_QUERY_INFORMATION`, as .NET does) is an unknown class: an ordinary gate leaves it alone silently, and a High gate still tries the raise.

  An ordinary gate also leaves alone a server in the job of a live critical gate whose ticket holds, and any server already at High while a critical ticket holds, since that gate's watch raised it. The rule is `gate-watchdog`'s `server_target` and `server_left` over `PriorityClass`; the calls are `gate_win32::priority_of` and `set_priority`; the per-watch pass is the binary's `priority.rs`.

  The guard (the tickets, the claims and the other gates' jobs, opened by name) is read at most once a pass, and only when a server the gate would move asks; a pass with no server to move reads no state files. Tickets are read without deleting dead ones. A priority failure never fails a sample, and its two lines go to stderr and the log:
  - `gate: could not lower|raise <name> <pid>: <message>`, once per (pid, start), which is never tried again; it comes only from a failed set, with the plain Windows message; a server that has exited since the sample is passed over silently.
  - `gate: could not list the job of gate <id> (<message>); its build servers may be lowered|raised`, once per watch and gate, and that job is left out of the guard.
- Kill, on a STALLED, TIMED OUT or BACKSTOP alike: the diagnostics block (X3), then `TerminateJobObject(job, 124)`, `gate: terminated the job's <n> process(es)`, `gate: diagnostics written to <log>`, a 5 s wait for the command and exit 124 (X2). A failure inside the gate after the command started terminates the job first (X4).
- The diagnostics block (X3) is gathered before the job is terminated and goes to the log only, between the command's output and the kill line. A command that exits during the gathering keeps its own status and writes no block. Its lines:
  - `gate: diagnostics`, then `gate:   job <pid> parent <ppid> <name>: cpu <s> s, working set <mb> MB: <command line>` for each process of the job (up to 40, then `gate:   ... and <n> more job processes`), and `gate:   outside job ...` lines for the descendants outside the job (up to 20).
  - The build servers the last sample counted, one line each: `gate:   build server <pid> <name>: cpu <s> s (started after the command)` or `(already running; counts while the job holds a build client)`; a server gone by the time of the block shows `(exited)` in place of its name and `?` for its CPU. `gate:   build servers: none` when none was counted.
  - Up to six sample lines, `gate:   sample at <w> s (load-adjusted <a> s, free <p>%): log <n> B, err <n> B, job cpu <s> s, <n> processes, new servers cpu <s> s, shared servers cpu <s> s, build client in job: yes|no`, or `gate:   samples: none` when no sample was taken. `<w>` is the sample's `at` stamp.

    A `wsl` command's sample line also carries `wsl vm cpu <p>%` after the process count: the VM's CPU in percent of one core since the previous reading (`Watch::last_vm_percent`), `wsl vm cpu unavailable` when that sample's read failed, and 0% with no VM. Any other command shows nothing there.
  - `gate:   ... diagnostics cut at <n> ms` when the 2000 ms gather budget ran out. A gathering that failed writes `gate: diagnostics` then `gate: diagnostics failed: <message>` in its place.
  - The working set comes from `gate_win32::working_set` (`None` for a gone or exited process). stderr gets one line, `gate: diagnostics written to <log>` (also in the log), after the terminated line; the block's lines are not on stderr.
- A command still running 5 s after its job was terminated is reported: `gate: the command was still running 5 s after its job was terminated`. A job that cannot be terminated says `gate: could not terminate the command's job: <error>`. A killed run is not scanned for markers, and its release verdict is `stalled`, `timed-out` or `backstop` by the kill line that ended it.
- What a sample reads: the job's CPU by accounting (exited members included), the job's pids created at or after the command's start (a pid that answers access denied is not a member), the log's and `.err`'s sizes, and the system times. A critical run counts as High priority for the free share, and a light gate's demand is `min(cores, light threads)`.
- Ctrl+C and Ctrl+Break (X5): the gate handles them itself and prints `gate: interrupted; stopping the command` during a run. The command got the same Ctrl+C through the shared console, so the gate waits up to 5 s for it to exit on its own and exits with its status, then terminates whatever is left in the job.
- A command still running after those 5 s gets `gate: the command was still running 5 s after the interrupt; terminating its job`, the job is terminated with 0xC000013A and the gate exits 0xC000013A. Either way the verdict skips the marker scan and the release is journalled, with verdict `failed` when the status is 0xC000013A.
- Ctrl+C between taking the slot and launching the command: `gate: interrupted before the command started`, no command runs, the exit is 0xC000013A and the release verdict is `unknown`.
- Ctrl+C while queued: `gate: interrupted while waiting for a <kind> slot`, the ticket is deleted, the waiters are woken, an `abandon` line is journalled and the exit is 0xC000013A. No `still waiting` line follows it. A critical gate says `gate: interrupted while waiting for the machine` and also lets go of everything it took so far.
- A console close ends the gate at once, because Windows ends the process when the handler returns; the dead-gate cleanup reclaims its ticket and claim ([Abandoned slots](#abandoned-slots)). If the handler cannot be installed the gate says `gate: cannot install the Ctrl+C handler, so Ctrl+C ends the gate at once: <error>`.

The arithmetic (stall progress, the free share, the clock, the three kills and their lines, the box heartbeat) is a pure crate driven by samples and an injected clock; the Win32 layer takes the samples. The cores hint's average, peak, free cores and verdict are in it too. The kill lines are:

- `gate: STALLED: no output and no CPU for <quiet> s (machine free <p>% on average); killed <command> and its children`
- `gate: TIMED OUT after <timeout> s of load-adjusted time (wall <wall> s, machine free <p>% on average); killed <command> and its children`
- `gate: BACKSTOP: <wall> s of wall time (5 x <timeout> s ceiling, load-adjusted <adjusted> s, machine free <p>% on average); killed <command> and its children`, where the middle clause reads `machine load unknown: the sampler failed` after a sampler failure.

`<p>` is the free share averaged over the wall time the ceiling counted (paused seconds left out), a whole number rounded half to even, 100 while nothing is counted; the seconds are rounded the same way. After a sampler failure only the backstop is read.

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
  - a dynamic name cmd.exe expands though no variable is set: `%CD%`, `%DATE%`, `%TIME%`, `%RANDOM%`, `%ERRORLEVEL%`, `%CMDCMDLINE%`, `%CMDEXTVERSION%`, `%HIGHESTNUMANODENUMBER%`, `%__CD__%`, `%__APPDIR__%`;
  - the substring and substitution forms `%NAME:~a,b%` and `%NAME:a=b%` of a set or dynamic name, and a whole `%NAME:…%` name that is itself set.

  A lone `%` and an unset name pass. The gate and the box side share this one implementation.
- **Program resolution.** The program must resolve to an absolute path (kept, C6):

  - A name with a directory part is read against the working folder, `.` and `..` folded and separators written `\`; a drive-qualified or UNC name is kept as written.
  - A name with an extension but no directory is looked for as written in each absolute PATH folder.
  - A bare name is looked for in PATH × PATHEXT, the folders outer and the extensions inner, as `Get-Command` orders them.
  - The current folder and relative PATH entries are never searched, and an extensionless shim is never found.

  The program's path is then made a full path (`.` and `..` folded) before the batch check, and that path is the application.

  A name found nowhere is a start failure. Unlike the PowerShell gate, which lets CreateProcess search for the program (the current folder among others), the gate never passes an unfound name on. This is deliberate, for security: a repo cannot plant a `git.exe` at its root. The `gate-win32` layer passes the path as `lpApplicationName`, so CreateProcessW searches no folder (not the gate's, the current one, System32 or PATH).

  A start failure prints `gate: could not start <word> (<resolved>): <message>` and exits 127, releasing the slot (C7); the lines are in [Output lines](#output-lines).
- **Process creation.** The `gate-win32` layer is the contract the run uses (`Launch` → `Suspended` → `Running`). The command is created suspended with an explicit environment block, assigned to the gate's job `Local\gate-job-<gate pid>`, then resumed (kept, X1, P1). Creation flags are `CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT` plus the priority class, with no new console and no new process group.

  The job has no limits: no breakaway, no kill-on-close. (This is `gate run`; the box's command is created with `CREATE_BREAKAWAY_FROM_JOB`, [The box side](#the-box-side).) A job name already held is refused. If assigning to the job or resuming fails, the command is terminated with exit code 1 first, and a suspended command dropped without being resumed is terminated with exit code 1.

  The desktop-app policy `PROCESS_CREATION_DESKTOP_APP_BREAKAWAY_DISABLE_PROCESS_TREE` is set, as the PowerShell gate sets it. Per Microsoft's UpdateProcThreadAttribute remarks it matters only to Win32 apps packaged through the Desktop Bridge, whose children it keeps inside the desktop app runtime environment.

  The layer refuses a `.cmd` or `.bat` application (any case, trailing dots and spaces ignored): the caller wraps batch targets in `"<system folder>\cmd.exe" /d /v:off /s /c "<line>"` itself. It refuses a NUL in the application, command line, working folder, log path or `.err` path before anything is opened.

  The log and `<log>.err` are each opened fresh. Any file at the path is deleted first (a link itself, never its target), then created with `CREATE_NEW` and `FILE_FLAG_OPEN_REPARSE_POINT | SECURITY_SQOS_PRESENT | SECURITY_ANONYMOUS`; it must be a disk file, not a reparse point, with exactly one name.

  Both are shared for read, write and delete, so a process the command left running does not block the next run on that log; a second gate on a live log is kept out by the log's mutex ([Kernel object names](#kernel-object-names)). An orphan's old, delete-pending file makes the create fail loudly rather than be shared. stdin is `NUL`, opened `OPEN_EXISTING`.

  It inherits exactly three handles, through `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`: stdin from `NUL`, stdout to the log and stderr to `<log>.err`. The gate's own handles to them are not inheritable: the command inherits copies made under the spawn lock, which exist only while the lock is held, and a failed copy is a setup error, `copy a handle for the command to inherit: <message>`.

  The spawn lock is process-wide and is held while the gate starts any process, so only the launched command inherits the copies. A process the gate starts itself goes through `gate_win32::spawn`, which holds the lock for the start alone; one started any other way could inherit the copies if it started while they are open.

  The command shares the gate's console, so Ctrl+C reaches it. A nested gate's job nests inside its parent's.

  The start-failure message is the system message for the CreateProcessW error code, as `Win32Exception.Message` gives it, with no `(os error N)` suffix. That failure alone maps to status 127; every other start failure is the gate's own.
- **Priority.** BelowNormal, or the gate's own class when that is already BelowNormal or Idle; High for a critical gate and any gate under a critical marker (kept, P2). The classes of the build servers outside the job are moved while the command runs ([Watchdog](#watchdog), B2).
- **Environment.** Every command gets the gate's environment with these set over it (names compared case-insensitively): `GATE_SLOT_HELD=<kind>:<pid>:<start_ms>[:<h>]` (the held slot's marker, critical included, or the parent's for a covered gate), `MSBUILDDISABLENODEREUSE=1`, `DOTNET_CLI_USE_MSBUILD_SERVER=0` (kept, P3, B3), and `GATE_STATE_DIR=<root>` under a non-default root. Under the default root an inherited `GATE_STATE_DIR` is removed. The gate's own environment is never changed.

  The batch refusal's `%NAME%` check answers for this exact block.

  The block is built with names matched and sorted as Windows does (`CompareStringOrdinal`, case-insensitive over all of Unicode), and a later pair wins.
- **Working directory.** The caller's (P5).

## Log and tail

- The log is kept as the command's exact bytes. The log's directory is created, an earlier log and `<log>.err` are removed at start, stdout goes to the log and stderr to `<log>.err`, and at the end the `.err` bytes are appended to the log byte for byte (once) and `.err` is deleted. The gate's own lines follow, starting on a line of their own: every line it said on stderr, and the passed line. So one file holds the output, then the errors, then the gate's lines (kept, V1).

  The gate's lines are the run's notes, gathered while it goes and appended to the end of the log. The verdict lines are appended after the tail is printed, so the log ends with them and the printed tail does not hold them. A hard-killed gate's lines are only on stderr.
- Lines end at CR LF, LF or a lone CR, as .NET's `ReadLine` ends them, for the marker scan and the tail. Both decode the log as UTF-8 lossily: an invalid byte reads as U+FFFD. The gate writes the tail to stdout as UTF-8 (through the wide console API when stdout is a console).
- The failure-marker regex is built in (kept, V2): `^Build FAILED\.|error CS\d+|: error |Test run summary: Failed!|^\s*failed: [1-9]|gate: (TIMED OUT|STALLED|BACKSTOP)`. It is matched case-insensitively, per line. A zero exit whose log matches becomes status 1 with `gate: command exited 0 but its log reports a failure -> <log>`; on any non-zero status up to 40 matching lines are printed to stdout as `<line number>:<line>`; a killed run is not scanned. `--no-markers` turns the scan off.
- The scan reads the log streaming and never whole: at most 64 KiB of any one line, and it stops after 40 matches. The tail is read backwards from the end of the log, at most 1 MiB of it; when the last line is longer than that window, the tail shows `… ` followed by its last 4096 characters, starting on a whole character. Each printed tail or marker line is cut at 4096 characters, ending ` …`.
- At the end the last `--tail` lines go to stdout, taken from the log without the lines of a kill's diagnostics block (dropped by position, so the command's own lines are never lost), then the verdict line ([Output lines](#output-lines)) (kept, V3).
- A panic after the slot is taken terminates the job, appends the `.err` file and the gate's notes to the log (when completing the log fails, the notes are kept for the next write), deletes the claim and journals `release` with `status` null and `verdict` unknown.

## Routing and the remote.ps1 bridge

Routing is off on a machine with no `remote.json` in `%LOCALAPPDATA%\gate\`: nothing is read and nothing is written.

- **What routes** is kept (T2): a heavy gate, not `--local`, under no covering or lending parent, outside a git hook (`GIT_INDEX_FILE` set to a non-empty value), whose first word is not `git` or `prek`, in a git worktree of a repo `remote.json` lists, whose words with in-worktree paths rewritten (T3) match one of that repo's patterns.

  A matched command still runs here, saying why, when a word names a path outside the worktree or the gate's folder holds nothing the snapshot sends.

  `--no-build` is dropped from the builder's words of a dotnet command, saying so (T4); the fallback here drops each exact `--no-build` word of a dotnet command after every refusal, rebuilding locally as the builder would have, because an earlier routed step (a repo's test-all build) may have built only on the builder, so a later `dotnet test --no-build` that falls back here would find no build output.

  A step that really did build here pays a second local build on a refusal, a cost accepted. A fallback that drops no word reuses the first plan.

  Each routed run pushes a new snapshot commit, and the .NET SDK stamps the source revision into every assembly's informational version, so without a pinned revision a routed build recompiled every project on every run.

  A routed dotnet command whose verb (the next non-empty word after `dotnet`, in any letter case) is `build`, `test`, `publish`, `pack`, `run` or `msbuild` therefore gets `-p:SourceRevisionId=<the worktree's HEAD sha>` on the builder's words only: before the first standalone `--`, else at the end.

  The word is not added when a word before the `--` already sets `SourceRevisionId` (`-p:`, `/p:`, `-property:`, `/property:` or `--property:`, in any letter case, alone or in a list separated by `;` or `,`). The sha comes from `git rev-parse --verify -q HEAD` in the worktree root, asked only of a command that would get the word; when that fails or the branch is unborn the run routes without it.

  The SDK keeps an explicit value (its git targets fill `SourceRevisionId` only when it is empty) and a `-p:` global property overrides a repo's own `<SourceRevisionId>`, so the builder's DLLs read `1.0.0+<HEAD sha>` as a local build's do, and a no-change routed build compiles nothing. The fallback here, the journal and the claim keep the command as given, and no output line says the word was added.
- **The decision is `gate-router`'s**, pure, over facts the caller gives it and the decision asks for lazily, in this order. A light or critical gate, or one under a covering or lending parent, stays local silently and reads nothing. `--local` stays local, saying `gate: not routed: --local`. Only then is `remote.json` read: no file stays local silently, and a file that is not a file, cannot be read or is refused stays local saying `gate: ignored <path>: <reason>; running locally`.

  After the file: a git hook, a command with no non-empty word, or a first word whose file name without extension is `git` or `prek` (`git.exe` and a full path included, in any letter case) stays local silently.

  Git is asked only then: a folder in no worktree (any non-zero `git rev-parse` exit), a repo the file does not list (the main checkout's folder name as a repo name, in any letter case) and words no pattern of the repo matches stay local silently, the first matching pattern winning. A fact that cannot be read stays local saying `gate: not routed: <reason>`. A machine with no `remote.json` costs no git call.

  git is found along the gate's own `PATH` (absolute entries only) and run by its full path from the gate's folder, with the `GIT_*` variables that point it at another repository, index or config removed and `core.fsmonitor` off, so a gate in a hook that exports `GIT_DIR` still routes; each call gets 60 s, after which git is killed and the gate runs here saying `gate: not routed: git did not answer within 60 s` (in whole milliseconds, `1 ms`, when `--test-git-limit-ms` sets a limit that is not a whole number of seconds).

  The limit is long because a loaded PC took git over 10 s in tiny repos, which kept heavy gates here exactly when the builder would help most; a git that truly hangs costs a heavy gate a minute.

  `gate run` answers those facts in `route.rs`, before it queues and before any command starts. The routing file is `%LOCALAPPDATA%\gate\remote.json` (no file when `LOCALAPPDATA` is unset or empty). The hook rule reads `GIT_INDEX_FILE`: set and non-empty means a git hook, which never routes.

  The worktree comes from one `git rev-parse` (root, prefix below the root and common git folder), and a folder below the root asks `git ls-files` whether the snapshot would send anything there (tracked or untracked and not ignored, and still on disk). A `subst` drive is followed through `gate_win32::device_target`. A word that is not valid Unicode keeps a command that would route here, saying so.

  The queue line carries `notRouted` `--local` for a heavy `--local` gate and the remote tag for a routed one. When the remote client has a part missing, the routed run falls back before it takes a remote slot: the `fallback` line has reason `client`, the gate says `gate: remote <host> client could not start: ...; running here`, and the later `take` and `release` carry no remote fields.
- **The routing file's text** is JSON with `//` and `/* */` comments and trailing commas allowed outside strings, and a leading byte order mark skipped, as the PowerShell gate's reader allows. Keys match in any letter case, and unknown keys are ignored.

  An object holding two keys that differ only in letter case anywhere in the file refuses the whole file, with the PowerShell reader's error text. Single-quoted strings are refused (`Conversion from JSON failed with error: ...`), a recorded deviation: the PowerShell reader accepts them.

  `host` is an ssh alias or `user@host`, each part of ASCII letters, digits, `.`, `_` and `-` and not starting with `-`: the host reaches ssh's argument list, a git `host:path` URL and ssh_config `%h` expansion, and only that shape is safe in all three.

  Any other host refuses the file (`host must be an ssh alias or user@host of letters, digits, '.', '_' and '-', not starting with '-'`), a recorded deviation: the PowerShell reader accepts any non-empty string. An IPv6 literal or a `host:port` is written as an alias in `~/.ssh/config`.

  `slots` is a whole number 1 to 16 (1 when missing), `repos` an object whose values are arrays of non-empty `-like` patterns, and `outputs` an optional object of the same shape whose globs may not hold `[` or `]`; a repo with no `outputs` entry brings back `**/bin/**` and `**/.godot/mono/temp/bin/**`.
- **The `-like` matcher** is PowerShell's wildcard match, case-insensitive, over the words joined by single spaces. It folds case by simple case folding. Inside a `[...]` set, a non-ASCII member whose .NET case-insensitive equivalents differ (the Kelvin sign and `k`) can match in PowerShell and not here.
- **The in-worktree path rewrite** (T3) finds a path wherever a drive letter, a colon and a separator start one inside a word (`--flag=X:\a`, `-p:Name=X:\a`, a script's text), each start judged on its own.

  A path runs to the first of `, ; " ' ) | & < >`. In a word that holds one of those or a line break (a script word: a script's text or a list) white space ends a path too; in any other word, one argument, a path runs through its spaces. A drive other than the root's is followed through `subst` mappings.

  A bare first word (no separator, drive or `.`/`..`) becomes `./name` only when that file exists in the gate's folder; otherwise it stays bare, for the builder's `PATH`.
- **The work folder** on the builder is named `<repo name>-<key>`, the worktree root's last segment cut to letters, digits, `-` and `_` and 8 hex digits of the SHA-256 of the lower-cased root ([Glossary](GLOSSARY.md), work-folder key).
- **The routed run's life** is kept (T5) and built in `remote/mod.rs` and `run.rs`: its ticket is heavy with `remote: true` and queues apart from the local waiters, which never count it ahead of them, nor it them; it waits up to 60 s for a remote slot (`--test-remote-wait-ms` in a test build); it takes one with a claim of kind `remote` of its own and calls the remote client with the route, the log and the limits (`--stall` as given, the interrupt).

  A client that runs the command to its end hands back its status, which the gate passes through, and the run releases the slot with the remote tag. On `full` (no slot within the wait), a refusal (status 75 with a reason) or a client that cannot start, it journals `fallback` while still holding the remote slot (none was taken on `full`), deletes the claim, lets the slot go, and runs here, taking a heavy slot with its first arrival kept in the ticket.

  The attempt's lines stay in the log once, as notes: the client's own bytes in the log are removed, and the local run writes its log afresh.
- **The remote client** (GATE-43) is built in parts. The seam a routed run calls is `RemoteClient` in `crates/gate/src/remote/mod.rs` (the call, the outcome, the scripted client); part A, the snapshot, and part B1, the transport, the routed lock, the preflight, the push and the run call, and part B2, the sync-back, part C1, the watchdog, the stop, Ctrl+C, the sync-back's deadlines and the output cap, and part C2, the lost connection and the reboot, are built and dormant.

  The snapshot copies the real index to `<git path>/gate-remote-<pid>.index` (a leftover copy and its `.index.lock` are removed first; the copy is empty when the repo has no index). With `GIT_INDEX_FILE` on the copy it runs `git add -A`, `git write-tree` and `git commit-tree --no-gpg-sign` with no parent, under the identity `gate-remote <gate-remote@localhost>` and with `GIT_AUTHOR_DATE` and `GIT_COMMITTER_DATE` removed.

  The copy is always removed, and the real index and the worktree are never touched, so both are byte-identical before and after a snapshot. The snapshot takes a list of roots, one entry today.

  The bridge commit has the snapshot's tree and, as its parent, the newest local `refs/gate-remote/*` not ending `-back`; with none it has no parent. The push writes `refs/gate-remote/<key>` once the builder has the snapshot.

  Every git call of the client goes through `remote/git.rs`, which the routing decision's git calls share: it removes the `GIT_*` location variables, applies the routing decision's time limit (`--test-git-limit-ms`), starts git through `remote/launch.rs` (suspended, assigned to an anonymous kill-on-close job of its own and resumed, with its output on pipes read by a thread per stream), reads and drops stderr where nothing reads it and ends git's whole job on Ctrl+C.

  The calls the client reads (`Git::output`: the sync-back's and the snapshot's) read git's output up to 16 MiB and keep the last 64 KiB of its stderr; a call past the cap is ended and fails with `git <args> failed in <folder>: its output passed 16 MiB`. The routing decision's calls are not capped. A Ctrl+C that ends git is an interrupt.

  The sync-back's `update-ref` and `apply` and the snapshot's `update-ref` run on a hidden console and answer to no interrupt, so a Ctrl+C cannot end git while it holds a ref lock in the git folder.

  Once the fetched back commit is confirmed as this snapshot's, every step up to a successful `apply` (the changed-file listing, the worktree check, the patch and the `apply` itself) that fails or is interrupted first keeps the back commit at `refs/gate-remote/<key>-back` and says so:
  - a failure: `gate: the remote run failed: <reason>; the changes are kept: see git diff <snapshot 8 hex> <back 8 hex>`;
  - an interrupt: `gate: the run was interrupted; the changes are kept: see git diff <snapshot 8 hex> <back 8 hex>`, with the interrupt's own status;
  - a keep that fails too: `gate: the remote run failed: <reason>; the changes could not be kept either: <why>`, or after an interrupt `gate: the remote run failed: the run was interrupted, and the changes could not be kept: <why>` (status 1).

  A patch that applied but cannot be removed fails the run (`gate: the remote run failed: cannot remove <patch>: <error>`) and keeps nothing: the changes are already in the worktree.

  Once git has exited, its readers get 2 s to reach their pipes' ends, else the call fails with `cannot read git's output: it did not end within 2 s of git's exit`. A failure line carries the last 10 of git's lines joined by ` | `, escaped and cut to the last 4 KiB at a whole escape.

  Only `gate run --test-remote-snapshot` and `--test-remote-transport` reach the client. The first makes the snapshot, says `gate: remote snapshot <sha> bridge <sha or none>` on stderr and in the log, and ends with status 0. A snapshot that fails says `gate: the remote run failed: git <args> failed in <root> (status <n>): <git's lines joined by ' | '>` and ends with status 1, with no fallback.

  Ctrl+C ends it with `STATUS_CONTROL_C_EXIT` and no line. A log that cannot be written gives status 1, with no fallback.
- **The remote client's run** (GATE-43, parts B1, B2, C1 and C2) is built in `remote/{client,payload,retry,syncback,transport,watchdog}.rs` and dormant: a production gate runs the bridge instead. Only a build with the `test-seams` feature has it, chosen by `gate run --test-remote-transport local:<folder>`, which stands in for the builder with a local folder.

  The real ssh transport (`Ssh` in `remote/transport.rs`) is built and unit-tested and used by `gate push` ([`gate push`](#gate-push)) in every build; the rest of the client stays dormant, and the remote cutover (GATE-46) selects the client for routed runs. `transport.rs`'s ssh words and `Ssh::find` build without `test-seams` for the push.

  The phases run in this order. Any `gate: the remote run failed` line ends the run with status 1 and no fallback; Ctrl+C at any point after the snapshot ends the run as an interrupt.
  - **Lock.** The run takes the routed lock, `Local\gate-[<root hash>-]routed-<16 hex>` ([Kernel object names](#kernel-object-names)), before the remote slot, with a zero wait, and holds it for the whole remote attempt.

    A lock held elsewhere says `gate: remote <host> busy; running here` and falls back `busy` before any queueing, so two routed runs of one worktree give a deterministic `busy`, not a race. A lock that cannot be opened or taken says `gate: cannot open the mutex <name>: <error>` or `gate: cannot take the mutex <name>: <error>`, then the busy line.
  - **Snapshot.** Part A's snapshot and bridge commit.
  - **Run line.** The run call is one cmd line, `"%LOCALAPPDATA%\gate\bin\gate.exe" box run <base64 JSON>`, built before anything is sent. It may be at most 8127 characters, counting `%LOCALAPPDATA%` as 100 (8191, cmd's limit, less 64 for sshd's own `cmd.exe /c`). A longer one says `gate: the remote run failed: the run call is <n> characters, longer than the 8127 cmd takes (8191 less 64 for sshd's own cmd.exe /c); shorten the command` and sends nothing.
  - **Preflight.** `gate box preflight` over ssh. Status 255, ssh's own status for a lost connection, says `gate: remote <host> unreachable: <last stderr line>` and falls back `unreachable`; the line is the last `\r` update of the last stderr line, its first 4 KiB, escaped (Escaping, below). Status 9009 or 2 (no `gate box` verb there) says `gate: not routed: <host> has no gate box that takes this run (preflight status <n>)` and falls back `version`.

    Any other non-zero status says `gate: preparing <builder root> on <host> failed (status <n>)` and ends the run with that status. A reported version other than this PC's says `gate: not routed: <host> runs gate <theirs|unknown>, this PC <ours>` and falls back `version`; the builder's version is its first 4 KiB, escaped.
  - **Push.** `git push --no-verify --no-recurse-submodules --progress [--receive-pack=…] -- <host>:<builder root>/<repo>.git +<snapshot>:refs/gate/<key> [+<bridge>:refs/gate-bridge/<key>]`, with `GIT_SSH_COMMAND` carrying the ssh options. Success says `gate: pushed snapshot <8 hex> in <s> s (<size>|up to date)` and moves the local `refs/gate-remote/<key>`.

    The size is read from git's last `Writing objects: 100%` line, ignoring `remote:` lines, and only a number with a unit (or `bytes`) counts as one, else the line says `up to date`. Failure says `gate: pushing the snapshot to <url> failed (status <n>)` and falls back `unreachable`.
  - **Run call.** The line goes over `ssh -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=4 -o ForwardAgent=no -o ClearAllForwardings=yes -- <host> <line>`, each argument separate. The box's output passes through to the log and the console. A status 75 is a refusal only when the box said its busy line (`gate: remote work folder <key> is busy`); otherwise it is the command's own status.
  - **Outputs.** In `remote/outputs.rs`, for a run with output globs; a run without them (and `--no-build`) sends `Outputs` `[]` and `OutputsFull` false, no manifest and no marker, and says nothing here. The client builds the manifest ([The outputs module](#the-box-side)) before the run call, hashing every matching file, and writes the pending marker `<state root>\remote\<key>.outputs-pending` (an empty file; a marker whose existence cannot be read counts as present). The payload carries the globs as `Outputs` and `OutputsFull` (true when the marker was there), and the manifest goes as one line on the run call's stdin, which is then closed; a pre-banner retry feeds it again. A marker that cannot be written says `gate: the remote run failed: cannot write the outputs pending marker <path>: <error>`; a run call whose input could not be sent logs only `gate: the run call's input was not sent: <error>`.

    The client accepts only the last stderr line starting `gate-remote-outputs: `, kept as it is heard so a stdout flood cannot push it out, and echoes it like any box line. It must be `gate-remote-outputs: 0` or `gate-remote-outputs: <n> <key>-<this run's nonce>.tar.zst` with n above 0. After an accepted `0` the marker is removed (`gate: cannot remove the outputs pending marker <path>: <error>` if it cannot, and the status stands); every other end leaves it, so the next run sends `OutputsFull`. An accepted line naming an archive leaves the marker; the fetch of the archive to `<worktree>\.tmp\<key>-<nonce>.tar.zst` (part C2) and its install (part D) are not built yet.

    A passing run says at most one of these lines and then exits 1: `gate: remote outputs not brought back: the worktree changed during the run` (wins when it applies), `gate: remote outputs not brought back: the manifest could not be built: <msg>` (`<path> resolves outside the worktree`, `it is larger than the box takes (64 MiB)`, checked as the manifest is built before the rest is hashed, or the read error), or `gate: remote outputs not brought back: the box printed no 'gate-remote-outputs: <n> <archive>' line`.

  - **Sync-back.** In `remote/syncback.rs`, after a run call that ended with the command's own status (a refusal, an interrupt or a failed call never syncs back). `git ls-remote -- <url> refs/gate-back/<key>` then `git -c fetch.fsckObjects=true -c maintenance.auto=false -c gc.auto=0 fetch --progress --no-recurse-submodules --no-tags -- <url> <ref>` run with the push's `GIT_SSH_COMMAND` and `--upload-pack=pwsh -NoProfile -NonInteractive -Command git-upload-pack`; a bare repository on this machine names no pack command. The fetch runs with the console's interrupt, so it starts no automatic `maintenance run --auto` or `gc --auto` whose lock a Ctrl+C could leave in the user's repository.

    Their output stays out of the run's log. No back ref on the builder means the command changed nothing, and the run ends with the command's status and no line. A Ctrl+C during either call ends the run as an interrupt.
    - A back commit whose only parent is not this run's snapshot, or a fetched commit that is not the one `ls-remote` advertised for exactly that ref, says `gate: ignored stale remote changes <back 8 hex>: not made from snapshot <snapshot 8 hex>`, applies nothing and leaves the status.
    - A back commit that touches a path with a component starting with `.git` (letter case aside: `.gitignore`, `.gitattributes`, `.gitmodules`, `.git`, `.github`) or makes a symbolic link (mode 120000) or a submodule (160000) is never applied, since the builder is trusted less than this machine: such a change could widen what the next snapshot uploads or plant what runs here.

      It says `gate: remote changes not applied: they touch <path>, which a sync-back never writes; see git diff <snapshot 8 hex> <back 8 hex>`, keeps the back commit at the local `refs/gate-remote/<key>-back`, and a passing run becomes status 1. This check comes before the worktree comparison.
    - A worktree whose tree (what `git add -A` and `git write-tree` write through a copy of the index, `snapshot::worktree`) is no longer the snapshot's says `gate: remote changes not applied: the worktree changed during the run; see git diff <snapshot 8 hex> <back 8 hex>`. The back commit is kept at the local `refs/gate-remote/<key>-back`, a passing run becomes status 1, and a failing run keeps its status.
    - Otherwise the diff from the snapshot to the back commit is written by `git diff-tree -p --binary --full-index -r` (plumbing, so no diff setting or `GIT_EXTERNAL_DIFF` shapes it) to `<git dir>/gate-remote-<pid>-<nonce>.patch`, a file the gate creates new, applied with `git -c core.protectNTFS=true apply --whitespace=nowarn -- <patch>`, and the patch is removed whether or not it applied.

      The run says `gate: synced back <n> file(s): <names joined by ", ">`, names from `git diff-tree -r -z --name-only` with control characters escaped; it reads `0 file(s): ` with nothing after the colon when no files changed.
    - A git failure here ends the run with status 1 and `gate: the remote run failed: ...`, with no fallback.
    - The `ls-remote` is killed after the git limit (60 s); the `fetch` after a stall window with no output, or at the run's ceiling. Either fails the run the same way, with the reason.

  A pre-banner failure, which is exit 255 (128 for git) with empty stdout and a first stderr line starting `Connection timed out during banner exchange`, `kex_exchange_identification: ` or `banner exchange: `, says `gate: <host> did not answer the ssh banner; trying once more` and is retried once after a 2 s pause.

  Two pre-banner failures of the run call fall back `unreachable`; the sync-back's `ls-remote` and `fetch` are retried the same way, and two failures of either end the run with status 1.

  **The watchdog** (`remote/watchdog.rs`, `remote/client.rs`) watches the preflight, the push, the run call and the sync-back's fetch by the run's stall window and ceiling. A `--stall` under 15 s is raised to 15 for a remote run (the box's heartbeats come every 5 s) and says `gate: --stall <n> raised to 15 for a remote run (heartbeats come every 5 s)`. It looks every quarter of the window, at most once a second.

  A killed run call says `gate: STALLED: no output or remote CPU for <n> s` or `gate: TIMED OUT after <n> s`, stops the command on the builder and ends the run with status 124, with no fallback. A killed preflight or push never started the command, so an early stall says `gate: remote <host> unreachable: <the call> stalled` (or `outlived the run's ceiling`) and falls back `unreachable`.

  **The stop** is a `stop` call to the builder's `gate box`, made after a kill and after a Ctrl+C that ended a call that may have started the command. It is bounded to 30 s, is retried once after a pre-banner failure while 5 s of the bound would be left, and runs on a hidden console of its own (`CREATE_NO_WINDOW`), so a second Ctrl+C cannot end it.

  The stop says `gate: stopping the command on <host> did not finish in <n> s` or `gate: stopping the command on <host> failed (status <n>)`; the run ends the same either way.

  **A lost connection** is any exit 255 of the run call that is not a pre-banner failure (`End::Lost`); a command that itself exits 255 is treated the same, as the PowerShell wrapper does. Nothing is synced back. The client always sends the `stop` first, and a Ctrl+C by the end of the stop says the lost line below and ends with the Ctrl+C status, with no boot call.

  Then, only when the preflight gave a boot time (`gate: remote boot <time>`), it makes a bounded `boot` call (`gate box boot`): the same 30 s bound and pre-banner retry rule as the stop. The outcome:
  - Both boot values parse as timestamps and the new one is more than 30 s later: `gate: <host> restarted during the run (booted <time>); the run did not finish there`, and the run falls back `rebooted`.
  - The boot call gives no value, or either value is no timestamp: `gate: <host> unreachable after the connection was lost; the run did not finish there`, and the run falls back `unreachable`.
  - Otherwise (the same boot, an earlier boot, exactly 30 s later, or no boot time at the preflight): `gate: lost the connection to <host>` and the run ends with status 255.

  A boot call that fails says `gate: asking <host> for its boot time did not finish in <n> s`, `gate: asking <host> for its boot time failed (status <n>)` or `gate: asking <host> for its boot time failed: <why>`, and counts as giving no value.

  On a fallback after a loss, the run call's output lines are dropped from what the local rerun's marker scan reads. Unlike the PowerShell wrapper, the stop always comes first (it skips the stop on a restart), and a restart needs a later, parseable boot time (it compares unparsable values as strings and takes any difference over 30 s).

  **Ctrl+C** ends the call it interrupts and the run as an interrupt. A Ctrl+C whose kill of the call fails stays an interrupt and still sends the stop.

  **Jobs.** Every remote call (ssh or git) runs in an anonymous kill-on-close job, created suspended and assigned before it runs, so a kill ends its whole tree; the routing decision's local git calls start the same way (`remote/launch.rs`). Breakaway from the job is allowed only for the local test transport, whose stand-in box starts under the gate's own job.

  **The output cap.** Every call's line is capped at 64 KiB, and a call keeps only its last 1000 lines and at most 1 MiB of them, dropping the oldest first; the newest line is always kept. A `\r` resets the current line, as git's progress does (the Progress and Unlogged readers only). A progress update over 64 KiB, however the pipe splits it, ends a capped call with `... sent a line longer than 64 KiB`. The run call is not capped: its longer lines pass on in pieces.

  **Escaping.** Every text a builder supplies that a `gate:` line quotes is escaped: control characters, U+061C, U+200E, U+200F, U+202A-U+202E, U+2066-U+2069, U+2028 and U+2029 are written `\u{..}`. That covers the preflight's last line, the builder's version in the version refusal, and every `git ... failed` line of the snapshot, the saved ref and the sync-back. A failed git line lists git's stdout first and its stderr last, keeping the last 10 lines and 4 KiB so git's `fatal:` line survives.

  The client's git calls also drop `GIT_CONFIG_COUNT`, `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM` and every `GIT_CONFIG_KEY_n` and `GIT_CONFIG_VALUE_n`, so the caller's git configuration never reaches the builder's repo, and `GIT_SSH_VARIANT` and `GIT_EXTERNAL_DIFF`, so neither the ssh option syntax nor the diff a sync-back applies can be changed from outside.
- **Sibling worktrees** (GATE-19), planned, not built: some repos' builds reach into a sibling repo by relative path, and a routed run that sends one worktree's snapshot would compile something different on the builder without any error.

  `remote.json` declares each repo's siblings (`"siblings": {"app": ["app-server"]}`); a routed gate in a repo with siblings snapshots each sibling's worktree beside its own (`../{name}`, as the pair folder lays them out, or beside the main checkout), and the box lays the work folders out the same way, so relative paths between them resolve.

  A word naming a path inside a declared sibling's worktree is rewritten as an in-worktree path is.

  This lands before a repo with siblings is enabled in `remote.json`; a repo without siblings is enabled without it. What happens when a declared sibling is missing, whether the work-folder key is per pair or per repo, and how sync-back covers a sibling's changed files are open ([Open questions](#open-questions)).
- **Inputs** (GATE-22), planned, not built: a routed command can need ignored files only the PC has, and the snapshot sends only tracked and untracked-not-ignored files. A repo's optional `inputs` list of ignored-file globs, beside `outputs` in the routing file, is packed with the outputs' transport and hash-diffed against the builder's work folder, so an unchanged input costs nothing.
- **The per-host failure record** (GATE-27), planned, not built: a record in the state root holds a builder's failure reason and an expiry. A routed gate that finds an unexpired record says `gate: not routed: <host> <reason> <n> s ago`, runs here and journals `fallback` with the reason, rather than paying the snapshot, preflight and push again for the same reason.

  The windows per reason: `unreachable` a short one (60 s, distcc's default backoff); `parity` and `tools` until the snapshot's `global.json` or tool manifests change; `busy` and `full` are load, not faults, and are never recorded.

  The record also counts box-side losses (exit 255, `rebooted`, `unreachable` after the start, a `stop` that leaves processes behind) apart from command failures, which never count; past a threshold nothing routes to that builder until a preflight succeeds.
- **The bridge** (GATE-68) is the remote client of a production gate. Until the remote port, `gate.exe` runs the remote part through the PowerShell gate's remote client, `remote.ps1`, as a child process speaking its current protocol, and every builder keeps its PowerShell wrapper.

  Everything inside the remote attempt (the snapshot, the push, the remote watchdog, heartbeats, sync-back and outputs) stays in `remote.ps1`, and its outputs pass through `gate.exe` untouched. `gate.exe` owns the routing decision, the remote pool, the claim, the ticket, the journal lines and the fallback. The remote port replaces both ends in one drain and deletes the bridge.

  The entry point is a small repo-owned shim script, `tools/remote-bridge.ps1`, installed beside `gate.exe` as `remote-bridge.ps1`. It dot-sources `remote.ps1` and calls `Invoke-RemoteCommand`, as `gate.ps1` does in-process, and hands the status and the refusal reason back as control lines on stdout (below); `gate.exe` writes the verdict itself.

  The stand-alone CLI of `remote.ps1` rewrites no words and writes its own verdict, so it is not the entry point. A routed run behaves as under the PowerShell gate: `--no-markers`, per-repo `outputs`, colon-form words, the passed line's host and a FAILED line before a fallback are kept.

  `gate.exe` checks the client's parts before it takes the routed lock or a remote slot (`RemoteClient::ready`): `remote.ps1` at `%USERPROFILE%\.claude\tools\gate\remote.ps1`, the shim beside `gate.exe`, and `pwsh.exe` on PATH (never `powershell.exe`).

  A missing part falls back `client` with no remote take, saying `gate: remote <host> client could not start: <why>; running here`, with `<why>` one of `cannot find remote.ps1 at <path>`, `cannot find the bridge script at <path>`, `cannot find pwsh at <path>` (the test seam's path), `pwsh not found on PATH` (pwsh is looked up as `pwsh.exe` along PATH only, never through PATHEXT), `USERPROFILE is not set, so remote.ps1 cannot be found`, `USERPROFILE is not an absolute path, so remote.ps1 cannot be found` or `the remote.ps1|bridge script|pwsh path <path> is not valid Unicode`.

  After the slot is taken, a client that cannot go on still falls back `client`, with `<why>` one of `cannot start <pwsh>: <error>`, the shim's own error, or `the bridge ended (status <n>) before the remote run started`. A run that has started is never rerun locally: see the started rule below.

  The command line is `pwsh.exe -NoProfile -NonInteractive -File <shim> -RemoteScript <remote.ps1> -HostAlias <host>`. The child has stdin and stdout piped (`LaunchOutput::Piped`) and inherits the gate's stderr, so the remote run's live `gate:` lines reach the console. Its environment is the gate's less the `GIT_*` location and config variables.

  It is started through `gate-win32`'s `Launch` in the gate's own job (`KernelObject::Job` of the gate's pid), so `gate stop-tree` reaches it, and whatever it leaves in the job is terminated after it ends. Once the job is empty, the gate cancels a read of pwsh's stdout that a process outside the job still holds open, so it never hangs on one.

  **The request** goes on pwsh's stdin, and stdin is then closed. It is UTF-8 JSON with no byte order mark: `version` 2, `nonce` (32 hex digits, fresh per call), `root`, `repo`, `key`, `subfolder`, `outputs`, `words`, `log`, `timeoutSeconds`, `stallSeconds` and `tail`. No file is written for it or for the answer.

  The shim writes no control line while the request's nonce is unread; a failure before that goes only to stderr as `remote-bridge: <message>`. It refuses, each as an error line, a `-RemoteScript` that is not a full path to a `.ps1` file, a `-HostAlias` that is not a host `remote.ps1` takes, a `version` other than 2, and a key of the wrong type or range (`timeoutSeconds` and `stallSeconds` at least 1, `tail` at least 0).

  **Control lines** are the answer: one JSON object per line on pwsh's stdout, each carrying the nonce. They are `{"nonce","started":true}`, written right before `Invoke-RemoteCommand`; `{"nonce","status":n,"killed":bool,"reason":"busy"|"unreachable"|"parity"|"tools"|"rebooted"|null}`; and `{"nonce","error":"<message>"}`.

  The shim writes `\n` before each control line and ends each with `\n` (LF). The gate skips empty lines without counting them; a line over 64 KiB (65536 bytes before its `\n`) is read through and counted.

  Any other line is ignored and counted, and the count is said as `gate: the bridge ignored <n> line(s) on pwsh's stdout that were not its own`. A reason becomes the fallback's reason only with status 75; any other status is the command's own. The shim starts the remote clock itself.

  **The started rule.** Before a `started` line the remote run has not begun, and pwsh ending falls back `client` (the why is the shim's error, or `the bridge ended (status <n>) before the remote run started`). After `started` with no result line the run may have run, so it ends with status 1 and no local run, saying `gate: the bridge ended (status <n>) after the remote run started; it did not report how the run ended`, where the shim's error replaces the part after the semicolon.

  A result line that comes before `started` counts as the start. A Ctrl+C that arrives as pwsh ends, with no result line, ends the run as the interrupt (status 0xC000013A), started or not, after saying the shim's error as `gate: <error>` when it sent one.

  **Ctrl+C.** A first Ctrl+C with no `started` line read yet ends pwsh's job at once as an interrupt, with no stopping line. After `started`, the first Ctrl+C reaches pwsh too and `remote.ps1`'s own stop path runs. The gate waits with no timer and says `gate: stopping the remote command; Ctrl+C again to kill it here` once, on stderr only. A second Ctrl+C ends pwsh's job at once. Either way the run ends as an interrupt.

  The bridge's own lines go to stderr as they come and are appended to the log once pwsh and its job have ended.

  Besides the stopping line they are `gate: cannot send pwsh the request: <error>`, `gate: cannot read pwsh's stdout: <error>` or `gate: the bridge's reader of pwsh's stdout failed` (either way the gate then reads and discards the rest, so pwsh never blocks on a full pipe), `gate: cannot cancel the read of pwsh's stdout: <error>`, `gate: cannot list the processes in pwsh's job: <error>` (once a call), `gate: cannot wait for pwsh: <error>` (a Ctrl+C was raised, there is no result and the wait failed), `gate: cannot write the bridge's lines to <log>: <error>` and `gate: cannot end what pwsh left in <job>: <error>` (and, once a call, `gate: cannot end what pwsh left in <job> again: <error>` when a later terminate fails: until the job lists empty the gate ends it again every 10 ms, and only then cancels its read of pwsh's stdout, which it also does at once when the job cannot be listed).

  A control line the cancel cuts off is still read as a line, as at end of file.

  A call that falls back hands the run the lines remote.ps1 wrote to the log, so the run here says them again; a log that exists but cannot be read gives the one line `gate: could not read the remote run's lines from <log> (<error>)` instead.

  The host reaches `remote.ps1` as one `-HostAlias` argument of `pwsh -File`, never through `pwsh -Command`, which would put it through a second round of PowerShell parsing. It is the host `gate.exe` already validated against the allowlist (the routing file's text, above); `remote.ps1` never re-reads `remote.json`, whose PowerShell reader lacks that check.

  `gate-router` carries the host in the routed run as a `Host` newtype that only the routing file's reader builds, so no host string the reader did not check reaches a process. The Rust remote client starts ssh with each argument separate and `--` before the destination, and keeps the allowlist too, since a git `host:path` URL cannot carry `--`.

## The box side

A builder runs the same `gate.exe`, under the hidden `gate box` subcommand, from its own `%LOCALAPPDATA%\gate\bin`. The exe is pushed to every builder the local `remote.json` names, over ssh, by `gate push` ([`gate push`](#gate-push)), not by the local install script.

A routed run first learns the builder's gate version; when it differs from its own, the run falls back to local, saying `gate: not routed: <host> runs gate <version>, this PC <version>`, and journals `fallback` with reason `version`.

Builders are configured only in the local `%LOCALAPPDATA%\gate\remote.json`. The repo, its docs and its tests name no builder; the suite's remote cases run against a stub.

The box protocol keeps the PowerShell wrapper's wire format byte for byte, so the old `remote.ps1` can drive `gate box`. `gate box` ships dormant: builders run the PowerShell wrapper behind the bridge until the remote cutover (GATE-46).

Each verb is one ssh call today, and the call's cost is its connection, not its work. Measured warm, setup takes about 2.1 s (TCP about 0.2 s, key exchange and auth 1.5 s, the session 0.4 s) against 0.2 to 1.0 s of verb work, so a routed run's three verb calls spend about 6.4 s, 30 to 37% of a 17 to 26 s routed build, before its git transfers pay the same again.

A routed run therefore moves to one long-lived connection carrying every verb, `gate box serve`, and the per-call client stays until that lands (GATE-70 holds the measurement, and GATE-109 the work).

The payload is one base64 word (standard alphabet, padded) of UTF-8 JSON holding the wrapper's keys: `Key`, `Bare`, `Work`, `Lock`, `Index`, `OutputFolder` and `Job`, and for `run` also `Command`, `Subfolder`, `Identity`, `Outputs`, `Nonce` and `OutputsFull`, and for `outputs` also `Name`. Unknown keys are ignored, and `Outputs`, `OutputsFull` and `OutputFolder`, which the outputs transport owns (GATE-44), are read only by `run` (the outputs) and `outputs` (`Name`, `OutputFolder`); `preflight`, `stop` and `boot` ignore them.

For `run` the payload also needs a `Snapshot` key: exactly 40 lowercase hex characters, the commit the run checks out. Anything else is refused before the lock and the checkout with `gate: remote run payload's Snapshot '<value>' is not a 40-hex commit sha` and exit 2.

The value is empty for a missing key or JSON null, the string as given with control characters escaped, and compact JSON for any other non-string. Two differences from the wrapper are deliberate: the echoed value has its control characters escaped, and the check is stricter than the wrapper's .NET `$`, which accepts a trailing newline.

Every path comes from the payload and nothing is written under the gate's state root; `gate box` takes no `--state-dir`. Test seams are `test-seams`-gated flags, never environment variables, and sit before the verb (`gate box --test-... run <payload>`) in a `BoxSeams` struct on `BoxRequest`, apart from the `gate run` seams, so `--test-print-request` is unchanged.

The box seams, in any order before the verb and each also as `--<seam>=<value>`, are `--test-heartbeat-ms <n>`, the period of the command heartbeat; `--test-box-console fail-before`, which makes the command's UTF-8 console fail before the command starts (the only accepted value, else `gate: --test-box-console must be fail-before, got '<value>'`); `--test-box-park-launch <path>`, which parks the launch after the donor is in the job and before the output lock, writes the empty file `<path>.parked`, and goes on once `<path>` exists (at most 600 s); and `--test-outputs-budget-bytes <n>`, which replaces the 50 GiB outputs budget of the box's trim, in bytes.

A seam is refused as the `gate run` seams are: no value, given twice, and for the heartbeat and the outputs budget not a whole number above 0 or above 2147483647 (`gate: --test-outputs-budget-bytes must be a whole number above 0, got '<v>'`, `... must be at most 2147483647 ...`, `... needs a value`, `... is given twice`). Without the feature each word gets the box usage line.

The box leaves build servers warm on purpose: it does not set `MSBUILDDISABLENODEREUSE` (the local gate sets it to 1 for every command), so MSBuild nodes and compiler servers survive between runs. It sets `MSBUILDNODEHANDSHAKESALT` in the command's environment to the work-folder key, replacing any inherited value, so the nodes stay warm within a worktree and are never reused across them; the box's own git calls and tool restore do not get it.

Measured without the salt, one pool of worker nodes served routed builds from every work folder on the builder (sequential runs added no nodes, concurrent ones two per extra build, reclaimed after idling). A run in another work folder therefore inherited nodes started for a different one, which the salt stops, at the cost of each worktree's first routed build starting its own nodes.

`git` and `dotnet` are found with the gate's own program resolution, which never searches the current folder. A payload that is not base64 JSON prints `gate: box payload is not base64 JSON: <reason>` on stderr and exits 2.

Built verbs:

- **`boot`** prints `gate: remote boot <UTC time>` on stdout (ISO 8601, 7 fractional digits, `Z`; the system time less the tick count since boot) and exits 0.

  When the time cannot be read it prints `gate: remote could not read the boot time: <message>` on stderr and exits 1, so the client's boot query after a lost connection says the call failed rather than taking a status 0 with no time. The preflight says the same line and goes on: an unreadable boot time fails no preflight, and the client then makes no boot query.
- **`preflight`** creates the bare repo at `Bare` when missing and the work folder at `Work` with `.git`, `origin` set to `Bare`, `core.autocrlf false` and `core.eol lf`, and writes `.git\objects\info\alternates` as `<Bare with />/objects` and a line feed, without a byte order mark, only when the file is missing or empty.

  It prints on stdout, in order: `gate: remote created <Bare>` (only when it created the bare repo), `gate: remote version <gate version>`, the boot line, and `gate: remote sdks <v1,v2|none>`; git's own output passes through. A payload without `Bare` or `Work` prints `gate: box payload lacks <key>` and exits 2.
- A failing git step prints `gate: remote <step> failed (status <n>)` and exits with that status; a git that cannot start prints `gate: remote <step> failed: <OS message>` and exits 1. The steps are `git init --bare`, `git init`, `git remote add`, `git config core.autocrlf` and `git config core.eol`.

  A folder or file that cannot be made prints `gate: remote could not create the folder <path>: <error>` or `gate: remote could not write <path>: <error>` and exits 1. An SDK list that cannot be read prints `gate: remote could not read the SDK list: <message>`, omits the sdks line and still exits 0.
- **`run`** runs the payload's `Command` (a list of texts) in `Work`, and exits with the command's own whole 32-bit status after a successful run. Its stdout is the command's stdout, LF-terminated; its stderr is the command's stderr plus the box's own lines.

  The heartbeat: `gate box run` writes `gate-remote-heartbeat: <cpu ms>` on stderr every 5 s, on `gate-watchdog`'s `BeatClock` and `Pump`. It writes unconditionally before the command starts (from just before the checkout) and after the command's wait ends. While the command runs it writes only when the CPU of the command's job, plus every build server on the box outside the job, since the last beat reaches 1% of the time since it.

  Build servers are VBCSCompiler, and dotnet naming VBCSCompiler.dll or an MSBuild node, read by the gate's server reader. Every server counts as new, so warm MSBuild nodes count, as the PowerShell wrapper's BW9 does.

  The `CpuSource` is a monotone running sum: a server new since the last read, or whose total fell (a reused id), adds its whole CPU, and an empty or failed process snapshot, or a server whose times could not be read, keeps the previous values. Another run's busy build on the box can keep a silent idle command alive until the wall-clock timeout, as in PowerShell.

  A heartbeat line never splits a forwarded line: the pump writes under the output lock the forwarders take. The run's git children (fetch, checkout, clean, the back ref reset, the back commit's add, write-tree, rev-parse, commit-tree and push, and diff-tree) have their output forwarded the same way, line by line; the box's own lines are written whole under std's stream lock, which the pump's writes take too. A line still waiting for the lock when the readers stop is dropped.

  After git exits, the box waits at most 2 s for its pipes to close. A pipe a leftover process still holds says `gate: remote git's output is still held open by a leftover process; stopped reading` and the run goes on. A client that closed the connection while git's output was forwarded ends the run with 255 and `gate: remote the client closed the connection during git <step>`. A pipe that cannot be read says `gate: remote could not read git's stderr: <error>` (or `stdout`).

  A failed CPU read writes `gate: remote heartbeat stopped: cannot read the CPU of job <name>: <error>` (or `... cannot list the processes of job <name>: <error>`) and a thread that cannot start writes `gate: remote heartbeat stopped: cannot start the heartbeat: <error>`; the heartbeats end and the run does not.

  Ending the heartbeat waits at most 2 s (`Pump::stop_within`), so a pump stuck on a client that stopped reading is left behind and the work folder's lock is released.

  Payload refusals exit 2, each on stderr. The command's own: `gate: box payload Command is not a list of texts`, `gate: box payload lacks Command`, `gate: box payload Subfolder is not a text`, `gate: box payload Identity is not an object of texts`, `gate: box payload Identity may set only GIT_AUTHOR_* and GIT_COMMITTER_* names: <name>` and `gate: box payload Identity <name> holds a NUL`.

  The work folder's: `gate: box payload Lock must be <Work>.lock`, `gate: box payload Index must be <Work>.index`, `gate: box payload Key is not a work-folder key: <key>` and `gate: box payload Job is not this work folder's job name: <job>`.

  `Identity` may hold only `GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL`, `GIT_AUTHOR_DATE`, `GIT_COMMITTER_NAME`, `GIT_COMMITTER_EMAIL` and `GIT_COMMITTER_DATE`.

  `Key` is `[A-Za-z0-9_-]+` and is the last name of `Work`; `Job` is `Local\gate-remote-<Key>-<12 hex digits>`. `run`, `stop` and `preflight` check the key; `run` and `stop` check the job.

  The lock `<Work>.lock` is opened with no sharing and without following links, must be a plain disk file, and is held for the run's life; a held lock prints `gate: remote work folder <key> is busy` and exits 75. A lock that cannot be opened prints `gate: remote could not open the work folder lock <path>: <error>` and exits 1.

  `Work` must not end with a separator (`gate: box payload Work must not end with a separator: <work>`, exit 2). Every git call in `Work` passes `-c core.hooksPath=<Lock>\hooks -c core.fsmonitor=false`, a hooks path that cannot exist while the box holds the lock, so a repository's own hooks (one a build installed) never run in the box's git; fetch and push name `Bare` by its path, never `origin`.

  The payload is data, refused unless well formed; the command, and whatever it writes into `Work`, is the user's own code running as the user, and is trusted.

  The steps are fetch, the commit probe, checkout, clean and the back ref reset, the dotnet tool restore, then the command, then the sync-back. A failing step prints `gate: remote <fetch|checkout|clean|back ref reset> failed (status <n>)` and exits `n`.

  After the fetch of `refs/gate/<key>` from `Bare`, the probe runs `git cat-file -e <sha>^{commit}` in `Work`. A non-zero exit prints `gate: remote snapshot <sha> is not in the work folder's repository after the fetch` and exits 1, and a git that cannot start prints `gate: remote cat-file failed: <message>` and exits 1.

  The checkout is `checkout -q --force --detach <sha>`, the payload's commit and not `FETCH_HEAD`, so a later push of the key changes nothing a running run runs. The box's git calls for preflight and the probe share one start path (`preflight::Git::code`).

  The tool restore (`boxside/tools.rs`) runs `dotnet tool restore` in `{Work}\{Subfolder}` on a hidden console, in a job of its own named `{job}-tools`, created kill-on-close and never handed to dotnet, so the box's own handle keeps it: a box that dies or is killed takes the restore and everything it started with it, and a process the restore leaves running is terminated before the command starts.

  Its stdout and stderr are forwarded into the run's output line by line under the box's output lock, as the command's are, so they never split a heartbeat line. It has no time limit: a hung restore holds the work-folder lock, so every routed run of that worktree falls back `busy` meanwhile, and the run's stall watch does not see it, since the box heartbeat goes on outside the command (whether to cap it is GATE-105).

  A stop already set when the restore would start runs none: status 1, `gate: the gate failed while running the command: the job {job} was stopped before the command started`. A stop during the restore ends its job (dotnet and anything it started) and gives the same line naming `{job}-tools`.

  A client that closes the connection during it ends the job and the run exits 255 with `gate: remote the client closed the connection; stopping job {job}-tools`. None of these writes the record or starts the command.

  The manifests are found by walking from the place folder up to the work root, never above the work folder, `.config\dotnet-tools.json` before `dotnet-tools.json` in each folder; a walk that finds none looks for nothing and writes no record.

  The record `<Work>.tools`, beside the lock, is 64 lower-case hex digits (SHA-256 over, per manifest in walk order, the UTF-8 path relative to the work folder, a NUL, the file bytes, a NUL), UTF-8, no BOM, no newline; the restore is skipped while the record equals the hash of the manifests.

  A restore that cannot start prints `gate: remote could not start the dotnet tool restore: <message>`, and a non-zero one prints `gate: remote tool restore failed (status <n>)` then `gate: remote work folder <key> could not restore its dotnet tools`; both exit 75 with the command never started, the record unchanged and the back ref left cleared, and the PC side reads the 75 plus the work-folder line as fallback reason `tools`.

  A manifest that cannot be read prints `gate: remote could not read the tool manifest <path>: <error>`, one that does not lie under the work folder prints `gate: remote could not place the tool manifest <path> under the work folder <work>`, and a record that cannot be written prints `gate: remote could not write <record>: <error>`; the first and last exit 1.

  The record is read and written through a handle that refuses a link, a folder or a file with more than one name, as the lock's is; a read that fails for any reason but "not found" says `gate: remote could not read <record>: <error>; restoring the tools` and restores.

  Known limit, as in PowerShell: after the builder's NuGet cache is cleared, the `*.tools` records must be deleted by hand.

  A `Subfolder` is compared against the snapshot with per-character simple uppercase (the wrapper's OrdinalIgnoreCase): one that is not a folder inside `Work` prints `gate: remote subfolder <sub> is not a folder inside the work folder`, deletes the back ref and exits 2, and one missing from the snapshot prints `gate: remote subfolder <sub> does not exist in the snapshot` and exits 2, `<sub>` with its control characters escaped in both.

  The shared batch refusal (a `.cmd` or `.bat` word, [Command launching](#command-launching)) exits 2.

  The command runs outside the box's own job, created with `CREATE_BREAKAWAY_FROM_JOB` so it leaves the ssh session's job (the box's job refusing breakaway prints `gate: the box cannot start a command outside its own job: <message>` and exits 1). Its console is a donor's UTF-8 console (the console, below), else a hidden console of its own (`CREATE_NO_WINDOW`).

  It is assigned to the payload's named job, which has no limits and no kill-on-close, and inherits exactly NUL, the two pipe write ends and a query-only handle to its job, as copies made under the spawn lock ([Command launching](#command-launching)). The box's git and dotnet calls start through `gate_win32::spawn`, under the same lock.

  The stop event `<job>-stopped` ([Kernel object names](#kernel-object-names)) is a manual-reset event the run creates, unset, right after it takes the work-folder lock, before the fetch and checkout; one that cannot be created prints `gate: the gate failed while running the command: cannot create the event <job>-stopped: <error>` and exits 1.

  `gate box stop` sets it whether or not the job exists yet (a missing event is silent), then terminates the job when there is one. The run looks at it once just before it creates the job (set: the stop line and exit 1, nothing started), and again after the command is assigned to the job and before it is resumed, on every launch.

  Set means a `stop` came first: the command is ended unstarted, the job is terminated, and the run prints `gate: the gate failed while running the command: the job <name> was stopped before the command started` and exits 1. An event that cannot be read prints `gate: remote cannot read the stop signal of job <name>: <error>; starting the command`, and the command starts.

  A job name already held prints `gate: the job <name> already exists; a run's job name must be new` and exits 1. A command that cannot start prints `gate: could not start <word>[ (<resolved>)]: <message>` and exits 127, the word and the resolved path with their control characters escaped; a failure of the gate after the start prints `gate: the gate failed while running the command: <problem>` and exits 1. If the box panics after the resume, the job is terminated with 124.

  Output is forwarded from the pipes until both end. When a leftover process holds a pipe open after the command ended, the box stops reading after a 2 s drain and says `gate: remote command's output is still held open by a leftover process; stopped reading`. A pipe that cannot be read prints `gate: remote could not read the command's <stdout|stderr>: <error>`.

  A failed write by a forwarder to the box's stdout or stderr means the PC's ssh connection closed, and a closed connection always stops the job, whether it is noticed while the command runs or during the final drain. The box terminates the job with 124 and polls its processes every 100 ms for up to 10 s. It then drains the readers, skips the sync-back, releases the work-folder lock and exits 255.

  On stderr, best effort, it says `gate: remote the client closed the connection; stopped job <name>` once the job is empty. Otherwise it says `gate: remote the client closed the connection; stopping job <name>` followed by the could-not-stop line or the `still has <n> process(es)` line of `stop`. The stdin closing is not the signal. A command that itself exits 255 is indistinguishable from a lost connection, as with the PowerShell wrapper.

  After the command, the sync-back commits the tree when it changed: a copy of the index at `<Work>.index` is staged, and the commit (message `gate-remote back`, parent HEAD) is written to `refs/gate-back/<key>` in `Bare`. A sync-back that fails prints `gate: remote sync-back failed: <message>` and turns a 0 status into 1; an index copy that cannot be removed prints `gate: remote could not remove <index>: <error>` and changes no status.

  A payload with `Outputs` globs adds these steps (GATE-44):

  - Once the heartbeat beats and before the checkout, `run` reads the PC's manifest from stdin up to its first newline (at most 64 MiB; a manifest that cannot be read is said only once the command has passed), then removes the old archives from `OutputFolder`: each file named `<key>-<32 lower-case hex digits>.tar.zst`, whatever the key, last written more than a day ago, and no other file. An archive already gone counts as removed; one that cannot be removed, or a folder that cannot be listed, is said (`gate: remote could not remove the old outputs archive <path>`, `gate: remote could not list the old outputs archives in <folder>`) and fails nothing.
  - Just before the command, unless `OutputsFull`, it lists the files under the globs (size and last write); a listing that fails is said (`gate: remote could not list the outputs before the command: <error>`) and leaves every file counting as changed.
  - After a command that exited 0 and its back commit, with the commit pushed, it reads the commit's paths (`git diff-tree`) and packs the files to bring back into `<OutputFolder>\<key>-<nonce>.tar.zst`, created new. It says `gate-remote-outputs: <n> <archive name>` on stderr, or `gate-remote-outputs: 0` with no archive when nothing is to come back. The paths are read only after the push, so failing to read them fails the outputs and never the sync-back.
  - Right after a pack that wrote an archive, the box trims `OutputFolder` to at most 50 GiB of outputs archives (files named as in the first step; other files are neither counted nor removed). It removes the archives last written strictly before the run's own, oldest first (by path among equals), until the total fits; an archive written with the run's own or after it is never removed, so the folder may stay over the budget until the newer run's own trim.
  - The run's own archive is opened sharing reads only, so another run's trim cannot remove it mid-write; such a removal fails, is said as above (`gate: remote could not remove the old outputs archive <path>: <error>`) and that trim goes on to the next oldest. An entry whose metadata cannot be read is said (`gate: remote could not read the outputs archive <path>: <error>`, control characters escaped), left out of the total, and the trim goes on.
  - A run whose own archive alone is larger than the budget removes only its own archive, leaves the others, and says `gate: remote outputs not brought back: the archive is <X.X> GiB, more than the box keeps (50 GiB)`. A run whose own archive is gone before its trim says `gate: remote outputs not brought back: the archive was removed before it could be sent`. Either turns a passed 0 into 1.
  - A pack that fails removes its half-written archive, says `gate: remote outputs not brought back: <reason>` and turns the passed 0 into 1. A command that did not exit 0, or a failed sync-back, packs nothing.
- **`stop`** sets the job's stop event, then terminates the payload's job, taking no lock. It terminates the job again on every 100 ms poll of the job's process ids, until the job is empty or 10 s pass, so a process that joins late is ended; its processes end with 124.

  A terminate after the first that fails is not a failure of the stop: it is said once, `gate: remote could not terminate job <name> again: <error>`, after the other lines. An event that cannot be set prints `gate: remote could not signal the stop of job <name>: <error>` on stderr, and the job is terminated all the same. A stopped job prints `gate: remote stopped job <name>` on stdout and exits 0; no such job exits 0 with no output.

  The stop also ends the run's tool restore job `<name>-tools` when one exists: it opens that job before it sets the event and terminates it after the payload's job, with the same lines and rules for `<name>-tools`. It exits 1 when either job is left with processes or cannot be stopped, else 0.

  Processes still alive 10 s after the termination print `gate: remote job <name> still has <n> process(es) 10 s after it was terminated` and exit 1, and a job that cannot be terminated prints `gate: remote could not stop job <name>: <error>` and exits 1.

- **`outputs`** sends the archive the payload's `Name` names in its `OutputFolder` raw on stdout, then removes it. It takes no work-folder lock, so it never waits on a run.

  A payload without `Name`, `Work` or `OutputFolder`, or with a `Work` or `OutputFolder` that is not the work folder's own, exits 2. A `Name` that is not `<Key>-<32 lower-case hex digits>.tar.zst` for the payload's own key (`gate: remote outputs archive name '<name>' is not an outputs archive of this work folder`), an archive that does not exist (`gate: remote outputs archive <name> does not exist`) or one that cannot be read or sent exits 1. The file is opened without following a link, and only a plain file is sent.

  A sent archive that cannot be removed prints `gate: remote could not remove the outputs archive <name>: <error>` and still exits 0; a later run's prune takes it. An archive already gone at its removal (another run's trim took it after it was sent) says nothing and exits 0.

**The outputs module** (GATE-44) is shared by both ends, this exe, in `crates/gate/src/outputs/`. The box's `run` packs with it and the box's `outputs` verb sends the archive; the PC's fetch and install are not built yet. The PC's manifest, the pending marker and the reading of the box's `gate-remote-outputs: ` line are built in the Rust remote client, which is dormant ([The remote client's run](#routing-and-the-remoteps1-bridge)).

- Globs: each is relative to the work folder's root and `/`-separated, matched without regard to case: `**/` is any number of folders (none included), `**` anything, `*` any run of characters within one name, `?` one character within one name. Never brought back, whatever the globs: anything under a `.git`, `.tmp` or `TestResults` folder at any depth, and a `.env` file (`.env`, `.env.<name>`, `<name>.env`).
- Walk: lists the files below a folder that the globs match, ordered by relative path, and does not enter an excluded folder. A link or a junction, to a file or to a folder, is neither followed nor listed. A folder or name that cannot be read, or is not Unicode, fails the walk.
- Manifest: the PC's list of its matching files, sent as one line of JSON and a newline, `{"files":{"<path>":{"size":<bytes>,"modified":<file time>,"sha256":"<hex>"}}}`, paths relative to the worktree root and found without regard to case.

  The box's pack takes each file the manifest lacks, and each it holds that the run created or changed (the box's stat listing from just before the command lacks it or holds another size or last write) whose SHA-256 differs from the manifest's, skipping the paths of the run's back commit. Under `OutputsFull` there is no listing, so every file counts as changed and each that differs from the PC's comes back.
- Archive: a tar stream compressed with zstd (level 3), named `<key>-<nonce>.tar.zst`, each file read once and hashed as it is packed. The last member, `.gate-outputs.json`, lists what was packed (path, size, SHA-256), and the reader checks the members against it after extraction. Both ends stream; memory does not grow with a file's size.
- Payload: `Nonce` is 32 lower-case hex digits, `OutputFolder` is `<root>\outputs` for the work folder `<root>\work\<key>`, `Outputs` a list of non-empty globs holding no `[` or `]`, `OutputsFull` true or false, and `Name` an archive name of the payload's own key and nonce, so that the outputs verb reaches no file in `OutputFolder` but an archive of this work folder. A refusal is a `gate: box payload ...` line.

`unpack` creates its destination, which must not exist, and refuses an archive for any of these, a member before anything of it is written:

- A member name that is not a plain relative path inside the destination: `.git` in any case, an 8.3 short name (`~` before a digit), a part ending in `.` or a space, any of `<>"|?*:` or `\`, a control or bidirectional character, a device name (`CON`, `PRN`, `AUX`, `NUL`, `CONIN$`, `CONOUT$`, `COM0` to `COM9`, `LPT0` to `LPT9`, also with an extension), or more than 64 parts.
- A GNU long name over 4096 bytes, and any pax, `g`, `K` or `S` entry.
- More than 50,000 files or folders.
- A member over the bytes left of the caller's limit (and over 16 GiB).
- Input as sent past the limit plus limit/64 plus 1 MiB, zstd's worst case for the limit.
- A last write in base-256 form, before 1970 or more than a day ahead of the machine's clock.
- A member that would be written through a link or a junction, or a member that is itself not a plain file.
- A member that differs from the list in size or SHA-256, one the list lacks, or a listed file the archive lacks.

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
- Aside copies that no process holds any more are deleted; one a running gate still holds stays until a later install.
- It adds `%LOCALAPPDATA%\gate\bin` to the user's PATH when missing, so callers run `gate`: the raw registry value of the user environment is read and written, keeping its kind and its unexpanded `%NAME%` entries; the change is broadcast (`WM_SETTINGCHANGE`, as `setx` does) so programs started from then on see it, and the script says to open a new terminal. It never touches the state root's files.
- `-WhatIf` lists every step without taking it. `-Exe` (install a prebuilt exe, skipping the build) and `-EnvironmentKey` (the registry key the PATH entry lives in) exist for the suite, which runs the script against a temporary `LOCALAPPDATA` and registry key.
- Every install also copies `tools/remote-bridge.ps1` to `%LOCALAPPDATA%\gate\bin\remote-bridge.ps1`, where the bridge looks for it. The copy goes to `remote-bridge.ps1.<pid>.tmp` beside it and is moved into place with `Move-Item -Force`.
- The script installs locally only. `gate push` copies the installed exe to each builder in `remote.json` (`host`, then `hosts`), with the same rename-aside on the builder, so a machine with builders runs `pwsh tools/install.ps1` and then `gate push` ([`gate push`](#gate-push)). It pushes the installed exe, never one from `target/`, and never installs here.
- Install is a script, not a `gate install` subcommand.

## Cutover

The PowerShell gate and `gate.exe` share slot mutexes and never share files. The default root's mutex names are the PowerShell gate's, so a PowerShell gate still running and a new `gate.exe` gate take turns on one set of slots and the machine is never over-subscribed; each generation writes its claims, tickets and journal in its own folder and format, and once the drain is over ops-tower reads `gate.exe`'s, plus the PowerShell claims of a slot whose mutex is open. Moving a machine to `gate.exe` is a drained cutover:

1. Before it runs, ops-tower's Gates tab already writes the default root's `slot-counts.json` (`%LOCALAPPDATA%\gate\v1\`) on every edit, through `gate-state`'s writer, since `gate.exe` never reads the PowerShell gate's `%LOCALAPPDATA%\gate\slot-counts.json` ([Counts file](#counts-file)).

   `gate cutover` copies the PowerShell gate's slot counts into the default root's `slot-counts.json`, so both generations count the same slots, then writes `%LOCALAPPDATA%\gate\cutover.flag`, holding its write time in RFC 3339 UTC (the launcher never reads it). The PowerShell launcher in each repo reads the flag, and from then on every new gate runs `gate.exe` through the compatibility layer.
2. It then waits until every PowerShell gate's claims and tickets in `%LOCALAPPDATA%\gate\` are gone or stale, naming on stderr what it still waits on, and exits 0 once none is left. The drain reads claims and tickets only and probes no mutex.

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

After the caller sweep there is no fallback: callers run `gate ...` on PATH, a machine without it gets "command not found", and each repo's docs name the install script. Before the sweep, the PowerShell launcher's own fallback for a machine without the gate (H2) is the launcher's, not `gate.exe`'s.

The compatibility layer is deleted once no caller uses `-Log`, `-TimeoutSeconds` or `-Slot`. The guard launcher stays: `sync-launchers.ps1` publishes both launchers.

## Crate layout

A virtual Cargo workspace (`resolver = "3"`) with every crate under `crates/`, listed by path. The root copies ops-tower's `[workspace.lints.clippy]` table, its `[profile.test-release]` and its `rustfmt.toml`, so moved code lints and formats as it did.

| Crate | Owns | Builds on Linux |
|---|---|---|
| `gate-state` | Every file format in the state root (claim, ticket, counts, journal, archive, `remote.json`), their readers and writers, path and object-name rules, journal history and `normalize_command`, the dashboard state ops-tower shows. Moved from ops-tower with its history (GATE-4). | yes; a pure crate |
| `proc-table` | The process snapshot model (`ProcessInfo`, `ProcessTable`, `ProcessTree`) and its Windows reader. Moved with `gate-state` (GATE-4). | yes; its `win32` module is Windows-only |
| `gate-win32` | The Win32 layer: job objects, process creation, named mutexes and events, priority, console, the WSL VM's CPU reader, the samples the watchdog consumes. | no |
| `gate-watchdog` | Free-share, load-adjusted clock, stall, ceiling and backstop arithmetic over samples with the kill lines, and the box heartbeat, on an injected clock; the cores-hint verdict is not yet built. | yes; a pure crate |
| `gate-args` | The gate's own argv: the subcommand CLI, the compatibility layer, the C-runtime and cmd.exe quoting, the `%VAR%` refusals and the PATHEXT resolution rules over an injected environment and file lookup. | yes; a pure crate |
| `gate-router` | The routing rules (T2-T4) over strings: what routes, the path rewrite, the `--no-build` drop. | yes; a pure crate |
| `gate` | The `gate` binary: the subcommands, the wait loop, the run, the journal writes, the bridge and `gate box`. | no |

Rules:

- No pure crate's API exposes `windows` or `windows-sys` types; Windows-only code sits in `cfg(windows)` modules. `proc-table` is not a pure crate: its `win32` module's API carries `windows-sys` handle and access-right types.
- `gate-state` and `proc-table` are also ops-tower's dependencies, by one pinned git rev, so their types unify there.
- The pure crates build with `cargo clippy` and `cargo test` on Linux in CI.

## Black-box suite harness

A Rust integration suite drives the built `gate.exe` through its CLI and reads only what it writes: exit codes, stdout, stderr, the log, the journal, claims and tickets.

- **Own state root per case.** Every case creates a fresh folder under the target directory's temporary folder. The folder is the case's working folder, holding its logs and its `LOCALAPPDATA`, and its `state` subfolder is the `--state-dir`, so cases never touch the machine's real slots, journal or counts, or each other's. Cases run side by side.
- **Load.** Watchdog-timing cases depend on the scheduler. The gate under test runs at Normal priority, and its command and the build servers it watches run at BelowNormal. Under an all-core load, the gate was measured runnable but not running for up to 13.7 s, and a BelowNormal command for up to 31.8 s with no CPU.

  So a case never races a fixed sleep against the watcher: its command sleeps past the 600 s hang guard, and only the behaviour under test ends it. A stall window a case relies on is far past those gaps, or the case is ignored and run with `--ignored` on a critical slot (the repo's nextup profile lists them).
- **Test seams.** Hidden `--test-*` flags, compiled only with the `test-seams` cargo feature, set the timing knobs: `--test-sample-ms` (sample period), `--test-heartbeat-ms` (wait heartbeat; on `gate box` the command heartbeat's period), `--test-poll-ms` (fallback poll) and `--test-stall-ms` (stall window below a second). Each takes whole milliseconds above 0 on `gate run`. The sample and stall knobs drive the watchdog.

  Two seams of `gate box` shape the command's console: `--test-box-console fail-before` makes the UTF-8 console fail before the command starts, and `--test-box-park-launch <path>` parks the launch until `<path>` exists ([The box side](#the-box-side)). `--test-outputs-budget-bytes <n>` replaces the 50 GiB outputs budget of the box's trim.

  Two more seams make a run end by Ctrl+C without a console: `--test-interrupt-after-samples <n>` raises the interrupt after the n-th sample, and `--test-interrupt-queued` raises it when the waiting line is printed, so once no slot is free for the gate. `--test-print-request` shows them as `testSeams.interruptAfterSamples` and `testSeams.interruptQueued`.

  On `cutover` the seams are `--test-poll-ms`, `--test-heartbeat-ms`, `--test-confirm-ms` (the confirmation window) and the switch `--test-cutover-interrupt-after-list`; `--test-print-request` shows them as `testSeams.pollMs`, `heartbeatMs`, `confirmMs` and `interruptAfterList` ([`gate cutover`](#gate-cutover)).

  On `push` the seams are `--test-push-ssh <path>` (the ssh program to run instead of the one found along `PATH`, the suite's `fake-ssh`) and `--test-push-bound-ms <n>` (each builder's bound); `push` has no `--test-print-request` ([`gate push`](#gate-push)).

  `--test-print-request`, on `run`, `stop-tree`, `cutover` and the compatibility form, prints the parsed request as one JSON line on stdout and exits 0, so the suite sees what a command line asked for; it is the one hidden switch the compatibility layer accepts.

  `--test-sampler-fail` (a switch) makes every sample fail with `--test-sampler-fail makes every sample fail (test seam only)`, so the run says `gate: sampler failed: ...; watching by wall time only` and only the backstop watches; `--test-print-request` shows it as `testSeams.samplerFail`, after `interruptQueued`.

  `--test-diagnostics-fail` (a switch) makes the diagnostics gathering of a kill fail, so the log carries `gate: diagnostics failed: <message>` in place of the block; `--test-print-request` shows it as `testSeams.diagnosticsFail`, after `samplerFail`.

  `--test-remote-client <path>` replaces the remote client with a scripted one that runs in the gate's own process: the file's steps, one a line, are `out`, `err`, `record`, `wait`, `interrupt`, `killed`, `status`, `refuse` and `fail-start`. `--test-remote-wait-ms <n>` (whole milliseconds above 0) replaces the 60 s limit of the wait for a remote slot. `--test-print-request` shows them as `testSeams.remoteClient` and `testSeams.remoteWaitMs`.

  `--test-git-limit-ms <n>` (whole milliseconds above 0, on `gate run` only) replaces the 60 s limit of each git call of the routing decision; `--test-print-request` shows it as `testSeams.gitLimitMs`, after `remoteWaitMs`.

  `--test-servers-under <folder>` (on `gate run` only; refused empty or given twice) makes a build server outside the job count only when its program path lies under the folder (whole path components, case-insensitive, the folder made absolute), so a case never counts the machine's real servers; `--test-print-request` shows it as `testSeams.serversUnder`, after `gitLimitMs`.

  `--test-server-image <file name>` (on `gate run` only; needs `--test-servers-under`, else `gate: --test-server-image needs --test-servers-under`; refused empty or given twice) makes the watch treat processes with that exact image name (case-insensitive) as a compiler server, like `VBCSCompiler.exe`; `--test-print-request` shows it as `testSeams.serverImage`, after `serversUnder`.

  The suite's priority cases use fake-tool copies named `gate-test-server.exe` with it, so another gate on the machine, which lowers every `VBCSCompiler.exe` outside its own job, cannot disturb them.

  `--test-remote-snapshot` (a switch, on `gate run` only) sends the run to the Rust remote client's snapshot alone ([The remote client](#routing-and-the-remoteps1-bridge)); `--test-print-request` shows it as `testSeams.remoteSnapshot`, after `serverImage`.

  `--test-remote-transport local:<folder>` (on `gate run` only) sends the run to the Rust remote client with a local folder standing in for the builder; `local-ssh:<folder>` does the same but pushes and fetches to `<host>:<folder>/<repo>.git` with the `--test-remote-ssh` program as git's ssh (which it needs), so git's own ssh calls reach the stand-in.

  `--test-remote-ssh <exe>` replaces ssh with a program (the test bin `fake-ssh`), found as the gate finds its own programs ([Command launching](#command-launching)): a name with a folder part against the working folder, a bare name along PATH only. One it cannot find ends the run before any call with `gate: cannot use the remote transport '<transport>': cannot find --test-remote-ssh <word>: <why>` and status 1, after the transport's own form is checked.

  `--test-remote-retry-pause-ms <n>` (whole milliseconds above 0) replaces the 2 s pause before the pre-banner retry; it and `--test-remote-ssh` need the transport, and `--test-remote-snapshot` cannot be given with it. `--test-print-request` shows them as `testSeams.remoteTransport`, `testSeams.remoteSsh` and `testSeams.remoteRetryPauseMs`, after `remoteSnapshot`.

  `--test-remote-bound-ms <n>` (whole milliseconds above 0) replaces the 30 s bound of the client's `stop` and `boot` calls. `--test-remote-interrupt-at <preflight|run>` makes the client raise the console interrupt itself after that call's first output byte. `--test-remote-timeout-ms <n>` (whole milliseconds above 0) replaces the run's ceiling for the client.

  `--test-remote-watch-call <preflight|push|run|ls-remote|fetch>` makes `--test-stall-ms` and `--test-remote-timeout-ms` apply to that one call alone, its ceiling counted from the call's own start, every other call keeping the run's own window and ceiling. With `ls-remote`, `--test-remote-timeout-ms` is that call's deadline and `--test-stall-ms` alone leaves it on the git limit. With `fetch`, the stall and ceiling seams apply to the fetch alone.

  A value outside a seam's calls says `gate: --test-remote-watch-call must be preflight, push, run, ls-remote or fetch, got '<v>'`, and `--test-remote-interrupt-at` likewise names `preflight or run`. The four need the transport; `--test-print-request` shows them as `testSeams.remoteBoundMs`, `remoteInterruptAt`, `remoteWatchCall` and `remoteTimeoutMs`, after `remoteRetryPauseMs`.

  `--test-remote-script <path>` and `--test-bridge-pwsh <path>` (on `gate run` only) replace the bridge's `remote.ps1` and `pwsh`; `--test-print-request` shows them as `testSeams.remoteScript` and `testSeams.bridgePwsh`, after `remoteTimeoutMs`.

  `--test-own-start-fails` (a switch, on `gate run` only) makes both reads of the gate's own start fail, so the run ends with the refusal line of [Output lines](#output-lines); `--test-print-request` shows it as `testSeams.ownStartFails`, after `bridgePwsh`.

  An installed gate has none of them, and without the feature each is an unknown option.
- **Spawn and wait helpers.** The suite's `start` launches the gate and returns a `Running` that can wait for a stderr line, say whether the gate is still running, and finish; dropping a `Running` kills a gate still running. Output readers are joined with a deadline, so a grandchild holding the pipes cannot hang the suite. A run's `Outcome` carries `raw_stdout`, the bytes as the gate wrote them, beside the decoded text.
- **Holding and killing a gate.** A case holds a run with a real command, `fake-tool wait <go>`, and lets it go by creating `<go>`. A case that kills a gate lets its command go and waits until the command's log is closed (`support::wait_until_closed`), since the job has no kill-on-close. `support::run_words(log, extra, command)` builds the words of a light run.
- **Ordering, never elapsed time.** A case asserts the order of events in the journal and the log (a waiter's `take` after the holder's `release`, a heartbeat line before the inner gate's take, a kill line before the exit), never a wall-clock bound a busy machine could cross. A case's own deadline only guards against a hang and is generous.
- **Injected clock.** The watchdog arithmetic is unit-tested in `gate-watchdog` with an injected clock and scripted samples, not through the binary.
- **Fake process trees.** A small helper executable, `fake-tool`, is built only with the `test-seams` feature.

  It runs its steps in order:

  - `argv`, `pid` (prints `pid <its own pid>`), `env <name>` (prints the variable's value) and `exit <n>` (any 32-bit status).
  - `out <text>`, `err <text>`, `bytes <hex>` (writes those bytes to stdout as they are) and `lines <n> <text>` (prints `<text>1` to `<text><n>`).
  - `sleep <ms>` and `wait <path>` (until the path exists, giving up after 600 s with exit 1).
  - `burn <stop-path>` (prints `burning`, then spins on the CPU until the file exists), `burn-threads <n> <ms>` (spins on `n` threads for `ms` of wall time, then goes on) and `chatter <path>` (prints numbered lines `chatter 1`, `chatter 2`, ... about every 50 ms until the file exists).
  - `spawn-wait <path>` starts `fake-tool wait <path>` with the caller's handles, prints `spawned <pid>` and does not wait.
  - `spawn-orphan <path>` runs `fake-tool spawn-wait <path>` as a child, waits for it to end, then continues with its next step, so the waiting helper is left without a parent and its `spawned <pid>` line is the up signal.

  It is copied under the names the gate classifies (`VBCSCompiler.exe`, `dotnet.exe`, `MSBuild.exe`).

  A second helper, `fake-ssh`, stands in for ssh in the remote client's cases (`--test-remote-ssh`). Its steps are `ok`, `hold`, `version`, `boot <time>` (as `version`, for the box's boot line), `no-boot` (drops the box's boot line), `drop` (exits 255 at once, writing nothing), `exit`, `fail-banner`, `fail-other`, `out-then-drop`, `ok-then-out <line>` (as `ok`, then writes the line on stderr, as the box says its outputs line), `say-exit <n> <line>` (writes the line on stderr and exits `<n>`, as the box says its own refusal), `flood <n>`, `say-hold <word> <path>` and `progress-ok <n>`. A line that starts `pwsh ` and holds `-EncodedCommand` is a `gate push` call (`push` in its call log): `ok` runs it with the local `pwsh`, `LOCALAPPDATA` set to `<GATE_FAKE_SSH_BUILDERS>\<host>`, so each builder is a folder the case owns. Started with `--version` alone, `fake-ssh` answers `gate 0.0.0-old`, the stand-in for an older gate on a builder.

  It spawns children, burns CPU, prints its argv, writes output or sleeps on command, so build-server, orphan and kill-tree cases need no real toolchain.
- **No fixture from the checkout path.** No routing pattern, expected path or assertion is derived from where the repo is checked out; a case builds its own repository and routing file in its root. `checkout::repo_file` is the one route to a checked-in file (the install script), never for a path or pattern a case asserts on.
- **Launcher cases** (GATE-41): `crates/gate/tests/suite/launcher.rs` has 8 cases. They run the synced `tools/gate.ps1`. A case's `LOCALAPPDATA` is its `appdata` folder, whose `gate\v1` is the default root with the real slot names, so every such case also sets `GATE_STATE_DIR` to a non-default folder. The flag-unset case points the launcher at a stub canonical gate through `CLAUDE_CONFIG_DIR`. A new suite module that calls `repo_file` updates the callers pin in `checkout.rs`.
- **Cutover cases** (GATE-9): `crates/gate/tests/suite/cutover.rs` has 15 cases: the counts copy, the flag, the refused words and the printed request, and six over the drain. A live PowerShell gate is a `fake-tool wait` child whose claim or ticket, written by `support::write_powershell_claim` or `write_powershell_ticket` under `support::powershell_folder`, carries the child's real pid and start time; a stale one names a pid that has ended or a start that differs from the running process's.

  The six cases wait on a live claim and name it, wait on a live ticket and name its command line, ignore stale claims and tickets, name a late arrival, resume after an interrupt, and run the launcher's hand-over after a finished cutover.

  They use the launcher cases' helpers (`stub_claude`, `install_gate`, `run_launcher`, `run_direct`). Their waits are ordering-only: `Running::wait_for_stderr_while_running` returns the stderr once the line is there and the gate has not exited, and no case asserts how long anything took.
- **Running it.** The suite is a test target with `required-features = ["test-seams"]`, run through the gate with `cargo test --workspace --profile test-release --all-features`. Without the feature (no `--all-features`) cargo skips the suite silently.

## Cores hint

When a run that passed did not fit its slot, the gate says how many cores the command kept busy, so the caller learns whether heavy or light was right (GATE-6). For example: `gate: used 7.4 cores on average (peak 15, 0.6 free) on a light slot; work that keeps more than 2 busy belongs on heavy`.

- The line is said on stdout, just before the passed line, only for a passed run that took its own light or heavy slot, ran 10 s or more of unpaused wall time, and had no gate under it take a slot of its own. A critical run is never judged, and a covered or lent run takes no slot.
- `avg` is the job's CPU (user plus kernel time of every process ever in the job, exited ones included, with a final read at exit) plus the CPU of the build servers the watchdog counts while the job holds a build client (the final stretch counted with the last sample's servers), over the run's unpaused wall time; the CPU of paused samples is left out. `peak` is the highest own CPU over one sampler interval, paused samples skipped. `free` is the mean over the unpaused samples of the cores others left free. `avg` and `free` show one decimal; `peak` is rounded to a whole number, ties to even.
- A light run averaging over 2 cores is told heavy would fit. A heavy run averaging under 1.5 cores with at least 4.0 free on average is told light would fit. The peak is reported but does not change the verdict.
- Whether a gate under the run took a slot of its own is read from the journal (`nested_took_slot`): a gate under `Nesting::Under` writes `parentPid` on its `take`. When the CPU at the end or the journal cannot be read, the hint is left out with a log-only note ([Output lines](#output-lines)).
- The journal's `release` line records `avgCores`, `peakCores` and `freeCores` ([Journal](#journal)), the history `--slot auto` will read (GATE-8). `--slot auto` runs a command with too little history on heavy and says it guessed; it never chooses critical.
- Learned durations and `--slot auto` are built after the local cutover; the v1 formats reserve their optional fields ([Reserved fields](#reserved-fields)).

## Open questions

What no ruling settles yet, with the issue that owns it:

- **The box protocol and `remote.json`'s next format** (GATE-10): verbs, payload and transport between PC and builder, whether ssh keeps cmd.exe as the builder's shell, and the routing file's version and matcher. Sibling worktrees in a routed run leave three questions of their own (GATE-19).
- **A prediction a run has passed** (GATE-14): what a holder past its p90 reports in wait lines, and whether the critical gate re-prices its shadow end and closes or shrinks the lent window when a blocker overruns.
- **The `--timeout auto` spelling** (GATE-16): how the caller's floor travels with `auto` in the new CLI.
- **The surfaces of `gate status` and `gate drain`** (no issue yet). Install stays a script (`tools/install.ps1`) until a reason for a `gate install` subcommand appears.
- **The Windows facilities to spike before their slice** (GATE-5): other users' command lines (B1), the desktop-app breakaway policy (X1) and nested jobs, each of which changes kill behaviour if it silently fails.
