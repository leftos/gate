# gate

gate is a Windows tool that queues heavy commands (builds, test suites, formatters) from many agent sessions and people onto a fixed number of CPU slots, so they take the machine's cores in turn instead of all at once. It watches each command for stalls and for runaway time, judged against how busy the machine is, and kills the command's whole process tree when one trips, leaving a log that says why.

```
gate run --slot heavy --timeout 900 --log .tmp/test.log -- cargo test --workspace
```

It can also send heavy and light commands to builders, other Windows machines or Macs that this PC reaches over ssh, and bring their results back.

## Status

gate is in daily use on its author's machines and builders, and it is still changing: the CLI, the output lines and the file formats can change between commits, and there are no releases yet. It runs on Windows 11; its pure crates (formats, watchdog arithmetic, routing and quoting rules) also build and test on Linux, and a Mac builder runs its own box, built from this repo.

gate replaces a PowerShell gate, its predecessor: a fresh machine never needs it, and `gate cutover` is only for migrating from it.

## Install

Needs Windows 11, PowerShell 7.3 or later and git. Then:

```
git clone https://github.com/leftos/gate.git
cd gate
pwsh tools/install.ps1
```

The script offers to install a missing Rust toolchain and the Visual Studio C++ Build Tools with winget, builds `gate.exe` and its desktop helper, installs them in `%LOCALAPPDATA%\gate\bin` and puts that folder on your user PATH. Open a new terminal afterwards.

## A first command

```
gate run --slot light --timeout 60 --log .tmp/hello.log -- git --version
```

gate waits for a light slot, runs the command with its output going to `.tmp/hello.log`, prints the log's last lines (git's `git version ...`) and a verdict line, and exits with the command's status. `gate --help` lists every subcommand and option.

A repo calls gate through a committed copy of [tools/gate.ps1](tools/gate.ps1), which hands every call to the installed `gate.exe` and runs the command directly on a machine without one:

```
pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/test.log --timeout 900 --slot heavy -- cargo nextest run
```

## What it does

- **Slots.** `--slot heavy` for work that fills several cores, `light` for one or two, `none` for a command that only watches remote work, and `critical` for a timing run that needs the machine to itself. The counts come from the core count and can be set in `%LOCALAPPDATA%\gate\v1\slot-counts.json`.
- **Watchdog.** A command past its `--timeout` (load-adjusted seconds), quiet for the stall window, or past the wall-time backstop is killed with its whole process tree, and the gate exits 124.
- **Verdicts.** A command that exits 0 but whose log holds a failure marker (`Build FAILED.`, `error CS1234`) fails. After a run whose slot did not fit, gate says which slot the command belongs on.
- **Learned durations.** With `--timeout auto:<seconds>` the ceiling rises from the command's own history, and a run says what to expect and when it is far past its usual time.
- **Builders (optional).** With a `%LOCALAPPDATA%\gate\remote.json`, matching commands run on a builder and their changes and outputs come back to the worktree. `gate push` deploys gate to the builders, `gate check --builder <host>` asks one what it lacks, and `tools/builder/` holds the scripts that prepare one.

## Docs

- [Getting started](docs/getting-started.md): from a bare Windows PC to gated commands, then to routed runs on a builder.
- [Calling gate from an agent](docs/agents.md): the rules for agents and scripts, with a block to paste into a repo's CLAUDE.md or AGENTS.md.
- [docs/README.md](docs/README.md): the design, the architecture, the glossary and the routing procedure.

## Feedback

Report problems and questions as [GitHub issues](https://github.com/leftos/gate/issues).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option: MIT OR Apache-2.0.
