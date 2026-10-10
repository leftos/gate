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
- The port is the owner's top priority. claude-config's CC-212 is its umbrella; the PowerShell gate in `~/.claude/tools/gate/` is frozen to bug fixes, which stay claude-config's.
- A steer or a finding the item does not fix gets an **add**, in the project whose files it shares, else in `Backlog`. A change another repo needs (claude-config's launcher, docs or skills; ops-tower's use of `gate-state`) goes as a repo inbox request.
- Tracker: **triage** as plan-operations says.

## Decisions

The owner wants this project worked as autonomously as possible and holds no strong views on technical choices. The decision round settles every technical decision itself: formats, APIs, crate layout, Win32 mechanics, seams and test shape. It picks the option with the best worst case, quotes the reasoning into the brief, and **settle**s it into the issue.

The owner is asked only about what they know as a user and a developer: how they and the agents in the repos that use the gate experience it. That covers output a person or agent reads, waits and their feedback, failure messages, defaults that change how a caller's run behaves, and workflows in those repos. A feature branch counts as technical: the round decides it and says so.

Merging is the agents' call too. Once every item under a feature marker has landed and its gates are green, the session runs `ship` on the branch and merges the feature PR without asking.

## Agents

- Explore: `Explore`. Behaviour questions about the PowerShell gate point the explorer at `~/.claude/tools/gate/` by path, and at the behaviour inventory, `docs/research/2026-10-09-powershell-gate-inventory.md`.
- Implementer: `implementer`. Review: `code-review` at `high`, plus `security-auditor` when the diff touches how a command is launched, quoted or given its environment, the ssh transport, or files other users' processes can write.
- Concurrency ceiling: three implementers.

## Gates

Until the cutover, every command runs through the PowerShell gate: `pwsh -NoProfile -File tools/gate.ps1 -Log .tmp/<name>.log -TimeoutSeconds <n> -Slot heavy|light|critical -- <command>`.

| The diff touches | Gate commands |
|---|---|
| any Rust | `-Slot light -- cargo fmt --all --check`, then `-Slot heavy -TimeoutSeconds 900 -- cargo clippy --workspace --all-targets --all-features --profile test-release -- -D warnings`, then the same clippy without `--all-features` (the build `gate.exe` ships; code that names a `test-seams` item outside its `cfg` compiles only with the feature), then `-Slot heavy -TimeoutSeconds 900 -- cargo test --workspace --all-features --profile test-release` (`--all-features` builds the black-box suite, which needs the `test-seams` feature) |

A test that asserts on timing must hold on a busy machine: no wall-clock bound a loaded PC can cross (the PowerShell suite's CC-209 and CC-211 are what this avoids). A test that needs the machine alone runs on `-Slot critical`, and the profile names it here when it exists.

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

## Repo traps

- An installed `gate.exe` runs from `%LOCALAPPDATA%\gate\bin`, never from `target/`: `~/.claude/tools/clean-builds.ps1` empties build folders daily, and a rebuild overwrites an exe live gates hold open.
- Until a cutover, the PowerShell gate owns the machine's slots. A test of `gate.exe` takes slots under its own test prefix, never the real names, or it fights every session's gates.
- Commits carry `Refs: GATE-<n>` trailers.
