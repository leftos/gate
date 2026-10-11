# Gate behaviour inventory for the Rust port (CC-212)

Date: 2026-10-09. Sources: claude-config `tools/gate/` at c9ace7bd, with `feat/gate-remote-outputs` and `feat/gate-durations`, and ops-tower `crates/gate-state`.

Written for `docs/plans/gate-rust-port.md`, step 1. The port may redesign the CLI, the output and every file format, so this lists what the gate does and who depends on it, not how the PowerShell is organised.

## Conventions

- Line numbers are `main` at c9ace7bd unless a tag says otherwise. `[outputs]` is `feat/gate-remote-outputs` (worktree `C:\Users\someone\.claude-wt\feat-gate-remote-outputs`, 352469a6, merge base 5d7dc918). `[durations]` is `feat/gate-durations` (worktree `C:\Users\someone\.claude-wt\feat-gate-durations`, c48b029a, merge base 5a945ae3). `[planned]` is designed in `docs/plans/gate-durations.md` parts 2 to 5 and in no branch yet.
- Files: `G` is `tools/gate/gate.ps1`, `L` is `gate-launcher.ps1`, `R` is `remote.ps1`, `W` is `remote-wrapper.ps1`, `GS` is `gate.selftest.ps1`, `RS` is `remote.selftest.ps1`, `README` is `tools/gate/README.md`, `OT` is `X:\dev\ops-tower\crates\gate-state\`.
- Marks: **essential** (the port must keep the behaviour, in whatever form), **incidental** (a PowerShell or .NET workaround the port need not carry), **open** (a design question for the redesign).
- "Pins:" names the self-test cases that hold the behaviour. A behaviour with no pin says so: that is a gap the Rust suite should close.
- Journal times are UTC, ISO "o" with seven fractional digits; "ticks" are .NET ticks (100 ns since 0001-01-01 UTC). Both formats leak into files other tools read (section 3).

## 1. Behaviours

### 1.1 CLI, arguments and usage

- **C1 Options and required inputs.** `-Log`, `-TimeoutSeconds` (whole number above 0, no sign or space), `-Slot heavy|light|critical` (lowered to lower case) and the command after `--` are required with no default, because a gate with no ceiling "holds its caller for an hour".

  `-StallSeconds` (default 120), `-Tail` (20), `-NoMarkers` and `-Local` are optional. Every missing or unreadable input is named on its own `gate: ` line, then the usage block and `all four of -Log, -TimeoutSeconds, -Slot ... and the command are required.`, exit 2, before any slot is queued.
  - Pins: `Test-Usage` rows `no-log`, `no-timeout`, `bad-timeout` (`abc`), `no-slot`, `bad-slot` (`medium`), `no-command`, `no-value` (`-TimeoutSeconds` last, `needs a value`). Each row expects exit 2 and the line.
  - Where: G:1514-1533 (`Read-Argument`), G:1550-1602 (`Get-InputProblem`), G:3891-3902, G:424-429. **Essential** (the contract; the flag spellings are free).
- **C2 Option colon form.** `-Name:value` in one word is read as that option; a switch (`-NoMarkers`, `-Local`) takes its bare form only, and any `-Switch:<value>` sets it true (so `-Local:false` inverts silently; CC-194). Where: G:1461-1489. **Settled** (a switch's colon form is read by its value; see design.md, Compatibility layer, and section 6, CC-194).
- **C3 Words after `--` reach the command unchanged.** Colon-form words such as `-p:Name=Value`, `-getItem:Compile`, `-warnaserror:CS0168` must arrive whole.

  Under `pwsh -File` the gate reads the process argv (`Get-RawWord`, `Get-ScriptArgument`) because PowerShell's binder splits the word; from an in-session call it rejoins `-p:` with the next word (`Join-ColonWord`) and flattens a nested array like a native call (`Expand-Word`). A lone `-x:` meant as its own argument cannot get through (README:29).
  - Pins: `Test-Argument` (a word with quotes and a space, one ending in a backslash, one with a space ending in a backslash), `Test-ColonSwitch`, `Test-ColonSwitchInSession`, `Test-InSessionPathArgument`, `Test-NestedArrayCommand`, `Test-LauncherColonSwitch`.
  - Where: G:1404-1457, G:1495-1510. **Essential** is only "the argv arrives byte-exact" (Rust reads `GetCommandLineW` and splits it). The binder workarounds are **incidental**.
- **C4 Command line quoting for CreateProcess.** Each word is written so the Microsoft C runtime reads it back as one argv entry: empty or whitespace/quote-bearing words are wrapped, `"` becomes `\"`, and backslash runs before a quote or at the end are doubled. Where: G:1354-1362, G:2764-2778. Pins: `Test-Argument`. **Essential**.
- **C5 `.cmd` and `.bat` targets.** A resolved file ending `.cmd` or `.bat` runs as `"<ComSpec>" /d /v:off /s /c "<line>"`. Each word is written for cmd.exe: wrapped in quotes when empty or holding whitespace or any of `" & | < > ^ ( ) , ; =`, an embedded `"` becomes `""`, and backslash runs before it or at the end are doubled for the C-runtime program the shim hands `%*` to. `/v:off` keeps `!` a plain character.
  - Refusal: a word holding a CR or LF, or `%NAME%` with NAME set in the command's environment (the scan resumes at the closing `%`, so `%A%B%` with A unset and B set refuses), and the batch file's own path is checked too: exit 2, one `gate: a .cmd or .bat command cannot take a word holding a line break or %NAME% of a set variable, which cmd.exe would split or expand: <word>` line, before any slot is queued. A lone `%` or an unset name passes.
  - Pins: `Test-BatchArguments` (a batch file in a folder with a space in its name feeding `node`, which prints its argv; a refused call never runs the batch file). Gaps: dynamic names (`%CD%`, `%DATE%`, `%RANDOM%`, ...) and `%PATH:~0,3%` forms pass today (CC-151).
  - Where: G:1373-1381, G:2754-2830, G:1387-1398. **Essential** (exact rules are free; the box wrapper has its own copy at W:1380-1457, which the port should share).
- **C6 Program resolution.** A bare name resolves to the first file on PATH whose extension is in PATHEXT (skipping the extensionless bash shims a Claude Code plugin puts ahead of `uv` and `python`); a name with an extension or a directory is taken as written, a relative directory read against the caller's location. Where: G:1387-1398, G:2764-2770. **Essential**.
- **C7 Start failure is 127.** A `CreateProcessW` failure (not on PATH, absolute path missing) writes `gate: could not start <word> (<resolved>): <Windows message>` to stderr and the log, runs the verdict path, exit 127, and the slot is released so the next gate runs. Pins: `Test-StartFailure` (a program nowhere on PATH; an absolute path that does not exist). Where: G:2844-2855, G:3777-3788. **Essential**.
- **C8 `-StopTree <pid>`** is a separate mode (see 1.9 X6).
- **C9 Test-only environment variables are validated at start**, each refusal exit 2 with its own line: `GATE_SAMPLE_SECONDS` and `GATE_TEST_HEARTBEAT_SECONDS` (seconds above 0), `GATE_TEST_SLOT_PREFIX` (`^[A-Za-z0-9-]{1,32}$`), `GATE_TEST_POLL_MILLISECONDS` (50 to 2000), `GATE_TEST_REMOTE_CONFIG` (a file), `GATE_TEST_REMOTE_STUB` (a `.ps1` file), `GATE_TEST_REMOTE_WAIT_SECONDS` (whole number above 0).

  Pins: `Test-PollMilliseconds` (rows incl. 250 and unset). Where: G:1590-1630. **Open**: the seams are how the PowerShell suite drives timing; the Rust suite needs its own (section 7).

### 1.2 Slots, pools and counts

- **S1 Three pools of named mutexes.** `heavy` = `Local\gate-slot-<i>`, `light` = `Local\gate-light-slot-<i>`, remote = `Local\gate-remote-slot-<i>`; `-Slot critical` takes slots of both local pools. The pools never wait on each other, so a serial test run does not queue behind builds. `Local\` is the logon-session namespace, which is the gate's scope.

  `GATE_TEST_SLOT_PREFIX` goes in front of the name after `Local\`. Where: G:432-435, G:445, G:1734-1737, G:1921-1924. Pins: `Test-SlotHeavy`, `Test-SlotLight` (two gates of a kind on one slot run one after the other), `Test-SlotPool` (a gate of one kind runs while the other kind's only slot is held). **Essential**; the primitive (named mutex) is **open** (section 2, mutexes).
- **S2 Default counts.** `max(1, floor((logical processors - 1) / threads))` with threads 4 for heavy and 2 for light (G:1701). Counts come from `%LOCALAPPDATA%\gate\slot-counts.json`: an object with optional `heavy` and `light`, each a whole number 1 to 64 (`2.0` counts, a string or fraction does not).

  The file is re-read on every wait-loop pass, so a count changed while gates wait applies to them, and a gate holding a slot the new count leaves out keeps it. A missing file or key means the default. An unreadable, non-object or invalid file/value is ignored for that kind with one line `gate: ignored <path>: <reason>; using <n> <kind> slots`, said once per wait.
  - Pins: `Test-SlotCountLive` (an inner gate takes a slot the moment the file adds one), `Test-SlotCountInvalid`, `Test-FormatFixture` (the `slot-counts.json` fixture is read as is).
  - Where: G:1634-1715, G:1780-1794. **Essential**. **Open**: PowerShell's JSON reader is lenient (`{heavy: 4}`) where ops-tower's is strict (`OT/src/lib.rs:5`), and key names match case-insensitively (OT `property()`); pick one rule. delve-the-dungeon restates the formula in prose (`docs/DEVELOPMENT.md:279-281`, `docs/GLOSSARY.md:75-76`).
- **S3 Taking a slot is a non-blocking mutex try.** `Request-Slot` creates the named mutex and calls `WaitOne(0)`; failure disposes it. An abandoned mutex (its gate was killed holding it) counts as taken and says `gate: took <name>, which a killed gate had left held` on stderr and in the log.

  A gate keeps its mutex for its whole run and releases it in `finally`, after deleting its claim file and writing the journal `release`. Why mutexes: "a killed gate abandons its mutex, which the next gate then takes, where a semaphore's count would be lost for good." Where: G:1720-1763, G:2303-2320.
  - Pins: none for the abandoned-takeover path (gap; CC-205 wants it journaled). **Essential** (crash recovery); the mechanism is **open**.
- **S4 The wait loop.** Every pass (2 s, or `GATE_TEST_POLL_MILLISECONDS`): read counts, read live tickets, check the parent-lend case, compute the barrier, write/delete the pause marker, try each slot index the pass may try, then sleep.

  It prints `gate: waiting for a heavy slot (<n> busy)` once and `gate: still waiting for a heavy slot (<s> s)` every 30 s (`GATE_TEST_HEARTBEAT_SECONDS`), to stderr and the log, so a gate around it, whose stall window reads that output, does not kill it for waiting. Its watchdog clocks start only once it holds a slot.
  - Pins: `Test-WaitHeartbeat` (an inner gate's heartbeat lines show in the outer's `.err`), `Test-PollMillisecondsWait` (a waiter takes the slot within 1.0 s of the holder's journal `release` at a 250 ms poll). Where: G:1798-1812, G:1849-1878. **Essential** (the heartbeat is a contract with outer gates and with agents watching output); the 2 s polling is **open** (a kernel wait could replace it).
- **S5 Mutex "open" probing.** `Test-MutexOpen` uses `Mutex.TryOpenExisting`: a slot is "held" while any handle to its name exists. A holder keeps it for its run, but a gate that is merely trying creates and disposes a handle, so a probe can read a transient "held". ops-tower does the same with `OpenMutexW(SYNCHRONIZE)` (`OT/src/win32.rs:140-148`) to show a slot as held without a claim.

  Where: G:1977-1983, G:2153-2161. **Open**: ops-tower's "held with no claim file" state and the critical gate's `Measure-OtherHeld` depend on it.

### 1.3 Queueing, tickets and fairness

- **Q1 Arrival tickets.** A gate that must wait writes `%LOCALAPPDATA%\gate\<prefix>waiting\<pid>-<start ticks>.json` holding `kind`, `arrived` (UTC ticks), `pid`, `startTicks`, `holds` (true only for a critical gate that holds the machine), plus `remote: true` for a gate queued for the remote pool.

  It is written via `<file>.tmp` then moved over, deleted once the gate holds its slot or ends, and re-read by others. A ticket whose pid is gone or has another start time is ignored and deleted by the first reader. A gate that cannot write its ticket says `gate: could not write the arrival ticket <path> (<reason>); a critical request cannot see this gate wait` and waits anyway.
  - Pins: `Test-StaleTicket` (a dead critical ticket ahead of a light gate, a dead heavy ticket ahead of a critical gate: neither delays, and the reader deletes it), `Test-FormatFixture` (keys and JSON types against `fixtures/ticket.json`), `Test-FormatFixtureRemote` (the remote ticket).
  - Where: G:2630-2751. **Essential** (ops-tower's queue view and the critical drain read it).
- **Q2 Fairness.** Tickets order only the critical gate against everyone else: a critical request is a barrier for gates that arrive after it and waits for gates that arrived before it. Among gates of one kind there is no FIFO: whichever polls first after a slot frees takes it. ops-tower displays the queue by `arrived`, so its order can differ from the real take order. Where: G:1849-1878 (no ticket compare), G:2748-2751. **Open** (a real FIFO is cheap in the new design).
- **Q3 Barrier for later gates.** While a live critical ticket arrived before its own, a heavy gate tries no slot (`gate: waiting for a heavy slot behind the critical request of gate <pid>`); a light gate tries only the reserved light slots (K3). Where: G:1820-1837. Pins: `Test-CriticalBlocksLater`, `Test-CriticalBarrier`, `Test-CriticalLetsEarlierRun`. **Essential**.

### 1.4 Claims

- **L1 Claim files.** A gate holding a slot writes `%LOCALAPPDATA%\gate\slots\<mutex name without Local\>.json` with `kind`, `index`, `pid`, `processStart` (ISO "o"), `started`, `cwd`, `command` (each word written as C4 would, joined by spaces), `log` (as the caller gave it, possibly relative to `cwd`), `timeoutSeconds`; a remote slot's claim adds `remote: true`, `host`, `key`.

  Written via `<file>.<pid>.tmp` then moved over; deleted before the mutex is released so it never deletes the next holder's claim. A killed gate leaves its file; the next gate to take that slot overwrites it; readers treat a file whose pid is gone as stale.

  A write failure says `gate: could not write the slot claim <path> (<reason>); the Gates tab shows the slot without its command` and the gate runs on. A critical gate writes one claim, `gate-critical.json`, index 0; its individual slots have none.
  - Pins: `Test-SlotClaim` (names pid, slot and command; gone after the gate ends), `Test-FormatFixture` and `Test-FormatFixtureRemote` (keys and JSON types against `claim.json` and `remote-claim.json`; the claim's `log` must resolve to an absolute path when the gate was given a relative one).
  - Where: G:2265-2320. **Essential**.
- **L2 Live claim test.** A claim is live when its pid is a running process with the claim's `processStart` (gates compare exact ticks; ops-tower allows 2 s of drift, `OT/src/state.rs:18`). `Get-OtherClaim` reads `<prefix>gate-*.json` except the caller's own file and the remote family. Where: G:2487-2517. **Essential**.

### 1.5 The critical drain, reserved light slots and lending

- **K1 What critical asks for.** The whole machine but the reserved light slots, for a timing-critical run.

  It writes a ticket, then takes `Local\<prefix>gate-critical` once no live critical ticket arrived before its own (so two critical requests run one at a time in arrival order: `Test-CriticalPair`), waits until no earlier heavy ticket (and no earlier light ticket while some light slot is unreserved) is live, then takes every heavy slot and every unreserved light slot, re-reading the counts each pass so a raised count adds slots.

  Once it holds its first slot it lets go of nothing. Where: G:2108-2126, G:2081-2099, G:2185-2200.
  - Pins: `Test-CriticalWaitsForHolder`, `Test-CriticalRunsAlone`, `Test-CriticalBlocksLater`, `Test-CriticalLetsEarlierRun`, `Test-CriticalWaitsForEarlierLight`, `Test-CriticalDrainsUnreservedLight`, `Test-CriticalBarrier` (a heavy gate arriving during the run stays held back even when the counts file adds a heavy slot). **Essential**.
- **K2 Reserved light slots.** `min(2, light count)` of the highest light indices (every light slot when the count is 1 or 2); a critical gate never takes them, so up to two light gates run beside it. With every light slot reserved, the drain waits for earlier heavy gates only.

  Where: G:438, G:1741-1752, G:2145-2149. Pins: `Test-CriticalLightBeside`, `Test-CriticalKeepsLightRunning`, `Test-CriticalOneLightSlot`, `Test-CriticalIgnoresReservedOnlyLight`, `Test-CriticalWaitingFillsReserved`. **Essential**.
- **K3 Light gates beside a critical gate.** While it still waits for the machine any free reserved slot will do; once it holds the machine a light gate takes one only while fewer light gates than there are reserved slots hold light slots beside it, whatever their numbers (`Measure-LightBeside`: reserved slots whose mutex is open, plus unreserved claims and claims past the count), so a count raised during the run lets no more in.

  It says `gate: waiting for a light slot beside the critical request of gate <pid>` once and `still waiting ... (<s> s)` every heartbeat. Pins: `Test-CriticalThirdLightQueued`, `Test-CriticalCountRaisedKeepsTwo`, `Test-CriticalHeavyStillWaiting`. Where: G:1820-1837, G:1995-2020. **Essential**; the mutex-open probe it counts with is **open** (S5).
- **K4 Priority.** A critical gate's command and every gate under it start at High; its watchdog raises the build servers it sees to High, the ones in its own job included (Windows starts a High process's children at Normal), except servers in the job of another live gate holding a slot; ordinary gates leave a server already at High while a critical gate holds the machine and lower it again on a later pass once none does.

  Pins: `Test-CriticalPriority` (a critical command reads High, a heavy gate's after it BelowNormal), the live probe `tools/gate/prio-check.ps1` (arms holding, waiting, none, beside, own-job, relower; no case runs them). Where: G:2834-2839, G:3029-3191. **Essential**.
- **K5 The take line and its snapshot.** On taking the machine it says on stderr and in the log `gate: holds the machine (critical) at <time> after <s> s: counts heavy <H>, light <L>; other gates hold <h> heavy and <l> light slots (<r> light slot[s] reserved for light gates beside it); other live claims: <pid> <kind> <index> <command>, ... | none`, and journals the same as `counts`, `otherHeld`, `otherClaims` (or `snapshotError`).

  While waiting: `gate: waiting for the machine (critical): <h> heavy and <l> light slots held (<r> light slots reserved for light gates beside it), <n> earlier waiters`, then `still waiting ... (<s> s)` every heartbeat; that wait is never charged to its ceiling (the watchdog starts after). test-all.ps1:110 matches `gate: waiting for the machine \(critical\)` and `gate: holds the machine \(critical\)` on stderr.
  - Pins: `Test-CriticalTakeSnapshot`, `Test-CriticalSeesHighIndexClaim` (a live claim at a slot index past the counts is named). Where: G:2153-2180, G:2521-2558. **Essential** (the journal fields feed ops-tower's history and the line feeds test-all).
- **K6 A routed gate falls back in its first place.** Its ticket is rewritten without `remote` and with its first arrival, so a critical request that arrived after the routed gate's first arrival still waits for it. Critical drains ignore tickets marked `remote`, and a critical gate neither takes remote slots nor waits for their holders. Pins: `Test-CriticalWaitsForFallback`, `Test-CriticalSkipsRemote`. Where: G:2121, G:2178, G:4217-4228. **Essential**.
- **K7 Critical under a heavy or light marker is refused**: exit 2, `gate: -Slot critical cannot run under the <kind> slot of gate <pid>: it would wait for that slot for good; run the critical command outside that gate`. A critical marker covers every kind. Pins: `Test-CriticalUnderOrdinary`, `Test-CriticalNested`. Where: G:2232-2262, G:3918. **Essential**.

### 1.6 Nesting, parent slots and pause markers

- **N1 `GATE_SLOT_HELD=<kind>:<pid>:<start ticks>`** is set in the command's environment by a gate that holds a slot (`Get-SlotMarker`; critical's is `critical:<pid>:<ticks>`) and handed on by a gate it covers. A gate whose marker names a live gate (pid with that start time) whose kind covers its own takes no slot and says `gate: running under the <kind> slot of gate <pid>`: heavy covers heavy and light, light covers light, critical covers all.

  A malformed marker or one naming a dead gate is ignored with a line (`gate: ignored GATE_SLOT_HELD='<m>', which is not <kind>:<pid>:<start ticks>; taking a <kind> slot` or `... gate <pid> is no longer running ...`) and the gate takes a slot. A heavy gate under a light marker does not run on it: it waits for a heavy slot (`gate: the light slot of gate <pid> does not cover a heavy gate; taking a heavy slot`).
  - Pins: `Test-Nesting` (one light slot; an inner gate that took a slot would wait on its parent's), `Test-NestingCovered`, `Test-NestingLightOuter`, `Test-StaleMarker` (a marker of an ended holder, or a bare `1`, does not skip the only heavy slot). Journal: `covered` with `parentPid`.
  - Where: G:2220-2262, G:4000-4010. **Essential** (build hooks under a gated `git commit` rely on it).
- **N2 Lending while a critical request drains.**

  A gate under a parent's slot that does not cover it (a heavy gate under a light parent) takes no slot and writes no ticket while a critical request is pending (a live critical ticket, or `Local\gate-critical` open) and the critical gate still waits for the machine and the parent's slot is one it waits for; it runs on the parent's slot saying `gate: running under the <kind> slot of gate <pid> while a critical request drains; taking no slot`, journals `parent`, and hands the parent's marker on.

  A parent on a reserved light slot lends nothing; once the critical gate holds the machine no parent lends and the child waits behind the critical request. Pins: `Test-NestedUnderDrain`, `Test-CriticalReservedParentChildWaiting`, `Test-CriticalHoldingChildWaiting`, `Test-CriticalHoldingMissingClaimNoLend` (a deleted parent claim: no lend), `Test-RemoteFallbackToParent`. Where: G:2024-2055, G:1857-1860. **Essential**.
- **N3 Pause markers.** A gate under a parent's slot that waits behind a critical request rewrites an empty file `%LOCALAPPDATA%\gate\paused\<parent pid>-<parent start ticks>-<pid>-<start ticks>.pause` every wait pass and deletes it once it takes a slot or ends.

  While one names a live gate (pid and start ticks match) still holding its arrival ticket, written within 30 s, and a critical request is pending, the parent's watchdog stops its clocks: the seconds count neither toward the load-adjusted ceiling nor the backstop (the sampler running or failed), the stall window restarts after the wait, and the `machine free` share leaves them out.

  A marker of an ended gate, one whose gate holds its slot (ticket gone), one not rewritten for 30 s, one with no critical request pending and one naming an earlier process of the parent's pid are ignored. An unreadable pause folder pauses nothing and says once `gate: could not read the pause markers in <folder> (<reason>); a gate under this one pauses nothing`. The folder is not prefixed by `GATE_TEST_SLOT_PREFIX`.
  - Pins: `Test-CriticalChildWaitPausesParentCeiling`, `Test-CriticalChildWaitPausesParentBackstop` (sampler down), `Test-CriticalChildWaitPausesParentStall`, `Test-PauseMarkerNeedsParentTick`, `Test-PauseEndsOnChildHold`, `Test-PauseIgnoresStaleMarker`, `Test-PauseDirUnreadable` (icacls deny).
  - Where: G:1881-1918, G:3336-3390. **Essential** (the behaviour); the file protocol is **open** (a Rust parent could hear its children over a named event or pipe, but ops-tower never reads these files, so only gate-to-gate compatibility matters and a drained cutover removes it).

### 1.7 The watchdog: stall check, CPU sampling, load-adjusted clock, ceiling, backstop

- **W1 Sampler.** After the command starts, the gate waits on the process in `SampleMs` slices (`GATE_SAMPLE_SECONDS`, default 5 s, floor 100 ms), takes a sample each slice and tests for a kill.

  Each sample reads: system idle/kernel/user times, a process snapshot, the job's accounting (`TotalUserTime + TotalKernelTime` including nested jobs and exited processes, and `ActiveProcesses`), the job's process ids, log and `.err` sizes. Nothing goes through WMI ("a CIM query can block for seconds or for good"). Where: G:2929-2989, G:3285-3330, G:3648-3670. **Essential**.
- **W2 Stall.**

  `gate: STALLED: no output and no CPU for <n> s (machine free <p>% on average); killed <command> and its children` when for `-StallSeconds` none of these happened: the log or `<log>.err` grew; the job gained at least `StallSeconds * 100000` ticks of CPU since the last progress (1% of one core: job CPU is charged in 15.625 ms ticks and a hung `dotnet test` still gains one every few seconds); the wsl VM busy (below); a process new to the job appeared; a build server the command started gained CPU while the job holds a build client; a shared compiler server gained CPU while the job holds a build client; a gate under this one waits behind a critical request (N3).

  Where: G:3285-3330, G:3423-3444.

  Pins: `Test-Stall`, `Test-CpuTrickle` (about 1% of a core of trickle is still a stall), `Test-OutputProgress`, `Test-CpuProgress`, `Test-OrphanProgress` (a silent busy grandchild whose parent is gone), `Test-PackagedProgress`, `Test-MsysProgress`, `Test-NestedJob`, `Test-ShortLivedChild` (CPU only from children that exit between samples), `Test-OldServer`, `Test-UnrelatedServer`, `Test-SharedCompiler`, `Test-NoBuildClient` (`dotnet ... --no-build` is not a client). **Essential**.
- **W3 Load-adjusted clock.** Each sample adds `interval * free` to a clock that the ceiling `-TimeoutSeconds` runs on, `free = clamp((cores - others) / demand, 0.05, 1)`, `busy = 1 - idle/total` over the system times, `others = max(0, busy * cores - ownCores)`, `ownCores = ownGained / 1e7 / interval`; demand is every logical processor, or `min(cores, 2)` for a light gate; a High-priority command (critical, or under one) is not slowed by others so `free` is 1; the share never falls below 5%.

  The command's own CPU is the job tree's, plus build servers' it uses only while the job holds a build client (new servers: what they gained; a shared compiler server: only what it gained this sample). The last partial interval is added at the last sample's share when the command exits. `gate: TIMED OUT after <ceiling> s of load-adjusted time (wall <w> s, machine free <p>% on average); killed ...`.
  - Pins: `Test-FreeShare` (idle, half busy elsewhere, filling the machine itself, light gate with 8 and 15 cores busy elsewhere, the floor, High command on a full machine, own CPU past the busy share, continued CPU counts only both-sampled processes, zero for a still set), `Test-Ceiling` (reported as `skip` when the log says the machine was under 30% free).
  - Where: G:3263-3282, G:3373-3380, G:3664-3669. **Essential**; the 5% floor and `others` arithmetic are policy and **open** to retuning.
- **W4 Backstop.** `gate: BACKSTOP: <w> s of wall time (5 x <ceiling> s ceiling, load-adjusted <a> s, machine free <p>% on average)` when `wall - paused >= 5 * ceiling`, whatever the load. Kill precedence in one sample: STALLED, TIMED OUT, BACKSTOP. Where: G:3438-3443. Pins: `Test-SamplerFailure`. **Essential**.
- **W5 Sampler failure.** A sample that throws says once `gate: sampler failed: <message>; watching by wall time only` (log and stderr) and no more samples are taken; only the backstop (a clock) can kill; the passed line reads `load-adjusted unknown: the sampler failed`.

  The self-test forces it with `GATE_TEST_SAMPLER_FAIL=1`. Where: G:3395-3410. Pins: `Test-SamplerFailure` (two gates side by side: one lets a command finish, one kills a hung command at the backstop). **Essential**; the hook is a test seam.
- **W6 WSL.** For a command whose first word is `wsl` or `wsl.exe` (a path allowed), the VM's CPU is read from the `vmmemWSL` instance of the `Process\% Processor Time` counter (the first read primes the counter and is discarded): `core-seconds since last progress >= StallSeconds * 0.10` counts as progress (Docker keeps an idle VM at a 2.4% mean, 10.7% at most over a second).

  If the counter cannot be read the gate says once `gate: WSL VM CPU unavailable (<message>); retrying each sample, and until a read succeeds this wsl command counts only output and job CPU as progress` and retries each sample. WSL's relay processes escape the job (documented), so the VM CPU is the only signal.

  Pins: `Test-WslBusy` (skipped without WSL or when the VM is already busy), `Test-OutsideJob`. Where: G:2889-2927. **Essential** (ci-local runs `gate.ps1 ... -- wsl -d Ubuntu ...`, `tools/ci-local/README.md:35,199`); the counter API is **open**.
- **W7 Final sample of a command that exits first.** A command that exits on its own between the sample that found a kill reason and the kill, diagnostics time included, keeps its own status and its log gets no diagnostics (`Watch-Command` re-checks `HasExited` twice). Where: G:3656-3658. **Essential** (race semantics).

### 1.8 Build servers

- **B1 Classification.** A build server is `VBCSCompiler.exe`, or a `dotnet.exe` whose command line holds `VBCSCompiler.dll`, or `MSBuild.dll` with `nodemode`; a dotnet command line is read once per process (via `NtQueryInformationProcess(ProcessCommandLineInformation)`). A build client in the job is `MSBuild.exe`, or a `dotnet.exe` whose command line lacks `--no-build`.

  Servers outside the job count as the command's progress only while the job holds a build client (the job refuses breakaway, so an outside server was started by another process and a client in the job is the only sign the command uses it); old MSBuild nodes never count because the gate's builds start nodes of their own. Where: G:3014-3027, G:3126-3148, G:3169-3191. **Essential**.
- **B2 Priority of servers.** An ordinary gate lowers any build server it sees above BelowNormal on every pass (not once), unless the server is in the job of a critical gate holding the machine, or is at High while a critical gate holds it; one that cannot be changed is named once (`gate: could not lower <name> <pid>: <message>`) and does not fail the gate.

  A High gate raises servers (not in another live gate's job). Pins: the live probe only (prio-check.ps1); no case. Where: G:3029-3124, G:3153-3161. **Essential**.
- **B3 Environment for MSBuild.** The command runs with `MSBUILDDISABLENODEREUSE=1` and `DOTNET_CLI_USE_MSBUILD_SERVER=0` so its worker nodes run inside its job (CPU measured, priority inherited, die with a kill); the compiler server stays shared ("a build without it ran three to five times slower"). The caller's own values are untouched. Pins: `Test-BuildEnvironment` (in-session `&` call: environment and priority unchanged after). Where: G:457-461, G:2861-2887. **Essential**.

### 1.9 Kill, diagnostics, job objects and `-StopTree`

- **X1 The command's job.** The gate creates the command suspended, assigns it to a fresh job named `Local\gate-job-<gate pid>` with no limits (nothing may break away, and no kill-on-close), then resumes it, so nothing it starts can run outside the job.

  The desktop-app policy `PROC_THREAD_ATTRIBUTE_DESKTOP_APP_POLICY = PROCESS_CREATION_DESKTOP_APP_BREAKAWAY_DISABLE_PROCESS_TREE` keeps the children of a packaged app (the Store's pwsh) in the job. A nested gate's job nests inside its parent's, so the outer kill reaches the inner command.

  The job does not kill its processes when the gate ends: the compiler server a build starts was measured inside the job and a normal exit must leave it running. Refusing breakaway matters because the MSYS runtime (Git for Windows' sh) creates every child with `CREATE_BREAKAWAY_FROM_JOB` whenever the job allows it, which put a gated `git commit`'s hooks outside the job (measured).

  With the job set to refuse it, such a start lands inside a job nested in this one and fails when this job is the process's own (the CreateProcess documentation says both). A job name another job already holds is an error.
  - Pins: `Test-OrphanKill`, `Test-PackagedKill`, `Test-MsysKill`, `Test-NestedKill` (an outer kill reaches the sleeper through the inner gate's job), `Test-QuickExit` (a command that exits the instant it resumes keeps its status, ten runs), `Test-StopJob`.
  - Where: G:782-851, G:1017-1030, G:3773. **Essential** (the core mechanism).
- **X2 Kill sequence.** On a kill reason the gate gathers the diagnostics (below), then `TerminateJobObject(job, 124)`, adds `gate: terminated the job's <n> process[es]` (n from the job's accounting before the kill), waits up to 5 s for the command (`gate: the command was still running 5 s after its job was terminated` if not), and exits 124. Where: G:3633-3644. **Essential**.
- **X3 Diagnostics block.**

  Before the terminate line the log gets `gate: diagnostics`, then each line indented: `gate:   job <pid> parent <ppid> <name>: cpu <s> s, working set <mb> MB: <command line>` for each process in the job (command line cut at 200 characters plus `...`, `?` for a part that cannot be read), `gate:   outside job <pid> ...` for each live descendant of the command or of a job process (created at or after its parent and after the command started) that is not in the job, at most 40 job lines and 20 outside lines each followed by `gate:   ... and <n> more <label> processes`, `gate:   build server <pid> <name>: cpu <s> s (started after the command | already running; counts while the job holds a build client)` or `gate:   build servers: none`, and the last six samples as `gate:   sample at <w> s (load-adjusted <a> s, free <p>%): log <n> B, err <n> B, job cpu <s> s, [wsl vm cpu <p>%, ]<n> processes, new servers cpu <s> s, shared servers cpu <s> s, build client in job: yes|no` (or `gate:   samples: none`).

  Gathering has a 2 s budget (`gate:   ... diagnostics cut at <n> ms`), and a failure ends the block with `gate: diagnostics failed: <message>` and the kill goes ahead (`GATE_TEST_DIAGNOSTICS_FAIL=1`).
  - Pins: `Test-DiagnosticsFailure` (still killed, status 124, the log says why), `Test-DiagnosticsCap` (45 waiting processes: 40 listed, the rest counted), `Test-OutsideJob` (WSL relay named outside the job).
  - Where: G:3451-3629. **Essential** (the content is the debugging record); exact wording is free.
- **X4 A failure inside the gate after the command started** terminates the job, adds `gate: the gate failed while running the command: <message>` to the log, prints the lines to stderr and rethrows (pwsh exit 1). Where: G:3714-3733, G:3794-3797. **Essential** (no orphan command, no hidden error).
- **X5 Ctrl+C or a stopped gate** (`Stop-Job` on an in-session run) terminates the job in `finally` so the command does not outlive its gate; the command shares the gate's console so Ctrl+C reaches it directly. A hard kill (`taskkill /F`) runs no `finally` and leaves the job's processes running and the slot's mutex abandoned. Pins: `Test-StopJob`. Where: G:3799-3806. **Essential**.
- **X6 `-StopTree <pid>`** (the only word after it, a live pid) takes one process snapshot, builds the tree of descendants of `<pid>` (a child counts only if created at or after its parent, so a recycled pid adopts no one), terminates the job `Local\gate-job-<id>` of every process in the tree (the gate's own pid excepted from process stops), then terminates the tree's processes deepest first, giving a job member up to 2 s to end first, and only counts the processes it had to stop itself.

  Output on stdout: `gate: stopped <n> gate job(s) and <m> process(es) under <pid>`; each failure on stderr as `gate: could not stop: <reason>` with exit 1; a missing/invalid pid exits 2 with the usage. Why: a process whose parent has exited is invisible to `taskkill /T`, and only the gate's job holds it.

  Pins: `Test-StopTree` (a grandchild whose parent exited is reached through the job; launcher and the A process through the tree), `Test-LauncherStopTree`. Where: G:1096-1262, G:3868-3889, L:299-365. Consumers: `opening-hand/tools/test-all.ps1:171`, `in-the-sky/tools/test-all.ps1:175`. **Essential**.
- **X7 Other gates' jobs are listed by name** (`OpenJobObjectW(JobObjectQuery, "Local\gate-job-<pid>")` then the process id list) so a gate can tell which build servers belong to another live gate's job. Where: G:1073-1094, G:3053-3077. **Essential** (priority rules depend on it).

### 1.10 Command launching

- **P1 Process creation.** `CreateProcessW` with no application name, the quoted command line (C4/C5), the caller's current directory, `CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT` plus a priority class, `bInheritHandles = TRUE`, `STARTF_USESTDHANDLES`.

  The command inherits exactly three handles (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`): standard input from `NUL`, standard output to `-Log` (`CREATE_ALWAYS`, share read/write/delete so the gate reads size and tail while it writes), standard error to `<log>.err`. It shares the gate's console (no new window, no new process group), so Ctrl+C reaches it. Where: G:782-833. **Essential**.
- **P2 Priority.** BelowNormal for every command ("a gated command carries no `nice` of its own"), or the gate's own class when the gate already runs at BelowNormal or Idle; High for a critical gate and a gate under a critical marker. The class is a creation flag, so every descendant inherits it. Pins: `Test-Priority` (run from a Normal parent), `Test-CriticalPriority`. Where: G:2834-2839, G:3986-3990. **Essential**.
- **P3 Environment.** The command gets the gate's environment with `GATE_SLOT_HELD` (the held slot's marker, or the parent's for a covered gate), `MSBUILDDISABLENODEREUSE=1` and `DOTNET_CLI_USE_MSBUILD_SERVER=0` set over it (case-insensitive names); the gate sets them in its own process only for the duration of `CreateProcess` and restores the caller's values (a variable that was unset is removed again).

  Where: G:457-461, G:2782-2790, G:2861-2887. **Essential** (the values); the set-and-restore is **incidental** (pass an explicit environment block).
- **P4 Console and code page.** The local gate sets no console code page and attaches no console; the command writes raw bytes to the log. The gate prints the tail by decoding the log as UTF-8 (PowerShell 7 `Get-Content`) and re-encoding for the console. Where: G:3701. **Open** (what to do with non-UTF-8 output; the box wrapper does give its command a UTF-8 console, BW7).
- **P5 Working directory and relative `-Log`.** The command runs in the caller's working directory; a relative `-Log` is read against it (in-session callers that `Set-Location` have a PowerShell location that .NET's current directory does not follow, hence `GetUnresolvedProviderPathFromPSPath`). Pins: `Test-RelativeLog`. Where: G:3974-3983. The Set-Location handling is **incidental**; resolving once at start is **essential**.
- **P6 WSL command detection and `Spoken`.** `Spoken` is the command words joined by spaces for kill lines; `Wsl` is true when the first word's file name without `.exe` is `wsl`. Where: G:3766-3768. **Essential**.

### 1.11 Log files, failure markers, verdict and exit codes

- **V1 Log lifecycle.** `-Log`'s directory is created, an earlier log and `<log>.err` are removed at start; while running, stdout goes to the log and stderr to `<log>.err`; at the end `.err` is appended to the log and removed, then the gate's own lines (the notes it gathered: queue/take/wait lines, kill lines, diagnostics) follow, so one file holds the output, the errors after it, then the gate's lines.

  Where: G:3674-3683, G:3974-3983. Pins: `Test-LauncherFallback` ("streams": output and errors both in the log with the errors after, no `.err` left). **Essential**.
- **V2 Failure markers.** Regex `^Build FAILED\.|error CS\d+|: error |Test run summary: Failed!|^\s*failed: [1-9]|gate: (TIMED OUT|STALLED|BACKSTOP)` (the last alternative so a gate wrapping a gate reports the inner kill). A zero exit whose log matches becomes status 1 with `gate: command exited 0 but its log reports a failure -> <log>` on stderr; on any non-zero status up to 40 matching lines are printed to stdout as `<line number>:<line>`.

  A killed run is not scanned. `-NoMarkers` makes the command's status the verdict. The launcher fallback keeps a copy of the regex, held equal to the gate's by a parse-tree comparison in the self-test. Pins: `Test-Status` (failing status; a marker with exit 0; the same marker with `-NoMarkers`), `Test-LauncherMarkers`. Where: G:423, G:3686-3709, L:44. **Essential** (the regex is a contract with Godot/dotnet output; tune freely).
- **V3 Verdict output.** The last `-Tail` lines of the log go to stdout, then `gate: passed in <w> s (load-adjusted <a> s, ceiling <n> s). Full output: <log>` on stdout, or `gate: FAILED (status <n>). Full output: <log>` on stderr.

  A routed run's passed line is `gate: passed in <w> s on <host> (wall clock, ceiling <n> s).`; a sampler-failed one says `load-adjusted unknown: the sampler failed`. Pins: `Test-LauncherFallback` (`-Tail 2` on five lines: the last two and the verdict), `Test-Status`. Where: G:3736-3756. Consumers read the `Full output:` path and the tail (agents). **Essential**.
- **V4 Exit codes.** The command's own status; 1 for a zero exit whose log holds a failure marker (never with `-NoMarkers`); 124 for any watchdog kill (the status coreutils `timeout` uses, so no caller reads it as the command's own); 127 when the command cannot start; 2 for usage or a non-Windows machine (`gate: this gate runs on Windows only`); 1 for a gate-internal failure. Pins: `Test-Status`, `Test-StartFailure`, `Test-Usage`. Where: G:413-416, G:3736-3756, G:3787. **Essential**.

### 1.12 The journal, rollover and archive

- **J1 Events.** Every gate appends one JSON line per event to `%LOCALAPPDATA%\gate\<prefix>events.jsonl` (UTF-8 no BOM, LF) under the named mutex `Local\<prefix>gate-journal`, waiting at most 2 s (an abandoned journal mutex counts as owned): `queue`, `take`, `release`, `abandon`, `covered`, `parent`, `fallback`, each line starting `time`, `event`, `pid`, `processStart`, `kind`, `cwd`, `command`, then the event's fields (README:55-71 is the table).

  A routed gate adds `remote`, `host`, `key` to its `queue`, `take`, `release`, `abandon` and `fallback` lines until its `fallback`; the sequences (queue, take, release; queue, parent; fallback orders) are an interface ops-tower's `JournalCache` reads as runs keyed by (pid, processStart).
  - `release` is written before the slots are let go, so no later `take` precedes it in the journal; `fallback` is written while the routed gate still holds its remote slot and ends that take; a `take` with no `release` or `fallback` after it is a gate killed outright.
  - Pins: `Test-JournalLifecycle` (queue, take, release; the log's `queued`/`took` lines), `Test-JournalConcurrent` (six light gates on two slots: every line whole), `Test-JournalPidReuse` (runs told apart by processStart), `Test-FormatFixture`, `Test-FormatFixtureRemote` (shape against `remote-journal.jsonl`), `Test-CriticalTakeSnapshot`, `Test-RemotePoolFullFallsBack`, `Test-RemoteRefusalFallsBack`, `Test-RemoteFallbackToParent`.
  - Where: G:2338-2470, G:2578-2627. **Essential**.
- **J2 Rollover.** Before an append, a journal of 5 MB or more (`GATE_TEST_JOURNAL_BYTES`) is moved over `events.1.jsonl` (replacing it) and a new one begun, under the journal mutex. A move that fails (a reader holds the journal without delete sharing, as ops-tower may) leaves the journal where it is, appends to it, and is said once: `gate: could not write the journal <path> (could not move it over <path>.1: <message>; appended to it as it is)`.

  Pins: `Test-JournalRollover` (1500-byte cap: every gate's lines are in the two files together), `Test-JournalRolloverBlocked` (200-byte cap with a no-delete-share reader). Where: G:2381-2393. **Essential** (ops-tower detects the roll by `events.1.jsonl`'s length and write time and `events.jsonl`'s creation time, `OT/src/journal.rs:244-269`).
- **J3 Torn line.** A last line a gate killed mid-append cut short gets a line feed before the next line (the last byte is read with share `ReadWrite|Delete`). Pins: `Test-JournalTornLine`. Where: G:2397-2411. **Essential**.
- **J4 Journal failure is non-fatal.** A gate that cannot write a line (mutex not free in 2 s, a folder where the file should be) says once on stderr `gate: could not write the journal <path> (<reason>)`, skips only that line and runs on; a journal throw at the end is said the same way so the ticket and slots are still released. Pins: `Test-JournalUnwritable` (the command still runs, exit 0, one line though it tried at queue, take and release). Where: G:2444-2470, G:3941-3956. **Essential**.
- **J5 Release facts and archive [durations].**

  `release` gains `wallSeconds` (command wall time less the seconds paused behind a critical request, as the backstop counts it; a routed run's is the whole remote wall time), `adjustedSeconds` (null for a routed run and after a sampler failure) and `verdict` (`passed`, `failed`, `log-failure`, `stalled`, `timed-out`, `backstop`, `unknown` when the gate ended before the command had one, its two times then null); a routed run is judged a kill only when the gate killed it (a command that exits 124 itself after an inner `TIMED OUT` is `failed`).

  The gate whose append moved the journal writes `events.1.jsonl`'s `release` lines, gzipped, to `%LOCALAPPDATA%\gate\<prefix>archive\events-<yyyyMMddTHHmmssZ>.jsonl.gz` after releasing its slot (`Complete-JournalArchive`, in `Exit-Gate`), skipping and counting lines that do not parse (`gate: archived the release lines of <path>; skipped <n> line(s) that do not parse`), then deletes archives older than 180 days and stray temporary files older than a day; a gate killed after its rolling append never archives.

  Pins: `Test-JournalVerdict`, `Test-RemoteVerdict`, `Test-JournalArchive`, `Test-JournalArchiveRetention`. Where: [durations] G:`Get-RunVerdict`, `Get-RemoteVerdict`, `Save-ReleaseArchive`, `Complete-JournalArchive`, `Write-EndEvent`. **Essential** (feeds [planned] durations).

### 1.13 The launcher and its fallback

- **H1 Launcher.** A repo carries `tools/gate.ps1` = `gate-launcher.ps1`, a no-param-block script that reads the same raw words and, when `$CLAUDE_CONFIG_DIR\tools\gate\gate.ps1` (else `$HOME\.claude\...`) exists, runs it in the same process (`& $gatePath @words; exit $LASTEXITCODE`), so `-StopTree` and the status are the gate's own.

  It changes only when its fallback or the gate's options change; `tools/sync-launchers.ps1` copies it into 15 repos (`$gateRepos`, sync-launchers.ps1:34), removes an old `tools/gate.selftest.ps1` copy, and `-Check` reports drift. Pins: `Test-LauncherForwards`, `Test-LauncherColonSwitch`. Where: L:384-389, sync-launchers.ps1:36-37. **Essential** (callers keep saying `pwsh tools/gate.ps1 ...` until the sweep).
- **H2 Fallback when the gate is absent** (someone else's clone). One line on stderr first, `gate: <path> not found; running without slots or watchdog`, then it runs the command in the launcher's own process at BelowNormal (the launcher process's class is lowered and restored), keeps the log (`>`), `.err` appended then removed, the tail, the failure markers and the exit status (`& $Command @Rest > $Log 2> $ErrLog`, status 1 for a command that threw), and reads `-Local` and ignores it.

  It requires only `-Log`, the command and a numeric `-Tail`; `-TimeoutSeconds` and `-Slot` are read but not checked. There are no slots, no stall kill, no ceiling, no build-server handling. `-StopTree` falls back to one CIM `Win32_Process` snapshot walked deepest-first and `Stop-Process`, printing `gate: stopped 0 gate job(s) and <n> process(es) under <pid>`.
  - Pins: `Test-LauncherFallback` sub-checks Streams (exit 5), Marked/Unmarked (`Build FAILED.` with exit 0; `-NoMarkers`), Priority (BelowNormal), Tail, Usage (no `-Log`: exit 2 before anything runs), Inline (in-session call), Script (a caller's own `.ps1` exiting 4), Local (`-Local` ignored, exit 6); `Test-LauncherStopTree`; `Test-LauncherMarkers`.
  - Where: L:275-297, L:347-365. **Open** (the port decides whether the launcher stays PowerShell as the zero-install fallback; the plan keeps the launcher until the sweep, and other people's clones have no gate.exe).

### 1.14 Switches and test seams

- **F1 `-Local`** keeps a heavy gate on this PC even when `remote.json` routes the command (for a step whose outputs a later local step needs): it never reads the routing file, says `gate: not routed: -Local` once (stderr and log) and journals `notRouted: "-Local"` on its `queue` line; it changes nothing for a light or critical gate.

  Pins: `Test-RemoteLocalSwitch` (the stub is never called; a malformed routing file is never read; the same command without `-Local` goes to the stub), `Test-LauncherFallback`. Where: G:4141-4164. **Essential**.
- **F2 `-NoMarkers`** (V2), **`-Tail`** (V3), **`-StallSeconds`** (W2; a routed run raises a value under 15 to 15 with `gate: -StallSeconds <n> raised to 15 for a remote run (heartbeats come every 5 s)`, R:1447-1452). Callers pass `-Tail 15` (godot-mcp `run.ps1:365,882`) and `-NoMarkers` (godot-mcp `run.ps1:1388`). **Essential**.
- **F3 `-Expect` and `-TimeoutSeconds auto` [planned].** See D5.
- **F4 The seams** (all `GATE_TEST_*` plus `GATE_SAMPLE_SECONDS`, `GATE_TEST_SAMPLER_FAIL`, `GATE_TEST_DIAGNOSTICS_FAIL`, `GATE_TEST_NATIVE_CACHE`, `GATE_TEST_JOURNAL_BYTES`, `GATE_TEST_SLOT_COUNTS`, `GATE_TEST_REMOTE_*`, `GATE_TEST_HEARTBEAT_SECONDS`, `GATE_TEST_POLL_MILLISECONDS`, `GATE_TEST_SLOT_PREFIX`, and the box wrapper's `GATE_TEST_TOOL_RESTORE`). **Open** (section 7).

### 1.15 Routing to the build box (gate side)

- **T1 Routing file.** `%LOCALAPPDATA%\gate\remote.json` (`GATE_TEST_REMOTE_CONFIG`): `host` (required non-empty string, an ssh destination), `slots` (optional whole number 1 to 16, default 1; the pool's size lives here and never in `slot-counts.json` because ops-tower's writer of that file drops keys it does not know), `repos` (required object; each value an array of non-empty wildcard patterns; an empty array never matches; a pattern `-like` cannot use makes the file unusable).

  A missing file routes nothing and writes nothing; an unusable file is ignored as a whole with `gate: ignored <path>: <reason>; running locally` on stderr and in the log. Read afresh each time. [outputs] adds optional `outputs` (D1).
  - Pins: `Test-RouteConfigMissing`, `Test-RouteConfigMalformed`, `Test-RouteConfigDefaults`, `Test-RouteMatch`, `Test-RouteRefusals`, `Test-FormatFixtureRemote` (the `remote.json` fixture is read as is).
  - Where: R:369-471. **Essential**; the format is free (**open**: JSON `-like` globs vs another matcher, section 7).
- **T2 What routes.**

  A command routes only when: the config is present, the gate is heavy and not `-Local`, no parent slot covers or lends to it, `GIT_INDEX_FILE` is unset (a gated `git commit`'s hooks never route), the first non-empty word's file name without extension is not `git` or `prek` (`git.exe`, a full path to git also stay local), the repo name is a key of `repos` case-insensitively (the main checkout's folder name with leading dots dropped and anything outside letters, digits, `-` and `_` turned into `-`: `~/.claude` is `claude`), the gate sits in a git worktree, and the words with inside paths rewritten (T3), joined by single spaces, match a pattern with PowerShell `-like` (case-insensitive, `[ab]` a class, first match wins).

  A command a pattern matches still runs here, saying why, when a word names a path outside the worktree (`gate: not routed: <word> is outside the worktree`) or the gate's folder holds nothing the snapshot sends (`gate: not routed: <subfolder> holds nothing the snapshot sends`, from `git ls-files --cached --others --exclude-standard -- <prefix>` read to its first line). When the decision throws (a git call, the routing file's read) the gate says `gate: not routed: <reason>` and runs here.
  - Pins: `Test-RemoteRoutedPass` (stub run, passed line names the host, remote claim gone at the end, journal queue/take/release with remote fields), `Test-RemoteRoutedFailure` (a failing status passes through), `Test-RemoteUnlistedStaysLocal`, `Test-RemoteKindStaysLocal` (light and critical), `Test-RemoteCoveredStaysLocal`, `Test-RemoteRouteErrorRunsLocal`, `Test-RemoteOutsideStaysLocal`, `Test-RouteRefusals`.
  - Where: G:4057-4164, R:476-482, R:621-632. **Essential**.
- **T3 Path rewrite.** Strings only, no file or git read: a path is a drive letter, a colon and a separator not preceded by a letter, digit, `_` or `-`, anywhere in a word (`--flag=X:\a`, `-p:Name=X:\a`, `"X:\a;X:\b"`), running to the next character that is no part of a path name (`, ; " ' ) | & < >` or whitespace).

  A path inside the worktree loses the worktree root (and the separators after it at the root) and gains one `..` per level from a folder below the root (written with `\` when the path held one); a path naming the gate's folder becomes `.`.

  Compare case-insensitively, `\` as `/`, on a separator boundary (`X:/a/wt` is not inside `X:/a/w`). Outside: a `..` that climbs out, a UNC path, a drive root, a subst or junction spelling of the worktree.

  A word with no drive path counts only when it starts with `..` or holds a separator and then only when its `..` climbs above the root; the text after the last `=` and after a leading switch prefix is read the same way; a word starting with a separator and no drive (`/p:`, `/c`) is a switch. The first word has no exemption.
  - Pins: `Test-RoutePaths` (a table of Prefix, Words, Expect or Outside), `Test-RemoteWordsRewritten` (from the root, and from `sub` and `tests/Caf<e-acute>`, each holding a tracked file). Where: R:486-616. **Essential**; CC-165 lists five known edge defects (section 6).
- **T4 `--no-build` is dropped** from a routed command and from its local fallback (every word exactly `--no-build`; `--no-build=x` stays), saying `gate: dropped --no-build: the last build may have run on the other machine`; the claim and journal keep the original command. Pins: `Test-RemoteNoBuildDropped`. Where: G:4090-4111. **Essential**; CC-165 says it should apply to dotnet only.
- **T5 The routed gate's life.** (1) Queue: ticket with `remote: true`, journal `queue` with remote fields, `gate: queued for a remote slot on <host> at <time>`. (2) Wait for a slot of `Local\gate-remote-slot-<i>` for 60 s (`GATE_TEST_REMOTE_WAIT_SECONDS`), polling like a local wait (`gate: waiting for a remote slot on <host> (<n> busy)`, `still waiting ... (<s> s)`); a critical request does not hold it back.

  (3) Take: journal `take`, claim `gate-remote-slot-<i>.json` with remote fields, `gate: took remote slot <i> on <host> at <time> after <s> s`, no `GATE_SLOT_HELD`. (4) Run through the remote client with the rewritten words and subfolder; every status is the gate's own, 124 and 255 included, except a 75 with a reason.

  (5) Fallback on `full` (pool still full after 60 s) or a refused run: journal `fallback` with the reason first, while it holds the remote slot, then let go of the slot and claim, rewrite the ticket (no `remote`, first arrival), say `gate: remote <host> <reason>; running here`, wait for a local heavy slot, run the command here (status and verdict are the gate's).

  The log then holds the local run's output, then the remote attempt's lines, the fallback line and the local wait's lines (carried as notes, since the local run's start replaces the log); a gate stopped while waiting for the local slot writes those lines as its log (`Write-PendingNote`).
  - Reasons: `full`, `busy`, `unreachable`, `parity`, `rebooted`, `tools`.
  - Pins: `Test-RemotePoolFullFallsBack`, `Test-RemoteRefusalFallsBack` (unreachable, busy, parity, rebooted, tools in turn; in the busy row a second routed gate queued for the one remote slot takes it only after the fallback is journaled), `Test-RemoteFailureNoFallback` (a stub returning 3 with reason busy: exit 3; 75 with no reason: exit 75), `Test-RemoteWaitKillLeavesNothing` (Ctrl+C-style stop while waiting for the pool or for the local slot after fallback leaves no ticket and no claim), `Test-RemoteFallbackToParent`, `Test-CriticalWaitsForFallback`, `Test-CriticalSkipsRemote`.
  - Where: G:3815-3861, G:4172-4228, G:1944-1960. **Essential**.

### 1.16 The remote client (`remote.ps1`)

- **RC1 Snapshot commit.** The worktree (HEAD plus uncommitted and untracked, not ignored, files) is written as a parentless commit through a copy of the index (`GIT_INDEX_FILE=<git dir>/gate-remote-<pid>.index`, `git add -A`, `git write-tree`, `git commit-tree` under the identity `gate-remote <gate-remote@localhost>`), so the real index is never touched; with no index yet the copy starts empty.

  Why parentless: yaat's pack is 1.78 GiB. The local ref `refs/gate-remote/<key>` moves only after the push succeeded. Pins: `Test-SnapshotIncludesWorktree`, `Test-SnapshotParentless`, `Test-RefMovesAfterPush`, `Test-SnapshotUnbornHead`, `Test-SnapshotLeavesIndex`. Where: R:341-357, R:653-664. **Essential** (the transport is free to change; "send the worktree as it stands, never touch the index" is the requirement).
- **RC2 Bridge commit.** Each push also sends a commit of the new tree whose parent is the previous snapshot to `refs/gate-bridge/<key>` (git marks only a pushed commit's parents uninteresting; a 2.00 MiB re-send of an unchanged 2 MB file became 278 bytes); with no snapshot of its own it bridges from the newest of any worktree of the repo, never a `-back` ref; no bridge when the bare repo was just created.

  Pins: `Test-BridgeOnlyWithPrevious` (incl. a second worktree), `Test-RemoteIncrementalPush` (second push under a tenth of 2 MB), `Test-RemoteSecondWorktreeIncremental`, `Test-RemoteFetchCopiesNothing`, `Test-RemoteAfterGc`. Where: R:636-673. **Open** (depends on keeping git as the transport).
- **RC3 Keys and layout.** Key = worktree folder name cut to letters/digits/`-`/`_` (leading dots trimmed; `repo` if empty) + `-` + first 8 hex of SHA-256 of the lower-cased root; the bare repo is `D:\gate-remote\<repo>.git` (repo = the main checkout's folder name, shared by every worktree), work folder `D:\gate-remote\work\<key>` with `<key>.lock`, `<key>.index` (and `<key>.tools`), wrapper `D:\gate-remote\bin\gate-remote-<first 8 hex of its SHA-256>.ps1`.

  The URL is `<host>:D:/gate-remote/<repo>.git`, pushed with `--receive-pack`/`--upload-pack` `pwsh -NoProfile -NonInteractive -Command git-receive-pack|git-upload-pack` (cmd keeps git's single-quoted path as literal quotes). Where: R:325-337, R:718-727, R:1339-1380. **Open** (the box protocol is free to change).
- **RC4 Calls to the box.**

  Every call is `ssh -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=4 <host> "if exist <wrapper> (pwsh -NoProfile -NonInteractive -File <wrapper> <verb> <base64 UTF-8 JSON>) else (exit 99)"`; the line may not exceed 8191 less 64 = 8127 characters (a longer one fails before anything is pushed: `gate: the remote run failed: the run call is N characters, longer than the 8127 cmd takes ...; shorten the command`); on 99 the wrapper's bytes are uploaded on the stdin of an `ssh ... pwsh -EncodedCommand` script (temp file, SHA-256 prefix check, move into place, always remove the temp) and the call is repeated.

  Pins: `Test-CommandTooLong`, `Test-RemotePassFail` (live). Where: R:94-103, R:704-714, R:772-806. **Open** (a native wrapper binary changes all of this; cmd is the sshd default shell).
- **RC5 Preflight and health refusals.**

  The preflight creates the bare repo and work folder when missing and prints `gate: remote boot <UTC "o">`, `gate: remote sdks <v1,v2,...|none>`; before the push the snapshot's `global.json` is resolved against the box's SDKs by dotnet's `rollForward` table (nine policies, `allowPrerelease`, x.y.znn version shapes with band and patch) and a failure is 75 `parity`: `gate: remote toolchain mismatch: <repo> needs .NET SDK <version> (rollForward <policy>); <alias> has <versions>`; with no readable pin and a tree holding `global.json`, `*.sln`, `*.slnx` or `*.csproj`, the box needs the major of this PC's newest SDK (`a .NET <major> SDK`; `gate: global.json has no readable sdk pin; checking the SDK major only`).

  Pins: `Test-SdkRollForward`, `Test-SdkPinFromSnapshot`, `Test-ParityNoPinScope`, `Test-RemoteParityRefused` (live). Where: R:1065-1287. **Essential**.
- **RC6 Watchdog of a remote run.** One-second checks: `gate: STALLED: no output or remote CPU for <n> s` when the log took no line for `-StallSeconds` (heartbeat lines count as progress), `gate: TIMED OUT after <n> s` when this PC's wall clock passes the ceiling (never load-adjusted, heartbeats included); the kill stops the local ssh (`Process.Kill(true)`) and sends `stop` (30 s limit, `gate: stopping the command on <host> did not finish in 30 s`).

  The heartbeat is exactly `gate-remote-heartbeat: <digits>` on stderr; the same text on stdout or with anything after the number is an ordinary log line.

  Pins: `Test-SharedHeartbeatPrefix` (both scripts name one prefix), `Test-HeartbeatIsProgress`, `Test-HeartbeatLineExact`, `Test-RemoteTimeoutIsWallClock` (a clock past the ceiling kills, one under does not, heartbeats every 200 ms or not), `Test-RemoteStallFloor`, `Test-RemoteStallKill` (live), `Test-RemoteHeartbeatKeepsBusyRun` (live). Where: R:808-831, R:1447-1452, R:130-258. **Essential**.
- **RC7 Lost connection and reboot detection.** ssh exit 255 during a run: `gate: lost the connection to <host>`, then (when the preflight printed a boot time) one `boot` call (30 s; `gate: asking <alias> for its boot time did not finish in 30 s`); two boot times differ only when more than 30 s apart (LastBootUpTime is derived from uptime, so a clock correction moves it a little).

  Booted later: 75 `rebooted`, no stop, no sync-back (`gate: <alias> restarted during the run (booted <time>); the run did not finish there`); box unreachable: `gate: <alias> unreachable after the connection was lost; the run did not finish there`, still sends `stop`, 75 `unreachable`; otherwise stop is sent and the status stays 255. Pins: `Test-RebootStatus`, `Test-RemoteRefusedSkipsSync`, `Test-RemoteBootLine`, `Test-RemoteBootVerb` (live). Where: R:870-923, R:973-990. **Essential**.
- **RC8 Refusal reasons for a 75.** `unreachable` (preflight ssh failed with 255, a failed push, a box unreachable after a lost connection), `parity`, `rebooted`, and from the run call's output only: `busy` (`gate: remote work folder <key> is busy`) and `tools` (`gate: remote work folder <key> could not restore its dotnet tools`); a command that exits 75 itself has no reason and its 75 is synced back as any status.

  Pins: `Test-RemoteCommand75NotBusy`, `Test-RemoteToolsReason`, `Test-RemoteRefusedSkipsSync`. Where: R:1384-1410, R:1425-1443. **Essential** (the reason must come from a channel a command cannot forge: the exact-line match in the run's output is a weak version of that; **open**).
- **RC9 Sync-back.**

  After the run, `refs/gate-back/<key>` is fetched (`ls-remote` then `fetch -q`, with `--upload-pack` as above); a back commit not made from this run's snapshot (its single parent must equal the snapshot) is ignored (`gate: ignored stale remote changes <sha8>: not made from snapshot <sha8>`); when the worktree's tree still equals the snapshot's, `git diff --binary` is applied with `git apply --whitespace=nowarn` (`gate: synced back <n> file(s): <names>`); otherwise nothing is applied, the changes are kept at `refs/gate-remote/<key>-back` and a passing run becomes exit 1 (`gate: remote changes not applied: the worktree changed during the run; see git diff <snap8> <back8>`).

  Pins: `Test-SyncBackApplies`, `Test-SyncBackRefusesChangedWorktree`, `Test-StaleBackIgnored`, `Test-RemoteSyncBack` (live), `Test-RemoteLineEndings` (LF in, LF back). Where: R:994-1039. **Essential** (the refusal's breadth is **open**, CC-204).
- **RC10 Break handling.** A Ctrl+C/stop/exception in the phase kills the local ssh/git process and, once the run call had started, sends `stop`; a run that stopped early returns status 1. Pins: `Test-RemoteExceptionStopsRun`, `Test-RemoteCommandNoVerdict` (the gate-facing call writes no verdict, no tail), `Test-RemoteLogTypeLazy` (dot-sourcing compiles nothing). Where: R:1459-1509. **Essential**.
- **RC11 Stand-alone CLI.** `remote.ps1 -HostAlias <alias> -Log <path> -TimeoutSeconds <n> [-StallSeconds 120] [-Tail 20] -- <command...>` run from the worktree root sends the folder it runs in as the subfolder and rewrites no words; it writes its own verdict (the same markers/tail/passed-or-FAILED lines). Pins: `Test-RemotePassFail`, `Test-RemoteDashWords` (live). Where: R:1512-1564. **Incidental** once gate.exe routes in-process (the plan's bridge keeps this CLI until the remote port).

### 1.17 The box wrapper (`remote-wrapper.ps1`)

- **BW1 Verbs.** `preflight`, `run`, `stop`, `boot` (and `outputs` [outputs]) with a base64 UTF-8 JSON payload (`Key`, `Bare`, `Work`, `Lock`, `Index`, `Job`; for run also `Command`, `Subfolder`, `Identity`), dates read as text (`Test-WrapperPayloadDates`).

  No verb: `usage: remote-wrapper.ps1 preflight|run|stop|boot <base64 JSON payload>` and exit 2. Dot-sourced it defines functions and runs nothing. Where: W:1775-1802. **Open** (the protocol is free; a native `gate-box.exe` could take JSON on stdin).
- **BW2 Preflight.**

  Creates the bare repo (`gate: remote created <bare>`) and work folder when missing, adds `origin` even when `.git` exists (so a half-finished preflight is repaired), sets `core.autocrlf false` and `core.eol lf` in the work folder's own config, writes `.git/objects/info/alternates` (forward slashes, LF) pointing at the bare repo's objects (otherwise every fetch copied the whole tree), prints the boot and SDK lines (a value it cannot read prints no line and `gate: remote could not read the boot time: <message>` / `... the SDK list: ...` on stderr; the check is skipped), then touches its own file's last-write time and removes other `gate-remote-<8 hex>.ps1` copies last written more than 7 days ago (failures reported on stderr, never fatal).

  Pins: `Test-WrapperPreflightRepairs`, `Test-WrapperPrunesOldCopies`, `Test-WrapperPruneUnlistedFolder`, `Test-RemoteBootLine`, `Test-RemoteLineEndings`. Where: W:1319-1345, W:1714-1773. **Essential** (the effects; the 7-day prune is a PowerShell-era artefact of content-addressed copies: **open**).
- **BW3 Work lock.** `run` opens `work\<key>.lock` with `FileShare.None` (exclusive) and holds it for the wrapper's life; ERROR_SHARING_VIOLATION (32) or ERROR_LOCK_VIOLATION (33) is busy: `gate: remote work folder <key> is busy`, exit 75. .NET opens files non-inheritable and the command inherits only its handles, so no process a command leaves running can keep the folder busy.

  Pins: `Test-WrapperLockBusy`, `Test-RemoteBusy` (live; flaky, CC-177), `Test-RemoteLeftoverChildNotBusy` (live). Where: W:1350-1362. **Essential**.
- **BW4 Checkout.** `git fetch -q origin refs/gate/<key>`, `git checkout -q --force --detach FETCH_HEAD`, `git clean -fdq` (ignored files, i.e. `bin/` and `obj/`, survive so builds stay incremental), and the previous run's `refs/gate-back/<key>` is deleted.

  A Subfolder that is rooted or climbs out of the work folder is refused before the checkout (exit 2, `gate: remote subfolder <sub> is not a folder inside the work folder`, back ref deleted); one the snapshot does not hold as a folder (git keeps no empty folder) after it (`gate: remote subfolder <sub> does not exist in the snapshot`, exit 2). Pins: `Test-WrapperSubfolder` (all three), `Test-RunPayloadSubfolder`. Where: W:1461-1474, W:1619-1629, W:1653-1695. **Essential**.
- **BW5 Dotnet local tools.**

  Before the command, `dotnet tool restore` runs in the Subfolder when the manifests dotnet would read for it (that folder and its parents up to the work root; each folder's `.config/dotnet-tools.json` else `dotnet-tools.json`) changed since the last restore, recorded as one SHA-256 over their relative paths and bytes (separated by NULs) in `work\<key>.tools`; a failed restore exits 75 with `gate: remote tool restore failed (status N)` and `gate: remote work folder <key> could not restore its dotnet tools`, the command never run.

  Known limit: after the box's NuGet cache is cleared, `work\*.tools` must be deleted by hand. Pins: `Test-WrapperToolRestore`, `Test-WrapperToolRestoreWalk` (a manifest only in a subfolder; moving a manifest's bytes to another path restores again). Where: W:1510-1596, call site W:1724. **Essential**; **open** whether the port keeps dotnet-specific knowledge in the gate.
- **BW6 Running the command.** In the Subfolder (PowerShell location and process current directory both, restored after), with a `.cmd`/`.bat` refused (exit 2) for a word holding a line break or `%NAME%` of a variable set on the box; the first word resolved on PATH with PATHEXT; the line quoted as C4/C5.

  The command runs in a named job `Local\gate-remote-<key>-<nonce>` (the nonce 12 new hex characters per run, so a late stop never reaches the next run of the worktree; an existing name is refused), created suspended with `CREATE_BREAKAWAY_FROM_JOB` so it leaves the ssh session's kill-on-close job, assigned to the box job (no kill-on-close, so build servers stay warm), then resumed.

  It inherits four handles: `NUL` (stdin), two pipe write ends (stdout and stderr) and the job's own handle (the name lives only while a handle is open; the wrapper dies with the ssh session when the watchdog cuts it, and `stop` has to open the job by name afterwards).

  Pins: `Test-WrapperCommandLine` (a `.cmd` in a folder with a space), `Test-WrapperBatchRefusal`, `Test-RemoteBatchMetacharacters` (live), `Test-WrapperJobPerRun`, `Test-WrapperStopByName`, `Test-RemoteStop`, `Test-RemoteStopSparesNextRun`. Where: W:1396-1457, W:815-935, W:1566-1651. **Essential**.
- **BW7 UTF-8 console.**

  The command gets a hidden console of its own whose input and output code pages are 65001 from its first instruction (a .NET tool's non-ASCII output decodes right): a suspended donor `cmd.exe /d /q /k` is put in the job; the wrapper leaves its own console (`FreeConsole`, first starting a keeper `cmd.exe` on it when alone so it never closes), `AttachConsole(donor)`, `SetConsoleCP(65001)`, `SetConsoleOutputCP(65001)`, creates the command to inherit that console, waits (10 s) until it is attached (`GetConsoleProcessList`), then returns through the keeper or its parent, restoring any standard handle that was not a console handle (the ssh pipes).

  If this cannot be done the command runs on a plain hidden console with the box's default code page and the wrapper says `gate: remote could not give the command a UTF-8 console (<reason>); running it on the box's default code page` (a command already started on the UTF-8 console is first ended with its whole job and started again in a replacement job of the same name; the run never fails for it).

  Pins: `Test-RemoteUtf8Console` (live), `Test-WrapperAttachFailureEndsJob`, `Test-WrapperDonorFailureFallsBack`. Where: W:482-686, W:696-795. **Incidental** as a mechanism (a pseudo-console or a direct UTF-8 pipe decoder removes the donor dance and CC-154); **essential** as an outcome.
- **BW8 Output forwarding and drain cutoff.** Output is forwarded line by line to the wrapper's own stdout/stderr (LF, UTF-8, one lock so lines never interleave with heartbeats); after the command's root process exits the wrapper reads at most 2 s more and stops, so a leftover child holding the pipe holds neither the run nor the lock: `gate: remote command's output is still held open by a leftover process; stopped reading`.

  Pins: `Test-WrapperDrainCutoff`, `Test-RemoteDetachedChildHoldsOutput` (live). Where: W:319-365, W:379-397, W:1583-1585. **Essential**.
- **BW9 Heartbeats.**

  `gate-remote-heartbeat: <CPU ms counted so far>` on stderr every 5 s while it checks out, drains and syncs back (unconditionally); while the command runs, on the 5 s tick only once the CPU of the command's job plus every build server on the box (VBCSCompiler and `dotnet` running `VBCSCompiler.dll` or an MSBuild node, in whatever job) since the last heartbeat reaches 1% of one core (10 ms per second, carried forward; `BeatClock`; a server new in a sample, or whose total fell because its id was reused, counts all its CPU).

  A write that fails ends the heartbeats silently; a CPU sample that cannot be read writes `gate: remote heartbeat stopped: <message>`. Known limit: another run's busy build on the box can keep a silent idle command from stalling until the wall-clock timeout. Pins: `Test-HeartbeatBeatClock`, `Test-HeartbeatServerSum`, `Test-HeartbeatWriteFailure` (flaky under load: a 2 s wall-clock bound, CC-209), `Test-HeartbeatOutsideCommand`. Where: W:1118-1284. **Essential**.
- **BW10 Back commit.** After the command, the changed tree is committed (`add -A` through a copy of the work folder's index, `write-tree`, compare to `HEAD^{tree}`, `commit-tree -p HEAD`, `push -q origin <sha>:refs/gate-back/<key>`); nothing when unchanged. A failure writes `gate: remote sync-back failed: <reason>` and makes a passing status 1. Pins: `Test-RemoteSyncBack` (live), `Test-RemoteLineEndings`. Where: W:1596-1614, W:1683-1688. **Essential**.
- **BW11 `stop`.** Opens the job by name, terminates it (exit code 124), waits up to 10 s for its processes to end, prints `gate: remote stopped job <name>` (exit 0 with no line when no such job; `gate: remote job <name> still has <n> process(es) 10 s after it was terminated` and exit 1 when processes remain).

  A leftover that inherited no handles cannot be reached by name once the root exits. Pins: `Test-WrapperStopByName`, `Test-RemoteStop` (live: the command and a child it started are both gone). Where: W:937-968, W:1697-1707. **Essential**.
- **BW12 `boot`** prints only `gate: remote boot <time>` (the CIM `Win32_OperatingSystem.LastBootUpTime` as round-trip UTC). Where: W:1744-1756. **Essential**; **open** how the native version reads boot time (`GetTickCount64` or the system time-of-day boot time).

### 1.18 Branch deltas and planned work

- **D1 [outputs] Bring a routed build's outputs back.** After the sync-back, a passing routed run fetches the files its command wrote under the repo's output globs (default `**/bin/**` and `**/.godot/mono/temp/bin/**`; `remote.json`'s per-repo `outputs`, validated like `repos` and refusing `[`/`]`; `obj/` is opt-in; never `.git/`, `.tmp/`, `TestResults/` or any `.env`).

  The PC hashes its own files under the globs (SHA-256, no cache) and sends the manifest `{"files":{"<path>":"<sha256>"}}` as one line on the run call's stdin (never the command line; the wrapper reads up to the newline before the command starts; the command's own stdin is NUL).

  The box lists (path, length, last write) before the command, afterwards hashes files the manifest lacks or the run created or changed, and packs the ones whose hash differs into `D:\gate-remote\outputs\<key>-<nonce>.tar.xz` (bsdtar, xz level 9) with `.gate-outputs.json` at its root; it reports `gate-remote-outputs: <n> <archive>` or `gate-remote-outputs: 0` on stderr; the `outputs` verb streams the archive's raw bytes on stdout (the only verb whose stdout is not text) and deletes it.

  The PC refuses members that are not relative paths inside the worktree under the globs, checks every file's SHA-256, skips the whole step when a PC file the archive would overwrite changed since it was hashed (size or last write), requires every overwritten file to open for writing with no sharing, clamps last-write times to this PC's clock, deletes no file, and logs `gate: outputs: <n> file(s), <X.X> MB (<Y.Y> MB compressed) in <Z.Z> s`.

  The marker `%LOCALAPPDATA%\gate\remote\<key>.outputs-pending` (`GATE_TEST_OUTPUTS_PENDING` in tests) is written before the run call and removed only when the step completed; a run that finds it sends `OutputsFull` and the box packs every differing file. A failure anywhere makes a passing run exit 1 with `gate: remote outputs not brought back: <reason>`; the box drops archives older than 24 h at every run.

  Pins (a partial list from the branch's `sync-back` section): `Test-OutputsPackDiffersOnly`, `Test-OutputsPackExcludes`, `Test-OutputsObjOptIn`, `Test-OutputsNothingWritten`, `Test-OutputsExtractApplies`, `Test-OutputsClobberSkips`, `Test-OutputsRejectsEscapingMember`, `Test-OutputsHashMismatch`, `Test-OutputsMtimeClamped`, `Test-OutputsSkippedOnChangedWorktree`, `Test-OutputsFailingRunKeepsStatus`, `Test-OutputsGlobMatchersAgree`, `Test-OutputsOldCopyPacked`, `Test-OutputsUnchangedNotPacked`, `Test-OutputsFullResends`, `Test-OutputsPendingMarker`, `Test-OutputsNonAsciiRefused` (bsdtar reads its list in the box's ANSI code page), `Test-OutputsTargetLockedMovesNothing`, `Test-OutputsFetchKilledSaysSo`, `Test-OutputsSkipBackCommitPaths`, `Test-OutputsManifestProblemFails`, `Test-PublishBackCommitPaths`; gate side `Test-GateRouteCarriesOutputs`.

  **Essential** (the behaviour; CC-163's rules); the transport (tar.xz over ssh stdout) is **open**.
- **D2 [outputs] Pre-banner retry.**

  Every ssh call of a run (preflight, upload, push, run call, outputs fetch, stop, boot) and sync-back's `ls-remote`/`fetch` is retried once after `gate: <host> did not answer the ssh banner; trying once more` and a 2 s pause when it ended with ssh's 255 (git's 128 for push/`ls-remote`/`fetch`), was not killed by the watchdog, wrote nothing on stdout and its first stderr line starts with `Connection timed out during banner exchange`, `kex_exchange_identification: ` or `banner exchange: `; stop and boot keep 30 s for both attempts together (no retry with under 5 s left); an outputs fetch that already copied a byte is never retried.

  Pins: `Test-RemotePreBannerRetried`, `Test-RemoteSyncPreBannerRetried`, `Test-RemoteBoundedRetry`, `Test-RemoteKilledPushIsFinal`. **Essential** (CC-146).
- **D3 [outputs] Gate-facing change.** `Invoke-RemoteWatched` passes the route's `Outputs` globs; `Get-RemoteRoute` returns `{Host; Slots; Pattern; Outputs}`; the self-test counts become 88 local and 22 remote cases (README on the branch).
- **D4 [durations] Journal facts and archive** (J5); `gate.selftest.ps1` grows to 5,356 lines and `journal` to 11 cases (adds `Test-JournalVerdict`, `Test-JournalArchive`, `Test-JournalArchiveRetention`, `Test-RemoteVerdict`).
- **D5 [planned] Durations summary, `-Expect`, ETAs, `auto`, backfill** (`docs/plans/gate-durations.md`, CC-182 to CC-185, CC-197, CC-199): `%LOCALAPPDATA%\gate\durations.json` (key `<place>:<class>:<repo>[/<sub>] | <normalised command>` with a family key; p50/p90/p99 of `wallSeconds` over the last 30 passing runs or 14 days, 5 runs needed, `keyVersion` rebuilds from the archive), `gate: expected about <n> min (usual <p50>, p90 <p90>, <n> runs)` at take, `gate -Expect -- <command>`, ETAs in wait lines, `expected {p50,p90,n,key}` in claims and tickets (ops-tower fixtures and struct change), `-TimeoutSeconds auto` as a floor (`max(n, 3 x load-adjusted p99)`), hang hints at p90 and 3 x p90, and backfill guests during a critical drain (window file `%LOCALAPPDATA%\gate\backfill\<pid>.json`; guest admitted when its p90 + 10% ends before the shadow end, ceiling at most 2x the window, key qualified and not barred, command safe to repeat; a guest still running when the drain completes is killed and re-queued per the CC-198 ruling; the key barred 24 h).

  The plan builds these in Rust, not in PowerShell. **Essential** (as roadmap, not as port scope).

### 1.19 Things the self-tests do that the new suite must replace

- **Z1 Side-by-side processes.**

  The PowerShell suite starts each slot or waiting case in a pwsh of its own, at most half as many at once as there are logical processors scaled by the share free (never fewer than two), with a prefix of its own on every mutex, ticket folder, claim, journal and archive (`GATE_TEST_SLOT_PREFIX` = `selftest-<pid>-`), a counts file of its own, and `GATE_SLOT_HELD` set to a heavy marker naming the case's process so ordinary cases never wait on or hold up other sessions' gates.

  The timing cases run on a critical gate (`stall`, `kill-tree`, `sampler`) so other sessions' load cannot make them flaky. Each started gate gets 90 s of wall time, each case process 600 s; a timing case that fails while its log says the machine was under 30% free prints `skip` instead.
- **Z2 Doubles.** A stub remote (`GATE_TEST_REMOTE_STUB`) redefines `Invoke-RemoteCommand`; a routing file of the run's own; a repository named `repo` under `.tmp/gate-selftest/route-<pid>`; `cmd.exe` copied under a build server's name to fake `VBCSCompiler.exe`/`dotnet.exe`; WSL is used when present. ops-tower skips journal lines whose `cwd` holds `gate-selftest` (`OT/src/journal.rs:28`), so a suite that writes the real journal must keep that marker or use a prefix.
- **Z3 Landing run.** `tools/gate/test-all.ps1` runs three suites at once through the live gate: timing sections on a `critical` gate (ceiling 900 s), the remaining gate sections on a `light` gate (1800 s) and `remote.selftest.ps1` on another `light` gate; it waits (60 s at most) until the critical gate has queued (stderr shows `waiting for the machine (critical)`) before starting the other two, which take the reserved light slots beside it.

  A `-Check` run verifies that every case is in exactly one section.

## 2. Win32 and OS surface

`windows` crate feature names are the module path under `Win32::`; `windows-sys` has the same paths (ops-tower's `gate-state` uses `windows-sys` 0.61 with `Win32_Foundation`, `Win32_Security`, `Win32_System_Threading`). "Rust:" is the obvious mapping where there is one.

### Job objects and process control

| Call and use | Where | Rust |
|---|---|---|
| `CreateJobObjectW(NULL, "Local\gate-job-<pid>")`; `GetLastError()==ERROR_ALREADY_EXISTS` (183) refuses a name another job holds; no limits set (no `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`), so no breakaway and no kill-on-close | G:1017-1030 | `System::JobObjects::CreateJobObjectW` |
| `AssignProcessToJobObject` on a suspended command; on failure `TerminateProcess(1)` then throw | G:837-851 | `JobObjects::AssignProcessToJobObject` |
| `QueryInformationJobObject` `JobObjectBasicAccountingInformation` (1): `TotalUserTime + TotalKernelTime` (nested jobs and exited processes included), `ActiveProcesses` | G:855-862 | `JOBOBJECT_BASIC_ACCOUNTING_INFORMATION` |
| `QueryInformationJobObject` `JobObjectBasicProcessIdList` (3): header of two counts then ids; grow the buffer until the listed count reaches the assigned count; `ERROR_MORE_DATA` (234) | G:1034-1069 | `JOBOBJECT_BASIC_PROCESS_ID_LIST` |
| `OpenJobObjectW(JOB_OBJECT_QUERY 0x4 [and JOB_OBJECT_TERMINATE 0x8], "Local\gate-job-<pid>")`; `ERROR_FILE_NOT_FOUND` (2) means no such gate | G:1073-1094, G:1153-1192 | `JobObjects::OpenJobObjectW` |
| `TerminateJobObject(job, 124)` (kill and `-StopTree`); termination is asynchronous so the gate waits on the process | G:1264-1270, G:3633-3644 | `JobObjects::TerminateJobObject` |
| `IsProcessInJob(process, job)` (true for nested jobs too) | G:959-975 | `JobObjects::IsProcessInJob` |
| `CreateProcessW` flags `CREATE_SUSPENDED 0x4`, `CREATE_UNICODE_ENVIRONMENT 0x400`, `EXTENDED_STARTUPINFO_PRESENT 0x80000`, `BELOW_NORMAL_PRIORITY_CLASS 0x4000` or `HIGH_PRIORITY_CLASS 0x80`; `bInheritHandles = TRUE`; `STARTF_USESTDHANDLES`; environment NULL (inherited) | G:782-833 | `Threading::CreateProcessW`, `STARTUPINFOEXW` |
| `InitializeProcThreadAttributeList(2 attributes)` + `UpdateProcThreadAttribute`: `PROC_THREAD_ATTRIBUTE_HANDLE_LIST 0x20002` (NUL, log, err) and `PROC_THREAD_ATTRIBUTE_DESKTOP_APP_POLICY 0x20012` with value `PROCESS_CREATION_DESKTOP_APP_BREAKAWAY_DISABLE_PROCESS_TREE` (2) | G:796-806 | `Threading::UpdateProcThreadAttribute` |
| `ResumeThread` (failure value 0xFFFFFFFF), `TerminateProcess`, `WaitForSingleObject` (`WAIT_OBJECT_0`, `WAIT_TIMEOUT 0x102`), `GetExitCodeProcess` | G:739-748, G:506-536 | `Threading::*` |
| `OpenProcess` with `PROCESS_QUERY_LIMITED_INFORMATION 0x1000`, `PROCESS_VM_READ 0x10`, `SYNCHRONIZE 0x100000`, `PROCESS_TERMINATE 0x1`; failure `ERROR_INVALID_PARAMETER` (87) means the pid is gone | G:903-1012, G:1197-1232 | `Threading::OpenProcess` |
| `GetProcessTimes` (creation FILETIME, kernel+user) for CPU and for pid-reuse checks (a child counts only when created at or after its parent and after the command started) | G:903-926, G:1100-1150 | `Threading::GetProcessTimes` |
| `K32GetProcessMemoryInfo` working set (tries `VM_READ` first, then the limited right alone) | G:931-956 | `ProcessStatus::K32GetProcessMemoryInfo` |
| `NtQueryInformationProcess(ProcessCommandLineInformation = 60)`: size query then read, `UNICODE_STRING` result, Windows 8.1+ | G:978-1012, W:1027-1061 | `Wdk::System::Threading::NtQueryInformationProcess` or ntdll by hand |
| `CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS)` + `Process32FirstW/NextW` (pid, parent pid, exe name; one snapshot per sample and per `-StopTree`) | G:875-899 | `Diagnostics::ToolHelp::*` |
| `GetSystemTimes` (idle, kernel including idle, user; 100 ns) for the load share | G:865-873 | `Threading::GetSystemTimes` |
| `Process.PriorityClass` get/set on a build server (`GetPriorityClass`/`SetPriorityClass`; needs `PROCESS_SET_INFORMATION`); `(Get-Process -Id $PID).PriorityClass` for the gate itself | G:3101-3124, G:2837 | `Threading::GetPriorityClass`/`SetPriorityClass` |
| `Process.StartTime` (creation time) as UTC .NET ticks, the identity of a gate for tickets, claims, markers and pause files | G:2204-2217 | FILETIME to ticks: `ticks = filetime + 504911232000000000` |
| `PerformanceCounter("Process", "% Processor Time", "vmmemWSL", readOnly)`; `NextValue()` is percent of one core; the first read primes | G:2906-2927 | PDH: `System::Performance::PdhOpenQueryW`, `PdhAddEnglishCounterW`, `PdhCollectQueryData`, `PdhGetFormattedCounterValue` (**open**: or `GetProcessTimes` on the `vmmemWSL` process if it can be opened) |
| `Environment.ProcessorCount` (logical processors the process may use) | G:1701, G:2938 | `GetActiveProcessorCount` or `std::thread::available_parallelism` (check the 64-processor group rule) |
| `Environment.GetCommandLineArgs()` for the raw argv under `-File` | G:1428-1442 | `GetCommandLineW` |

### Named mutexes and namespaces

| Call and use | Where | Rust |
|---|---|---|
| `new Mutex(false, "Local\gate-slot-<i>")`, `WaitOne(0)` per attempt; `AbandonedMutexException` (WAIT_ABANDONED) treated as taken; `ReleaseMutex` in `finally`; ownership is thread-affine and a killed owner abandons it | G:1720-1763, G:2303-2320 | `Threading::CreateMutexW`/`WaitForSingleObject`/`ReleaseMutex`; take and release on one OS thread |
| Same for `Local\gate-light-slot-<i>`, `Local\gate-remote-slot-<i>`, `Local\gate-critical` (taken for the whole critical run) | G:1734-1737, G:1921-1924, G:1963-1965, G:2113-2119 | as above |
| `Mutex.TryOpenExisting(name)` as "any handle to this name exists" (also ops-tower's `OpenMutexW(SYNCHRONIZE)`) | G:1977-1983, `OT/src/win32.rs:140-148` | `Threading::OpenMutexW(SYNCHRONIZE)` |
| `Local\gate-journal` `WaitOne(2000)`; abandoned counts as owned | G:2368-2376, G:2417-2440 | as above |
| Namespace: `Local\` = the logon session's local namespace (a gate in another session, a service or the box's session 0 shares no slot) | README:101 | `CreateMutexW` with the `Local\` prefix |

### Files, sharing and atomic renames

| Use | Where | Rust |
|---|---|---|
| Log open for the command: `CreateFileW(path, GENERIC_WRITE, share 7 (read, write, delete), inheritable, CREATE_ALWAYS)`; stdin `NUL` `GENERIC_READ`/`OPEN_EXISTING` | G:766-791 | `Storage::FileSystem::CreateFileW` with `SECURITY_ATTRIBUTES.bInheritHandle` |
| Claim/ticket/counts written to `<file>.tmp` then `Move-Item -Force` (replace) so readers never see a torn file | G:2290-2293, G:2645-2646 | `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` |
| Journal append `File.AppendAllText` (share read) under the mutex; rollover `File.Move(a, b, overwrite)`; last-byte check opens `Open/Read/ReadWrite+Delete` | G:2397-2440 | `OpenOptions::append`, `MoveFileExW`, `share_mode(FILE_SHARE_READ, WRITE, DELETE)` |
| Pause marker `WriteAllText('')` rewritten every pass (last-write time is the liveness signal) and deleted | G:1899-1918 | `std::fs::write` (or `SetFileTime`) |
| Box work lock `FileStream(OpenOrCreate, ReadWrite, FileShare.None)` and the HResult low word 32/33 test | W:1350-1362 | `OpenOptions::share_mode(0)`; `ERROR_SHARING_VIOLATION` 32, `ERROR_LOCK_VIOLATION` 33 |
| Wrapper install: temp `<path>.<pid>`, SHA-256 prefix check, `File.Move` (a loser of a race treats an existing path as success) | R:772-791 | rename without replace |
| Native-type cache `GateNative_<hash>.dll` move into place | G:1285-1307 | incidental, not needed |
| `D:\gate-remote` on the box (D: is a Dev Drive) | R:96, `tools/build-box/install-toolchains.ps1:127` | plain paths |

### Console (box wrapper only)

| Use | Where | Rust |
|---|---|---|
| `FreeConsole`, `AttachConsole(pid)` / `AttachConsole(ATTACH_PARENT_PROCESS = -1)`, `SetConsoleCP(65001)`, `SetConsoleOutputCP(65001)`, `GetConsoleProcessList`, `GetConsoleMode`, `GetStdHandle`/`SetStdHandle` (restore the ssh pipes after `AttachConsole` replaces the standard handles) | W:253-279, W:482-686 | `System::Console::*`; **open**: `CreatePseudoConsole` (ConPTY) would give the command a UTF-8 console without the donor |
| `CreatePipe`, `SetHandleInformation(HANDLE_FLAG_INHERIT)` for the command's output pipes and the job handle | W:206-209, W:797-802 | `System::Pipes::CreatePipe` |
| `CreateProcessW` with `CREATE_BREAKAWAY_FROM_JOB 0x01000000` (and `CREATE_NO_WINDOW 0x08000000` for the plain console) so the command leaves the ssh session's kill-on-close job | W:169-174, W:696-763 | `Threading::CREATE_BREAKAWAY_FROM_JOB` |
| `Win32_OperatingSystem.LastBootUpTime` (CIM) for the boot line; `Win32_Process` (CIM) in the launcher fallback `-StopTree` | W:1749, L:304 | boot time: `GetTickCount64`/`NtQuerySystemInformation`; the second dies with the fallback |

### Other APIs with OS semantics

- `SHA-256` hashes: the work-folder key (8 hex of the lower-cased root), the wrapper's file name (8 hex of its bytes), the tool-manifest set, outputs [outputs]; `SHA1` (8 hex) names the native class (incidental). R:332-337, R:688-693, W:1499-1511.
- `ssh.exe` (OpenSSH, found with `Get-Command`), `git.exe`, `bsdtar` [outputs], `dotnet` (`--list-sdks`, `tool restore`), `cmd.exe` as sshd's default shell on the box (8191-character limit), `GIT_SSH_COMMAND` (git runs it through its own sh, so each word is single-quoted: `ConvertTo-ShWord`, R:282-285; pinned by `Test-ShQuoting`, `Test-RemoteBatchMetacharacters`).
- Timestamps: `DateTime.UtcNow` ISO "o" (seven digits, trailing `Z`) in claims and the journal; .NET ticks in tickets, markers and `GATE_SLOT_HELD`. `Stopwatch` for all intervals.
- ssh options that matter: `BatchMode=yes`, `ConnectTimeout=10`, `ServerAliveInterval=15`, `ServerAliveCountMax=4`; ssh exit 255 means connection problem.

## 3. Files and names other processes see

### Under `%LOCALAPPDATA%\gate\` (`<p>` is `GATE_TEST_SLOT_PREFIX`, empty in production)

| Name | Writer | Readers | Notes |
|---|---|---|---|
| `slot-counts.json` | ops-tower Gates tab (`write_slot_counts`, tmp+move, 10 attempts 100 ms apart), agents by hand | every gate on every wait pass; ops-tower (`OT/src/counts.rs`) | keys `heavy`, `light`, 1 to 64; `GATE_TEST_SLOT_COUNTS` redirects both |
| `remote.json` | by hand | each gate's router (`Read-RemoteConfig`); ops-tower reads only `slots` (`OT/src/remote.rs`); fixture `fixtures/remote.json` | `host`, `slots`, `repos`; [outputs] `outputs`; `GATE_TEST_REMOTE_CONFIG` |
| `slots\<p>gate-slot-<i>.json`, `gate-light-slot-<i>.json`, `gate-remote-slot-<i>.json`, `gate-critical.json` | the holding gate | other gates (`Get-OtherClaim`, `Test-ReservedParent`); ops-tower `read_claims` (names matched lower-cased) and its activity join (`pid`, `command`, `log`, `cwd`, `kind`, `index`) | fields in L1; `.json` extension required; `.pid.tmp` siblings ignored |
| `<p>waiting\<pid>-<startTicks>.json` | the waiting gate | other gates (`Read-Ticket`, which also deletes stale ones); ops-tower queue view | fields in Q1; `<file>.tmp` sibling during a write |
| `paused\<parentPid>-<parentTicks>-<pid>-<ticks>.pause` | the waiting child | the parent's watchdog only | not prefixed; empty files |
| `<p>events.jsonl`, `<p>events.1.jsonl` | every gate (append under `Local\<p>gate-journal`) | ops-tower `JournalCache` (history and per-command p50/p90/max), the scripts under `docs/research/2026-10-09-gate-lock-waits/`, human `rg` | roll detected by length/mtime of `.1` and creation time of the live file; ops-tower skips lines whose `cwd` holds `gate-selftest` |
| `<p>archive\events-<stamp>.jsonl.gz` [durations] | the gate whose append rolled | [planned] the durations summary rebuild | 180 days |
| `GateNative_<hash>.dll` | any gate (move into place) | any gate | incidental |
| `durations.json`, `backfill\<pid>.json`, the 24 h bar record [planned] | gates | gates; `durations.json` stays internal, `expected` goes into claims/tickets | parts 2 to 5 |
| `remote\<key>.outputs-pending` [outputs] | `remote.ps1` | `remote.ps1` | `GATE_TEST_OUTPUTS_PENDING` |

### Mutex, job and environment names

| Name | Who creates / reads |
|---|---|
| `Local\<p>gate-slot-<i>`, `Local\<p>gate-light-slot-<i>`, `Local\<p>gate-remote-slot-<i>` | gates create and hold; ops-tower probes by open (`MutexProbe`); the critical gate probes by open |
| `Local\<p>gate-critical`, `Local\<p>gate-journal` | gates only |
| `Local\gate-job-<gate pid>` (not prefixed) | the gate creates; `-StopTree`, other gates' build-server rules and `prio-check.ps1` open it by name |
| `Local\gate-remote-<key>-<nonce>` | the box wrapper creates; its `stop` verb opens it by name |
| `GATE_SLOT_HELD=<kind>:<pid>:<start ticks>` | written by a gate into its command's environment; read by a nested gate (a build hook's gate under a gated `git commit`); no repo outside `tools/gate/` and the plans names it |
| `MSBUILDDISABLENODEREUSE=1`, `DOTNET_CLI_USE_MSBUILD_SERVER=0` | set for every gated command |
| `GIT_INDEX_FILE` | read by the router (set inside git hooks: never route); the snapshot sets its own for the temp index |
| `CLAUDE_CONFIG_DIR`, `HOME` | read by the launcher to find the gate |
| `LOCALAPPDATA`, `ComSpec`, `SystemRoot`, `PATH`, `PATHEXT`, `COMPUTERNAME` (ops-tower) | read |
| the `GATE_TEST_*` and `GATE_SAMPLE_SECONDS` family | read; ops-tower honours `GATE_TEST_SLOT_COUNTS` and `GATE_TEST_SLOT_PREFIX` the way the gate does |

### Process-visible facts (read by other tools)

- **ops-tower waiting-gate detection** matches process command lines with `(?i)gate\.ps1\S*\s.*-Slot\s+<kind>\b` (`OT/src/state.rs:245`): a gate that is waiting is a process with that line, no live claim and no child other than `conhost.exe`. A port that renames the executable or the `-Slot` flag breaks the Gates tab's "waiting" list silently.
- **ops-tower activity join** (`ops-tower/src/agents/activity.rs:332-333`) splits a gate's command line at the first ` -- ` to name the gated command, matches it to a Claude Code Bash call's text (escape rules "mirror `gate.ps1`'s `Format-Argument`", ops-tower `docs/ARCHITECTURE.md:519`), and attributes a claim to the agent row whose process is the nearest ancestor of the gate's pid.
- **Process start time** is the key everywhere (tickets, claims, markers, journal `processStart`); ops-tower tolerates 2 s of drift when it compares a claim with a live process.
- **Hooks** read the command text: `~/.claude/hooks/block-bad-dotnet.ps1:159` strips the gate head with `(pwsh(\s+-\S+)*\s+)?\S*gate\.ps1\s.*?\s--` before it checks the gated command for `dotnet -q`; `yaat/tools/hooks/claude-guard-bash.sh:129-139` denies dotnet calls not wrapped by `gate.ps1`; `hooks/subagent-context.ps1:15` tests for the file `tools/gate.ps1` in the subagent's working directory; `hooks/gate-reminder.ps1:41` prints the command template.

### Output lines and exit codes other tools parse

| Text | Reader |
|---|---|
| `gate: (STALLED|TIMED OUT|BACKSTOP)` at line start in the log, exit 124 | `delve-the-dungeon/tools/worktree/New-Worktree.ps1:208` (`Get-GateKill`, retry on BACKSTOP at :297), `godot-mcp/run.ps1:322` (`Get-GateKill`), the gate's own marker regex (a gate wrapping a gate) |
| `gate: waiting for the machine (critical)`, `gate: holds the machine (critical)` on stderr | `tools/gate/test-all.ps1:110` |
| `gate: passed in ... Full output: <log>` / `gate: FAILED (status n). Full output: <log>` and the log tail | agents and skills read the tail; the `delegate` bench smoke checks that a transcript contains `gate.ps1` (`bench/smoke.py:137`) |
| `gate: stopped <n> gate job(s) and <m> process(es) under <pid>` | callers of `-StopTree` print it (`opening-hand/tools/test-all.ps1:171`) |
| Exit 2 usage; 127 not started | callers; `New-Worktree.ps1`, `run.ps1` treat any non-zero as a failed step |
| Journal event and field names | ops-tower history; the claude-config `docs/research/2026-10-09-gate-lock-waits/*.py` scripts |

### Git refs and files on the PC and the box

- PC: `refs/gate-remote/<key>` (last pushed snapshot), `refs/gate-remote/<key>-back` (kept changes), `<git dir>/gate-remote-<pid>.index` and `.patch` (temp), `GIT_AUTHOR_*`/`GIT_COMMITTER_*` = `gate-remote <gate-remote@localhost>` for snapshot commits.
- Box: `D:\gate-remote\<repo>.git` with `refs/gate/<key>` (snapshot), `refs/gate-bridge/<key>`, `refs/gate-back/<key>`; `D:\gate-remote\work\<key>` (+ `.lock`, `.index`, `.tools`); `D:\gate-remote\bin\gate-remote-<hash>.ps1`; [outputs] `D:\gate-remote\outputs\<key>-<nonce>.tar.xz`. The git config `core.autocrlf=false`, `core.eol=lf` is the work folder's own.
- Box protocol lines the PC parses: `gate: remote created <bare>`, `gate: remote boot <t>`, `gate: remote sdks <list>`, `gate: remote work folder <key> is busy`, `gate: remote work folder <key> could not restore its dotnet tools`, `gate-remote-heartbeat: <digits>` (stderr only), `gate: remote stopped job <name>`, [outputs] `gate-remote-outputs: <n> [<archive>]`; exit codes 75 (refuse), 99 (wrapper missing), 124, 255 (connection lost), 2, 1.
- Fixtures other tools pin: `tools/gate/fixtures/` (`claim.json`, `ticket.json`, `slot-counts.json` captured from a gate; `remote-claim.json`, `remote-ticket.json`, `remote.json`, `remote-journal.jsonl` hand-written) against `OT/tests/state/fixtures/`; a deliberate format change re-captures ops-tower's first, copies here, and sends ops-tower an inbox request (README:47).

## 4. Consumers outside `tools/gate/`

Searches (run 2026-10-09; quote them to re-size the sweep):

- `~/.claude`: Grep tool, path `C:\Users\someone\.claude`, pattern `tools/gate\.ps1|gate\.ps1|gate-launcher|-Slot (heavy|light|critical)|gate-slot|LOCALAPPDATA.{1,3}gate|events\.jsonl|gate-remote|remote-wrapper|tools[/\\]gate[/\\]|gate:`, output count, glob `!{.tmp,projects,sessions,file-history,shell-snapshots,paste-cache,cache,debug,backups,plugins,tasks,jobs,session-env,skill-observations,history.jsonl,debug.log,daemon.log,.git}/**`.

  Result: 82 files, 960 occurrences, of which `tools/gate/` itself holds 10 files and 656.
- Each repo under `X:\dev` (top-level folders, `_ext` and `*.wt` skipped): `.tmp/consumers.sh` in `~/.claude` (untracked helper) runs `rg --hidden -g '!.git' -g '!.tmp' -g '!node_modules' -g '!target' -g '!bin' -g '!obj' -g '!.venv' -g '!dist' -g '!build' -g '!.godot' -g '!.vs' -g '!*.log' -g '!*.jsonl' -g '!*.lock' -c -e <pattern> <repo>` with three patterns: `calls` = `gate\.ps1|gate-launcher`, `slot` = `-Slot[ =:]+(heavy|light|critical)`, `state` = `gate-slot|gate-remote|LOCALAPPDATA.{0,4}gate|events\.jsonl|GATE_(SLOT|TEST|SAMPLE)|gate:`.

  The per-file lists are in `.tmp/consumers2.out`. Counts are files/matches and include each repo's launcher copy (1 file, 11 call matches, 18 state matches), so a repo showing exactly `calls=1f/11m` has no consumer beyond its launcher. `rg` respects `.gitignore`, so ignored files (including most `.claude/settings.local.json`) are not counted.

### By repo

| Repo | calls | `-Slot` | state | Consumers beyond the launcher copy |
|---|---|---|---|---|
| `~/.claude` (claude-config) | 82 files / 960 matches overall (see above) | n/a | n/a | global rule `CLAUDE.md:114-118`; hooks `block-bad-dotnet.ps1:159`, `gate-reminder.ps1:41`, `subagent-context.ps1:15-16` and their tests (`test-block-bad-dotnet.ps1` 12 rows, `test-gate-reminder.ps1`, `test-batch-and-state-hooks.ps1:64`); agents `implementer.md:96`, `balance-analyst.md:19`; skills `claude-config-nextup/SKILL.md:32-51` (Gates table: it calls `pwsh tools/gate/gate.ps1`, there is no launcher in this repo), `conventions-sync/SKILL.md:81`, `ship/SKILL.md:117`, `harness-retro/references/skill-families.md:35`, `delegate` bench (`bench/tasks.py:33` `_CANONICAL_GATE`, `smoke.py:26,137`, tests, 20+ stored task briefs); tools `sync-launchers.ps1:34-37`, `ci-local/README.md:35,199`, `ck/index-active.sh:7,30`, `build-box/*`, `gate/test-all.ps1:44,84`; docs `ARCHITECTURE.md` (10), `GLOSSARY.md` (24), plans, research |
| delve-the-dungeon | 35 f / 125 m | 26 / 87 | 16 / 57 | `dtd.ps1` (16), `tools/test-all.ps1` (16; `:104-134`), `tools/worktree/New-Worktree.ps1` (`:196-208,297`: runs warm-up steps through the gate, parses `gate: (STALLED|TIMED OUT|BACKSTOP)`), `tools/smoke/Smoke-Common.ps1:16`, `src/Delve.Client/godot-mcp.json:2` (`prepWrapper` command line with `-Log {log} -TimeoutSeconds {ceiling} -Slot heavy --`), `.claude/skills/delve-nextup/SKILL.md` (5, the Gates table), `prepare-release`, `CLAUDE.md`, `docs/DEVELOPMENT.md` (12), `docs/GLOSSARY.md`, `docs/TEST_ALMANAC.md`, many archived plans |
| yaat | 32 / 97 | 29 / 74 | 59 / 103 | `CLAUDE.md:14,231,265-272`, `AGENTS.md`, `CONTEXT.md`, `tools/hooks/claude-guard-bash.sh:129-139` (denies dotnet not wrapped by `gate.ps1`), `tools/speech_telemetry.py:475-480` (builds a gate command line in Python), `.claude/workflows/pathfinder-v2-verdict-pass.js:8`, `.claude/skills/*` (consolidate-recordings, crc-update-check, land-bot-pr, layout-inspect (11), prepare-release, stt-pipeline-debugging, test-fix, `yaat-nextup/ship.md`, `dotnet-test-suite-profiling/collect.md`), `docs/test-harness.md`, `docs/plans/phraseology-implementation.md` |
| ops-tower | 23 / 95 | 17 / 51 | 35 / 158 | `crates/gate-state/` (the second reader of every format; 2,020 src lines), `src/agents/activity.rs` (26 matches, parses gate command lines), `src/agents/detail.rs`, `prek.toml:40-95` (7 hooks through `gate.ps1`, incl. `-Slot critical` for `timing::`), `tools/install.ps1:42-45`, `tests/http/gate.rs`, `tests/http/agents.rs`, `tests/browser/specs/gates.spec.ts:307-310`, `examples/fixture_server.rs` (writes claims/journal under a prefix), `ui/gates.js:450`, `docs/ARCHITECTURE.md` (13 matches: gate-state, formats, fixtures, "launchers"), `.claude/skills/ops-tower-nextup/SKILL.md` (4) |
| godot-mcp | 13 / 33 | 3 / 18 | 8 / 35 | `run.ps1` (20 matches: `:317-385` `Invoke-Gated`/`Get-GateKill`, `-Tail 15`, `-NoMarkers` at :1388, every command through the gate), `tools/hidden-desktop.ps1`, `src/GodotMcp.Server/Tools/ProjectProfile.cs:323` and `tests/.../PrepWrapperTests.cs:26` (the `prepWrapper` feature that wraps the Godot project's own steps with the gate), `tests/tools/test_drive.py`, `docs/DEVELOPMENT.md`, `docs/README.md`, `.claude/skills/godot-mcp-nextup/SKILL.md` |
| opening-hand | 14 / 57 | 10 / 42 | 8 / 37 | `duel.ps1` (20), `tools/test-all.ps1` (11; `:111`, `-StopTree` at `:171`), `tools/scratch/Run-Scratches.ps1:118`, `docs/design/balance.md` (11), `docs/DEVELOPMENT.md`, `.claude/skills/duel-nextup/SKILL.md` |
| in-the-sky | 13 / 42 | 7 / 31 | 8 / 38 | `sky.ps1` (23), `tools/test-all.ps1` (13; `-StopTree` at `:175`), `docs/DEVELOPMENT.md`, `.claude/skills/sky-nextup/SKILL.md`, `.claude/agents/event-writer.md` |
| rigcheck | 6 / 24 | 3 / 7 | 4 / 21 | `.claude/skills/rigcheck-nextup/SKILL.md` (3), `CLAUDE.md`, `tests/test_shell.py` (string fixtures such as `C:\hooks\gate.ps1`: false positives for the sweep) |
| rustling-tulip | 5 / 18 | 2 / 3 | 5 / 35 | `.claude/skills/rustling-tulip-nextup/SKILL.md`, `docs/plans/gate-dashboard.md` (14; a retired dashboard plan) |
| towercab-3d | 4 / 18 | 4 / 7 | 3 / 22 | `.claude/skills/towercab-3d-nextup/SKILL.md` (4), `prepare-release/SKILL.md` (2) |
| worktree-sweep | 3 / 14 | 3 / 4 | 2 / 19 | `.claude/skills/worktree-sweep-nextup/SKILL.md` (2), one archived brief |
| vs-chromium-mcp | 2 / 12 | 2 / 3 | 1 / 18 | `.claude/skills/vs-chromium-mcp-nextup/SKILL.md` |
| yaat-server | 2 / 3 | 2 / 4 | 11 / 16 | `CLAUDE.md` (3; calls `../yaat/tools/gate.ps1`: it has no launcher of its own), `docs/crc-update.md`; `.githooks/` present |
| claude-swap, craft-tester, free-space-watcher, towercab-3d-vnas | 1 / 11 each | 1 / 2 | 1 / 18 each (claude-swap 5 / 27) | launcher copy only; claude-swap's extra hits are `consume-gate-slot-removed` strings (false positive) |
| gate (`X:\dev\gate`, new, 0 commits) | 1 / 1 | 1 / 1 | 1 / 2 | `CLAUDE.md` only |
| fm-assistant, info-tool-web, lang-tutor, leftos.dev, openpax, pirep-encoder, stay-fresh-lsp-proxy, TopNotify, vatsim_control_recs, vVoiceSwitch, zoa-reference-cli | 0 | 0 | 0 | none |
| non-git folders: HarnessAgents | 1 / 1 | 0 | 0 | one stored spawn request JSON; `tools` (ComfyUI) and the others: noise or none |

- 15 repos carry the launcher: exactly `$gateRepos` in `tools/sync-launchers.ps1:34` (the paths there mix `D:\` and `X:\dev\` spellings).

  `-StopTree` is called from 2 repos (opening-hand and in-the-sky `test-all.ps1`); `-Tail` and `-NoMarkers` from godot-mcp's `run.ps1`; no repo script passing `-Local` was found (agents may); `-NoMarkers`, `-StopTree` or `-StallSeconds` appear in the docs of 6 repos (in-the-sky, godot-mcp, delve-the-dungeon, yaat, rustling-tulip, opening-hand). delve-the-dungeon also restates the gate's slot-count formula and `slot-counts.json` override in prose (`docs/DEVELOPMENT.md:279-281`, `docs/GLOSSARY.md:75-76`), so a changed default count is a doc change there too.
- Each `<project>-nextup` profile (11 repos, plus `claude-config-nextup` here) has a Gates table of `pwsh tools/gate.ps1 ...` commands; the sweep must rewrite every one. The global `CLAUDE.md` rule and the `implementer`/`nextup` skills say the same.
- Other places that call it by absolute path rather than the launcher: `tools/ci-local` (WSL job runs `pwsh tools/gate/gate.ps1 ... -- wsl -d Ubuntu ...`), `tools/ck/index-active.sh` (`GATE="$HOME/.claude/tools/gate/gate.ps1"`), `tools/gate/test-all.ps1` (`$HOME/.claude/tools/gate/gate.ps1`), delegate bench (`Path.home()/.claude/tools/gate/gate.ps1`).

## 5. Size and cost facts

### Line and function counts (main, `wc -l` and `rg -c '^function '`)

| File | Lines | Functions |
|---|---|---|
| `gate.ps1` | 4,236 (the embedded C# is lines 468-1280, 811 lines) | 186 |
| `gate-launcher.ps1` | 389 | 18 |
| `remote.ps1` | 1,564 (embedded C# `GateRemoteLog` 130-258) | 89 |
| `remote-wrapper.ps1` | 1,802 (embedded C# `GateRemoteJob` 95-1286, about 1,190 lines) | 30 |
| `gate.selftest.ps1` | 5,189 | 288 |
| `remote.selftest.ps1` | 3,288 | 156 |
| `test-all.ps1` | 245 | 9 |
| `prio-check.ps1` | 213 | 5 |
| `README.md` | 329 | n/a |
| `fixtures/` | 7 files, 31 lines | n/a |

- The sum without `prio-check.ps1` is 16,713 lines, the figure in the plan and CC-212. With it, 16,926.
- ops-tower `gate-state`: 2,020 lines in `src/` (`claims.rs` 242, `counts.rs` 255, `journal.rs` 579, `lib.rs` 26, `model.rs` 337, `remote.rs` 48, `state.rs` 343, `win32.rs` 190) and 2,534 in `tests/state/`.
- `[outputs]`: `gate.ps1` 4,237 / 186; `remote.ps1` 2,234 / 129; `remote-wrapper.ps1` 2,084 / 46; `gate.selftest.ps1` 5,228 / 290; `remote.selftest.ps1` 4,518 / 212. `[durations]`: `gate.ps1` 4,414 / 197; `gate.selftest.ps1` 5,356 / 298.
- Test hooks that exist only for the suite: about 14 environment variables (F4).

### Cases per section (README:290-318, verified against the `$sections` tables)

| `gate.selftest.ps1` | cases | `remote.selftest.ps1` | cases |
|---|---|---|---|
| stall | 13 | snapshot | 6 |
| kill-tree | 13 | quoting | 2 (one remote) |
| sampler | 1 | sync-back | 4 (one remote) |
| arguments | 7 | route | 6 |
| launcher | 6 | wrapper-job | 19 (two remote) |
| environment | 6 | health | 10 (three remote) |
| native-cache | 1 | heartbeat | 10 (one remote) |
| slots | 11 | console | 2 (both remote) |
| format-fixture | 2 | live-remote | 14 (eleven remote) |
| remote-slot | 15 | | |
| nesting | 14 | | |
| critical | 23 | | |
| journal | 7 | | |
| total | 119 (39 waiting + 8 CPU + 72 slot cases) | total | 73 (52 local + 21 remote) |

Branch deltas: `[outputs]` 88 local and 22 remote cases (the README's own count), `sync-back` 26, `route` 9, `wrapper-job` 26, `health` 14, `live-remote` 15, `remote-slot` 16; `[durations]` `journal` 11.

### Costs

- A bare `pwsh -NoProfile` start costs 416 to 1,134 ms (three runs; `docs/plans/gate-rust-port.md:7`), paid by every gate call and every wrapper call on the box. `Add-Type` compile of the gate's native class costs "about a second of CPU" per gate without the cache (G:306); the box wrapper pays about 0.8 s per run call to compile its job type (CC-187). A routed ssh push of a few hundred bytes takes 3 to 7 s (README:225).
- Landing runs, from the journal (`%LOCALAPPDATA%\gate\events.jsonl` and `events.1.jsonl`, `release` lines whose command holds `selftest.ps1`, read with `.tmp/landing-times.ps1` and `.tmp/landing-times2.ps1`), 2026-10-09:
  - timing sections on a critical gate (`gate.selftest.ps1 -Section stall,kill-tree,sampler`): 112 s, 153 s, 116 s, 176 s, 114 s held (five landings);
  - the remaining sections on a light gate: 405 s, 454 s, 521 s, 1,330 s (cc-181, beside two other sessions' suites and a pytest run) and 475 s;
  - `remote.selftest.ps1` without a host (local cases): 150 s to 700 s (150, 304, 358, 610, 700); with `-HostAlias <box>` (live cases): 259 s (cc-169b), 541 s (cc173-land), 963 s and 1,055 s (cc-163), 1,139 s (cc-179), 1,142 s (cc-146);
  - before `test-all.ps1` the full `gate.selftest.ps1` on a critical gate took 320 s to 328 s and the remote suite 712 s to 1,134 s. The plan's "9 to 22 minutes" matches 541 s to 1,330 s.
- Slowest cases of one `[outputs]` rebase run (`C:\Users\someone\.claude-wt\feat-gate-remote-outputs\.tmp\rb-remote-slot.log`; the log ends `4 case(s) failed.`): `Test-FormatFixtureRemote` 117.3 s, `Test-RemoteOutsideStaysLocal` 102.2 s, `Test-RemoteWaitKillLeavesNothing` 74.1 s, `Test-FormatFixture` 68.1 s, `Test-RemoteNoBuildDropped` 65.5 s, `Test-RemoteRefusalFallsBack` 57.7 s, `Test-RemoteWordsRewritten` 51.5 s, `Test-RemotePoolFullFallsBack` 39.1 s. The same `Test-FormatFixtureRemote` alone took 32.7 s (`rb-fixture-remote.log`).
- `test-all.ps1` ceilings: 900 s critical, 1,800 s light; each case process 600 s; each started gate in the suite 90 s.
- Journal volume: `events.jsonl` 4.3 MB and `events.1.jsonl` 5.2 MB on 2026-10-09 (rolls at 5 MB; about 22.5K lines, two days; `docs/plans/gate-durations.md`/CC-178).

## 6. Known defects and open issues, with the port's disposition

The Linear read-only tools, team CC, projects "Gate and launchers", "Remote slot", "Remote outputs follow-ups" and "Gate durations" (fetched 2026-10-09). Done issues are left out except where a ruling shapes the port. Disposition: **fixed by design** (the new design removes the cause), **carried** (a feature the port must build or keep), **moot** (a PowerShell or self-test problem the port deletes).

| Issue | Title | Disposition and why |
|---|---|---|
| CC-211 | Gate self-test: `remote-outside-unlisted` fails in a checkout whose path contains "remote" | **Moot** for the PowerShell suite; **fixed by design** as a rule for the new suite: no fixture may derive a pattern from the checkout path |
| CC-209 | `Test-HeartbeatWriteFailure`'s 2 s wall-clock bound fails under load | **Moot**; the new suite's timing cases must be load-proof from the start (the plan's own rule); the case itself (a stuck write must not block unwatch) is carried as an ordering test |
| CC-205 | Journal a slot taken over from a gate that died holding it | **Carried**: an `abandoned` field on `take` (or an event), also for the journal-mutex path that swallows it; free to design with the mutex model (S3, no pin today) |
| CC-194 | Read the colon forms of `-NoMarkers`/`-Local` by their value | **Fixed by design**: the Rust parser does not sit behind PowerShell's binder; define `-Switch:true|false` or drop the colon form |
| CC-192 | Split `Invoke-Main` under the complexity limit | **Moot** (PowerShell function shape) |
| CC-187 | Cache the box wrapper's compiled job type | **Moot**: no `Add-Type` in Rust; the 0.8 s per call disappears with a native wrapper |
| CC-177 | `Test-RemoteBusy` fails intermittently on a snapshot ref collision | **Fixed by design**: two runs of one worktree pushed to the same `refs/gate/<key>`; use a per-run ref (or lock before push) and make the busy test deterministic. Note the issue text's `refs/gate/{key}-{8 hex}` is stale: the code pushes `refs/gate/<key>` (R:939) and the key already ends in 8 hex |
| CC-132 | Home build box to replace the rented one (Blocked) | **Moot** for the port (infrastructure); the port must not hard-code the host |
| CC-12 | Every repo runs the agent-mail-guard hook through the tracked launcher (Blocked) | **Moot** for the gate; the same `sync-launchers.ps1` publishes both launchers, so the sweep must keep the guard launcher working |
| CC-104 | Gate: remote heavy slot on the build box (umbrella, In Progress) | **Carried**: open items are step 8's yaat enable (waits on CC-161) and godot-mcp, and the sub-issues below |
| CC-203 | Count lost connections apart from command failures; stop routing to a box that keeps dropping runs | **Carried** (new in Rust): a per-host loss counter, route nothing past a threshold until a preflight succeeds |
| CC-202 | Remember a box failure briefly so later routed gates go local at once | **Carried**: per-host record in `%LOCALAPPDATA%\gate\` with an expiry (`unreachable` short; `parity`/`tools` until global.json or manifests change; `busy`/`full` not recorded) |
| CC-201 | Measure MSBuild node sharing across work folders on the box; salt per work folder | **Carried** (measure first); set `MSBUILDNODEHANDSHAKESALT` to the key in the native wrapper if nodes are shared |
| CC-200 | Stop a box job whose PC has gone away so it frees the work folder | **Carried**; **fixed by design** is plausible: a native wrapper can watch its ssh stdin/stdout pipe and stop the job when the PC side is gone, or use a lease (HTCondor `JobLeaseDuration` 40 min; NativeLink 60 s) |
| CC-168 | A critical pool on the build box for ops-tower's timing-sensitive gates (Blocked) | **Carried, deferred**: waits on CC-166 and on ops-tower narrowing its critical runs; the drain rules of `Enter-Critical` carry over to remote claims |
| CC-166 | Enable ops-tower on the build box, its cargo outputs brought back | **Carried** (config plus `outputs` entry; needs the outputs step, so after #176 and the remote port) |
| CC-165 | Fix the routed path rewrite's edge cases (spaces, unstaged deletes, globs, `--no-build`, bare first word) | **Fixed by design**: rewrite per argv element with real path parsing instead of regexes on joined text; `ls-files` with literal pathspecs and a snapshot-based emptiness check; `--no-build` only for dotnet commands; a bare first word at the root becomes `./name` or its resolved path; claim records what was run. All five items need failing cases first |
| CC-160 | Refuse with `parity` when the box lacks a .NET workload the repo needs | **Carried** (`gate: remote workloads <ids>` beside the SDK line; refuse with 75 `parity`) |
| CC-154 | A stop during a fallback restart can miss the new job | **Moot or fixed by design**: the restart exists only for the UTF-8 console donor (BW7); a pseudo-console or direct decode removes it, or create the replacement job before closing the old |
| CC-151 | Refuse cmd's dynamic and substring `%VAR%` forms in batch words | **Fixed by design**: one shared implementation of the cmd word rules for gate and wrapper, covering `%CD%`, `%DATE%`, `%TIME%`, `%RANDOM%`, `%ERRORLEVEL%`, `%CMDCMDLINE%`, `%CMDEXTVERSION%`, `%HIGHESTNUMANODENUMBER%`, `%__CD__%`, `%__APPDIR__%` and `%PATH:~0,3%`/`%PATH:a=b%` forms |
| CC-161 | Send a repo's declared sibling worktrees with a routed run | **Carried** (settled: `remote.json` `siblings`; open questions: missing sibling, key per pair, sync-back of sibling files); blocks enabling yaat |
| CC-204 | Narrow the sync-back "worktree changed" refusal to changes that could matter | **Carried** (measure refusals first; candidate rules: refuse only when a path the back commit changes also changed locally) |
| CC-193 | Verify three review notes (2 GB hashing, archive open error, manifest read before heartbeat) | **Fixed by design**: stream-hash, report the IO error from the archive open path, start the heartbeat before the manifest read |
| CC-188 | Stop routed .NET builds recompiling everything because each snapshot is a new commit | **Carried** (measure with `IncludeSourceRevisionInInformationalVersion=false`; options: pass the property for routed dotnet commands, reuse HEAD's sha, repo setting) |
| CC-180 | Measure, and cache if slow, the output-file hashing on each routed run | **Carried**; native streaming SHA-256 plus a (path, size, mtime) cache is the likely shape |
| CC-171 | Allowlist godot-mcp's test and itest build steps once routed outputs come back | **Carried** (config, after #176 merges) |
| CC-170 | Send a routed command's ignored inputs to the box, compressed | **Carried** (an `inputs` list beside `outputs`, same manifest/pack transport) |
| CC-210 | Merge `feat/gate-durations` (#215) | **Process**: the plan lands #176 and #215 with part 1 only, then freezes the PowerShell gate |
| CC-182 | Durations part 2: the summary, `-Expect`, the expected-time line | **Carried**, built in Rust |
| CC-183 | Durations part 3: ETAs in wait lines and `expected` in claims and tickets | **Carried**; changes the claim/ticket format and ops-tower's struct (the redesign can fold it in) |
| CC-184 | Durations part 4: `-TimeoutSeconds auto` and hints | **Carried**; a CLI-shape decision (the `auto` form that carries `n`) |
| CC-185 | Durations part 5: backfill guests during a critical drain | **Carried**; kill-and-requeue per CC-198 |
| CC-199 | Weigh recent runs and keep cold and warm runs apart in the estimate | **Carried** (a decision before CC-182) |
| CC-197 | Correct a prediction once a run passes it | **Carried** (a decision before parts 3 and 5) |
| CC-178 | Learn each command's usual runtime and schedule with it (umbrella) | **Carried**. Its description still says an overrunning guest is "let finish, never killed"; the CC-198 ruling and `gate-durations.md:70` say killed once the drain completes |
| CC-212 | Port the gate to Rust (this) | umbrella |

Done issues that shape the port: CC-206 (lending a heavy slot while a command waits on a lock: measured, not worth building; `docs/research/2026-10-09-gate-lock-waits.md`), CC-198 (backfill guests are killed and re-queued when the drain completes), CC-196 (`-TimeoutSeconds auto` is a floor, never a learned kill time), CC-186 (`-Local`), CC-172 (two light gates beside a critical gate), CC-146 (pre-banner retry), CC-145/CC-147 (cmd quoting, wrapper quoting, UTF-8 console), CC-169/CC-167/CC-155 (self-test sections, parallel landing run).

## 7. Open decisions the redesign forces

1. **CLI shape and who still parses it.** Today: `pwsh tools/gate.ps1 -Log L -TimeoutSeconds N -Slot K [-StallSeconds N] [-Tail N] [-NoMarkers] [-Local] -- cmd...` and `-StopTree <pid>`.

   Outside readers of the *text of the command line*: ops-tower's waiting-gate regex (`gate\.ps1\S*\s.*-Slot\s+<kind>\b`, `OT/src/state.rs:245`) and its ` -- ` split (`activity.rs:332`), `block-bad-dotnet.ps1:159`, `claude-guard-bash.sh`, `subagent-context.ps1:15`, godot-mcp's `prepWrapper` JSON, `speech_telemetry.py`.

   Decide the spelling (`gate run ...`, `gate stop-tree <pid>`, `gate expect -- ...`) before the sweep, and whether a compatibility shim keeps the old regexes alive until each reader is changed. Also: exit codes 0/N/1/2/124/127 (callers branch on 124 and on non-zero), and whether `-Slot` stays required.
2. **Process identity and the waiting-gate signal.** ops-tower infers "waiting" from a process with `-Slot kind`, no claim and no non-`conhost` child. A native design can publish waiting gates in the ticket files (the tickets already exist) and drop the process-scraping, but then ops-tower and the gate's `gate-state` move together.
3. **State primitives.** Named mutexes (abandonment gives free crash recovery and a probe by open) versus lock files with liveness (pid + start time already used by every file) versus a daemon. Decide: the probe semantics ("held but no claim"), FIFO among same-kind waiters (Q2), polling (2 s) versus kernel waits, `Local\` scope (the box's ssh sessions and scheduled tasks live in other sessions), and thread-affinity of mutex release in a Rust runtime.
4. **File formats** (claim, ticket, counts, journal, `remote.json`, pause markers). All free to change, but each is read by ops-tower's `gate-state` (moving into the gate repo) and by `tools/gate/fixtures/`.

   Decide: keep JSON and the field names (cheapest for ops-tower), add a version field, ISO-"o"/ticks time encodings (the ticks scale is .NET-specific; epoch nanoseconds is simpler), whether counts and remote config become one TOML or stay JSON (ops-tower's writer of `slot-counts.json` drops unknown keys, which is why `slots` lives in `remote.json`), strict versus lenient JSON, case-insensitive keys.
5. **Config location and test seams.** One state directory with an override (`GATE_STATE_DIR` or `--state-dir`) replacing the prefix trick (`GATE_TEST_SLOT_PREFIX` prefixes mutex names, waiting folder, journal and archive, but not the pause folder or job names) and replacing `GATE_TEST_SLOT_COUNTS`, `GATE_TEST_REMOTE_CONFIG`, `GATE_TEST_NATIVE_CACHE`, `GATE_TEST_JOURNAL_BYTES`.

   Whether timing knobs (`GATE_SAMPLE_SECONDS`, heartbeat, poll, remote wait, sampler-fail, diagnostics-fail) are hidden flags, environment variables or a test-only build feature.
6. **Crate layout.** `gate-state` (claims, tickets, counts, journal, remote config; the shared model ops-tower depends on by git rev, `windows-sys` 0.61 as today), a Windows layer (job objects, process snapshot, mutexes, priority, console), the watchdog (pure arithmetic over samples: testable without Windows, W3), the CLI/launch crate, the router (T1-T4: pure string rules, easy to table-test), the remote client, and the box wrapper (a separate binary).

   Decide whether the pure parts compile on other platforms so their tests run in WSL/CI.
7. **How the suite drives timing** (1.19).

   Candidates: an injectable clock and sampler (unit-level) plus black-box cases whose bounds are far beyond the normal time (ordering instead of wall time); a fake process tree built from `cmd.exe` copies as today; a stub remote implemented as a separate executable named by configuration; running the timing cases on a critical gate (as `test-all.ps1` does) is itself a behaviour of the suite, not of the gate; slot cases need their own state directory per case instead of a prefix.

   Also the self-test's `cwd` marker that ops-tower filters (`gate-selftest`).
8. **How the box wrapper is deployed.** Today: content-addressed `gate-remote-<8 hex>.ps1` uploaded on stdin of an ssh `-EncodedCommand` script when `if exist` fails (exit 99), pruned after 7 days. A native `gate-box.exe` could keep the same install-on-miss protocol (hash of the exe), be pushed by the install script (the plan: `%LOCALAPPDATA%\gate\bin\gate.exe`, "on the box, the same path, pushed over ssh"), or be the same binary as `gate.exe` with a hidden subcommand.

   Also whether the box keeps `cmd` as its ssh shell (the 8191-character limit and the 99 convention), whether the payload moves from base64 JSON on the command line to stdin, and whether the git snapshot transport stays (RC1-RC3).
9. **Windows-only or cross-platform.** The gate exits 2 on a non-Windows machine today, the box is Windows, and ci-local reaches WSL by running the gate on the Windows host. Decide whether only the pure crates build on Linux (test speed) while the binary stays Windows-only, or whether the remote client/router should run in WSL.
10. **Launcher and the zero-install fallback.** The 15 launcher copies run the PowerShell fallback on a machine without the gate (someone else's clone). The plan has the launcher exec `gate.exe` until the sweep. Decide what the fallback is after the sweep (nothing? a PATH `gate` shim?) and whether a launcher change needs another `sync-launchers.ps1` round.
11. **Drained cutover mechanics.** Slots, claims, tickets and counts are not shared between the two gates, so every gate on the PC must be drained (and, for the remote cutover, the box).

    Decide the drain procedure (a `gate` mode that waits for the PowerShell mutexes and claims to be free, then flips), what `-StopTree` does to the other generation's jobs (`Local\gate-job-<pid>` is the one name both generations could keep common), and how ops-tower shows two generations during the window (it should not).
12. **Log and tail encoding.** The local gate stores raw bytes and prints the tail by decoding as UTF-8; the box uses a UTF-8 console. Decide what the port does with non-UTF-8 output (lossy tail, validate, leave raw) and whether the Godot/dotnet marker regex runs on bytes or text.
13. **Command-line semantics to keep or to retire.** Colon-form option values (C2), the "join a word that starts with `-` and ends with `:` to the next word" rule (C3, an in-session PowerShell workaround), nested array flattening (incidental), the PATHEXT/shim resolution (C6, essential), and the cmd metacharacter rules (C5, essential and to be shared with the wrapper).
14. **Failure-marker regex and verdict wording.** Keep the regex (V2) as a built-in or move it to configuration per repo; decide which `gate:` lines are a stable interface (`STALLED|TIMED OUT|BACKSTOP`, `waiting for the machine (critical)`, `Full output:`) versus prose, and publish that list so the sweep can test for them.
15. **Dependence on Windows facilities that may not map.** `vmmemWSL` CPU via PDH (W6), `NtQueryInformationProcess` command lines of other users' processes (B1), `DESKTOP_APP_POLICY` (X1), Windows 8+ nested jobs, and the ConPTY alternative to the console donor (BW7). Each needs a spike before design freeze because a silent loss changes kill behaviour (W2 progress signals, the Store pwsh escape).
16. **Scope of the freeze and the in-flight branches.** The plan lands `#176` (remote outputs) and `#215` (durations part 1) then freezes the PowerShell gate. Both branches add file formats (the `release` facts, the archive, the outputs manifest/marker/tar.xz) the Rust port must either reproduce or deliberately drop. Decide before step 3 which of D1-D4 are in the port's first slice.

## DOC DRIFT

- `docs/plans/gate-rust-port.md:8` (and CC-212's text) says the native parts are "57 lines of C#". Code: the gate's embedded C# is 811 lines (`tools/gate/gate.ps1:468-1280`), the box wrapper's about 1,190 (`remote-wrapper.ps1:95-1286`), the remote log writer 127 (`remote.ps1:130-258`); the named mutexes are .NET `Mutex`, not C#. The plan is wrong (perhaps the 52 `DllImport` lines were counted); owner: `docs/plans/gate-rust-port.md`.
- `docs/GLOSSARY.md:42` (**Guest**: "one that overruns is let finish and its key is barred from backfill for 24 h") against `docs/plans/gate-durations.md:70` ("Once the drain completes, a guest still running is killed, and its own gate re-queues it. Its key is barred ... 24 h") and the CC-198 ruling. The glossary is stale; owner: `docs/GLOSSARY.md`. CC-178's description also still says "let finish, never killed" (Linear, not a doc).
- `X:\dev\ops-tower\crates\gate-state\src\journal.rs:71-74` lists the `fallback` reasons as `full`, `busy`, `unreachable`, `rebooted`, `parity` (or `unknown`), omitting `tools`, which `tools/gate/README.md:65` and `remote.ps1:1387` define. ops-tower's comment was stale; fixed: the gate repo's `crates/gate-state/src/journal.rs` now lists `tools`.
- CC-177's description says `Publish-RemoteSnapshot` names its ref `refs/gate/{key}-{8 hex}`; `remote.ps1:939` pushes `refs/gate/$($Context.Key)` and the key already ends in 8 hex (`remote.ps1:332-337`). The issue text is the stale side (Linear, owner CC-177).
- `tools/gate/README.md:43-47` names the fixtures the gate pins; ops-tower's fixture folder also holds a captured `journal.jsonl` (`OT/tests/state/fixtures/journal.jsonl`) that `tools/gate/fixtures/` does not mirror, so the gate pins its journal shape only against the hand-written `remote-journal.jsonl`. Not wrong, a gap in "Formats other tools read"; owner `tools/gate/README.md`.

## TASK INDEX GAP

The inventory took far more than a Task Index row and one doc (about 110 reads and searches across the PowerShell, two branches, ops-tower, Linear and every repo). A row should exist:

- Task in the caller's words: "Port the gate, or size a cutover: what does the gate do, and who reads or calls it".

  Files, in order: `docs/plans/gate-rust-port.md` → `tools/gate/README.md` ("Formats other tools read", "Journal", "Load-critical requests", "Remote runs") → `tools/gate/gate.ps1` (the help header, lines 3-411, is the behaviour spec; the native C# at 468-1280) → `tools/gate/remote.ps1` and `remote-wrapper.ps1` (help headers) → the `$sections` tables of `tools/gate/gate.selftest.ps1:179` and `remote.selftest.ps1:155` (case names are the edge-case record) → ops-tower `crates/gate-state/src/` (`claims.rs`, `state.rs`, `journal.rs`, `win32.rs`) and `src/agents/activity.rs` → the consumer list: `tools/sync-launchers.ps1:34` (`$gateRepos`), `hooks/block-bad-dotnet.ps1:159`, `hooks/gate-reminder.ps1`, `hooks/subagent-context.ps1:15`, each repo's `.claude/skills/*-nextup/SKILL.md` Gates table, `tools/ci-local/README.md`, `tools/ck/index-active.sh` → `.tmp/map-gate-behaviour.md` while the port is open.
- A second row: "Find every caller of the gate in a repo" → `tools/sync-launchers.ps1` (`$gateRepos`) then `rg 'gate\.ps1|gate-launcher|-Slot'` per repo (the three patterns in section 4).

## MISSING ENTRY POINT

- No missing `docs/README.md` or `docs/ARCHITECTURE.md`; the architecture doc has a Task Index.

  Glossary gaps (terms the gate docs and the port plan use in a project sense with no entry in `docs/GLOSSARY.md`): **Claim (gate)** (only the inbox claim is defined, line 22; the gate's claim file is used throughout the README), **Slot marker** / `GATE_SLOT_HELD` and **covered** (a gate a parent's slot covers), **Drain** (a critical gate's wait for the gates ahead of it), **Holds the machine**, **Take / release** as journal events, **Verdict** (the journal field on `[durations]`), **Key** (the work-folder key; only "Work folder" mentions it), **Strangler**, **Drained cutover** and **Cutover sweep** (`docs/plans/gate-rust-port.md`).

  On `feat/gate-remote-outputs` the glossary already adds **Outputs** and **Outputs-pending marker**.
