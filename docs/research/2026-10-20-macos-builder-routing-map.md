# GATE-591: a second builder on macOS arm64 (research, no change planned)

Public repo: no builder host, address, user or key appears below; "the Windows builder" is the one in `remote.json`.

## Answer in brief

- **Routing is one builder today.** `remote.json` has one `host` and an optional `hosts` that only `gate push` reads (`crates/gate-router/src/config.rs:1-3,234-252`, `docs/design.md:1115`). `settle` always sets `Target.host = config.host()` (`crates/gate-router/src/decide.rs:1054`). The live file has `host` and no `hosts` (measured, read this run). A Mac needs a per-repo or per-command builder choice first; nothing else in the router picks between builders.
- **`gate.exe` cannot build for macOS.** `gate-win32` is `#![cfg(windows)]` (`crates/gate-win32/src/lib.rs:20`), and the `gate` crate depends on it and on `windows-sys` unconditionally (`crates/gate/Cargo.toml:30-48`). 459 `gate_win32`/`windows_sys`/`std::os::windows` references sit in 64 files of `crates/gate/src` (measured, Grep count), the PC-side client and `run.rs` included. The design lists "Running on Linux or macOS" as a non-goal (`docs/design.md:23`).
- **The pure crates are the reusable part:** `gate-args`, `gate-router`, `gate-watchdog`, `gate-state` (its `win32` module is cfg-gated, `lib.rs:26`) and `proc-table` (`lib.rs:5`) carry no unconditional Windows dependency (`Cargo.toml` of each). Whether they build for `aarch64-apple-darwin` is unmeasured (no build run, read-only task); `design.md:3636-3651` claims Linux.
- **Portable work is the .NET and Python work.** delve-the-dungeon, opening-hand, in-the-sky, yaat and yaat-server run `dotnet build/test/format` and `csharpier` green on ubuntu in their own CI. They are 60% or more of the routed load (see D).
- **Smallest real step:** route opening-hand's `dotnet build/test/format` to the Mac. A single repo with no LFS, no siblings, no inputs, and a Linux CI that runs the same commands.

## A. Gated work by portability to macOS arm64

**Durations.** Measured from the gate journal, `%LOCALAPPDATA%\gate\v1\events.jsonl`, `events.1.jsonl` and `archive\*.gz`: heavy `release` lines, `wallSeconds`, 2026-10-11 19:06Z to 2026-10-20 16:49Z, 29,061 releases (a script run this session). R means routed to the Windows builder, L means run on the PC. "min" is the sum of wall minutes over all runs (concurrent runs add up, so it is load, not elapsed time).

Routing was added repo by repo during the window, so an L figure may predate the repo's routing; I did not separate them. Example: yaat's `test-all.ps1` was L 7-61 a day until 10-16, R 45-60 a day from 10-18 (measured).

**Recent routed load, 10-18 to 10-20 (2.7 days, measured):** 13,888 min in all. yaat 3,691; delve 3,345; gate 1,578; godot-mcp 1,133; opening-hand 1,083; rustling-tulip 1,034; ops-tower 626; agent-park 577; worktree-sweep 180; race-anywhere 22; yaat-server 10.

**Fallback demand (measured, whole window):** 2,328 routed `queue` events; fallbacks `busy` 39, `unreachable` 12, `version` 3; `full` none. The Windows builder is rarely saturated, so a second builder adds capacity and an extra place to run, not relief for a queue that exists today. Local-only classes and parallelism are the gain.

Portable "yes" is by reading plus the repo's ubuntu CI. Nothing here was run on a Mac.

| Repo | Gated class | Toolchain | Portable as written? | What decides it | Window (measured) |
|---|---|---|---|---|---|
| opening-hand | `dotnet build/test/format`, `csharpier` | dotnet 10.0.401 (`global.json`), pwsh 7.4 (`.gate/toolchain.toml`) | yes | CI ubuntu runs the same: `.github/workflows/ci.yml:51-63`; all `net10.0`; no LFS, no `DllImport` | build R 753 min n=1204 med 37s; format R 433 min n=515 med 48s; test R 103 min n=198 med 28s |
| opening-hand | `uv run --directory tools?analysis\|voice pytest*` | uv, Python | yes | CI ubuntu `ci.yml:85-95` | not in the top rows (small) |
| opening-hand | `*Godot_console* --headless --path src?Duel.Client --build-solutions --quit` | Godot 4 .NET headless | build yes, the word no | binary name `Godot_console.exe` is the Windows file (`crates/gate/src/boxside/probe.rs:38`); Mac binary is `Godot.app/Contents/MacOS/Godot` | R 9 min n=7 med 71s |
| in-the-sky | `dotnet build/test/format`, `csharpier` | dotnet 10.0.401 | yes | CI ubuntu `ci.yml:68-84`; no `DllImport`/OS guards found in `src`/`tests` | not in the top rows |
| yaat | `pwsh tools/test-all.ps1` (builds yaat and yaat-server, then both test suites) | pwsh, dotnet, wasm-tools workload (`.gate/toolchain.toml`) | yes by read | no Windows token in `tools/test-all.ps1` (searched `IsWindows`, CIM, `taskkill`, `.exe`, `Win32`, `subst`, `Get-NetTCP`, `cmd /c`: no hit); CI ubuntu builds and tests both repos `ci.yml:76-132` | R 2,062 min n=180 med 707s (the largest class) |
| yaat | `dotnet test/build/format`, `dotnet run --project tests/*` | dotnet | yes | CI ubuntu `ci.yml:70-84`. Windows-only projects (`Yaat.WindowRecorder`, `Yaat.ClientDriver.Mcp`: `net10.0-windows`, P/Invoke) set `EnableWindowsTargeting` so they compile off Windows; whether their tests run or are filtered on Linux is unmeasured | test R 1,172 min n=792 med 63s; build R 519 min n=241 med 118s; format R 219 min n=181 med 69s |
| yaat | the sibling pair with yaat-server | git | needs the box's pair layout | `.gate/siblings.toml`; `remote.json` `siblings`; layout code is in the Windows box (`boxside/preflight.rs`, `run.rs`) | - |
| yaat-server | `dotnet build/test/format` | dotnet, wasm-tools | yes | CI ubuntu `ci.yml:55-73` | tiny recent load (10 min) |
| delve-the-dungeon | `dotnet build Delve.slnx` (Debug and Release), `dotnet test`, `dotnet format`, `dotnet csharpier` | dotnet 10 | yes | CI ubuntu `ci.yml:51-61`; all projects `net10.0` | build R 1,841 min n=1961 med 50s; format R 1,020 min n=626 med 93s; test R 849 min n=1175 med 37s; csharpier R 417 min n=945 med 25s |
| delve-the-dungeon | `"C:\Program Files\dotnet\dotnet.exe" ...` | dotnet | no, the word | the first word is a Windows path; the router sends it as a program name, which a Mac lacks as `dotnet.exe` | R 312 min n=244 med 74s |
| delve-the-dungeon | `Godot_console* --headless --path src?Delve.Client --build-solutions\|--import\|--script` | Godot 4.7.2 .NET headless | work yes, word no | Delve's own `release.yml` runs the mono editor headless on ubuntu (`:35-42`) and macOS universal (`:223-235`); the word `Godot_console.exe` and `GODOT_PATH`/probe are Windows (`probe.rs:38`) | build-solutions R 613 min n=360 med 85s (+ L 199 min); import and theme `--script` are light-slot, never routed |
| delve-the-dungeon | `pwsh tools/test-all.ps1` smokes (six servers plus headless clients), `pwsh dtd.ps1 client -Import` | pwsh, Godot, dotnet | no | `Get-NetTCPConnection` (`tools/ServerPort.ps1:45`, `dtd.ps1:411`), `Get-CimInstance Win32_Process` (`ServerPort.ps1:78,90`), `subst` (`tools/test-all.ps1:108`), `Delve.Server.exe` name (`ServerPort.ps1:145`), `Godot.exe` beside the console binary (`dtd.ps1:114`), Inno Setup (`dtd.ps1:1090`) | test-all R 5 min n=1 |
| godot-mcp | `dotnet build` / `dotnet test --project tests?GodotMcp.Tests` | dotnet | no | `GodotMcp.slnx` holds `GodotMcp.Capture` (`net10.0-windows10.0.26100.0`) with no `EnableWindowsTargeting` (none found in props or csproj), so the solution build fails with NETSDK1100 on a Mac; 17 `DllImport` files (`Server/Session/DesktopProcess.cs`, `HiddenDesktop.cs`, Capture); CI is `windows-2025` only | build R 421 min n=1048 med 21s |
| godot-mcp | integration tests behind `tools/hidden-desktop.ps1`, `run.ps1 itest-csharp` | Godot C#, dotnet, pwsh | no | the script's header says "it is Windows only": `CreateDesktopW`, window station, job object, DXGI/audio capture | R 1,059 min n=303 med 188s plus L 1,082 min n=572 med 97s |
| ops-tower | `cargo test/nextest/clippy`, `cargo build --release -p ops-tower` | Rust 1.89, nextest | no | main crate has `cfg(windows)` `windows-sys` deps (`Cargo.toml:43-55`); CI builds it on `windows-latest` and only `--workspace --exclude ops-tower` on ubuntu (`ci.yml:10,71-88`) | R 346 min n=310 |
| ops-tower | `pwsh tools/browser-test.ps1` | pnpm, Playwright, node | no | drives the Windows ops-tower binary | R 243 min n=141 med 78s |
| rustling-tulip | `cargo nextest/clippy`, `pwsh ./rt.ps1 test` | Rust 1.92 | unknown, probably not | Windows-first: Credential Manager, ConPTY, `rt-tracer.exe` (`CLAUDE.md:125,176`); `cfg(windows)` in 8+ files, `cfg(unix)` in 3; no CI workflow; not tried | nextest R 1,268 min n=755 med 70s; clippy R 292 min |
| agent-park | `cargo nextest/clippy/build` | Rust | unknown | 6 files with `windows` markers, 1 with `unix`; not read further | R 460 min n=435 |
| gate | `cargo clippy`, `cargo nextest run` | Rust | no | this repo is the Windows layer; the black-box suite drives `gate.exe` | nextest R 1,702 min plus L 4,075 min; clippy R 248 min |
| towercab-3d | `pnpm run typecheck`, `cargo clippy` | pnpm/node, Rust (Tauri) | yes by read, unrun | `tsc --noEmit`; its `release-macos.yml` builds aarch64 natively; `windows-sys` is `cfg(windows)` (`src-tauri/Cargo.toml:65`) | low volume |
| race-anywhere | `uv run pytest -n 4`, `ruff`, `ty` | uv, Python | likely yes, unrun | pure Python per `CLAUDE.md:11-18`; native wheels unchecked | L 136 min n=59 med 131s (`uv run`, not routed today) |
| race-anywhere | `pwsh tools/unreal/package.ps1` (Win64 package) | Unreal 5.8 | no | builds the `Windows\RaceAnywhere.exe` target; UE on a Mac builds Mac targets, not Win64 | R 22 min n=5 |
| worktree-sweep | `dotnet build/test/format` | dotnet | likely yes, unread | same shape as the other .NET repos | R 180 min n=263 |

Not examined (not on the GATE-192 list, routed in `remote.json`): ue-mcp (Unreal, npm), herdr-gpui (WSL step), collie (bun), vatsim_control_recs (uv), claude-config (GL probe on the builder's desktop). Their folders were not found on the PC.

## B. What in gate assumes the box is Windows

**Does it compile off Windows?** No for `gate`, `gate-win32`, `gate-desktop` (design table `design.md:3640-3645`, and `Cargo.toml`). Yes by declaration for the five pure crates (above; unmeasured). `cfg(unix)` appears only in a test helper (`crates/gate/src/resolve.rs:94`). The `#[cfg(not(windows))]` arms in `gate-state/src/claims.rs:316` are test stand-ins.

**Sizes (measured, `wc -l`, tests inline):** `crates/gate/src/boxside/` about 22,200 lines in 24 files; `crates/gate-win32/src/` about 16,100 lines in 26 files; `crates/gate/src/remote/` about 29,400 lines.

### Per module

| Module | What it does | Win32-only parts | Neutral parts | On a Mac |
|---|---|---|---|---|
| `boxside/mod.rs` (276) | verb dispatch, request read | `set_default_dll_directories_system32`, `check_installed_file` tripwire (`mod.rs:47,105`): box runs elevated, refuses an install any other process of its user could replace | request line: base64 JSON, 16 MiB cap (`mod.rs:39,134`) | keep the dispatch; replace the tripwire with an owner/mode check (`fstat`, uid 0 or the build user, no group/other write) or drop it for a single-owner Mac |
| `boxside/payload.rs` (1,739) | payload parse, refusals | none of its own | `Key/Bare/Work/Lock/Index/Snapshot/Env/...`; paths arrive as PC-built strings `D:\gate-remote\work\<key>` (tests `remote/payload.rs:506-509`) | reusable if moved to a shared crate; path words need a separator per builder |
| `preflight.rs` (1,298) | bare repo and work folder, version/boot/sdk/tool lines | `open_beneath_as`, `rename_beneath*`, `delete_opened` (handle-relative ops, no links), `gate_win32::spawn` (20 refs) | the line protocol | `openat`-style or plain `std::fs` with symlink checks; `git` calls as `Command`; sdk line from `dotnet --list-sdks` |
| `boot.rs` (103) | boot time line | `gate_win32::boot_time` (`boot.rs:41`) | `round_trip` formatting | `sysctl kern.boottime` |
| `run.rs` (4,147) + `ceiling.rs` (281) | lock, checkout, restore, launch, watch, back commit | restricted token (`token_facts`, `restricted_token`), job object with DACL (`box_job_descriptor`), `OpenedJob`, `own_session`, `breakaway_refused`, `Share` locks, `current_user_sid`, PATH/`Git\usr\bin` first, `.cmd`/`.bat` launch via cmd, `\\?\` handling | ceiling/backstop arithmetic (`gate-watchdog`), env refusals (`gate-args/values.rs`), tagged lines | `setsid`/`process_group(0)`, `killpg` on stop and ceiling, `flock` on the work lock, `posix_spawn`; no restricted token; the Mac's single user is the trust boundary |
| `stop.rs` (314), `inflight.rs` (141) | stop by named job; count held locks | named job `Local\gate-remote-<key>-<nonce>` open, lock probe by open with no sharing | request shape | pid-group file under the work folder, `kill(-pgid)`; `flock(LOCK_NB)` probe |
| `forward.rs` (951), `heartbeat.rs` (535) | stream forwarding, `gate-remote-heartbeat` | pipe cancel I/O, zero-byte `WriteFile` probe, WSL VM CPU (`heartbeat.rs`) | line splitting (CR, LF, lone CR), `gate-watchdog` pump | `std::process` pipes, `SIGPIPE`/`EPIPE` for a gone client; CPU from `proc_pidinfo`/`getrusage` |
| `msys.rs` (1,035) | MSYS keeper (`cat.exe`) against the MSYS set-up lock | all of it (Git for Windows runtime) | - | not needed |
| `tools.rs` (434), `packages.rs` (1,338) | `dotnet tool restore`, pnpm/bun/npm restore | `check_plain_file`, jobs, restricted token | lockfile hashing, records | `Command` plus the same records; pnpm/bun/npm exist for arm64 |
| `probe.rs` (1,422) | tool probes | registry `netfx-sdk`, `Godot_console.exe`, Unreal folders | `rust`, `node`, `pnpm`, `pwsh`, `git-lfs` version lines | Godot path `Godot.app/Contents/MacOS/Godot`; `netfx-sdk` absent (Unreal on Windows only) |
| `inputs.rs` (2,391), `lfs.rs` (1,054), `outputs.rs` (813) | ignored inputs, LFS store, outputs archive | 56, 22 and 4 `gate_win32` refs: `open_beneath`, file ids, protected DACL on every folder, "an unelevated box, whose folders are labelled Medium, refuses every call" (`lfs.rs:7-9`) | the tar.zst format, manifest and SHA-256 diff (`crates/gate/src/outputs/{manifest,hash,archive,glob}.rs`; `archive.rs`/`walk.rs` have 7 refs each) | later steps; not needed for a build-and-test route |
| `register.rs` (51), `prune.rs` (459) | register desktop task; prune idle work folders | `register_task` (COM Task Scheduler), `names_beneath`, `delete_tree_opened`; the prune task is a Windows scheduled task logging to the Event Log source `gate-prune` (`docs/routing-a-repo.md:56-58`) | age rules | `register-desktop`: never. `prune`: a `launchd` plist with `StartCalendarInterval`, plain `fs` |
| `desktop/`, `run/desktop_run*` (about 3,400) | run on the logged-on desktop | helper exe, pipes, adoption, sessions, Task Scheduler | decision tables in `desktop/lines.rs`, `verify.rs` | never; the desktop route stays on the Windows builder |
| `crates/gate-win32` (16.1k) | jobs, tokens, DACLs, named mutex/event, pipes, console, tasks, registry, GPU, WSL CPU | all | - | no equivalent crate needed for a first box; `libc` calls instead |
| `crates/proc-table` | process snapshot | `win32.rs` (cfg-gated) | `ProcessTree` model | not needed on the box |
| `crates/gate-args` | argv, `cmdline.rs` C-runtime quoting, `batch.rs`, `program.rs` PATHEXT | one `cfg(windows)` (`values.rs:816`) | whole CLI, `BoxRequest` (`boxcmd.rs`), env refusals | reuse; PATHEXT/batch rules do not apply to a Mac word |
| `crates/gate-router` | decide, config, like, key, rewrite, warm | none | all of it; but the rewrite and key are Windows-shaped: `rewrite.rs` (drive letters, `subst`, `\\?\`, share, device), `key.rs` (`DEVICE_NAMES` CON, NUL, COM1...; key hashes the lower-cased PC root) | reuse; the box-side path form is a PC-side concern (see C) |
| `crates/gate/src/remote/` | the PC client | all of it is PC-side and Windows (`gate_win32`: `git.rs` 14, `held.rs` 14, `syncback.rs` 7) | the protocol | stays on the PC; the changes are the per-builder constants below |
| `remote/transport.rs` | ssh words | `BUILDER_ROOT = D:\gate-remote` (`:258`), receive/upload pack through `pwsh -NoProfile -NonInteractive -Command git-receive-pack` (`:259-260`), `ssh.exe` (`:262`), push URL `host:D:/gate-remote/<repo>.git` (`:299`) | `ssh_words`, options (`:17-37`) | constants per builder: root, pack commands (a Mac login shell has no `pwsh -Command`), box line |
| `remote/payload.rs` | the box call line | `BOX_EXE = "C:\Program Files\gate\bin\gate.exe"` literal (`:18`) | - | a per-builder box command (e.g. `~/bin/gate box`) |
| `remote/install.rs`, `outputs/` | install of outputs into the PC | PC-side; box tree path ends in one `\`; `-p:PathMap=<box tree>\=<PC root>` (`design.md:1683-1687`) | - | box path form per builder |
| `push.rs` | install `gate.exe` and helper to each listed builder | an elevated `pwsh -EncodedCommand` script into `C:\Program Files\gate\bin` with ownership and link checks (`push.rs:58-132`) | exe hash compare | a Mac: `scp` or a build on the Mac; `push` must skip or branch by builder OS |
| `tools/builder/` | `install-toolchains.ps1` (895 lines), `wsl-setup.sh`, test script | all: winget-style installs, VS Build Tools, WSL, tasks, ACLs, `icacls` | - | a separate Mac script (brew or pkg: git, dotnet, pwsh, node/pnpm/bun, uv, Rust, nextest, git-lfs, Godot) |

CRLF: output forwarding already accepts CR LF, LF and lone CR and emits LF (`forward.rs:1-3`), and repo files are LF, so CRLF is not a blocker; `.cmd`/`.bat` resolution (`gate-args/batch.rs`) is Windows-only and unused for a Mac word.

### Size of a macOS `gate box` for a first routed command (estimates)

Two ways; neither is a flag flip:

1. **Port `gate` to unix.** cfg-gate 64 files and the `Cargo.toml` deps, split `gate-win32` use behind a trait. Large, touches the slot loop and watchdog too; contradicts the non-goal. Not recommended for a first step.
2. **A new box crate/bin that speaks the same wire** (the hidden `gate box` words, payload, tagged lines, statuses), reusing `gate-args` (`boxcmd.rs`, `values.rs`), `gate-watchdog` (heartbeat, ceiling) and the payload parser, which first moves out of `crates/gate/src/boxside/payload.rs` (1,739 lines, no Win32 of its own, estimate). Version text must equal the PC's (`design.md:2597`, `gate-desktop/src/version.rs`): build the Mac box from the same commit with `GATE_BUILD_ID` set.

Estimated new code for option 2 (lines are my estimates; the existing counts above are measured):

| Verb | Mac piece | Est. lines |
|---|---|---|
| `preflight` | work folder and bare repo with `git init --bare`, version/boot/sdk/tool lines, tool probes | 600-900 |
| `run` | lock (`flock`), checkout of the snapshot, `dotnet tool restore`, spawn in a new process group, forward, heartbeat, ceiling, back commit, tagged lines | 1,200-1,800 |
| `stop` | pgid file and `killpg` | 150-250 |
| `inflight` | `flock` probe | 50-100 |
| `outputs` (build-only routes can send none) | reuse `outputs/` after removing 7+7 `windows-sys` refs | 300-500 |
| `inputs`, `lfs` | not for a first route (no LFS or inputs in opening-hand, in-the-sky, yaat, yaat-server) | 0 first |
| `prune` + launchd plist | plain `fs` walk; plist | 200-300 |
| `heartbeat` | reuse the pure pump | 100-150 |
| total | | about 2,600-4,000 new lines, against 22,200 lines in the Windows `boxside/` |

## C. What the router and client need for two builders of different OS

Today (measured by reading):

| Question | Answer | Where |
|---|---|---|
| How many builders does `remote.json` name? | `host` plus optional `hosts`; `hosts` only for `gate push` and `gate check --builder` | `config.rs:36-40,234-252`; `push.rs:455-461`; `check/builder.rs:82-94` |
| How does the router pick one? | It does not: `Target.host = config.host()` for every routed run | `decide.rs:1054` |
| Does a builder carry capabilities (OS, tools)? | No config. Capability is read live: the preflight reports sdks/workloads/tools, and `remote/parity.rs` plus `toolchain.rs` (`KNOWN_TOOLS`, the 10 names) judge the repo's `.gate/toolchain.toml`. A mismatch falls the run back to the PC (`parity`, status 75), it never tries another builder | `toolchain.rs:26-37`; `design.md:1914-1916`; `FallbackReason` `journal_writer.rs:230-280` |
| Remote pool | one pool `Local\gate-remote-slot-<i>`, size `slots`; no host in the name | `design.md:712`; `claims.rs:84,128`; `config.slots()` |
| Routed lock | `Local\gate-routed-<hash of key>`, no host | `GLOSSARY.md:189` |
| PC caches per worktree | keyed by work-folder key only, no host: outputs pending, outputs/inputs/sent-build hashes, nothing-back record (a nothing-back record does name a host), warm record (names a host) | `gate-state/src/v1.rs:115-158,238-256` |
| Work-folder key | `<leaf>-<8 hex of SHA-256 of the lower-cased PC root>`, same on every builder | `key.rs:24-35` |
| Journal and durations | tagged with `host`; durations are per builder | `journal_writer.rs:391`; `durations.rs:1035` |
| Fallbacks | `full`, `busy`, `unreachable`, `parity`, `rebooted`, `tools`, `layout`, `version`, `client`, `inputs`, `cold`, `desktop`, `install`, `lfs`: all end in a local run | `journal_writer.rs:262-280` |

What must change (ordered by need for the first route):

1. **Builder choice.** A `repos.<repo>` entry is a pattern list with no builder. Options: `repos.<repo>` objects with `builder` and patterns; a top-level `builders: {name: {host, os, root, slots, ...}}` with `repos` naming a builder per pattern group; or an ordered list per repo with first-capable-wins. The decision stays in `gate-router` (pure) and needs a `Target.host` chosen per command.
2. **Per-builder constants** now global: root (`transport.rs:258`), pack commands (`:259-260`), box exe line (`payload.rs:18`), path form of the box tree (backslash, drive letter), `ssh` options. Move them into the builder entry or derive from `os`.
3. **Per-builder pools and locks.** A remote slot pool per builder (the Mac has its own core count; the research note lists 15 or 18 cores with no SMT), a routed lock and PC caches that include the builder, else a run to the Mac and one to the Windows builder for one worktree share a lock and a hash cache that describe different boxes. Journal `host` already exists.
4. **Capability and the busy/down fallback.** Parity today is checked after the builder is chosen and its only exit is local. Needed: when the first-choice builder is `busy`, `full`, `unreachable` or fails parity, try the next capable builder before falling back, and never send a command to a builder whose `os` cannot run it. Both are PC-side loops around `remote/client.rs` `Attempt::phases`.
5. **Per-OS allow lists.** A pattern that names `Godot_console`, `.exe` or `C:\...` must stay on Windows. The `-like` matcher runs on rewritten words, so a repo can list Mac-safe patterns separately; the router has no OS tag on patterns.
6. **Outputs.** Default globs are `**/bin/**` and `**/.godot/mono/temp/bin/**` (`config.rs:25`), and cargo's `target/*/*.exe`: a Mac build must not bring those back to a Windows worktree.

   For Mac routes set `outputs: []` (the existing form for gate, rustling-tulip, `remote.json`) and keep build-then-`--no-build` pairs on one builder: a Mac `dotnet build` leaves the PC's `bin` stale and the nothing-back record makes the next local `--no-build` rebuild (`design.md:1636-1659`). `yaat test-all.ps1` as one routed command avoids the split (build and test in one box run).
7. **`.ps1` wrappers and Windows paths in words.** `pwsh tools/test-all.ps1`, `pwsh -NoProfile -File ...` need `pwsh` on the Mac (`toolchain.toml` `pwsh = "7.4"`; macOS has 7.6, per the research note). In-box `tools/gate.ps1` degrades to "running the command without slots or watchdog" when no `gate.exe` is found (`tools/gate.ps1:26,437`), so nested gates run ungated on a Mac. Exec bit: scripts committed from Windows may lack `+x`; the `pwsh -File` form avoids it.
8. **Git, LFS and inputs to a second box.** `git push host:<root>/<repo>.git` uses the builder's own bare repo per builder, so snapshots and back refs are naturally per box. The cost is the first push per builder (full history). LFS (`remote/lfs.rs`, box `lfs`) and inputs need the box verbs; none of the first-step repos use either. Case-insensitive APFS by default: the research note's case and `core.autocrlf` notes apply (`research/...:81,93`).
9. **Docs and tests.** The suite's remote cases use a fake ssh (`bin/fake-ssh.rs`) and a local transport; a second builder needs a two-builder case there.

## D. Ranking and first step

Score = recent routed load (measured 10-18..10-20) x portability (1 yes by CI, 0.5 yes after a small change, 0 no). Time saved on the PC is zero for work already routed to the Windows builder; the gain is capacity (see Fallback demand) and speed if the Mac is faster (unmeasured; the research note says near or slightly under a 9950X for sustained all-core).

| Rank | Work class | Load (min / 2.7 d) | Portability | Needs from B and C |
|---|---|---|---|---|
| 1 | yaat `pwsh tools/test-all.ps1` and `dotnet build/test/format` (+ yaat-server) | 3,691 | 1 by CI | Mac box with preflight, run, stop; sibling pair layout; wasm-tools workload; per-repo builder choice |
| 2 | delve `dotnet build/test/format/csharpier` | about 3,000 of 3,345 | 1 | box; builder choice; `dotnet.exe` full-path pattern dropped for Mac |
| 3 | opening-hand `dotnet` + `uv run pytest` | 1,083 | 1 | box; builder choice; no siblings, LFS or inputs |
| 4 | Godot headless `--build-solutions` (delve, opening-hand) | about 600+ in the whole window | 0.5 | Godot resolution without `.exe`, `GODOT_PATH` per builder; import caches (`inputs`) would need the box `inputs` verb |
| 5 | rustling-tulip, agent-park cargo | 1,034 + 577 | unknown | first check whether they compile on macOS |
| 6 | towercab-3d typecheck/clippy, race-anywhere pytest, in-the-sky | small | 1 | box |
| none | godot-mcp (Windows desktop), gate, ops-tower, Unreal packaging, Delve smokes, anything on `desktop` | - | 0 | stays on the Windows builder |

**Smallest first step that routes something real:** opening-hand's `dotnet build/test/format` (1,083 min recently, no siblings, no LFS, no inputs, CI proves the commands on Linux). Order:

0. Measure before building anything: on the Mac, clone opening-hand and run the build, test and format by hand; compare with the Windows builder's medians above (37s, 28s, 48s). No gate change.
1. The Mac box with `preflight`, `run`, `stop`, `inflight` (option 2 above), reached by a second `hosts`-style entry.
2. Router: a builder per repo (or per pattern) so opening-hand's patterns go to the Mac, with `outputs: []` for it, and the per-builder constants of C.2.
3. Install by `scp` or a build on the Mac (`push.rs` is Windows-only).
4. Then yaat/yaat-server (needs the pair layout on the Mac box) and delve's dotnet classes.

## E. Open decisions

Owner:
- Which repos go to the Mac first (the ranking assumes the .NET repos), and whether the Mac may take work from repos the Windows builder holds today or only classes that cannot route now.
- Whether the Mac is also a dev machine (interactive login, a logged-in GUI session, FileVault autologin) or a headless server: it decides the trust model for the box (single user versus elevated), sleep settings, and whether the Mac may ever run Godot windowed or Playwright headed.
- Whether Mac exports of Godot (delve already ships a macOS dmg from CI, `release.yml:203-299`) or an Unreal Mac target matter. If so, signing needs a keychain in an ssh session (research note section 4); that is out of scope for routed checks.
- Whether gate's non-goal "Running on Linux or macOS" (`design.md:23`) is lifted for the box side only, or entirely.
- Whether the Mac is a mini or a laptop (the research note assumes a Mac mini M5 Pro; the issue says "Apple-silicon Mac (M5 Pro)"): thermals and always-on use.

Technical, not settled by the code:
- One box codebase (port) or a separate unix box crate (this note leans to the separate crate).
- Config shape for builders (C.1) and whether capabilities are declared (`os`, tools) or only probed.
- Pool and lock naming per builder (C.3) and the migration of existing PC caches (`remote\<key>.*`).
- Whether the Mac box should enforce an install trust check at all.
- Whether a failed Mac attempt falls to the Windows builder or the PC.
- Whether the pure crates build for `aarch64-apple-darwin` (unmeasured), and whether yaat's `EnableWindowsTargeting` projects and `wasm-tools` work on macOS (unmeasured).
- Which L rows are historic versus routing gaps (the journal does not separate them; `routing-a-repo.md:19` says a `--local` shows as `notRouted: --local` with a reason, 116 in the window, measured).

## DOC DRIFT

- `docs/design.md:3651` ("The pure crates build with `cargo clippy` and `cargo test` on Linux in CI") vs the repo: no CI workflow exists (`ls -a` shows no `.github/`; `git ls-files` has no `.github` entry). Doc is wrong, or the CI lives elsewhere; owner `docs/design.md` Crate layout.
- `docs/routing-a-repo.md:133` ("remove it from `hosts`, or remove `host` when it is the only builder", as the way to drain a builder) vs `docs/design.md:1115` and `crates/gate-router/src/decide.rs:1054` (a routed run goes to `host` alone; `hosts` only reaches `gate push`): removing a `hosts` entry drains nothing, and a `host` entry cannot be removed while others stand. The procedure is wrong for any builder other than `host`; owner `docs/routing-a-repo.md`.
- `docs/GLOSSARY.md:29` ("Builder: a remote Windows machine") will be wrong once a Mac is one; also the `Builders list` (`:31`) says routing uses `host` alone. Owner `docs/GLOSSARY.md`.
- `docs/research/2026-10-20-macos-builder-toolchains.md` is untracked and absent from `docs/README.md`. Owner `docs/README.md`.

## TASK INDEX GAP

This answer took the Task Index plus about 60 reads and searches (design.md sections, config/decide/transport/payload/push, 8 Cargo.toml files, the caller repos, the journal). Missing rows:

- "Route to a second builder, or a builder of another OS": `docs/design.md` (remote.json, Routing) → `crates/gate-router/src/{config,decide}.rs` (`Config::builders`, `settle`) → `crates/gate/src/route.rs` → `crates/gate/src/remote/{mod,transport,payload,client,parity,toolchain}.rs` → `crates/gate-state/src/v1.rs` (per-key PC files) → `crates/gate/src/push.rs` → `docs/routing-a-repo.md` → `docs/research/2026-10-20-macos-builder-toolchains.md`.
- "Add a box verb or box behaviour for a non-Windows builder": `crates/gate/src/boxside/mod.rs` → `crates/gate-args/src/boxcmd.rs` → `boxside/payload.rs` → the Windows module being replaced (table in B).

## MISSING ENTRY POINT

none for `docs/README.md` and `docs/ARCHITECTURE.md` (both exist, the latter has a Task Index). `docs/agents/explore.md` (the overlay) does not exist. No glossary entry for "work-folder key" under that name: `Key folder` (`GLOSSARY.md:117`) covers part of it.
