# gate

`gate.exe` runs a heavy command (a build, a test suite, a formatter) under one machine-wide queue, so the agents and people sharing a Windows PC take its cores in turn, and can send allowlisted commands to a remote build box. It replaces the PowerShell gate in `~/.claude/tools/gate/` (claude-config), which stays the gate every repo runs until the cutover.

Status: the port is being planned. Why it exists and the owner's rulings: claude-config's `docs/plans/gate-rust-port.md` (CC-212).

Docs start at [docs/README.md](docs/README.md).
