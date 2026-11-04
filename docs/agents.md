# Calling gate from an agent

How an agent (or a script, or a git hook) runs a busy command through gate: test suites, builds, formatters, runs of a game engine with no window (such as Godot with `--headless`), and a `git commit`, `git push` or `prek run` whose hooks build, test or format.

gate gives the command a machine-wide slot, so parallel sessions take the cores in turn, watches it for stalls and runaway time, and may run it on a builder. Setting gate up is [getting-started.md](getting-started.md); the full behaviour is [design.md](design.md).

A repo's agent instructions carry the short block at the end of this page.

## How to call it

Always through the repo's launcher, `tools/gate.ps1`, a copy of this repo's file:

```
pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/<name>.log --timeout <seconds> --slot heavy|light|critical|none [--alone] [--stall <seconds>] [--tail <lines>] [--no-markers] [--env NAME[=value]] [--local] [--reason "<why>"] [--cd <folder>] [--state-dir <path>] -- <command...>
pwsh -NoProfile -File tools/gate.ps1 stop-tree <pid>
```

`--log`, `--timeout`, `--slot` and the command are required. `--state-dir <path>` (or `GATE_STATE_DIR`) uses another [state root](GLOSSARY.md) than `%LOCALAPPDATA%\gate\v1`; an ordinary call leaves it out.

The PowerShell spelling (`-Log`, `-TimeoutSeconds`, `-Slot`) is also accepted. Nothing calls `gate.exe` directly: the launcher hands every call to the installed `gate.exe`, and on a machine or a fresh clone without one it says `gate: no gate.exe found; running the command without slots or watchdog` and runs the command itself.

A repo with no launcher yet gets one by copying `tools/gate.ps1` from this repo into its own `tools/` folder, unchanged.

A routed run does not carry the caller's environment. `--env NAME=value` sets a variable for the command whether it routes or not, and a bare `--env NAME` forwards the caller's value, the form for a secret.

The gate passes a value through as it receives it, so quote it as the calling shell needs: in Bash, an unquoted backslash is an escape the shell removes before the gate sees it (`--env X=C:\Windows\x.exe` arrives as `C:Windowsx.exe`), so write `--env 'X=C:\Windows\x.exe'` or use forward slashes.

### Commits and pushes

A gated commit holds a slot for all of its hooks: `--slot light` when the hooks only check the staged files, `--slot heavy` when they build or test across cores. A commit on `--slot none` takes a light slot and says `gate: a commit's hooks take a slot: pass --slot heavy or --slot light`.

A push whose pre-push hooks build or test is gated on `--slot heavy`, unless each of those hooks calls the launcher itself, when the push runs bare.

Write a commit's message to a file new to that commit and pass it with `-F`, so a failed write can never ship an earlier commit's message.

## Choosing the slot

- **Heavy:** the command itself fans out across cores: a build, `dotnet publish`, an unfiltered `dotnet test`, `cargo nextest run`, `cargo test`, a whole-repo formatter, a test run that builds first.
- **Light:** the command keeps one or two threads busy for its whole run: a test run filtered to one class with no build, one run of a game engine with no window, a linter on a small project, a script.
- **None:** the command only dispatches or watches remote work and keeps no core busy: `gh run watch`, a deploy that polls. It takes no slot and waits on nothing.
- **When unsure, heavy.** A light command on a heavy slot only wastes capacity; a heavy command on a light slot oversubscribes the machine.
- **Critical:** only for a run that load would skew, such as a profiling or timing measurement, never for a test run. See [Critical requests](#critical-requests).

The slot is the caller's judgement of the command's load, not of its importance. The cores hint after a run corrects it ([Reading the result](#reading-the-result)).

## `--local` and `--reason`

Gate a command as usual and let gate decide where it runs. A command that a repo's patterns match but gate keeps here says why, `gate: not routed: <why>`; a command no pattern matches, or one in a repo `remote.json` does not list, stays here without a line, so silence never means it routed.

**Never add `--local` unless that command was tried routed and failed.** Then rerun locally only what failed, with the narrowest command that reproduces it, and report the routed failure as a GitHub issue on this repo: the command, gate's lines, the log and what the local rerun showed. A command that must stay local is declared in the repo's `.gate/local.toml`, so callers never carry the flag.

A routed run brings back what the command left in the worktree: tracked edits and untracked files git does not ignore come back after the run, passed or failed. Ignored files (build output) come back only through the repo's `outputs` globs in `remote.json`. A formatter or a code generator therefore routes like any other command, and its rewrites land in the worktree.

A `--local` run, a `--slot critical` run and an `--alone` run each need `--reason "<why>"`, a free-text reason that gate records; `--reason` is accepted on any run. A missing or blank reason exits 2 with nothing queued: `gate: --local needs --reason "<why>"`, `gate: --slot critical needs --reason "<why>"`, `gate: --local and --slot critical need --reason "<why>"` or `gate: --reason must not be empty`.

## Worktrees and `--cd`

A worktree's command is gated with `--cd <worktree root>`, from wherever the shell sits:

```
pwsh -NoProfile -File {worktree}/tools/gate.ps1 run --cd {worktree} --log .tmp/build.log --slot heavy --timeout 600 -- dotnet build
```

`--cd` runs the command in that folder and routes from it. A relative `--log` resolves against the `--cd` folder, so the log above is `{worktree}/.tmp/build.log`; an absolute `--log` is used as given. A relative `--state-dir` stays the caller's.

Never name a worktree's paths from the main checkout without `--cd`: gate judges such paths outside the worktree, runs the command here and says `gate: not routed: the command works in <tree>; run it with --cd <tree>`.

## Sequences of gate runs

Several gate runs in a row (a CI recipe split into routed steps, a list of setup steps) go in one script, started as one background command, running one gate at a time per worktree.

- Killing the background shell does not stop the script on Windows: its later steps keep starting gate runs. Stop it with `pwsh -NoProfile -File tools/gate.ps1 stop-tree <the script's pid>`.
- A file the script copies into a tree for the run is removed by a `trap '...' EXIT`, so a stopped run leaves nothing behind.
- After shells are killed, look for surviving scripts before starting the sequence again, and stop-tree each one.
- **gate refuses to start for a script that lost its caller.** When the walk up its ancestors ends at a shell running a script or a command whose parent is gone, it exits 2 with `gate: refused: the script that started this gate lost its caller (<image> <pid>, whose parent <ppid> is gone); ...`. A gate started inside another gate's command is never refused by this check.

  So a killed sequence's running step finishes under its own ceiling and every later step is refused. **Never stop-tree the pid this line names**: it is an ancestor of the refused gate, so the agent reading the line may sit inside that tree and would end itself.
- In a Git Bash script, a `cmd` call needs `//c` (or `MSYS_NO_PATHCONV=1`), or the shell turns `/c` into a path.

## Passing the command's arguments

The words after `--` reach the command exactly as `gate.exe` received them. From Git Bash they pass whole. From pwsh, quote any word that starts with `-` and holds `:` or `.`: pwsh splits such an unquoted word before `gate.exe` starts, and gate cannot tell.

- After the first bare `--`, an unquoted word that starts with `-` and a letter or `_` is cut after the `:` that ends its name: `-p:Name=Value` becomes `-p:` and `Name=Value`.
- With or without `--`, it is cut at `.`, `[`, `,`, `(` or `{`: `-Log=out.txt` becomes `-Log=out` and `.txt`.

What works from pwsh: quoting the word (`'-p:Name=Value'`, `"-Log=out.txt"`) or putting `--%` before the `--`.

A `.cmd` or `.bat` command runs through `cmd.exe` with its words quoted for it. A word holding a line break, or a `%NAME%` of a variable that is set, is refused with exit 2 before any slot is queued ([design.md, Command launching](design.md#command-launching)).

## Critical requests

A critical gate waits for the gates ahead of it, takes every heavy slot and all but a couple of light slots, and runs its command at High priority; gates that arrive after it wait behind it. Critical requests run one at a time, in arrival order. A critical gate holds up every other session's builds and tests until it ends, so it is never used to jump the queue.

A critical gate started inside a heavy or light gate's command is refused with exit 2, since it would wait for its parent's slot for good: run it outside that gate.

`--alone` (with `--slot critical` and `--reason "<why>"`) also takes the light slots a plain critical gate leaves free: it waits for every running gate and every later gate queues behind it. It is for a profiling or timing measurement, never an ordinary test.

It logs a load snapshot before and after the command, since processes outside any gate keep running. It is refused with exit 2 on any other slot (`gate: --alone needs --slot critical`) or from inside a gate's command ([design.md, Critical drain and nesting](design.md#critical-drain-and-nesting)).

## Project wrappers

A project's own wrapper (a `test-all` script, a task runner recipe) counts as a gate call only when it calls the launcher, `tools/gate.ps1`, itself. Check before trusting one.

## Reading the result

The command's output goes to the log. The console gets gate's own lines and the last lines of a local run's output (a routed run's output shows as it goes). The last line is the verdict:

- `gate: passed in <s> s (...). Full output: <log>`, or `gate: passed in <s> s on <builder> (...). Full output: <log>` for a routed run;
- `gate: FAILED (status <n>). Full output: <log>`.

**The verdict reads failure markers as well as the exit status.** A command that exits 0 but whose log holds a marker line fails with status 1 and `gate: command exited 0 but its log reports a failure -> <log>`, and the marker lines are printed.

The markers, matched per line ignoring case: `Build FAILED.` at the start of a line, `error CS<digits>`, `: error `, `Test run summary: Failed!`, `No test projects were found`, `failed: <n>` with `n` above 0, and gate's own `gate: TIMED OUT`, `STALLED`, `BACKSTOP`, `TRANSFER TIMED OUT` and `TRANSFER FAILED` lines.

The word "fail" alone matches none of them. A passing suite whose tests print an error path's message (`<name>: error <text>`) does; such a run takes `--no-markers`, which makes the command's status the whole verdict.

**The log stamps and tags every line**, routed runs included: `<time> <tag> <text>`, where the tag is `out` (the command's standard output), `err` (its standard error) or `gat` (gate's own lines and its process lines). A search of a log therefore never anchors at the line start: `rg ' gat gate: (STALLED|TIMED OUT|BACKSTOP)' <log>` finds the kill lines, and `rg ' (out|err) ' <log>` shows only the command's own output ([design.md, Log and tail](design.md#log-and-tail)).

The hint lines, said before the verdict, each ask for an action:

- **The cores hint**, when the slot did not fit: `gate: used 7.4 cores on average (peak 15, 9.3 free) on a light slot; work that keeps more than 2 busy belongs on heavy`, `... on a heavy slot; work that keeps under 1.5 busy fits on light`, or `... on no slot; work that keeps 1.5 or more busy belongs on light or heavy`. A commit's hint names its hooks: `gate: the commit's hooks used ...`.

  The next run of the same command uses the slot the line names, and the repo's docs or scripts that set that command's slot are corrected in the same change.
- **The ceiling hint**, `gate: the ceiling <n> is below this command's load-adjusted p90 of <p90>`: the `--timeout` is shorter than the command's usual runs; raise it, or give `--timeout auto:<seconds>`, which raises it from the command's history and says `gate: ceiling <n> (3x ... p99 of ..., above the given <floor>)`.
- **The hang hint**, `gate: still running after <time>, <k>x its usual p90 of <p90>; <phase> for <time>`: the run is far past its usual time. It kills nothing.
- `gate: not routed: <why>` says why a matched command ran here, such as `faster here: about <time> here, <builder> about <time>` for a light run proven faster on this PC.
- `gate: remote <builder> <reason>; running here` (or `; trying <next builder>`) says a routed run fell back; `version` means the builder needs a `gate push`.

## Never sleep or poll a run

Run a long gate as a background command and wait for its completion notice; the verdict is its last line. Never `sleep` to wait for a gate, and never loop on its log for a line: a loop that matches only the success line spins to its own timeout when the run fails. Read the log once the run has ended, or when a decision needs a fact before then.

A gated command carries no `nice`, priority class or other priority setting of its own: gate runs it at below-normal priority, and a critical gate at High.

## Stalls and exit codes

**The stall window is 120 s** by default (`--stall <seconds>` sets it): a command with no output, no CPU use, no new process and no thread caught waiting for a processor twice running for that long is killed as STALLED, with its whole process tree.

A command the machine starves of CPU is not idle, so it is not stalled; one starved for good ends at the timeout. A test suite whose hang guards are about 2 minutes takes `--stall` well above its longest guard (300 for 2-minute guards), so a hung test fails with its own message instead.

| Exit | Meaning |
|---|---|
| 0 | The command exited 0 and its log holds no failure marker. |
| `N` | The command's own non-zero status, passed through whole. |
| 1 | A failure marker in the log of a command that exited 0, or gate itself failed. |
| 2 | A usage error, a refused `.cmd` or `.bat` word, a missing `--reason`, a refused critical or `--alone` gate, or a script that lost its caller. Nothing was queued. |
| 74 | A routed command exited 0, but bringing its changes or outputs back failed on every attempt (`TRANSFER FAILED`). |
| 124 | gate killed the command: STALLED, TIMED OUT, BACKSTOP or TRANSFER TIMED OUT. |
| 127 | The command could not be started. |
| `0xC000013A` | Ctrl+C stopped the gate or its command. |

The full table is [design.md, Exit codes](design.md#exit-codes).

## For a repo's CLAUDE.md or AGENTS.md

Paste this block into the repo's agent instructions. It assumes the repo carries `tools/gate.ps1`; a repo without one copies it from this repo first.

````markdown
## Busy commands run through gate

Test suites, builds, formatters, game-engine runs with no window, and a `git commit`, `git push` or `prek run` whose hooks build, test or format run through the repo's gate launcher, never bare and never through `gate.exe` directly:

```
pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/<name>.log --timeout <seconds> --slot heavy|light|critical|none [--reason "<why>"] [--cd <worktree root>] -- <command>
```

- `--slot heavy` when the command fans out across cores, and when unsure; `light` when it keeps one or two threads busy; `none` when it only waits on remote work; `critical` only for a timing or profiling run, never a test run.
- Never add `--local` unless the command was tried routed and failed; `--local`, `--slot critical` and `--alone` each need `--reason "<why>"`.
- A worktree's command is gated with `--cd <worktree root>`.
- The verdict is the last line, and the whole output is in the log; act on a cores hint by moving the command to the slot it names.
- Wait for a background gate's completion notice; never sleep or poll its log.
- The full rules: https://github.com/leftos/gate/blob/main/docs/agents.md
````
