# gate.ps1 fixtures

Files captured from a real run of the gate, not written by hand (except three of the `remote*` files and the two pid 58200 lines of `journal.jsonl` and `journal-powershell.jsonl`, below), so `fixtures.rs` proves `gate-state` reads what the gate writes. `journal.jsonl` comes from an early `gate.exe` that launched nothing and took no slot; `remote-run-journal.jsonl` comes from the current `gate.exe`; the other captured files are still from `~/.claude/tools/gate/gate.ps1`.

Each is kept as captured (UTF-8 without a byte order mark, one line), except that the worktree path the capture ran in is replaced by `C:\work\repo` in `cwd` and `log`, and the repo's `end-of-file-fixer` hook adds the trailing newline the gate does not write (the tests parse the JSON, so it does not matter).

| File | What it is | Where the gate writes it |
|---|---|---|
| `claim.json` | The claim file of a heavy gate holding its slot while it runs `pwsh -NoProfile -Command Start-Sleep 20`, started with the relative `-Log .tmp/o28-capture/first.log`; the gate writes that log as an absolute path. | `%LOCALAPPDATA%\gate\slots\<mutex name>.json`, here `ops28-gate-slot-0.json` |
| `ticket.json` | The arrival ticket of a second heavy gate waiting for the only heavy slot while the first holds it. | `%LOCALAPPDATA%\gate\<prefix>waiting\<pid>-<start ticks>.json`, here `ops28-waiting\60080-639265725620427420.json` |
| `slot-counts.json` | The counts file the capture ran under, one heavy and two light slots, as the Gates tab writes it; the gate only reads it, and the second gate queuing shows it read it. | `%LOCALAPPDATA%\gate\slot-counts.json` |
| `journal.jsonl` | The journal of three `gate.exe` runs (statuses 0, 3 and 0), each a `queue`, `take` and `release` line with `"v":1`; the early `gate.exe` launched nothing and took no slot, so the held and wall seconds are about 0. No captured run abandons, so the hand-written queue and abandon lines of pid 58200 (with `"v":1`) are appended after the captured lines. | `<state root>\events.jsonl` |
| `journal-powershell.jsonl` | The PowerShell gate's journal as captured before the v1 format (no `v`): four light gates under one light slot, `cmd /c exit 0` (ok), `cmd /c exit 3` (failed), `pwsh -NoProfile -Command Start-Sleep 10` (ok, held 10 s), and a second `cmd /c exit 0` that queued behind it and waited 8 s (ok), plus the hand-written queue and abandon lines of pid 58200. Kept so the reader's no-`v` path is tested against real output. | `%LOCALAPPDATA%\gate\<prefix>events.jsonl`, here `ops72-events.jsonl` |
| `remote-claim.json` | **Hand-written**: the claim file of a heavy gate holding remote slot 1, a heavy claim's keys plus `remote: true`, `host` (the ssh destination) and `key` (the remote work folder's key). | `%LOCALAPPDATA%\gate\slots\gate-remote-slot-<n>.json`, here `ops225-gate-remote-slot-1.json` |
| `remote-ticket.json` | **Hand-written**: the arrival ticket of a heavy gate routed to the remote pool and waiting for a remote slot, a heavy ticket plus `remote: true`. | `%LOCALAPPDATA%\gate\<prefix>waiting\<pid>-<start ticks>.json`, here `ops225-waiting\60090-639265725620427420.json` |
| `remote.json` | **Hand-written**: the gate's remote settings, two remote slots plus routing keys that `gate-state` ignores. | `%LOCALAPPDATA%\gate\remote.json` |
| `remote-journal.jsonl` | The journal of the real PowerShell gate's remote route (no `v`), captured sandboxed by `capture-remote-journal.ps1`: a remote run that passed, runs that fell back to the local queue (`full`, then one for each reason `unreachable`, `busy`, `parity` with a failing command, `rebooted`, `tools`), a run that fell back onto its parent gate's slot (a `parent` line follows its `fallback`), and a local control run. | `%LOCALAPPDATA%\gate\<prefix>events.jsonl`, here `ops225-events.jsonl` |
| `remote-run-journal.jsonl` | The v1 journal of `gate.exe`'s remote route (69 lines, 15 runs), captured by `capture-remote-run.ps1`: a remote run that passed, `full`, `unreachable`, `busy`, `parity` (its command fails), `rebooted` and `tools` from the scripted client, a parent fallback, a local control run, then `unreachable`, `rebooted`, `tools`, `parity`, box-lock `busy` and `version` from the Rust client over the `local:` transport and fake-ssh. | `<state root>\events.jsonl` |

The three other `remote*` files (`remote-claim.json`, `remote-ticket.json`, `remote.json`) are the PowerShell gate's formats, written by hand from the gate's remote-slot design (`docs/design/gate-remote-slot.md`), since that gate does not write a remote claim or ticket a test can capture. `gate.exe` writes the v1 forms, whose reference examples are the `v1-remote-*` files below, and `capture-remote-run.ps1` captures only its journal. claude-config keeps a copy of `remote-journal.jsonl` (`tools/gate/fixtures/remote-journal.jsonl`).

## The v1 reference examples

The `v1-*` files are the reference examples of the formats `gate.exe` writes under a state root, written by hand from the tables in `docs/design.md` "Formats" as `gate.exe` writes them (compact JSON, `v` first, epoch-millisecond machine times, ISO UTC times with seven fractional digits); `v1.rs` reads them, and `cutover.rs` and `journal.rs` read them as the state and the history read `gate.exe`'s default state root `%LOCALAPPDATA%\gate\v1` once `%LOCALAPPDATA%\gate\cutover.flag` exists.

The files without the prefix are the PowerShell gate's captures and hand-written files described above, apart from `journal.jsonl`.

The claims and tickets other than `v1-claim.json` and `v1-ticket.json` name processes started at `2026-10-01T11:00:00.5Z` (epoch ms `1790852400500`), the start the state tests give their fake processes.

| File | What it is | Where `gate.exe` writes it |
|---|---|---|
| `v1-claim.json` | The claim of a heavy gate holding slot 0, with every required field and none of the optional ones. | `<state root>\slots\gate-slot-0.json` |
| `v1-ticket.json` | The arrival ticket of a light gate waiting in the local queue, `holds` false. | `<state root>\waiting\<pid>-<start>.json`, here `60080-1759439760571.json` |
| `v1-slot-counts.json` | A counts file setting three heavy and six light slots. | `<state root>\slot-counts.json` |
| `v1-critical-claim.json` | The claim of critical gate 52000 holding the machine, `kind` `critical` and `index` 0. | `<state root>\slots\gate-critical.json` |
| `v1-critical-ticket.json` | The ticket critical gate 52000 keeps to its end, `holds` true. | `<state root>\waiting\52000-1790852400500.json` |
| `v1-remote-claim.json` | The claim of heavy gate 53000 holding remote slot 1: `kind` `remote`, then `remote`, `host` and `key`. | `<state root>\slots\gate-remote-slot-1.json` |
| `v1-remote-ticket.json` | The ticket of heavy gate 53100 queued for the remote pool, `remote` true. | `<state root>\waiting\53100-1790852400500.json` |
| `v1-covered.json` | The covered marker of light gate 54000 running under the slot of critical gate 52000: `processStart` and `since` in Unix epoch milliseconds, as a ticket's machine times are. | `<state root>\covered\54000-1790852400500.json` |
| `v1-journal.jsonl` | A journal of five gates: light gate 56000 runs on `gate-light-slot-0` and lends it to 55000 (a lone `parent` line) before releasing it; critical gate 52000 takes eight slots with its machine snapshot (`counts`, `otherHeld`, `otherClaims`) and covers light gate 54000 (a lone `covered` line); critical gate 52100's take carries `snapshotError` and it fails. | `<state root>\events.jsonl` |

## Re-capturing them

From the worktree root, in pwsh, with `LOCALAPPDATA` pointed at a scratch folder and the self-test prefix set so no real gate is touched:

1. `$lad = "$PWD\.tmp\o28-capture\localappdata"; New-Item -ItemType Directory -Force "$lad\gate"; [IO.File]::WriteAllText("$lad\gate\slot-counts.json", '{"heavy":1,"light":2}')`
2. `$env:LOCALAPPDATA = $lad; $env:GATE_TEST_SLOT_PREFIX = 'ops28-'; $env:GATE_SLOT_HELD = $null`
3. Start the first gate in the background: `Start-Process pwsh -NoNewWindow -ArgumentList '-NoProfile','-File',"$HOME\.claude\tools\gate\gate.ps1",'-Log','.tmp/o28-capture/first.log','-TimeoutSeconds','120','-Slot','heavy','--','pwsh','-NoProfile','-Command','Start-Sleep 20'`, and once `$lad\gate\slots\ops28-gate-slot-0.json` exists copy it as `claim.json`.
4. Start the second gate the same way with `-Log .tmp/o28-capture/second.log` and `Start-Sleep 1`; it queues, and once `$lad\gate\ops28-waiting\<its pid>-<ticks>.json` exists copy it as `ticket.json`.
5. Copy `$lad\gate\slot-counts.json` as `slot-counts.json`.
6. Replace the worktree path in `claim.json` (JSON-escaped, `X:\\...`) with `C:\\work\\repo`, and update the pid, times and ticket name `fixtures.rs` expects to the new capture's.

## Re-capturing the remote journal

From the repo root, in pwsh: `pwsh -NoProfile -File tools/gate.ps1 -Log .tmp/cap.log -TimeoutSeconds 600 -Slot light -- pwsh -NoProfile -File crates/gate-state/tests/state/fixtures/capture-remote-journal.ps1`, with an optional `-Scratch <folder>` (default `.tmp\cap-remote-journal\run-<stamp>`, left for you to delete).

The script runs the real gate under a scratch `LOCALAPPDATA`, a self-test slot prefix and a stub remote, so no live journal, mutex or build box is touched, and writes `remote-journal.jsonl` with LF line ends, the scratch repo's path as `C:\work\repo`, renumbered pids and shifted times. Update the figures `fixtures.rs` expects if the sequence changes.

## Re-capturing the remote run journal

From the repo root, in pwsh: `pwsh -NoProfile -File tools/gate.ps1 -Log .tmp/cap-remote-run.log -TimeoutSeconds 900 -Slot light -- pwsh -NoProfile -File crates/gate-state/tests/state/fixtures/capture-remote-run.ps1`, with an optional `-Scratch <folder>` (default `.tmp\cap-remote-run`).

The script builds `gate.exe` with `--features test-seams` and runs it into a scratch state root. Pass 1 drives the scripted client (`--test-remote-client`) through a remote run that passes, `full`, `unreachable`, `busy`, `parity`, `rebooted` and `tools`, then a parent fallback and a local control run. Pass 2 drives the Rust client over the `local:` transport and fake-ssh for `unreachable`, `rebooted`, `tools`, `parity`, box-lock `busy` and `version`.

It writes `remote-run-journal.jsonl` with LF line ends, the repo path as `C:\work\repo`, the root hash as `5ca1ab1e`, the work-folder key as `repo-c0ffee00`, renumbered pids and every ISO time shifted to start at `2026-10-12T06:00:00Z`. Update the figures `fixtures.rs` expects if the sequence changes.

## Re-capturing the journal

From the repo root, in pwsh: `pwsh -NoProfile -File tools/gate.ps1 -Log .tmp/cap-journal.log -TimeoutSeconds 600 -Slot light -- pwsh -NoProfile -File crates/gate-state/tests/state/fixtures/capture-journal.ps1`, with an optional `-Scratch <folder>` (default `.tmp\cap-journal\run-<stamp>`, left for you to delete).

The script builds `gate.exe` without `test-seams` through the gate, runs three real `cmd /c exit <status>` commands (statuses 0, 3, 0) into a scratch state root, rewrites `cwd` and `log` to `C:\work\repo`, and writes `journal.jsonl` with LF line ends, the hand-written pid 58200 queue and abandon lines after the captured ones.

A re-capture gives takes that name real slots and non-zero held and wall times, so the figures `fixtures.rs` expects would change with it. Update the pids, times and figures `fixtures.rs` expects if the sequence changes.
