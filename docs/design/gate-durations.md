# Gate durations: learn each command's usual runtime and schedule with it

Design for CC-178 (Gate: learn each command's usual runtime and schedule with it), built on `feat/gate-durations`. The rulings below are the owner's (CC-178 description, two decision rounds).

## What v1 does

1. Records what each finished gated command took, and keeps a per-command summary the gate reads on its hot path.
2. Prints the expected time when a gate takes its slot, and answers `gate expect -- <command>` without running anything.
3. Puts an ETA into the wait lines and an `expected` field into claims and tickets, which ops-tower's Gates tab shows.
4. Accepts `--timeout auto:<n>` and prints hang and ceiling hints.
5. Lets a waiting gate run as a **guest** while a critical gate drains, when it is known to finish before the slowest gate the drain waits on.

## Parts and order

Each part is a sub-issue of CC-178 and lands on `feat/gate-durations`. Part 1 comes first, part 2 next, and parts 3, 4 and 5 consume part 2 and may run side by side where their files allow.

### Part 1: journal facts and the archive

- A `release` line gains `wallSeconds` (the command's own wall time, `Watch.Clock`), `adjustedSeconds` (its load-adjusted time) and `verdict` (`passed`, `failed`, `stalled`, `timed-out`, `backstop`, `log-failure`, or `unknown` with no run facts). `heldSeconds` stays as it is.
- At each journal rollover, the gate whose append rolled the journal gzips the release lines of the rolled `events.1.jsonl` into `<state root>\archive\events-<UTC stamp>.jsonl.gz` and deletes archives older than 180 days, before it releases the journal mutex, in the same hold as the append. A failed archive write is said once on standard error, and the rollover goes on. `events.1.jsonl` is left as it is, since ops-tower detects the roll by its identity.
- The archive folder lives in the state root, so a test root isolates it as it does the journal.

### Part 2: the durations summary, `gate expect` and the expected-time line

- **Key.** `<place>:<class>:<repo>[/<sub>] | <normalised command>`. `place` is `local` or `remote:<host>`; `class` is `crit` for a critical gate, else empty. Repo and subfolder are derived from git, so no machine path is configured or hard-coded:
  - The repo is the main checkout's folder name, taken from `git rev-parse --git-common-dir`, so every worktree of a repo shares one key.
  - The subfolder is the `cwd` relative to the worktree root.
  - A folder git cannot place falls back to its path's shape (a `{repo}.wt/{slug}/…` path is repo `{repo}` at the folders after the slug); any other folder is keyed as outside git, on its full path in lower case with `/` separators.
- **Normalised command**, rules applied in order:
  1. The first word becomes its basename without `.exe`, `.cmd` or `.bat`.
  2. A word under the run's own worktree root loses it, with `/` separators. The `.wt` component rule of the history key applies to the paths of other worktrees and to the value of a `--flag=value` word.
  3. A `.tmp/` word, or a word under the system temp root, becomes `…`, the elision the history key uses.
  4. A bare word of 7 to 40 hex characters holding a digit and a letter becomes `<sha>`.
  5. The words after `--include` or `--files`, up to the next `-` word, become `<files>`.
  6. Filter values (`-Filter`, `--filter*`, `-t` except under `dotnet`, `-E`) are kept in the **exact key**; the **family key** replaces them with `<filter>`. A filter flag never takes a following word that starts with `-`.
  7. A key over 200 characters is cut to 200 plus 8 hex characters of its SHA-256.

  The words are split, and written back, by the rules `gate_state::normalize_command` uses (`split_words` and `quote_word` in `journal.rs`, shared with `normalize_for_durations`): the C runtime's argument rules for the split, the word rules on each unquoted word, and `quote_arg_str`'s quoting for the output.

  The normalise table in `crates/gate-state/tests/state/durations.rs` is the fixture for these rules. `keyVersion` in the summary names the rule set; a mismatch rebuilds the summary from the archive and the live journal.
- **Estimate.** p90 and p99 (nearest rank) of `wallSeconds` (and of `adjustedSeconds` for ceilings) over the last 30 passing runs or 14 days, whichever holds fewer runs. p50 is the median of the 5 most recent of those runs, so it follows a real change within three runs while one outlier cannot move it (Tsafrir et al. find recent runs predict better than a long history); p90 and p99 are raised to p50 when a slower recent stretch puts it above them.

  The adjusted p90 and p99 are not: p50 is a wall figure and the adjusted seconds are load-adjusted, so the two do not compare. Each is none unless at least 5 of the window's runs carry adjusted seconds. A key needs 5 passing runs.

  The mean of the window's `wallSeconds` is the expected time the line names.

  Runs carry no cold- or warm-cache mark: nothing the gate sees tells a cold run reliably, and a cold run past its window is caught by the backfill overrun rule, which kills it once and bars its key for 24 h. The exact key is used when it qualifies. Otherwise the family key is used for lines and hints, and for backfill only when its p90 is at most 2x its p50. Failures are counted, never timed.
- **Store.** `durations.json` in the gate's state root, updated by the gate right after the `release` line, inside the journal mutex, written to a temp file and moved over the old one (the claim files' pattern). Hot-path readers take no lock. Its shape is `{"v":1,"keyVersion":1,"keys":{"<key>":{"last":"<iso>","failed":N,"samples":[{"time","wall","adjusted","avgCores","peakCores"}]}}}`, at most 30 samples per key, and a key with no release in 14 days is pruned. ops-tower does not read it.

  A missing, stale (`keyVersion`) or unreadable store needs a rebuild. One gate rebuilds at a time, holding the `durations.rebuilding` marker (`{"pid","start"}`) in the state root; a marker whose holder is dead, or that is unreadable, is taken over. The rebuild reads the live journal newest first, then the archives within 14 days, outside the journal mutex; then, under the mutex, it files the releases written since and swaps the store in.
- **Lines.** When the gate takes its slot: `gate: expected about <mean> (usual <p50>, p90 <p90>, <n> runs)`, each figure in whole seconds, or in whole minutes from a mean of 90 s, and never below 1; or nothing for a command without history. `gate expect -- <command>` prints the same figure for the current folder, or `no history`, and exits 0 without queueing.

### Part 3: ETAs in wait lines, claims and tickets

- The wait lines gain the ETA of the first holder to free its slot where it is known, e.g. `gate: waiting for a heavy slot (2 busy, the first frees in about 4 min)`; a critical gate's lines name the last holder to free its slot, as it waits for them all. No wait line holds command text, so the Rust gate escapes nothing.
- Once that holder is past its expected end, the line gives how far past its usual time it is and its ceiling, never a new guess: `gate: still waiting for a heavy slot (2 busy, the first is 3 min past its usual 4 min, ceiling 15 min)`.
- Claims and tickets gain `expected: {p50, p90, n, key}`, written when the gate has history, absent otherwise.
- ops-tower: the v1 fixtures are hand-written, not re-captured, and ops-tower gets an inbox request for the ticket struct field and the tab's display.

### Part 4: `--timeout auto:<n>` and hints

- `--timeout auto:<n>` (`auto` in any ASCII case) is a floor, never a kill time of its own: it carries the caller's number `n` and resolves to `max(n, ceil(3 x the command's adjusted p99))`, capped at 2147483647, so a learned duration can only raise the kill time above what a caller chose.

  A command without an adjusted p99 runs on `n`. The adjusted p99 comes from the take's estimate: none unless at least 5 of the window's runs carry adjusted seconds, and not used from a family key whose p90 is above 2x its p50.

  The ceiling resolves at each take, a lent take included. A run on no slot, a covered run and a late-slot run have no estimate and run on `n`. A routed run's builder key resolves the same way once it has 5 adjusted samples (the builder's own figures), and runs on `n` before; after a fallback the ceiling returns to `n` until the local take resolves it. `-TimeoutSeconds` refuses `auto:<n>`: the form exists in the new CLI only.

  A late-slot run (a covered gate that takes its own slot when its parent gate ends, `crates/gate/src/late.rs`) files nothing in the durations store at its release, like every other covered run: a covered run opens no durations (`Run::open_durations`), so `write_release` has nothing to file. Its time includes the stretch it ran covered under the parent, so filing it would skew the command's learned duration.

  Why not a learned kill time: Tsafrir, Etsion and Feitelson keep the user's estimate as the kill time and use predictions for scheduling only, since about 20% of jobs are underpredicted and would be killed early, and a p99 over at most 30 runs sits close to the sample maximum (`docs/research/2026-10-09-gate-prior-art-local.md`, section 4).
- A line after the expected-time line when history raised the floor: `gate: ceiling <resolved> (3x this command's p99 of <p99>, above the given <n>)`.
- A hint after it when the resolved ceiling is below the command's adjusted p90: `gate: the ceiling <resolved> is below this command's load-adjusted p90 of <p90>`.

  Callers give a ceiling for a quiet machine and the kill runs on the load-adjusted clock, so the wall p90, which grows under load, never triggers it; a command with no adjusted p90 gets no hint. Figures are the expected-time line's: `<n> s` below 90 s, whole minutes from 90 s. A family key's lines read `similar commands'` in place of `this command's`.
- A one-time hint while running, once the run passes 3x its p90: `gate: still running after <running>, <k>x its usual p90 of <p90>`, `k` rounded down. A local run counts its wall time less paused time, checked at each watch sample; a routed run counts the wall time since the builder's start marker reached the client, said live. It is never said for a p90 under 1 s, and it kills nothing.
- The claim's `timeoutSeconds`, the TIMED OUT kill and the 5x backstop, the remote call's timeout and the passed line's `ceiling <n> s` all use the resolved ceiling.

### Part 5: backfill during a critical drain

- **Publisher.** In `Enter-Critical`, once the drain is under way, the critical gate prices each blocker (heavy claims, and the light claims beyond the beside allowance of light gates) as `started + p25..p50 remaining`. It writes a window file `%LOCALAPPDATA%\gate\backfill\<pid>.json` with the shadow end (the slowest blocker's expected end) and the slot it lends, checked by pid and start ticks like a ticket. When any blocker is unpriced, it publishes no window.

  When any blocker passes its expected end, the critical gate deletes its window file and admits no new guest for the rest of the drain, without re-pricing: the worst case is plain FCFS, no worse than no backfill, where a window widened to the blocker's ceiling would admit guests the drain's end then kills and bars. A guest already running is left to the Overrun rule.
- **Lending.** `Add-MachineSlot` releases the lent slot and skips it until the window closes or the guest is gone. This relaxes the critical gate's "lets go of nothing" rule, and the README says so.
- **Guest.** In `Get-SlotChoice`, a gate barred by the critical request takes the lent slot when all of these hold:
  - its p90 plus 10% ends before the shadow end;
  - its ceiling is at most 2x the window;
  - its key qualifies;
  - its key is not barred;
  - its command is safe to repeat: never a `git commit`, and never a command that writes outside the worktree's build output.

  The guest's `take` line carries `backfill: true` and the window.
- **Overrun.** Once the drain completes, a guest still running is killed, and its own gate re-queues it. Its key is barred from backfill for 24 h, recorded beside the window files. This is HTCondor's defrag rule, which admits only interruptible jobs to a draining machine and evicts them when the drain ends: the critical run the drain exists for never waits on a mispredicted guest (`docs/research/2026-10-09-gate-prior-art-local.md`, sections 3 and 4).
- Nesting (`Test-ParentLend`, pause markers) and the beside allowance of light gates (CC-172's reserved light slots, kept as a count in `gate.exe`) keep working unchanged.

## In the Rust gate

Parts 2 to 5 are built in the Rust gate (GATE-7) after the local cutover, on the journal facts of part 1 (GATE-33). The rulings above hold with these spellings and boundaries:

- `gate expect -- <command>` is the whole of the old `-Expect` (the launcher maps none, and the compatibility layer refuses it), and `--timeout auto` and `--slot auto` are options of the new CLI only, with no compatibility-layer spelling. `--timeout auto:<n>` carries the caller's number `n` (part 4).
- The key and command normalisation are `gate_state::duration_key` and `normalize_for_durations`, over the history key's word rules, and the key is git-derived as in part 2: the repo is the main checkout's folder name from `git rev-parse --git-common-dir`, the subfolder is the cwd relative to the worktree root, and a folder git cannot place falls back to its path shape, else is keyed as outside git. gate-state never runs git: the caller passes the place (`KeyPlace`).

  No machine path is configured or hard-coded.
- The library (key, estimate, store, rebuild, and `append_line_then` in `journal_writer.rs`, which runs the store write in the journal hold) is in `gate-state`. The `gate` crate calls it from `durations.rs`: it says the expected-time line, files each release in the store and rebuilds the store.

  `expect.rs` is `gate expect`: it reads the store with no lock and prints one line per place with an estimate (here, then each builder in host order), or `no history`, as specified in [`gate expect`](../design.md#gate-expect).
- Every run that takes a slot of its own runs one `git rev-parse` before it queues to place its folder, reusing the answer of the routing decision for a heavy run whose route asked; a failed call keys the folder by its shape, silently. A `--slot none` run, and one a parent's slot covers or lends, has no key and is neither timed nor filed.
- The expected-time line is said once the run's claim is written and the waiters woken: at a local light or heavy take, at a critical take after its holds line, at a routed run's remote take (keyed by the builder) and, for a fallback, a second time at its local take. Its figures are in [Output lines](../design.md#output-lines): `about` is the mean wall time over the estimate's window, `usual` the p50.
- A release is filed by every slot-taking run that reaches a verdict, inside the journal hold of its `release` line, which is unchanged. A rebuild runs after the release, outside the hold, when a read found the store missing, stale or torn and the store is still so just before the claim; a rebuild leaves out `--slot none` runs, as live filing does. A store under a `keyVersion` above this build's is read as no history and is neither filed into nor rebuilt.
- The `expected` field is optional in the v1 claim and ticket formats ([Claim](../design.md#claim), [Ticket](../design.md#ticket)), so part 3 fills it without a format change or a second drain. gate-state exposes it, and ops-tower is told by repo inbox that its ticket view can show it. The durations store is read once per run, before it queues, and that read serves the ticket, the claim and the expected-time line; the wait lines read other gates' live claims.
- `--timeout auto:<n>` resolves to `max(n, ceil(3 x adjusted p99))` as a floor and is never a learned kill time; a command with no adjusted p99 runs on `n`. `resolve_ceiling`, the raised-ceiling line, the ceiling hint and the hang hint are in `durations.rs`; the local hang hint is checked by `watch.rs` and the routed one by `remote/client.rs`.
- A guest is killed and re-queued by its own gate when the drain completes before it ends, and its key is barred 24 h. Lending a slot releases a `Local\` mutex the critical gate holds, which relaxes the critical drain's rule that the critical gate lets go of nothing, so the design names the released slot and the named release event it signals when part 5 is built.

### `--slot auto` (GATE-8)

`gate run --slot auto` chooses heavy or light from the command's recorded average and peak cores (the journal's `avgCores` and `peakCores`), using the history store and the key normalisation of part 2. A command with no history, or too few runs, runs on heavy, and the gate says it guessed: no history means heavy. `critical` is never chosen automatically. It is built after the durations store (GATE-13).

## Not in v1

From the brainstorm, left for later: shortest-expected-first ordering with aging, remote routing by expected time, a regression signal for the friction review, and slot-count tuning.
