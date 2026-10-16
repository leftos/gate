# Builder warm-up (GATE-347)

A repo declares warm-up commands in a tracked `.gate/warmup.toml`. Before any routed work for a worktree goes to the builder, those commands warm that worktree's builder work folder. A short-ceiling routed run (yaat's 30 s filtered `dotnet test`) then meets an incremental build instead of a cold one. The owner rulings and the decisions taken before the invariant pass are in GATE-347's description. This file holds the invariant pass (`oracle`, on main 23f34a5), the decisions that replaced the ones it found unworkable, and the brief order.

## Decisions after the invariant pass

These replace the matching items in GATE-347's earlier orchestrator decision.

- **No remote slot.** A warm-up takes the key mutex (with a bounded wait) but no remote slot, ticket or claim. A PC-wide cap of one running warm-up (`remote\warm.lock`) is taken after the per-key lock. Queuing for the single default slot would make every other worktree's routed run fall back `full`.
- **Liveness is a lock file.** `remote\warm\<key>.lock` is opened with no sharing, without following links, and held by the warming process for its whole life. A route counts a `warming` record as live only while a probe of the lock (open with full sharing, see a sharing violation, as `boxside/inflight.rs:93-106` does) finds it held. A pid in the record is only for the line. `Local\` objects are per logon session and pids are reused, so neither can show liveness.
- **Consent is checked late on the PC.** The background `gate warm` re-reads `remote.json` just before the run call and checks consent against the bytes it is about to send. The box cannot see this PC's consent. With no consent or no file at the recheck, there is no box call and the record is deleted.
- **Payload keys.** The warm-up's commands go in `WarmCommands` (an array of word arrays), with `WarmHash` beside it and no `Command`, so a box that ignored unknown keys would stop at the missing `Command` (status 2). After the commands pass, the box writes `{Work}.warm` and says `gate: remote warm <hash>`. The PC writes `warm` only after it reads that line with the hash it sent.
- **Failure classes.** Retry timing depends on the outcome:
  - the commands failed or timed out: `failed`, retried after 30 min;
  - a refusal where no command ran (`unreachable`, `rebooted`, `version`, `parity`, `tools`, `layout`, `inputs`, `client`, `busy`, or the key mutex wait timed out): `failed`, retried after about 60 s;
  - Ctrl+C, or no consent or file at the recheck: the record is deleted and a retry can start at once.

  A change of host, warm-up file hash or PC gate build voids any backoff.
- **The warm check sits after the routing decision.** A pure judge (`gate_router::warm::judge`) runs in `route.rs` after every other local reason, the non-Unicode check included, never inside `decide`. The warm-up can then reuse the decision without recursing.
- **Detach.** The background process inherits no handle: NUL standard handles, its own log, a hidden console of its own, a new process group. It is started through `gate_win32` with an explicit handle list, and breaks away from the job only when the immediate job allows it. When breakaway is refused, it starts inside the job and a note line says so. The lock's probe covers its death.
- **The warm-up is laid out and worded like a routed run.** It gets the same sibling pair, the same rewrite, the `./name` mark and the `-p:SourceRevisionId=<HEAD>` stamp, through `target()` and `builder_words()` split out of `decide.rs`. A different layout makes `clear_work` wipe the warmth, and a missing stamp makes the next real run rebuild everything.
- **When the PC drops its record:** only on a `cold` fallback (the preflight finds no matching `{Work}.warm`) and a `layout` fallback. Dropping it on `version`, `tools` or `inputs` would re-warm every worktree after each `gate push` for nothing, since the preflight's `.warm` check catches a folder that really went cold.
- **Warm-up commands need not match the repo's routing patterns.** Consent is per repo, and a restore-only script that is never routed may still be a warm-up.
- **`gate warm` in the foreground on a warm folder** runs anyway: a warm-up of a warm folder is incremental.

## Invariants

- **I1. At most one warm-up per key at a time**, across sessions on this PC and across PCs on the builder. Enforced in this order: the warm lock, the key mutex (bounded wait, a new waiting variant of `hold_key`, `remote/client.rs:216-224`), the cap, then the box's `{Work}.lock` (`take_lock` in `boxside/run.rs`'s `run`). This never inverts a routed run's order (key mutex → remote slot → worktree locks → box lock).
- **I2. A stale `warming` record never keeps runs local longer than the warming process's own bounded life.** That life is bounded by the key mutex wait, the git limits, the client watchdog limits, and a self-deadline of 5× the warm-up ceiling plus slack. The warming process retries its own lock open for about 200 ms, to ride out a route's probe.
- **I3. A `warm` PC record means the box wrote a matching `{Work}.warm` for that host after the commands passed, and a run routes only if the box's `.warm` still matches at its preflight.** The box preflight says `gate: remote warm <hash|none>` (`warm_state` in `boxside/preflight.rs`), read before `ready()`. The client compares it after the version check (`client.rs:1188-1191`) and before parity and push. A mismatch ends `Refused(FallbackReason::Cold)`, a new reason in `gate-state/src/journal_writer.rs`. On `cold`, the run deletes the PC record while still holding the key mutex, then starts a warm-up.
- **I4. `{Work}.warm` never outlives the folder state it describes.** `lay_out` deletes it before writing `changing` (beside `.inputs`, in `lay_out` of `boxside/run.rs`), and also when the work folder has to be created. A warm-up deletes it under the lock before its first command. The preflight says `none` when the folder or the main tree's `.git` is missing, and for anything that is not exactly one 64-hex hash, including a read that collides with a write.
- **I5. A run never routes on a record whose host is not `remote.json`'s current host, or whose hash is not that of the file on disk now.** One hash function (SHA-256 of the raw bytes, BOM included) is shared by the route and the warm-up. The warm-up reads the file once, late, and uses those same bytes for the commands, `WarmHash` and the record.
- **I6. Consent is checked by the PC process that sends the commands**, from a fresh read of `remote.json` just before the run call, against the bytes it sends. So the warm payload is built after the recheck. A new `warmup` reader in `gate-router/src/config.rs` matches repo names in any letter case; a bad value means no consent.
- **I7. The commands come only from the file's bytes**, never from the record, the log, the starter's arguments or the builder. `gate warm` takes only `--cd`, `--background` and test seams.
- **I8. A warm-up lays out the work folder and treats its words exactly as the worktree's routed runs do** (see "The warm-up is laid out and worded like a routed run" above).
- **I9. A warm-up brings nothing back and changes no PC worktree state.**
  - The client skips sync-back and outputs (`client.rs:879-894`) and sets outputs to none, so no pending marker or cache is touched.
  - The warm-up never goes through `Run::routed`, so no nothing-back record or stale lines.
  - The box skips `RunOutputs::read`, `back_commit` and `bring_back`, but still clears the back refs.
- **I10. A box that does not know the warm payload refuses it** (`WarmCommands`, no `Command`); the version gate stays the main guard.
- **I11. A warm-up never makes a real routed run wait or fall back** (no remote slot).
- **I12. The background process cannot keep its caller waiting** (no inherited handles, see "Detach").
- **I13. The record, log and lock in the state root are hints.**
  - Write through `write_versioned` and its strict parser (`gate-state/src/v1.rs:638-658`).
  - Open the log and lock without following links, and require a plain file (`check_plain_file`).
  - Build failure reasons from structured outcomes, never from command output, and print them escaped.
  - A record that exists but cannot be read keeps the run here with a line and starts nothing.
- **I14. A failure backoff never outlives its cause** (see "Failure classes" above).
- **I15. The warm check runs only for a command that would otherwise route**, after every other local reason.
- **I16. One ceiling covers the whole warm-up, and a stop between commands starts no further command.** Each command's `Ceiling::begin` (inside `command` in `boxside/run.rs`) gets the remaining load-adjusted budget, `before_the_job` runs before each command, each command's job reuses the run's job name so `stop` finds it, and the client's limits use the warm-up ceiling.

## Briefs, in order

Each lands on `main` and stays dormant until B5. B1, B2, B3 and B5 touch central or wide files.

1. **B1 foundations** (state and router, no behaviour change):
   - `gate-state` `v1.rs`: the warm record, log and lock paths, and a strict `WarmRecord`.
   - `journal_writer.rs`: `Cold`.
   - `gate-router` `config.rs`: `warmup`.
   - a new `gate-router/src/warm.rs`: the file parser (1 to 8 commands, a timeout in seconds with a default of 1800, 64 KiB, BOM), the hash, the pure judge, the line texts.
   - `decide.rs`: split into `target()` and `builder_words()`.

   Tests: v1 rows and round trip, the `cold` text, `warmup` config shapes, the parser and the judge's truth table, and the existing decide tests unchanged.
2. **B2 box side** (`boxside/payload.rs`, `run.rs`, `preflight.rs`, `ceiling.rs`): warm requests, `.warm`, the preflight line, the command loop with one budget and a stop check per command. Conflicts with GATE-304 (`ready()`) and GATE-231 (`preflight.rs`).
3. **B3 client warm mode and the `cold` refusal** (`remote/mod.rs`, `remote/payload.rs`, `remote/client.rs`). Conflicts with GATE-231 in `client.rs`.
4. **B4 the `gate warm` verb** (`gate-args` cli, request and usage; `answer.rs`; a new `gate/src/warm.rs`), with a new suite module `warm`.
5. **B5 the route goes live** (`route.rs`, `run.rs`, with a spawn seam `--test-warm-spawn <file>`). `run.rs` is central.
6. **B6 the detached launch** (`gate-win32` `create.rs`, `launch.rs`, `piped.rs`, central). Sequenced after GATE-231, which rewrites `create.rs` and `launch.rs`.
7. **B7 `gate check`** (`check/local.rs`, `check/report.rs`): warm-up file problems, consent shape, record state.

Each brief's docs sweep lands with it:
- `docs/design.md`: the state-root files, the routing order, `remote.json`, the box payload keys, `.warm`, the fallback reasons, the `gate warm` CLI;
- the glossary: warm-up, warm, cold;
- an ARCHITECTURE Task Index row: "Make a routed run depend on a per-key builder condition";
- `docs/test-map.md` rows;
- a repo inbox request to ops-tower about the `cold` reason and the `remote\warm\*.json` records.
