# Gate durations: learn each command's usual runtime and schedule with it

Design for CC-178 (Gate: learn each command's usual runtime and schedule with it), built on `feat/gate-durations`. The explorer's evidence (file:line, the real-journal key table and the backfill replay) is in `.tmp/map-cc-178.md` while the work is open; the rulings below are the owner's (CC-178 description, two decision rounds, 2026-10-09).

## What v1 does

1. Records what each finished gated command took, and keeps a per-command summary the gate reads on its hot path.
2. Prints the expected time when a gate takes its slot, and answers `gate.ps1 -Expect -- <command>` without running anything.
3. Puts an ETA into the wait lines and an `expected` field into claims and tickets, which ops-tower's Gates tab shows.
4. Accepts `-TimeoutSeconds auto` and prints hang and ceiling hints.
5. Lets a waiting gate run as a **guest** while a critical gate drains, when it is known to finish before the slowest gate the drain waits on.

## Parts and order

Each part is a sub-issue of CC-178 and lands on `feat/gate-durations`. Part 1 comes first, part 2 next, and parts 3, 4 and 5 consume part 2 and may run side by side where their files allow.

### Part 1: journal facts and the archive

- A `release` line gains `wallSeconds` (the command's own wall time, `Watch.Clock`), `adjustedSeconds` (its load-adjusted time) and `verdict` (`passed`, `failed`, `stalled`, `timed-out`, `backstop`, `log-failure`, or `unknown` with no run facts). `heldSeconds` stays as it is.
- At each journal rollover, the gate whose append rolled the journal gzips the release lines of the rolled `events.1.jsonl` into `<state root>\archive\events-<UTC stamp>.jsonl.gz` and deletes archives older than 180 days, before it releases the journal mutex, in the same hold as the append. A failed archive write is said once on standard error, and the rollover goes on. `events.1.jsonl` is left as it is, since ops-tower detects the roll by its identity.
- The archive folder lives in the state root, so a test root isolates it as it does the journal.

### Part 2: the durations summary, `-Expect` and the expected-time line

- **Key.** `<place>:<class>:<repo>[/<sub>] | <normalised command>`. `place` is `local` or `remote:<host>`; `class` is `crit` for a critical gate, else empty. Repo and subfolder come from `cwd` by string rules alone, so the archive can be re-derived without git:
  - `X:\dev\[_ext\<x>\]<repo>.wt\<slug>[\rest]`: repo `<repo>`, and for a pair folder the first segment of `rest` is the repo.
  - `C:\Users\lefto\.claude-wt\<name>[\rest]` and `C:\Users\lefto\.claude[\rest]`: repo `claude`.
  - `X:\dev\<repo>[\rest]`: repo `<repo>`.
  - Anything else (scratch, temp clones): repo `(scratch)`, never estimated.
- **Normalised command**, rules applied in order:
  1. The first word becomes its basename without `.exe`, `.cmd` or `.bat`.
  2. A word starting with a worktree root (`X:/dev/<repo>.wt/<slug>/`, `C:/Users/lefto/.claude-wt/<name>/`, `/mnt/c/Users/lefto/.claude-wt/<name>/`, either slash) loses the root, with `/` separators.
  3. A word under a temp or scratchpad root becomes `<temp>`, and a `.tmp/` word becomes `<tmp>`.
  4. A bare word of 7 to 40 hex characters holding a digit and a letter becomes `<sha>`.
  5. The words after `--include` or `--files`, up to the next `-` word, become `<files>`.
  6. Filter values (`-Filter`, `--filter*`, `-t`) are kept in the **exact key**; the **family key** replaces them with `<filter>`.
  7. A key over 200 characters is cut to 200 plus 8 hex characters of its SHA-256.

  The map's 19-row table of real commands is the self-test's fixture for these rules. `keyVersion` in the summary names the rule set; a mismatch rebuilds the summary from the archive and the live journal.
- **Estimate.** p50, p90 and p99 of `wallSeconds` (and of `adjustedSeconds` for ceilings) over the last 30 passing runs or 14 days, whichever holds fewer runs. A key needs 5 passing runs. The exact key is used when it qualifies. Otherwise the family key is used for lines and hints, and for backfill only when its p90 is at most 2x its p50. Failures are counted, never timed.
- **Store.** `%LOCALAPPDATA%\gate\durations.json`, updated by the gate right after the `release` line, inside the journal mutex, written to a temp file and moved over the old one (the claim files' pattern). Hot-path readers take no lock. A torn or missing file reads as "no history" and is said once. Keys without enough history may live only in the archive, to keep the file small.
- **Lines.** When the gate takes its slot: `gate: expected about <n> min (usual <p50>, p90 <p90>, <n> runs)`, or nothing for a command without history. `gate.ps1 -Expect -- <command>` prints the same figure for the current folder, or `no history`, and exits 0 without queueing.

### Part 3: ETAs in wait lines, claims and tickets

- The wait lines gain the slowest holder's ETA where it is known, e.g. `gate: waiting for a heavy slot (2 busy, the first frees in about 4 min)`. Command text placed into a `-f` template is brace-escaped.
- Claims and tickets gain `expected: {p50, p90, n, key}`, written when the gate has history, absent otherwise.
- ops-tower: re-capture the fixtures there, copy them to `tools/gate/fixtures/`, and send ops-tower an inbox request for the ticket struct field and the tab's display.

### Part 4: `-TimeoutSeconds auto` and hints

- `-TimeoutSeconds auto` is a floor, never a kill time of its own: it carries the caller's number `n` and resolves to `max(n, 3x the command's load-adjusted p99)`, so a learned duration can only raise the kill time above what a caller chose. A command without history, and the launcher's fallback, which has no history, run on `n`. The form that carries `n` (an `auto:<n>` value or a second switch) is chosen when this part is built.

  Why not a learned kill time: Tsafrir, Etsion and Feitelson keep the user's estimate as the kill time and use predictions for scheduling only, since about 20% of jobs are underpredicted and would be killed early, and a p99 over at most 30 runs sits close to the sample maximum (`docs/research/2026-10-09-gate-prior-art-local.md`, section 4).
- A hint at start when a given ceiling is below the command's p90: `gate: the ceiling <n> s is below this command's usual p90 of <p90> s`.
- A one-time hint while running, once the run passes 3x its p90: `gate: still running after <n> s, <k>x its usual p90 of <p90> s`. It kills nothing.

### Part 5: backfill during a critical drain

- **Publisher.** In `Enter-Critical`, once the drain is under way, the critical gate prices each blocker (unreserved heavy and light claims) as `started + p25..p50 remaining`. It writes a window file `%LOCALAPPDATA%\gate\backfill\<pid>.json` with the shadow end (the slowest blocker's expected end) and the slot it lends, checked by pid and start ticks like a ticket. When any blocker is unpriced, it publishes no window.
- **Lending.** `Add-MachineSlot` releases the lent slot and skips it until the window closes or the guest is gone. This relaxes the critical gate's "lets go of nothing" rule, and the README says so.
- **Guest.** In `Get-SlotChoice`, a gate barred by the critical request takes the lent slot when all of these hold:
  - its p90 plus 10% ends before the shadow end;
  - its ceiling is at most 2x the window;
  - its key qualifies;
  - its key is not barred;
  - its command is safe to repeat: never a `git commit`, and never a command that writes outside the worktree's build output.

  The guest's `take` line carries `backfill: true` and the window.
- **Overrun.** Once the drain completes, a guest still running is killed, and its own gate re-queues it. Its key is barred from backfill for 24 h, recorded beside the window files. This is HTCondor's defrag rule, which admits only interruptible jobs to a draining machine and evicts them when the drain ends: the critical run the drain exists for never waits on a mispredicted guest (`docs/research/2026-10-09-gate-prior-art-local.md`, sections 3 and 4).
- Nesting (`Test-ParentLend`, pause markers) and CC-172's reserved light slots keep working unchanged.

## Not in v1

From the brainstorm, left for later: shortest-expected-first ordering with aging, remote routing by expected time, a regression signal for the friction review, and slot-count tuning.
