# gate — architecture

gate is a Cargo workspace whose `gate` binary (`gate.exe`) queues heavy commands onto a fixed number of CPU slots, watches them and kills runaway process trees. Its layers, bottom up: the shared state crates (`gate-state`, `proc-table`), the pure rule crates (`gate-watchdog`, `gate-args`, `gate-router`), the Win32 layer (`gate-win32`) and the binary that composes them.

The rule that shapes it: every file and kernel object another process sees is defined once, in `gate-state`, and the pure crates hold no Windows types, so they build and test on Linux. The design is [design.md](design.md); terms used in a project sense are in the [glossary](GLOSSARY.md).

Every crate below is planned: `gate-state` and `proc-table` move in from ops-tower with their history (GATE-4), the rest are written by the local port (GATE-5).

## Task Index

"I need to change X, which files?" One row per kind of change, files in the order to read them.

| Task | Files, in order | Deep doc |
|---|---|---|
| Change a file format (claim, ticket, counts, journal, archive) | `crates/gate-state/src/model.rs` → the format's module (`claims.rs`, `counts.rs`, `journal.rs`) → `crates/gate-state/tests/state/fixtures/` → [design.md, Formats](design.md#formats) → a repo inbox request to ops-tower, which reads every format | [design.md](design.md#formats) |
| Change how the gate reads slot state (claims, tickets, counts, mutexes) | `crates/gate-state/src/{model,state,counts,claims,remote,win32}.rs` → `crates/gate-state/tests/state/` and its fixtures | [design.md, Slots](design.md#slots) |
| Change how the journal is read | `crates/gate-state/src/journal.rs` → `crates/gate-state/tests/state/journal.rs` | [design.md, Journal](design.md#journal) |
| Re-capture the gate's fixtures | `crates/gate-state/tests/state/fixtures/README.md` → `crates/gate-state/tests/state/fixtures/capture-remote-journal.ps1` | `fixtures/README.md` |
| Move or change the process table | `crates/proc-table/src/lib.rs` → `crates/proc-table/src/win32.rs` → `crates/gate-state/src/state.rs` (its user here) | none |
| Change the CLI (a subcommand, a flag, a usage message) | [design.md, CLI](design.md#cli) → `crates/gate-args/` (the parser and the compatibility layer) → `crates/gate/src/` (the subcommand) → the readers of command-line text in [the inventory, section 3](research/2026-10-09-powershell-gate-inventory.md#3-files-and-names-other-processes-see) | [design.md](design.md#cli) |
| Change an output line callers parse | [design.md, Output lines](design.md#output-lines) → `crates/gate/src/` → the parsers in [the inventory, section 3](research/2026-10-09-powershell-gate-inventory.md#3-files-and-names-other-processes-see) ("Output lines and exit codes other tools parse") | [design.md](design.md#output-lines) |
| Change how a command is quoted, resolved or refused | `crates/gate-args/` → [design.md, Command launching](design.md#command-launching) → `crates/gate/src/` and `gate box` (they share the rules) | [design.md](design.md#command-launching) |
| Change stall, ceiling, backstop or cores-hint arithmetic | `crates/gate-watchdog/` → `crates/gate-win32/` (the samples) → [design.md, Watchdog](design.md#watchdog) | [design.md](design.md#watchdog) |
| Change what routes to a builder | `crates/gate-router/` → `crates/gate/src/` (the bridge) → [design.md, Routing](design.md#routing-and-the-remoteps1-bridge) | [design/gate-remote-slot.md](design/gate-remote-slot.md) |
| Add a test seam | [design.md, Black-box suite harness](design.md#black-box-suite-harness) → `crates/gate/Cargo.toml` (the `test-seams` feature) → `crates/gate/src/` | [design.md](design.md#black-box-suite-harness) |
| Port a behaviour of the PowerShell gate | [the inventory](research/2026-10-09-powershell-gate-inventory.md), its item in section 1 → the design.md section that cites that item → the crate the Layers below give it | [design.md](design.md) |
| Find every caller of the gate | [the inventory, section 4](research/2026-10-09-powershell-gate-inventory.md#4-consumers-outside-toolsgate) (its search patterns and per-repo table) | none |

## Layers

- **`proc-table`** (`crates/proc-table/`, planned): owns the process snapshot model (`ProcessInfo`, `ProcessTable`, `ProcessTree`) and its Windows reader. References `windows-sys` in its `win32` module only. Never depends on any gate crate: ops-tower and its other crates use it too.
- **`gate-state`** (`crates/gate-state/`, planned): owns every file format in the state root, their readers and writers, the path and object-name rules, journal history and the dashboard state ops-tower shows. References `proc-table`. Never depends on `gate-win32`, `gate-watchdog`, `gate-router` or the binary, and its API exposes no `windows` or `windows-sys` types.
- **`gate-watchdog`** (`crates/gate-watchdog/`, planned, pure): owns the stall, free-share, ceiling, backstop and cores-hint arithmetic over samples. References nothing of the gate's but plain data types. Never reads a clock, a process or a file itself: the clock and samples are injected.
- **`gate-args`** (`crates/gate-args/`, planned, pure): owns the gate's own argv (the subcommand CLI and the compatibility layer), C-runtime and cmd.exe quoting, the `%VAR%` refusals and the PATHEXT resolution rules. Never depends on `gate-win32`; the environment and file lookups it needs are injected.
- **`gate-router`** (`crates/gate-router/`, planned, pure): owns the routing rules, the in-worktree path rewrite and the `--no-build` drop, over strings. Never runs git, ssh or a process; its inputs are given.
- **`gate-win32`** (`crates/gate-win32/`, planned, Windows only): owns job objects, process creation, named mutexes and events, priority, console handling and taking samples. References `proc-table` and `windows-sys`. Never holds policy: what to kill and when is `gate-watchdog`'s.
- **`gate`** (`crates/gate/`, planned, Windows only): the binary. Owns the subcommands, the wait loop, the run, the journal writes, the `remote.ps1` bridge and `gate box`. References every crate above. Nothing depends on it.

## Integration Footguns

- **Change a file format** (a field, a type, a file name, an object name) → also change `gate-state`, re-capture its fixtures (`crates/gate-state/tests/state/fixtures/README.md`), raise `v` when an old reader would misread it, update [design.md, Formats](design.md#formats), and send ops-tower a repo inbox request, since it reads every format through a pinned rev of `gate-state`.
- **Change a CLI spelling** → also change the compatibility layer in `gate-args`, and every reader of command-line text: ops-tower's activity join and the hooks that inspect a gated command (inventory section 3, "Process-visible facts") match both spellings until the layer goes.
- **Change a stable output line** (STALLED, TIMED OUT, BACKSTOP, `waiting for the machine (critical)`, `Full output:`) → it is a breaking change for the scripts in inventory section 3 and the built-in marker regex; prose lines change freely.
- **Write a test that starts `gate.exe`** → give it a non-default state root (`--state-dir`): a gate on the default root takes the machine's real slots and writes the real journal, where it fights every session's gates.
- **Change the cmd.exe word rules** → the gate and `gate box` share them in `gate-args`; a fix in one place is a fix in both, and its test covers both callers.
- **Change the slot marker or a kernel object name** → nested gates, `gate stop-tree` and ops-tower's mutex probe all read names; the root hash rule in [design.md, State root](design.md#state-root) applies to every name.
- **Change `gate-state` or `proc-table`'s public API** → ops-tower builds against them by git rev; a breaking change needs its matching change there, by inbox request.

## Test locations

- `crates/gate-state/tests/state/`: every format against its fixtures, claims, counts, the critical and queue views, journal reading. Fixtures in `crates/gate-state/tests/state/fixtures/`.
- Unit tests inside each pure crate (`gate-watchdog`, `gate-args`, `gate-router`, and `gate-state`'s pure parts): run on Windows and on Linux. Quoting round-trips and path rewrites are property tests.
- `crates/gate/tests/`: the black-box suite, a test target with `required-features = ["test-seams"]`. A new behaviour of the binary gets a case here, with its own state root and ordering-only asserts.
- Run everything through the gate with `cargo test --workspace --profile test-release --all-features`; the pure crates alone on Linux with `cargo test` on those packages.

## Deep docs

- [design.md](design.md): the gate's design: CLI, exit codes, output lines, slots, state root, formats, watchdog, launching, routing, install, cutover, crates, the suite harness.
- [design/gate-durations.md](design/gate-durations.md): learned command durations, expected times and backfill guests (GATE-7).
- [design/gate-remote-slot.md](design/gate-remote-slot.md): the remote slot's open work (GATE-10).
- [design/gate-box-critical.md](design/gate-box-critical.md): a builder's critical pool (GATE-21).
- [research/2026-10-09-powershell-gate-inventory.md](research/2026-10-09-powershell-gate-inventory.md): the PowerShell gate's behaviour, Win32 surface, cross-process names, consumers and defects, item by item.
- `crates/gate-state/tests/state/fixtures/README.md`: how the fixtures are captured.
