# Remote cutover (GATE-46)

The Rust remote client has replaced the remote.ps1 bridge. Owner and agent rulings are in GATE-46's description. The steps R1 to R10 have shipped on `main` (the build id in `--version`, the production client over ssh, `gate box inflight`, the drain reader, the remote-stop line, the bridge's deletion in e927274, and the remote-cutover flag and subcommand's retirement in edb5977). The design is in [design.md, Routing](../design.md#routing).

What is left is the runbook below, and this file is deleted when GATE-192 closes.

## Runbook

Routing is off on the PC, so every run is local. The trial (GATE-191) has run; its findings are GATE-215 to GATE-220.

1. Turn routing back on for the Rust client (GATE-192). Confirm on the next ordinary routed runs: no `client` or `version` fallbacks, no nothing-back record after a build that brought its outputs back.
2. With routing back on, remove every routing exclusion made because of the bridge, putting each pattern back in `remote.json`, and send each caller repo whose routing changed a repo inbox note: the new remote gate is released, and it should be evaluated on its own merits, not by what the bridge did. The exclusions so far are listed under "Routing exclusions" below.

The builders' old PowerShell wrappers and claude-config's remote scripts are GATE-11's. GATE-109 (one ssh connection) follows the cutover.

## Routing exclusions

Patterns taken out of `remote.json` because of how the bridge behaved; runbook step 2 puts them back. The file's backup from before each change sits beside it as `remote.json.bak-{date}`.

None stands: godot-mcp's `scratch` and `csharp` itest groups are back, and godot-mcp has its note.
