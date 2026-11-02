# gate

The Rust gate (`gate.exe`), replacing the PowerShell gate in `~/.claude/tools/gate/`. Read `docs/README.md` first.

- The plan lives in Linear, team GATE (`linear: gate`); never edit `docs/plans/MAIN.md` by hand.
- Every repo (this one included) runs the gate through its launcher: `pwsh tools/gate.ps1 -Log .tmp/<name>.log -TimeoutSeconds <n> -Slot heavy|light|critical|none -- <command>`. The launcher hands every call to the installed `gate.exe`, and runs the command directly, without slots or watchdog, where none is installed.
- An installed `gate.exe` runs from `%LOCALAPPDATA%\gate\bin`, put there by the install script, never from `target/`. A landing that changes what the local gate or the builders run is followed by a reinstall and a `gate push` to the builders (the `gate-nextup` profile, "Reinstall").
- The PowerShell gate is frozen to bug fixes; behaviour it lacks is built here, not there.
- A session here keeps two background watches armed from its first turn, each re-armed after its report is handled: the repo inbox, whose actionable requests are worked as they arrive, and the local-run watch (the `route-local-runs` skill).
