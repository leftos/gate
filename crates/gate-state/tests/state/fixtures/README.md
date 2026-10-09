# gate.ps1 fixtures

Files captured from a real run of `~/.claude/tools/gate/gate.ps1`, not written by hand (except three of the `remote*` files, below), so `fixtures.rs` proves `gate-state` reads what the gate writes. Each is kept as captured (UTF-8 without a byte order mark, one line), except that the worktree path the capture ran in is replaced by `C:\work\repo` in `cwd` and `log`, and the repo's `end-of-file-fixer` hook adds the trailing newline the gate does not write (the tests parse the JSON, so it does not matter).

| File | What it is | Where the gate writes it |
|---|---|---|
| `claim.json` | The claim file of a heavy gate holding its slot while it runs `pwsh -NoProfile -Command Start-Sleep 20`, started with the relative `-Log .tmp/o28-capture/first.log`; the gate writes that log as an absolute path. | `%LOCALAPPDATA%\gate\slots\<mutex name>.json`, here `ops28-gate-slot-0.json` |
| `ticket.json` | The arrival ticket of a second heavy gate waiting for the only heavy slot while the first holds it. | `%LOCALAPPDATA%\gate\<prefix>waiting\<pid>-<start ticks>.json`, here `ops28-waiting\60080-639265725620427420.json` |
| `slot-counts.json` | The counts file the capture ran under, one heavy and two light slots, as the Gates tab writes it; the gate only reads it, and the second gate queuing shows it read it. | `%LOCALAPPDATA%\gate\slot-counts.json` |
| `journal.jsonl` | The journal of four light gates under one light slot: `cmd /c exit 0` (ok), `cmd /c exit 3` (failed), `pwsh -NoProfile -Command Start-Sleep 10` (ok, held 10 s), and a second `cmd /c exit 0` that queued behind it and waited 8 s (ok). A gate whose wait ends in its `-TimeoutSeconds` still takes its slot, so the capture writes no `abandon`: the queue and abandon lines of pid 58200 are written by hand from the writer, `Write-EndEvent` (`waitedSeconds`) over `Write-JournalEvent` (the fields every line carries). | `%LOCALAPPDATA%\gate\<prefix>events.jsonl`, here `ops72-events.jsonl` |
| `remote-claim.json` | **Hand-written**: the claim file of a heavy gate holding remote slot 1, a heavy claim's keys plus `remote: true`, `host` (the ssh destination) and `key` (the remote work folder's key). | `%LOCALAPPDATA%\gate\slots\gate-remote-slot-<n>.json`, here `ops225-gate-remote-slot-1.json` |
| `remote-ticket.json` | **Hand-written**: the arrival ticket of a heavy gate routed to the remote pool and waiting for a remote slot, a heavy ticket plus `remote: true`. | `%LOCALAPPDATA%\gate\<prefix>waiting\<pid>-<start ticks>.json`, here `ops225-waiting\60090-639265725620427420.json` |
| `remote.json` | **Hand-written**: the gate's remote settings, two remote slots plus routing keys that `gate-state` ignores. | `%LOCALAPPDATA%\gate\remote.json` |
| `remote-journal.jsonl` | The journal of the real gate's remote route, captured sandboxed by `capture-remote-journal.ps1`: a remote run that passed, runs that fell back to the local queue (`full`, then one for each reason `unreachable`, `busy`, `parity` with a failing command, `rebooted`, `tools`), a run that fell back onto its parent gate's slot (a `parent` line follows its `fallback`), and a local control run. | `%LOCALAPPDATA%\gate\<prefix>events.jsonl`, here `ops225-events.jsonl` |

The three other `remote*` files are written by hand from the gate's remote-slot design (`docs/design/gate-remote-slot.md`), since the gate does not write a remote claim or ticket a test can capture; they are re-captured from a real gate once it does. claude-config keeps a copy of `remote-journal.jsonl` (`tools/gate/fixtures/remote-journal.jsonl`).

## Re-capturing them

From the worktree root, in pwsh, with `LOCALAPPDATA` pointed at a scratch folder and the self-test prefix set so no real gate is touched:

1. `$lad = "$PWD\.tmp\o28-capture\localappdata"; New-Item -ItemType Directory -Force "$lad\gate"; [IO.File]::WriteAllText("$lad\gate\slot-counts.json", '{"heavy":1,"light":2}')`
2. `$env:LOCALAPPDATA = $lad; $env:GATE_TEST_SLOT_PREFIX = 'ops28-'; $env:GATE_SLOT_HELD = $null`
3. Start the first gate in the background: `Start-Process pwsh -NoNewWindow -ArgumentList '-NoProfile','-File',"$HOME\.claude\tools\gate\gate.ps1",'-Log','.tmp/o28-capture/first.log','-TimeoutSeconds','120','-Slot','heavy','--','pwsh','-NoProfile','-Command','Start-Sleep 20'`, and once `$lad\gate\slots\ops28-gate-slot-0.json` exists copy it as `claim.json`.
4. Start the second gate the same way with `-Log .tmp/o28-capture/second.log` and `Start-Sleep 1`; it queues, and once `$lad\gate\ops28-waiting\<its pid>-<ticks>.json` exists copy it as `ticket.json`.
5. Copy `$lad\gate\slot-counts.json` as `slot-counts.json`.
6. Replace the worktree path in `claim.json` (JSON-escaped, `X:\\...`) with `C:\\work\\repo`, and update the pid, times and ticket name `fixtures.rs` expects to the new capture's.

## Re-capturing the remote journal

From the repo root, in pwsh: `pwsh -NoProfile -File tools/gate.ps1 -Log .tmp/cap.log -TimeoutSeconds 600 -Slot light -- pwsh -NoProfile -File crates/gate-state/tests/state/fixtures/capture-remote-journal.ps1`, with an optional `-Scratch <folder>` (default `.tmp\cap-remote-journal\run-<stamp>`, left for you to delete). The script runs the real gate under a scratch `LOCALAPPDATA`, a self-test slot prefix and a stub remote, so no live journal, mutex or build box is touched, and writes `remote-journal.jsonl` with LF line ends, the scratch repo's path as `C:\work\repo`, renumbered pids and shifted times. Update the figures `fixtures.rs` expects if the sequence changes.

## Re-capturing the journal

From the worktree root, in pwsh, with no gate of your own holding a slot:

1. `$env:GATE_SLOT_HELD = $null; $env:GATE_TEST_SLOT_PREFIX = 'ops72-'; [IO.File]::WriteAllText("$PWD\.tmp\ops72-counts.json", '{"heavy":1,"light":1}'); $env:GATE_TEST_SLOT_COUNTS = "$PWD\.tmp\ops72-counts.json"`
2. `pwsh -NoProfile -File tools/gate.ps1 -Log .tmp/f1.log -TimeoutSeconds 60 -Slot light -- cmd /c exit 0`, then the same with `-Log .tmp/f2.log` and `cmd /c exit 3`.
3. Start a gate holding the slot in the background, `Start-Process pwsh -NoNewWindow -ArgumentList '-NoProfile','-File',"$PWD\tools\gate.ps1",'-Log','.tmp/f3.log','-TimeoutSeconds','60','-Slot','light','--','pwsh','-NoProfile','-Command','Start-Sleep 10'`, and 3 s later `pwsh -NoProfile -File tools/gate.ps1 -Log .tmp/f4.log -TimeoutSeconds 3 -Slot light -- cmd /c exit 0`, which queues behind it.
4. Copy `%LOCALAPPDATA%\gate\ops72-events.jsonl` as `journal.jsonl` with LF line ends, replace the worktree path (JSON-escaped, `X:\\...`) with `C:\\work\\repo`, put the hand-written queue and abandon lines back after the fourth gate's queue, update the pids, times and figures `fixtures.rs` expects, and delete `ops72-events.jsonl`.
