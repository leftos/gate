# gate

gate is a Windows tool that queues heavy commands (builds, test suites, formatters) from many agent sessions and people onto a fixed number of CPU slots, so they take the machine's cores in turn instead of all at once. It watches each command for stalls and for runaway time, judged against how busy the machine is, and kills the command's whole process tree when one trips, leaving a log that says why.

```
gate run --slot heavy --timeout 900 --log .tmp/test.log -- cargo test --workspace
```

## Status

Pre-release. Nothing here is ready to use yet, and gate is not yet a replacement for anything. The design is written ([docs/design.md](docs/design.md)); the code is being built slice by slice.

So far `gate` reads its command line (`--version`, `--help`, and the argument checks for `run`, `stop-tree`, `cutover`, `push`, `check`, `burn` and `expect`), and `gate run` starts a command under a slot, logs its output, prints the tail and a verdict line, and exits with the command's status.

The watchdog kills a stalled command, one past its ceiling and one past the backstop, and Ctrl+C stops a run; routing is not built yet. `gate stop-tree <pid>` stops a process tree and the gate jobs inside it. `gate push` copies the installed `gate.exe` to the builders.

`gate check` says what this PC lacks for the repo in the current folder (the tools its `.gate/toolchain.toml` names, the SDK its `global.json` pins, the sibling worktrees `.gate/siblings.toml` declares) and the command that installs each; it installs nothing, and exits 1 when something is missing or the check could not run (not in a git repository, or a `.gate/toolchain.toml`, `.gate/siblings.toml` or `global.json` it cannot read).

`gate burn [--cores <n>] --seconds <s>` spins the CPU on `n` threads for up to 180 s, for the load-burn tool, and ends early if the process that started it ends. `gate expect -- <command>` prints the expected time of a command in the current folder, here and on each builder, from the durations store, or `no history`; the launcher maps no `-Expect`, so callers run `gate.exe expect -- <command>` directly.

`gate cutover` copies the PowerShell gate's slot counts, writes the cutover flag so new gates start `gate.exe`, and waits for the PowerShell gates still running; deleting the flag by hand goes back. It succeeds a PowerShell gate its author runs today, which stays in use until gate can take its place.

## How it will work

- **Install.** `pwsh tools/install.ps1` builds a Release `gate.exe`, puts it in `%LOCALAPPDATA%\gate\bin` and adds that folder to your user PATH; open a new terminal afterwards to use `gate`. Reinstalling while gates are running is safe: the running copy is renamed aside, and the script prints the version it replaced and the version it installed (or that the same build is already installed). It installs on this machine only.

  An owner with builders then runs `gate push`, which copies the installed `gate.exe` to each builder their `remote.json` names (`host`, then the optional `hosts`) the same way and prints a line per builder.
- **Call.** `gate run --slot heavy|light|critical|none --timeout <seconds>|auto:<seconds> --log <path> -- <command>`. `heavy` is for work that fills several cores, `light` for one or two, and `critical` for a timing-sensitive run that needs the machine to itself. The command's output goes to the log; gate prints its tail and a one-line verdict, and exits with the command's status, or 124 when it killed the command.

  `--timeout auto:900` makes 900 a floor: from the command's recorded runs the ceiling rises to three times its adjusted p99 when that is longer, and the gate says so, warns when the ceiling is below the command's usual p90, and says once when a run goes past three times that p90. With no history the ceiling is the 900. The old `-TimeoutSeconds` takes whole numbers only.

  `none` is for a command that only dispatches or watches remote work, such as `gh run watch`, a deploy script polling a server or a remote image build. A remote-only watcher uses `none`: it takes no slot, never waits, starts at once even while a critical gate holds the machine, and keeps the log, the ceiling, the stall window and the kill.

  A critical gate does not wait for a `none` run and does not protect it.
- **Slots.** The number of slots per kind comes from the machine's core count and can be changed in `%LOCALAPPDATA%\gate\v1\slot-counts.json`.
- **Remote builders (optional).** A machine can send allowlisted heavy commands to other Windows machines it names in its own `%LOCALAPPDATA%\gate\v1\remote.json`. Builders are configured only there, on your machine; nothing about them lives in this repo. With no such file, gate runs everything locally.

gate runs on Windows. Its pure crates (formats, watchdog arithmetic, routing and quoting rules) also build and test on Linux.

## Docs

Docs start at [docs/README.md](docs/README.md): the design, the architecture and the glossary.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option: MIT OR Apache-2.0.
