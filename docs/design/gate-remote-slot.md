# Gate: remote heavy slot

Open work for CC-104 (gate: remote heavy slot on the build box). The landed design is in `tools/gate/README.md` ("Remote runs", "Routing file") and the box in `tools/build-box/README.md`, including its measured slot count and the godot-mcp session-0 facts. This file keeps only what is still open.

## Decisions

- **Allowlist:** delve-the-dungeon routes `dotnet test *`, `dotnet build *`, `dotnet format *` and `dotnet csharpier *` today. yaat gets the same patterns once siblings land; ops-tower (about a fifth of the PC's heavy slot time) gets an entry with its Cargo `target/` outputs once outputs come back (CC-166, after CC-163); the other repos on D:\ hold next to no heavy slot time and get an entry only once one shows up in the gate journal; godot-mcp gets an entry only once it starts hidden-desktop children on a window station that works in session 0 (CC-164).
- **Siblings** (CC-161): `remote.json` declares a repo's sibling repos (`"siblings": {"yaat": ["yaat-server"]}`), and a routed run sends each sibling's worktree beside its own, laid out on the box as on the PC (`../{name}`), with its paths rewritten as CC-159 does for the worktree's own. yaat needs it (`Yaat.GuideCapture.csproj` references `..\..\..\yaat-server` only when it exists), so yaat is enabled after it; delve is enabled without it.
- **Test seam:** the gate wiring is tested against a stub remote selected by a test-only environment variable, as `GATE_TEST_SLOT_PREFIX` and `GATE_TEST_SAMPLER_FAIL` already are.

## Open steps

8. **Enable, remaining.** The yaat entry in `%LOCALAPPDATA%\gate\remote.json`, after step 10. The godot-mcp entry, after godot-mcp's own session-0 fix (CC-164).
10. **Siblings** (CC-161). Send a repo's declared sibling worktrees with a routed run, then enable yaat. It edits `remote-wrapper.ps1`'s `Invoke-Run` and `Update-WorkFolder` (the checkout layout), `Initialize-WorkFolder`, and `remote.ps1`'s snapshot, publish, payload, sync-back and config-reader functions; CC-162 (the tools restore) shares `Invoke-Run`, so order them.

Also open under CC-104: workload parity in the health refusals (CC-160), the tools restore (CC-162), and ops-tower's confirmation that it reads the `gate-remote-slot-N` claim family.
