# gate.ps1 fixtures

Files captured from a real run of `~/.claude/tools/gate/gate.ps1`, not written by hand, so `fixtures.rs` proves `gate-state` reads what the gate writes. Each is kept as captured (UTF-8 without a byte order mark, one line), except that the worktree path the capture ran in is replaced by `C:\work\repo` in `cwd` and `log`, and the repo's `end-of-file-fixer` hook adds the trailing newline the gate does not write (the tests parse the JSON, so it does not matter).

| File | What it is | Where the gate writes it |
|---|---|---|
| `claim.json` | The claim file of a heavy gate holding its slot while it runs `pwsh -NoProfile -Command Start-Sleep 20`, started with the relative `-Log .tmp/o28-capture/first.log`; the gate writes that log as an absolute path. | `%LOCALAPPDATA%\gate\slots\<mutex name>.json`, here `ops28-gate-slot-0.json` |
| `ticket.json` | The arrival ticket of a second heavy gate waiting for the only heavy slot while the first holds it. | `%LOCALAPPDATA%\gate\<prefix>waiting\<pid>-<start ticks>.json`, here `ops28-waiting\60080-639265725620427420.json` |
| `slot-counts.json` | The counts file the capture ran under, one heavy and two light slots, as the Gates tab writes it; the gate only reads it, and the second gate queuing shows it read it. | `%LOCALAPPDATA%\gate\slot-counts.json` |

## Re-capturing them

From the worktree root, in pwsh, with `LOCALAPPDATA` pointed at a scratch folder and the self-test prefix set so no real gate is touched:

1. `$lad = "$PWD\.tmp\o28-capture\localappdata"; New-Item -ItemType Directory -Force "$lad\gate"; [IO.File]::WriteAllText("$lad\gate\slot-counts.json", '{"heavy":1,"light":2}')`
2. `$env:LOCALAPPDATA = $lad; $env:GATE_TEST_SLOT_PREFIX = 'ops28-'; $env:GATE_SLOT_HELD = $null`
3. Start the first gate in the background: `Start-Process pwsh -NoNewWindow -ArgumentList '-NoProfile','-File',"$HOME\.claude\tools\gate\gate.ps1",'-Log','.tmp/o28-capture/first.log','-TimeoutSeconds','120','-Slot','heavy','--','pwsh','-NoProfile','-Command','Start-Sleep 20'`, and once `$lad\gate\slots\ops28-gate-slot-0.json` exists copy it as `claim.json`.
4. Start the second gate the same way with `-Log .tmp/o28-capture/second.log` and `Start-Sleep 1`; it queues, and once `$lad\gate\ops28-waiting\<its pid>-<ticks>.json` exists copy it as `ticket.json`.
5. Copy `$lad\gate\slot-counts.json` as `slot-counts.json`.
6. Replace the worktree path in `claim.json` (JSON-escaped, `X:\\...`) with `C:\\work\\repo`, and update the pid, times and ticket name `fixtures.rs` expects to the new capture's.
