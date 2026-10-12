# Docs

- Design: [design.md](design.md), the source for the gate's CLI, output lines, formats, slots, crate layout and test harness, with the owner's rulings folded in.
- Architecture: [ARCHITECTURE.md](ARCHITECTURE.md), the Task Index, layers, integration footguns and test locations.
- Test map: [test-map.md](test-map.md), which tests each source area reaches, as cargo-nextest filtersets.
- Glossary: [GLOSSARY.md](GLOSSARY.md), every term the docs use in a project sense.
- Designs for queued features: [design/gate-durations.md](design/gate-durations.md) (learned durations, GATE-7), [design/gate-remote-slot.md](design/gate-remote-slot.md) (remote follow-ups, GATE-10), [design/gate-box-critical.md](design/gate-box-critical.md) (a builder's critical pool, GATE-21).
- Plan: Linear team GATE; `docs/plans/MAIN.md` is its generated snapshot.
- The PowerShell gate this replaces: claude-config's `tools/gate/README.md`, and its behaviour, Win32 surface, cross-process names and consumers as the port needs them: [research/2026-10-09-powershell-gate-inventory.md](research/2026-10-09-powershell-gate-inventory.md).
