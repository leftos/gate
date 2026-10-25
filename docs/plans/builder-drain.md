# Builder drain (GATE-559)

A way to drain the builder before a redeploy: `gate drain` raises a barrier on the builder, waits until nothing of a routed run is going there, and says so; the operator redeploys and lifts it. This file holds the design until the work lands; its rulings then move into `docs/design.md` and the glossary.

Inputs: the code map `.tmp/map-gate-559.md` and an invariant pass by oracle, taken at an earlier `main`; the code cites below are found again at `main` 5655afaf.

## Owner rulings (settled on GATE-559)

- While the barrier stands, a routed run from any repo waits on its PC for the lift and says it is waiting behind the builder drain, then starts on the builder in arrival order. It never falls back to a local run because of the drain.
- The wait does not count toward the run's `--timeout`: the time limit starts when the command starts, as after a slot or lock wait. The wait is bounded only by the barrier's lapse.
- A barrier lapses on its own 30 minutes after it was last raised, unless re-raised; the drain says when it lapses and how to re-raise it.

## What the box knows today

A run is visible on the builder only while one of `run`, `inputs` or `lfs` holds its share-none work lock, `{root}\work\{key}.lock` (`boxside/run.rs:1564`, `take_lock`). The preflight, the staging of both puts (before their lock: `inputs.rs:727`, `lfs.rs:412`), the git push and sync-back fetch (`git-receive-pack` / `git-upload-pack`), the outputs call, and a job left behind after the box process ended (the run's job has no kill-on-close, `run.rs:2368`; spared nested gates, `run.rs:2470`) are not registered anywhere. `gate box inflight` lists held locks only, with no command or age, and its probe makes a run whose `take_lock` lands during it refuse `busy` (`inflight.rs:1-5`).

On the PC, a run that waits inside its attempt holds its remote slot: with the default one remote slot (`gate-router/src/config.rs:646`) every other routed run falls back `full` after 60 s (`wait.rs:34`), runs of the same worktree fall back `busy` after 90 s (`run.rs:97`), and the setup's 1800 s transfer ceiling (`remote/client.rs:906`) would cut the wait as `unreachable`.

## Invariants

- **I1 Exclusion.** From the drained line until the barrier is lifted or lapses, no box process starts a git step, a probe, a restore or a command, and none writes under the root; everything that was going when it was raised has ended.
- **I2 No fallback from the drain.** A run that met the barrier ends only admitted and run on the builder, by Ctrl+C, or by a refusal after admission that is not the drain's. No PC limit (remote slot, key wait, unreachable while a barrier it saw stands) turns the wait into a local run.
- **I3 Arrival order.** Runs that met the barrier are admitted in ticket order, numbered under the drain lock at their first preflight; no run is admitted while an older live ticket waits, including runs that arrive after the lift.
- **I4 No lost waiter, no starvation.** A live waiter keeps its place while it polls within the ticket lease; a dead ticket holds the queue for at most that lease, counted from the latest of its last poll, the box's boot and the lift.
- **I5 Lift and lapse.** The barrier ends only by a lift or by its lapse; `gate drain` re-raises it while it waits, so a long run never lapses it mid-drain; a lapse is said to the drain, every waiter, the first run admitted after it, and the next `gate drain`.
- **I6 A stale barrier ends.** An unreadable barrier counts as standing (fail closed) only until its file's last write plus 30 minutes.
- **I7 Ctrl+C.** Ctrl+C during the drain leaves the barrier standing and says how to lift it; a rerun re-raises it and keeps the tickets.
- **I8 No run spans a box-exe swap.** No box process that waited goes on to do run work: the wait is short calls that exit, and an admitted run starts a fresh attempt, so a run admitted after a `gate push` preflights on the new exe.
- **I9 Session liveness.** An admitted run (a session) is live while one of its verbs holds its record, or within the session lease of its last box call; a dead session never revives, and a verb that finds its session dead while the barrier stands is a new arrival.
- **I10 Accounting.** The drain wait counts toward no learned duration, `--timeout auto` or hang hint.
- **I11 GATE-542.** The interrupted-command clear runs only inside admitted verbs, and the drained test waits for every live job a `{Work}.command` record names.

## Decisions (orchestrator; not owner rulings, open to the owner)

**Barrier home.** Files under `{Root}\drain\`, every one read and written through the box's record I/O that refuses links, `Root` taken from the payload as `inflight` does (no suite case may reach the real root):
- `lock`: the drain lock, opened exactly as `take_lock` opens the work lock and retried as `warm.rs:802-829` retries; held only around a read-modify of the files below, never across a wait or a child process.
- `barrier`: raised time, lapse time, generation.
- `tickets\{seq}-{session}`: one per waiting run, with its last poll time.
- `sessions\{session}`: one per admitted run, with its phase, last call time, PC host name, repo, command text (escaped, capped at 300 characters) and job name.

A Windows mutex is rejected: its waiters are not FIFO, a `Global\` name can be squatted, and the `gate\` private namespace needs elevation.

**Admission.** The PC picks a session id per run, constant across that run's attempts and waits. The preflight is the admission point: with no barrier standing and no older live ticket it creates the session record and goes on; otherwise it creates or renews the run's ticket and refuses at once, after the version line, with `gate-remote-refused: {nonce} drain`, a place-and-lapse line and exit 75. `inputs`, `lfs` and `run` pass for a live session and refuse with the same `drain` tag for a dead one (the PC then restarts from the preflight). `outputs` is never refused and renews its session while it runs. `stop`, `boot`, `inflight`, `register-desktop` and the drain verb are never gated. A ticket's lease and a session's lease are both 120 s.

**Drained.** The barrier stands, no session is live, no job a `{Work}.command` names still holds a process, and no `git-receive-pack` or `git-upload-pack` of the box's user runs. A new box verb, `gate box drain` (raise or refresh, status, lift), answers each call once and exits; the PC's `gate drain` loops over it.

**`gate drain` on the PC.** Drains the builder `remote.json`'s `host` names. `gate drain` raises (or re-raises) and waits, polling every 2 s; `gate drain --lift` lifts; `gate drain --status` reports without changing anything. Exit 0 drained or lifted, 1 a failure, 2 usage, 0xC000013A on Ctrl+C. A lift removes the barrier whoever raised it; the tickets stay and are admitted in order. A builder whose gate does not know the verb (box usage, status 2) is reported as too old, with `gate push` named.

**The PC's wait.** A `drain` refusal ends the attempt as `End::Draining`: the snapshot and worktree locks are released, the key and the remote slot are kept. The run polls the preflight every 5 s with its session id, says the waiting line once and the still-waiting line every 30 s, and once admitted starts a fresh attempt with fresh limits. While it waits it holds a live marker `{state root}\remote\drain\{host}` (lock-file liveness, as the warm-up's); while a live marker exists, the remote-slot limit and the key-wait limit end in no fallback for that builder, and an unreachable builder or a lost connection is still waiting while the last barrier it saw stands. The wait is left out of the run's recorded duration; no journal format changes.

**Warm-ups and checks.** A `gate warm` that meets the drain ends at once with its line and gives back the key. `gate check --builder` preflights with a no-ticket flag and reports the drain instead of waiting.

**Lines** (the builder is never named by host):
- PC, waiting: `gate: waiting behind the builder drain (raised {hh:mm} UTC, lapses {hh:mm} UTC); place {n} in line`
- PC, every 30 s: `gate: still waiting behind the builder drain ({s} s; place {n})`
- PC, admitted: `gate: the builder drain lifted; starting the routed run`, or after a lapse `gate: the builder drain lapsed at {hh:mm} UTC without a lift; starting the routed run`
- PC, warm-up: `gate: warm: the builder is drained; this warm-up ends without warming`
- drain, raised: `gate: drain: barrier raised on the builder; it lapses at {hh:mm} UTC unless gate drain runs again`
- drain, list: `gate: drain: waiting for {n} run(s) on the builder:`, then one `gate:   {pc}  {repo}  {age} s  {command}` per session
- drain, every 30 s: `gate: drain: still waiting for {n} run(s) ({s} s)`
- drain, done: `gate: drain: the builder is drained; gate drain --lift lifts it (it lapses at {hh:mm} UTC; gate drain re-raises it)`
- drain, Ctrl+C: `gate: drain: interrupted; the barrier stands; gate drain --lift lifts it, gate drain resumes the wait`
- lift: `gate: drain: barrier lifted; {n} waiting run(s) start in arrival order`
- status after a lapse: `gate: drain: the last barrier lapsed at {hh:mm} UTC without a lift`

**Not in this item** (each its own add): `gate push` and `install-toolchains.ps1` re-raising or checking a standing barrier; a `gate box receive-pack` wrapper so the push is gated, not only watched.

**No feature branch:** decided by the round. The work lands as three sub-issues on `main`, each reviewed before the next is briefed: the box side (the files, the lock, admission, the gated verbs, `gate box drain`), the PC's wait (`End::Draining`, the poll, the marker, the limits, warm-up and check), and `gate drain` with the docs. All three are briefed after GATE-542 lands, since it edits `boxside/{run,preflight,tools}.rs` now.

## Sites

`boxside/mod.rs:63-85` (dispatch), `preflight.rs:104-169` (gate before `ready` at `:143`), `inputs.rs:354` (before the stage unpack), `lfs.rs:142` (before the store is made ready), `run.rs:581` (before `take_lock` at `:593`; warm-ups and desktop runs go through it), `inflight.rs`; `remote/client.rs:310` (`RustClient::run`), `:1900-1923` (read the `drain` tag before the version refusal), `:4055` and `:4402` (the tag never a `FallbackReason`), `:2620-2668` (retry, unreachable, lost connection); `run.rs:1452-1680` (routed, wall time), `:1603-1875` (key and remote waits), `wait.rs:34`, `:327`; `warm.rs:802-829`; `check/builder.rs:147`; `gate-args` `lib.rs:57-82`, `cli.rs`, `usage.rs`; `answer.rs:31-39`; `cutover.rs:39-140` (the model for the drain's loop and Ctrl+C); `docs/routing-a-repo.md:200-207` (the manual maintenance procedure this replaces).
