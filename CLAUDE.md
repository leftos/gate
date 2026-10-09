# gate

The Rust gate (`gate.exe`), replacing the PowerShell gate in `~/.claude/tools/gate/`. Read `docs/README.md` first.

- The plan lives in Linear, team GATE (`linear: gate`); never edit `docs/plans/MAIN.md` by hand.
- Until the cutover, every repo (this one included) runs the PowerShell gate: `pwsh tools/gate.ps1 -Log .tmp/<name>.log -TimeoutSeconds <n> -Slot heavy|light|critical -- <command>`.
- An installed `gate.exe` runs from `%LOCALAPPDATA%\gate\bin`, put there by the install script, never from `target/`.
- The PowerShell gate is frozen to bug fixes; behaviour it lacks is built here, not there.
