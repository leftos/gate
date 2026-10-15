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
- **Focus fence: remote routing for every caller repo.** Until each repo GATE-192 names (delve-the-dungeon, godot-mcp, yaat, yaat-server, opening-hand, in-the-sky, rustling-tulip, towercab-3d) routes, or has its trial failure booked as GATE-192's ruling says, the queue holds only the items on that path, as a release fence does: the Urgent issues (each repo's next step) and the High ones (the tier after).

  Everything else waits, however high it sits in the project order; a finding off the path gets an **add**, not a fix. The path is kept in Linear's priorities: a step that lands moves the next one up.
- The port is the owner's top priority. claude-config's CC-212 is its umbrella; the PowerShell gate in `~/.claude/tools/gate/` is frozen to bug fixes, which stay claude-config's.
- A steer or a finding the item does not fix gets an **add**, in the project whose files it shares, else in `Backlog`. A change another repo needs (claude-config's launcher, docs or skills; ops-tower's use of `gate-state`) goes as a repo inbox request.
- Tracker: **triage** as plan-operations says.

## Decisions

The owner wants this project worked as autonomously as possible and holds no strong views on technical choices. The decision round settles every technical decision itself: formats, APIs, crate layout, Win32 mechanics, seams and test shape. It picks the option with the best worst case, quotes the reasoning into the brief, and **settle**s it into the issue.

Output text the gate prints for the agents that invoke it (its lines, refusals and failure messages) is designed by the round too, as well as the round can: the owner does not review its wording. The owner is asked only about what they know as a user and a developer and the round cannot settle from the code: a default that changes how a caller repo's runs behave, a change to a workflow in those repos, or anything that touches their machines beyond the builders' gate setup. A feature branch counts as technical: the round decides it and says so.

The primary goal is remote gates working for every caller repo under `X:\dev` as soon as possible. When ordering, splitting or merging work, prefer whatever routes the next repo soonest, and say the choice in one line.

Merging is the agents' call too. Once every item under a feature marker has landed and its gates are green, the session runs `ship` on the branch and merges the feature PR without asking.

## Agents

- Explore: `Explore`. Behaviour questions about the PowerShell gate point the explorer at `~/.claude/tools/gate/` by path, and at the behaviour inventory, `docs/research/2026-10-09-powershell-gate-inventory.md`.
- Implementer: `implementer`. Review: `code-review` at `high`, plus `security-auditor` when the diff touches how a command is launched, quoted or given its environment, the ssh transport, or files other users' processes can write.
- Concurrency ceiling: three implementers.

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

The machine-alone case is `stall::a_command_whose_only_work_is_a_child_outside_its_job_is_not_stalled`.

The machine-alone cases also include the escapee kill cases `stall::a_stall_kill_ends_a_child_outside_the_job_whose_launcher_has_ended`, `stall::a_stall_kill_ends_a_child_outside_the_job_whose_launcher_exited_at_once`, `ceiling::a_ceiling_kill_ends_a_child_outside_the_job_whose_launcher_has_ended` and `interrupt::a_ctrl_c_ends_a_child_outside_the_job_whose_launcher_has_ended`.

That case proves "not stalled" inside a short window, with an escapee that runs at BelowNormal and can be starved into a false STALLED. When the diff touches the watchdog, also run `-Slot critical -TimeoutSeconds 600 -- cargo nextest run -p gate --all-features --cargo-profile test-release --run-ignored only -E 'binary(suite)'`.

`stall::a_stall_kill_ends_a_packaged_app_the_command_started` is machine-dependent: it runs only where the first `pwsh` on PATH is the Store (WindowsApps) alias, and skips on this PC once the Store pwsh is removed. To drive it, run it through `gate.exe` directly with the alias first on PATH.

To check a timing case holds under load, run it inside `~/.claude/tools/load-burn/load-burn.ps1`, which burns every core only while it holds the machine on a critical slot. A burn takes the owner's whole PC: one burn is at most 120 s (`-TimeoutSeconds 120`), one investigation spends at most about 5 minutes of burn time in all, and a brief that allows load-burn states that budget. Anything longer is asked of the owner first.

load-burn does not starve a BelowNormal process: its critical gate runs the command at High, and Windows hands a child only Idle or BelowNormal, so a test under the burn runs at Normal beside the burners. A fault that shows only in the BelowNormal full suite is reproduced by suspending just the gate under test for seconds at a time (`NtSuspendProcess` / `NtResumeProcess` on the gate.exe the suite started), which costs no burn budget; GATE-128 was found that way.

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

A push that fails is reported in the landing's report and never undoes the local install; the next reinstall pushes again. A landing that changes only dormant code (the Rust remote client under `test-seams`) waits for the checkpoint, where one install and one push cover them.

The install is safe while gates run (rename-aside). A bad build is taken back by reinstalling the previous commit, or by deleting `%LOCALAPPDATA%\gate\cutover.flag`.

## Repo traps

- An installed `gate.exe` runs from `%LOCALAPPDATA%\gate\bin`, never from `target/`: `~/.claude/tools/clean-builds.ps1` empties build folders daily, and a rebuild overwrites an exe live gates hold open.
- The machine's real slots belong to every session's gates. A test of `gate.exe` takes slots under its own test prefix, never the real names, or it fights every session's gates.
- Commits carry `Refs: GATE-<n>` trailers.
