---
name: gate-nextup
description: Profile for the user-level `nextup` skill in the gate repo (the Rust gate, `gate.exe`) — loaded by `nextup` at its step 0 for this project's plan order, gates and traps. Not a loop of its own; invoke `/nextup`.
---

# gate profile for `nextup`

The generic loop is the user-level `nextup` skill; this file supplies only what is specific to the gate repo. Every unset field takes `nextup`'s no-profile default.

siblings: none
linear: gate

## Plan and tracker

- The plan lives in Linear: every task is an issue in team GATE, per `~/.claude/docs/plan-operations.md`; `docs/plans/MAIN.md` is its generated snapshot, never edited by hand.

  Project order, which is the order the queue is worked: `Design`, `gate-state move`, `Local gate`, `Remote`, `Cutover sweep`, `After cutover`, `Backlog` (claude-config's `docs/plans/gate-rust-port.md`, "Steps"). `After cutover` holds the Rust gate's new features (cores hint, learned durations, `--slot auto`), built once GATE-9 has landed.
- **What the Mac mini runs.** A route sends a repo's tests, format and lint to the Mac mini. A build whose outputs are launched on the PC afterwards stays on the Windows builder or the PC, until GATE-696 (cross-built Windows outputs) lands. On a Windows builder, a routed `dotnet build` of a project brings that project's `bin` back even when the repo's `outputs` list leaves it out (GATE-788).
- **Focus fence: remote routing for every caller repo.** Until each repo GATE-192 names (delve-the-dungeon, godot-mcp, yaat, yaat-server, opening-hand, in-the-sky, rustling-tulip, towercab-3d) routes, or has its trial failure booked as GATE-192's ruling says, the queue holds only the items on that path, as a release fence does: the Urgent issues (each repo's next step) and the High ones (the tier after).

  Everything else waits, however high it sits in the project order; a finding off the path gets an **add**, not a fix. The path is kept in Linear's priorities: a step that lands moves the next one up.
- The port is the owner's top priority. claude-config's CC-212 is its umbrella; the PowerShell gate in `~/.claude/tools/gate/` is frozen to bug fixes, which stay claude-config's.
- Routing a repo (its builder setup, `remote.json` entry, routing trial and what stays local) follows [docs/routing-a-repo.md](../../../docs/routing-a-repo.md); a trial runs the full command routed once and reruns locally only what failed.
- An issue that blocks a caller repo's adoption of remote gates, reported by another repo's agents (a repo inbox request, a trial failure), is created High, or raised to High, without asking the owner: this is the owner's standing ask for that class, which the focus fence then admits.
- A steer or a finding the item does not fix gets an **add**, in the project whose files it shares, else in `Backlog`. A change another repo needs (claude-config's launcher, docs or skills; ops-tower's use of `gate-state`) goes as a repo inbox request.
- Tracker: **triage** as plan-operations says.

## Decisions

The owner wants this project worked as autonomously as possible and holds no strong views on technical choices. The decision round settles every technical decision itself: formats, APIs, crate layout, Win32 mechanics, seams and test shape. It picks the option with the best worst case, quotes the reasoning into the brief, and **settle**s it into the issue.

Output text the gate prints for the agents that invoke it (its lines, refusals and failure messages) is designed by the round too, as well as the round can: the owner does not review its wording.

The owner is asked only about what they know as a user and a developer and the round cannot settle from the code: a default that changes how a caller repo's runs behave, a change to a workflow in those repos, or anything that touches their machines beyond the builders' gate setup. A feature branch counts as technical: the round decides it and says so.

The builders' setup is the session's own work, with the owner's standing permission: a session installs the toolchains and tools a routed repo needs and changes a builder's configuration (ACLs, environment variables, installs) directly over ssh, without asking and without a request to claude-config first.

A change to a builder's toolchains, tools, work root or git config is also made in this repo's `tools/builder/install-toolchains.ps1` in the same item, so a new builder gets the same setup. A change to the machine's provisioning (the private half) still goes to claude-config as a repo inbox note with what changed and the exact commands. The repo-is-public rule still holds: no builder host, address or user goes into a commit or an issue.

The primary goal is remote gates working for every caller repo under `X:\dev` as soon as possible. When ordering, splitting or merging work, prefer whatever routes the next repo soonest, and say the choice in one line.

Functional work comes before edge cases and hardening. The owner is the gate's only user, so new behaviour and routing more of the callers' work outrank security hardening and fixes for edge cases no caller's run hits today.

Such an item, and such a finding from a review or an invariant pass, is **add**ed with no priority (in its project, else `Backlog`) and never raised to Urgent or High. A defect that breaks or blocks a caller's routed run is not an edge case, and a flaky or timing-out test stays Urgent.

Merging is the agents' call too. Once every item under a feature marker has landed and its gates are green, the session runs `ship` on the branch and merges the feature PR without asking.

## Agents

- Explore: `Explore`. Behaviour questions about the PowerShell gate point the explorer at `~/.claude/tools/gate/` by path, and at the behaviour inventory, `docs/research/2026-10-09-powershell-gate-inventory.md`.
- Implementer: `implementer`. Review: `code-review` at `high`. No routine `security-auditor` pass: the owner is the gate's only user ("Decisions", functional work first); run one only when the owner asks.
- Concurrency ceiling: five implementers.

## Pre-loop hooks

- **Weekly local-run census.** On a full start, when the newest `docs/research/*-local-run-census.md` is 7 or more days old, run the `local-run-census` skill (`.claude/skills/local-run-census/SKILL.md`) over the days since that note, and book its findings as the skill says, before the slice is cut. Its bookings join the queue like any **add**.

## Gates

test mode: unstable

stable milestone: Cutover sweep

The stretch runs through the remote cutover and the caller sweep: the open issues of the projects `Remote` up to GATE-46 and `Cutover sweep`; /nextup suggests `stable` once `Cutover sweep` is done. What ships is guarded by the release step, not by each item's tests. The release step is the reinstall: `gate.exe` is the live gate every session's builds and tests run through, so the reinstall ("Reinstall" below) runs only after the full suite passed on the commit being installed.

Every command runs through the launcher: `pwsh -NoProfile -File tools/gate.ps1 -Log .tmp/<name>.log -TimeoutSeconds <n> -Slot heavy|light|critical -- <command>`. Tests run with cargo-nextest; its `--profile` picks a nextest config profile, so the cargo build profile is `--cargo-profile test-release`. The workspace has no doctests.

| The diff touches | Gate commands |
|---|---|
| any Rust | `-Slot light -- cargo fmt --all --check`, then `-Slot heavy -TimeoutSeconds 900 -- cargo clippy --workspace --all-targets --all-features --profile test-release -- -D warnings`, then the same clippy without `--all-features` (the build `gate.exe` ships; code that names a `test-seams` item outside its `cfg` compiles only with the feature), then the tests below |

The tests, by test mode:

- **unstable** (now): each item's last gate runs its reached tests, the tests its change reaches, from [docs/test-map.md](../../../docs/test-map.md): `-Slot heavy -TimeoutSeconds 900 -- cargo nextest run --workspace --all-features --cargo-profile test-release -E '<the union of the filtersets of every row the diff touches>'`; a diff that touches a central file, or a file the map does not list, runs the full suite.

  The brief writes that union from exactly the rows its diff touches, with no module added for safety, and each step's proving command runs the step's named tests alone: on a PC whose heavy slots other gates share, every extra module in a reached set costs minutes of wall time.

  The full suite, `-Slot heavy -TimeoutSeconds 1800 -- cargo nextest run --workspace --all-features --cargo-profile test-release --no-fail-fast` (`--all-features` builds the black-box suite, which needs the `test-seams` feature), runs before every reinstall (the release step), before every push (an unstable repo pushes at the /nextup checkpoint only), and before a cutover or a feature-branch merge; a red full run holds them and becomes the next item, Urgent.
- **stable**: each item runs the full suite as its last gate. A step's proving loop still narrows to the test map.

The map is kept true by the docs sweep: an item that adds a suite module, a seam flag or a source area adds its row.

A test that asserts on timing must hold on a busy machine: no wall-clock bound a loaded PC can cross (the PowerShell suite's CC-209 is what this avoids). A test that needs the machine alone is marked `#[ignore = "needs the machine alone: run with -Slot critical and --ignored"]` and named here.

A case that fails once and passes alone, or runs to nextest's per-case limit, is never a known red left in the backlog: its issue is booked or raised Urgent at once and is worked next (the `load-proof-tests` skill, "A flaky or timing-out test is Urgent"). A brief may name such a case as not the implementer's for that one run only.

Before working a flake seen in a worktree's run, check that the run's base holds the case's latest fix: `git log -S '<the failure text>'` finds the commit that last changed it, and `git merge-base --is-ancestor <fix> <base>` says whether the tree has it. GATE-760 was GATE-754's flake, seen in a tree cut before GATE-754 landed.

A routed suite case that ends -1 (0xFFFFFFFF) or ABORTs with no output was ended from outside the gate: the gate's own kills give 124, 1 or 0xC000013A, a job close gives 0 and a console close 0xC000013A. Before suspecting the case, check the journal (`%LOCALAPPDATA%\gate\v1\events*.jsonl` and `archive\`) for other repos' routed runs that overlapped it on the builder.

The machine-alone case is `stall::a_command_whose_only_work_is_a_child_outside_its_job_is_not_stalled`.

The machine-alone cases also include the escapee kill cases `stall::a_stall_kill_ends_a_child_outside_the_job_whose_launcher_has_ended`, `stall::a_stall_kill_ends_a_child_outside_the_job_whose_launcher_exited_at_once`, `ceiling::a_ceiling_kill_ends_a_child_outside_the_job_whose_launcher_has_ended` and `interrupt::a_ctrl_c_ends_a_child_outside_the_job_whose_launcher_has_ended`.

The machine-alone cases also include `remote_inputs::a_put_whose_box_works_silently_after_the_archive_is_sent_still_passes`.

`boxside::msys::tests::a_start_up_killed_beside_the_keeper_leaves_the_next_sh_working` is machine-alone on a builder: run it over plain ssh on an idle builder, never routed and never on this PC, whose long-lived MSYS processes hide the fault it proves.

`boxside::box_run_lets_an_elevated_session_s_inner_gate_judge_the_box_job` needs an elevated session on a builder: run it by hand over plain ssh with `--run-ignored only`, after a push of the build that carries it.

That case proves "not stalled" inside a short window, with an escapee that runs at BelowNormal and can be starved into a false STALLED. When the diff touches the watchdog, also run `-Slot critical -Reason "the watchdog timing case" -TimeoutSeconds 600 -- cargo nextest run -p gate --all-features --cargo-profile test-release --run-ignored only -E 'binary(suite)'`.

`stall::a_stall_kill_ends_a_packaged_app_the_command_started` is machine-dependent: it runs only where the first `pwsh` on PATH is the Store (WindowsApps) alias, and skips on this PC once the Store pwsh is removed. To drive it, run it through `gate.exe` directly with the alias first on PATH.

To check a timing case holds under load, run it inside `~/.claude/tools/load-burn/load-burn.ps1`, which burns every core only while it holds the machine on a critical slot. A burn takes the owner's whole PC: one burn is at most 120 s (`-TimeoutSeconds 120`), one investigation spends at most about 5 minutes of burn time in all, and a brief that allows load-burn states that budget. Anything longer is asked of the owner first.

load-burn does not starve a BelowNormal process: its critical gate runs the command at High, and Windows hands a child only Idle or BelowNormal, so a test under the burn runs at Normal beside the burners. A fault that shows only in the BelowNormal full suite is reproduced by suspending just the gate under test for seconds at a time (`NtSuspendProcess` / `NtResumeProcess` on the gate.exe the suite started), which costs no burn budget; GATE-128 was found that way.

Suspending gate.exe cannot starve a test stand-in such as `fake-ssh`: to reproduce a stand-in that gets no CPU in time, put a wrapper that delays before running the real one in place of `target\test-release\fake-ssh.exe` (an untracked build file) and restore it afterwards; GATE-488 was found that way.

The same wrapper in place of `target\test-release\fake-tool.exe`, delaying only its `pid-to` step, holds a parked git stub before it writes its pid file. A parked-run case starts other gate.exe calls before the box under test, so a rig picks the box by its seam in the command line (`--test-box-git-armed-by`), not as the first gate.exe child.

To hold a process's end open with no load (a process asked to end but not yet gone), attach as its debugger (`DebugActiveProcess`) and hold its exit-process debug event: it stays in Toolhelp and its handle stays unsignalled until the event is continued.

The debugger does not pass to the children the debuggee creates, so attach to each process itself; to catch a short-lived stand-in, watch for a file the box writes while it is still suspended, since a Toolhelp poll (about 16 ms a snapshot) misses it. GATE-694 was found that way (rig in `.tmp/map-694-rig/` while it lasts).

To make a gate wait on its journal, hold the case root's journal mutex, `Local\gate-<first 8 hex of the SHA-256 of the lower-cased state path>-journal`, for under 2 s: past 2 s the gate drops the line by design. To freeze a child gate outside a journal write, take the mutex first, suspend the child, then let the mutex go; GATE-658 was settled that way.

A gate writes a slot's claim file before its journal take line, on one thread, and stamps the take line's `time` before its journal wait: a case that needs the take waits for the take line and reads the claim after it, never the reverse.

To hold a gate between its claim and its take, hold the case root's light slot before the gate starts (the case folder is `<name>-<test pid>-0`, so the hash is known once the suite process starts), take the journal mutex once the queue line appears, then let the slot go; GATE-813 was settled that way.

The suspend rig freezes a whole process and reads the process tree once per cycle: it cannot starve one thread of a gate, a case whose gate lives under one `-FreezeMs` is never frozen (use about 300 ms frozen, 20 ms running for a short case), and a child that lives less than one running burst is never frozen.

To starve one thread, drive the real `gate_win32` type from a scratch crate under `.tmp/` with an empty `[workspace]` table and `SuspendThread` the thread found by its name (`GetThreadDescription`, e.g. `job-memory`); GATE-812 was found that way.

A stall case whose first output byte opens the window (`--test-stall-from-output`) must not need a later step inside the window: make the line the case reads the first byte, or write a pid with the silent `pid-to` step. To starve a stand-in without changing the job's process count, put a stand-in that implements the steps the case uses, with a delay, in place of `fake-tool.exe`, rather than a wrapper that runs the real one as a child.

Holding a process's exit-process debug event does not hold the exit time `GetProcessTimes` reports, so the job memory counts it ended; to model a killed process a starved thread keeps from ending, hold its attach breakpoint instead. GATE-814 and GATE-815 were settled that way.

The rig needs the case's gate.exe on this PC, but the launcher routes nextest to the builder: build once with `-Local -Reason "the rig needs this PC's gate.exe"` and `--no-run`, then run the suite binary directly under the rig. A fault in a parent gate's timing shows by freezing the child gate it waits on, not the parent (GATE-480). A rig that reads the case folder opens its files with delete sharing, or it causes the case's `os error 32` on folder removal itself.

A builder run over plain ssh has only a pseudo-console, so a fault that needs a real console (as the box gives a routed command) never shows there. Start such a repro with `CREATE_NEW_CONSOLE`, or route it.

Never suspend a thread during its start-up: it holds the loader lock, and the rig deadlocks itself.

`cdb -pv` plus `llvm-symbolizer --relative-address` reads a hung test's stacks without disturbing it.

The suspend rig cannot stretch a box whose own CPU use is small and whose git children live only briefly: a one-target freezer that catches the box at its creation (by the case folder in its command line) and cycles freeze and run on it alone does. GATE-859 was found that way (rig in `.tmp/859-rig/` of its worktree while it lasts).

A race in the Unix box's own file handling is opened deterministically by a `DYLD_INSERT_LIBRARIES` interposer filtered by `fcntl(F_GETPATH)` and argv. The suspend rig cannot do this: it freezes whole processes, on Windows only. GATE-895 was found that way (rig in `.tmp/895-rig/` of its worktree while it lasts).

A rig reaches the Mac only through `cargo nextest run *` (the only command that routes there), and ignored files don't travel. So it goes base64-encoded in a `--config target.<triple>.runner=[…]` word, written as TOML literal strings, since backslashes are refused.

Under `--success-output immediate`, a runner that runs the case several times must pass a `--list` call straight through.
To hold a box's command launch from outside, a case that sets no box seams of its own inherits `GATE_FAKE_SSH_BOX_SEAMS="--test-box-park-launch <path>"` from the suite's environment. The box writes `<path>.parked` once parked, and a rig can then `SuspendThread` its `job-memory` thread, found by `GetThreadDescription`. GATE-890 was found that way (rig in `.tmp/890-rig/` of its worktree while it lasts).

Holding a case's `out.log` open with delete sharing keeps its text past the case's drop.

An exited process's image cannot be read even while a handle keeps it, so a process line read after the exit says `?`.

A suite guard clocked from a call's start times the whole run, which a loaded full suite stretches: a hang guard is clocked from the event it guards, or is the suite's `HANG_GUARD`.

A gated command run in a worktree from elsewhere takes an absolute `-Log`: the launcher reads a relative one against the caller's folder before `-Cd` (design.md P5), so it lands in the main checkout's `.tmp`, where two agents' logs of one name overwrite each other. Every brief that names `-Cd` names its logs by absolute path under that worktree.

A log under a worktree does not move the run there: a gate snapshots the worktree of the folder it runs in, so every gated command for a worktree takes `-Cd <worktree>` (a routed run logging into another worktree of its repo is refused since GATE-901). A routed push of about 200 bytes (two commit objects) carried no tree change.

A routed cargo run builds only the targets its features enable: a suite or test file behind `required-features = ["test-seams"]` (gate's `suite`, gate-box-unix's `tests/run.rs`) is skipped silently without `--all-features` or `--features test-seams` (GATE-795).

A rig that reads `SystemProcessInformation` back to back holds a lock every `CreateProcess` on the PC waits on (a reused start went from 4 ms to 590 ms): sample at 100 ms or slower, or the rig measures itself. ESET's CPU (`ekrn`, 12-35% of a core with other sessions active) is never evidence about one job.

nextest's default profile ends a case after 15 periods of 60 s (`.config/nextest.toml`). A case that needs longer gets its own override there.

A heartbeat test's command sleeps (fake-tool `sleep`), never `wait`, whose 20 ms poll earns CPU ticks on its own.

A measurement that needs a quiet machine (a timing comparison, a phase profile, a before-and-after) runs on `-Slot critical -Reason "<why>"`, never by waiting for other sessions' load to drop. The critical gate drains every gate first, which can take tens of minutes behind a long heavy run, then holds the machine. Keep critical runs short and few, about 5 minutes of hold for one investigation, and ask the owner before going past that.

To time the suite, add `--profile timing` to the nextest command. Its junit report, with every case's seconds, lands in `target\nextest\timing\junit.xml`. To count the git processes a case starts, set `GIT_TRACE2_EVENT=<a folder under target\tmp>` on the run. It costs almost nothing, where a WMI process tracer slows a case four times over.

A commit goes through the gate: `-Slot heavy -TimeoutSeconds 300 -- git commit -F .tmp/msg-<slug>.txt`.

## Docs-owner map

| The diff touches | Owning doc |
|---|---|
| the CLI, an output line, a file or mutex other processes read | `docs/design.md` (and the public `README.md` when callers see it) |
| a crate's layout or a task's file order | `docs/ARCHITECTURE.md` |
| a new term | `docs/GLOSSARY.md` |
| a file format ops-tower reads | `docs/design.md` "Formats", the fixtures README under `crates/gate-state/tests/state/fixtures/`, and a repo inbox request to ops-tower |

The repo is public: no build-box host, address, ssh user or key, provider name, or personal machine path goes into a commit or a GATE issue.

There is no changelog.

## Reinstall

This PC is cut over: every gate on it runs the installed `gate.exe`, and each builder runs the copy `gate push` put there.

After each landing whose diff changes code the local gate runs (the run, watchdog, slots, waiting, launch, the CLI and its subcommands, the journal, gate-state's writers) or code the builders run (`gate box` and its verbs), run the full suite (Gates) on that commit in the main checkout on `main`, and only when it passed, run `pwsh -NoProfile -File tools/install.ps1` from there, then `& "$env:LOCALAPPDATA\gate\bin\gate.exe" push`.

The landing's report names the installed version and each builder's result line.

A landing that changes `tools/builder/install-toolchains.ps1` is followed by a run of each changed step on every builder: stage the landed copy (and `wsl-setup.sh`) in `C:\build-box\gate\` and run it elevated with `-User {builder user} -Only "<step>",...`, as `docs/routing-a-repo.md` ("Builder setup") gives it, once no gate is running there.

`gate push` installs only `gate.exe`, so an installer step that is never rerun stays missing on the builder; the landing's report names each step's `OK` or `FAIL` line.

A push that fails is reported in the landing's report and never undoes the local install; the next reinstall pushes again. A landing that changes only dormant code (the Rust remote client under `test-seams`) waits for the checkpoint, where one install and one push cover them.

A landing that changes `tools/gate.ps1`, the slim launcher, is followed by `pwsh -NoProfile -File ~/.claude/tools/sync-launchers.ps1` (claude-config's): it copies the launcher into every repo that opted in by carrying one and sends each an inbox note to commit it, and never gives a copy to a repo that has none. `-Check` lists the drift.

The install is safe while gates run (rename-aside). A bad build is taken back by reinstalling the previous commit.

## Repo traps

- An installed `gate.exe` runs from `%LOCALAPPDATA%\gate\bin`, never from `target/`: `~/.claude/tools/clean-builds.ps1` empties build folders daily, and a rebuild overwrites an exe live gates hold open.
- The machine's real slots belong to every session's gates. A test of `gate.exe` takes slots under its own test prefix, never the real names, or it fights every session's gates.
- Commits carry `Refs: GATE-<n>` trailers.
- Under Git Bash, a script's `bash` is started by an MSYS fork stub that exits at once, so its Windows parent always reads as gone while its MSYS parent lives. A change that reads process ancestry or parent liveness is proven from a real `bash -c 'bash script.sh'` calling the gate before it lands; GATE-729's first landing refused every bash script's gate and was reverted.
- A suite case that waits for a condition while a box call or a gate it started is still running waits with that call's `running.wait_until` (`support.rs`), never the free `wait_until`: the method fails at once with the program's exit status and output when the program ended before the condition held, where the free one sits out the whole hang guard and then fails with nothing to go on.
