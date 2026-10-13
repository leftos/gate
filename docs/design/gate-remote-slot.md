# Gate: remote heavy slot

Open work for CC-104 (gate: remote heavy slot on the build box). The landed design is in `tools/gate/README.md` ("Remote runs", "Routing file") and the box in `tools/build-box/README.md`, including its measured slot count and the godot-mcp session-0 facts. This file keeps only what is still open.

## Decisions

- **Allowlist:** delve-the-dungeon routes `dotnet test *`, `dotnet build *`, `dotnet format *` and `dotnet csharpier *` today. yaat gets the same patterns once siblings land. ops-tower (about a fifth of the PC's heavy slot time) gets an entry with its Cargo `target/` outputs once outputs come back (CC-166, after CC-163).

  The other repos on D:\ hold next to no heavy slot time and get an entry only once one shows up in the gate journal. godot-mcp gets an entry only once it starts hidden-desktop children on a window station that works in session 0 (CC-164).
- **Siblings** (CC-161, GATE-19): a repo declares its sibling repos under `.gate/` in the repo (GATE-213), not in `remote.json`, and a routed run sends each sibling's worktree beside its own, laid out on the box as on the PC (`../{name}`), with its paths rewritten as CC-159 does for the worktree's own. yaat needs it (`Yaat.GuideCapture.csproj` references `..\..\..\yaat-server` only when it exists), so yaat is enabled after it; delve is enabled without it.
- **Test seam:** the gate wiring is tested against a stub remote selected by the hidden `--test-remote-client <path>` flag of a `test-seams` build, one of the `--test-*` flags that replace the PowerShell gate's test environment variables (design.md, Black-box suite harness).

## Open steps

8. **Enable, remaining.** The yaat entry in `%LOCALAPPDATA%\gate\remote.json`, after step 10. The godot-mcp entry, after godot-mcp's own session-0 fix (CC-164).
10. **Siblings** (CC-161, GATE-19). Send a repo's declared sibling worktrees with a routed run, then enable yaat. The work lands in the Rust remote client (`crates/gate/src/remote/`: snapshot, push, payload, sync-back) and the box side (`crates/gate/src/boxside/`: the checkout layout and work folder), and the declaration is read from `.gate/` (GATE-213).

Also open under CC-104: workload parity in the health refusals (CC-160, GATE-18), and ops-tower's confirmation that it reads the `gate-remote-slot-N` claim family.
