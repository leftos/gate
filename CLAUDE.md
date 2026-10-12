# gate

The Rust gate (`gate.exe`), replacing the PowerShell gate in `~/.claude/tools/gate/`. Read `docs/README.md` first.

- The plan lives in Linear, team GATE (`linear: gate`); never edit `docs/plans/MAIN.md` by hand.
- Every repo (this one included) runs the gate through its launcher: `pwsh tools/gate.ps1 -Log .tmp/<name>.log -TimeoutSeconds <n> -Slot heavy|light|critical -- <command>`. This PC is cut over, so the launcher hands every call to the installed `gate.exe`.
- An installed `gate.exe` runs from `%LOCALAPPDATA%\gate\bin`, put there by the install script, never from `target/`. A landing that changes what the local gate or the builders run is followed by a reinstall and a `gate push` to the builders (the `gate-nextup` profile, "Reinstall").
- The PowerShell gate is frozen to bug fixes; behaviour it lacks is built here, not there.
