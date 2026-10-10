# gate

gate is a Windows tool that queues heavy commands (builds, test suites, formatters) from many agent sessions and people onto a fixed number of CPU slots, so they take the machine's cores in turn instead of all at once. It watches each command for stalls and for runaway time, judged against how busy the machine is, and kills the command's whole process tree when one trips, leaving a log that says why.

```
gate run --slot heavy --timeout 900 --log .tmp/test.log -- cargo test --workspace
```

## Status

Pre-release. Nothing here is ready to use yet, and gate is not yet a replacement for anything. The design is written ([docs/design.md](docs/design.md)); the code is being built slice by slice. So far `gate` reads its command line (`--version`, `--help`, and the argument checks for `run` and `stop-tree`), and `gate run` starts a command under a slot, logs its output, prints the tail and a verdict line, and exits with the command's status. The watchdog and routing are not built yet. `gate stop-tree <pid>` stops a process tree and the gate jobs inside it. It succeeds a PowerShell gate its author runs today, which stays in use until gate can take its place.

## How it will work

- **Install.** `pwsh tools/install.ps1` builds a Release `gate.exe`, puts it in `%LOCALAPPDATA%\gate\bin` and adds that folder to your user PATH; open a new terminal afterwards to use `gate`. Reinstalling while gates are running is safe: the running copy is renamed aside, and the script prints the version it replaced and the version it installed (or that the same build is already installed). It installs on this machine only.
- **Call.** `gate run --slot heavy|light|critical --timeout <seconds> --log <path> -- <command>`. `heavy` is for work that fills several cores, `light` for one or two, and `critical` for a timing-sensitive run that needs the machine to itself. The command's output goes to the log; gate prints its tail and a one-line verdict, and exits with the command's status, or 124 when it killed the command.
- **Slots.** The number of slots per kind comes from the machine's core count and can be changed in `%LOCALAPPDATA%\gate\v1\slot-counts.json`.
- **Remote builders (optional).** A machine can send allowlisted heavy commands to other Windows machines it names in its own `%LOCALAPPDATA%\gate\v1\remote.json`. Builders are configured only there, on your machine; nothing about them lives in this repo. With no such file, gate runs everything locally.

gate runs on Windows. Its pure crates (formats, watchdog arithmetic, routing and quoting rules) also build and test on Linux.

## Docs

Docs start at [docs/README.md](docs/README.md): the design, the architecture and the glossary.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option: MIT OR Apache-2.0.
