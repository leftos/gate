# Docs

- Design: [design.md](design.md), the source for the gate's CLI, output lines, formats, slots, crate layout and test harness, with the owner's rulings folded in.
- Architecture: [ARCHITECTURE.md](ARCHITECTURE.md), the Task Index, layers, integration footguns and test locations.
- Test map: [test-map.md](test-map.md), which tests each source area reaches, as cargo-nextest filtersets.
- Glossary: [GLOSSARY.md](GLOSSARY.md), every term the docs use in a project sense.
- Routing a repo: [routing-a-repo.md](routing-a-repo.md), how a caller repo's heavy gates come to route: builder setup, `remote.json`, the routing trial, and what stays local.
- Designs for queued features: [design/gate-durations.md](design/gate-durations.md) (learned durations, GATE-7), [design/gate-remote-slot.md](design/gate-remote-slot.md) (remote follow-ups, GATE-10), [design/gate-box-critical.md](design/gate-box-critical.md) (a builder's critical pool, GATE-21), [plans/desktop-session.md](plans/desktop-session.md) (a routed command on the builder's desktop, GATE-231).
- A second builder on macOS (GATE-591): [research/2026-10-20-macos-builder-routing-map.md](research/2026-10-20-macos-builder-routing-map.md) (what in gate and the caller repos is portable, what routing two builders needs), [research/2026-10-20-macos-builder-toolchains.md](research/2026-10-20-macos-builder-toolchains.md) (the machine, toolchains and headless ssh setup).
- What still runs on this PC: the `local-run-census` skill (`.claude/skills/local-run-census/SKILL.md`) counts the journal's local heavy runs per repo and command family and sorts each as ready to route, blocked on work or truly local; its first run is [research/2026-10-27-local-run-census.md](research/2026-10-27-local-run-census.md).
- Plan: Linear team GATE; `docs/plans/MAIN.md` is its generated snapshot.
- The PowerShell gate this replaces: claude-config's `tools/gate/README.md`, and its behaviour, Win32 surface, cross-process names and consumers as the port needs them: [research/2026-10-09-powershell-gate-inventory.md](research/2026-10-09-powershell-gate-inventory.md).
