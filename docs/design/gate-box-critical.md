# CC-168: ops-tower critical gate use

Checkout read: X:/dev/ops-tower (git root X:/dev/ops-tower). Journal: C:/Users/someone/AppData/Local/gate/events.1.jsonl + events.jsonl.

## 1. Where ops-tower asks for critical

Only two places, plus prose that describes them:

- `X:/dev/ops-tower/prek.toml:92-98`: the pre-push hook `cargo-test-timing`: `pwsh -NoProfile -File tools/gate.ps1 -Log .tmp/prek-cargo-test-timing.log -TimeoutSeconds 900 -Slot critical -- cargo test --workspace --profile test-release -- timing::`. Runs on every `git push`. The heavy sibling (prek.toml:81-87) runs `-- --skip timing::`.
- `X:/dev/ops-tower/.claude/skills/ops-tower-nextup/SKILL.md:29`: "`-Slot critical` is only for timing-sensitive tests, the `timing::` tests below (owner ruling); a browser suite or any run that is merely slow or may time out is not one".

  Line 31: "A test that a process finishes within a reasonable time under reasonable conditions runs critical; a process that must finish in time even under extreme load gets its own explicit test, run critical, that starts and stops a deterministic CPU burner itself."

  Line 33: `cargo test --workspace --profile test-release -- timing::` (critical, 900), scoped in briefs to `--test <suite>` or `-p <crate>`. Line 35: timing tests live in a module named `timing`; "a new one goes in such a module". Line 49: push runs bare because "the gate refuses a critical gate nested under a heavy or light one (exit 2)"; line 51: a push takes 15 to 77 s.
- Browser suite is explicitly NOT critical: SKILL.md:40 (`cargo build --release --example fixture_server` heavy, `pnpm --dir tests/browser test` heavy, "Not run by prek or CI", line 46).
- No other `-Slot critical` command: `tools/install.ps1:45` uses heavy; CLAUDE.md, README.md, docs/ARCHITECTURE.md have none. No `.claude/agents`, no briefs file. Doc comments only: `tests/briefing/run.rs:512`, `tests/http/gate.rs:756`, `tests/http/briefing.rs:582` ("Tests whose pass depends on CPU timing; run apart in the gate's critical slot").
- ops-tower-nextup profile skill is at X:/dev/ops-tower/.claude/skills/ops-tower-nextup/SKILL.md (none under ~/.claude/skills).

## 2. What the commands run and why critical

One command in all: `cargo test --workspace --profile test-release -- timing::` (also `--test http -- timing::` and `--manifest-path ... --workspace` variants in the journal). It compiles the whole workspace's test binaries (incremental, warm), then runs the `timing::` tests in 8 modules (55 tests by `#[test]`/`#[tokio::test]` count from each module's start to EOF of its file, an upper bound where the module is not last in the file):

| Module | Tests (approx) | Timing-sensitive assertions |
|---|---|---|
| `crates/claude-accounts/tests/store/locks.rs:281` | 3 | 1 s lock staleness, 50 ms toucher, 5 s deadline loop at :299-313 (lost-lock marked within 5 s of denial) |
| `crates/claude-accounts/tests/refresh/switch.rs:1625` | up to 17 | `LOCK_WAIT` 700 ms; `waited >= LOCK_WAIT` at :1665; `wait_for` 5 s deadline at :1635; 3 s/5 s lock timings :1642/:1675 |
| `tests/http/gate.rs:756` | 5 | gate-slot read must answer within `DEADLINE + 500 ms` while a snapshot is held (:~777); sleeps `MAX_AGE + 200 ms`; state age `>= 1.0 s` / `< 1.0 s` (:~810) |
| `crates/claude-accounts/src/drain.rs:133` | 3 | `wait_idle` 2 s bound; elapsed `>= 50 ms`/`150 ms` after 100/200 ms sleeps; times out after 100 ms (:153-189) |
| `tests/http/agents.rs:971` | 1 (module; 18 counted includes later tests) | polls must answer within 2 s while a summary call never finishes (:~1002); 200x50 ms wait for the call to start |
| `tests/http/briefing.rs:582` | 1 | run outlives dropped request; 500 x 10 ms poll (5 s) |
| `src/daemon/client.rs:1152` | 3 | keepalive idle 300 ms + pong wait 300 ms; disconnect must happen `< 2 s` after connect (:1195); 1.5 s quiet period must stay connected (:1206) |
| `tests/briefing/run.rs:512` | 2 | `timeout_kills_the_process_and_is_timeout`: 200 ms limit on `ping -n 30`, returns `< 5 s` (:546-553); `timeout_kills_the_grandchildren_too`: 5 s limit, ping found within 4.5 s, leftover ping gone within 3 s (:517-540) |

Why critical: timing. The reason in the docs is "pass depends on CPU timing" (the three doc comments above) and the owner ruling at SKILL.md:29. Memory pressure plays no part. Several bounds are generous (2 s, 5 s) against a ~100-300 ms real duration, so they tolerate ordinary heavy-slot load; the tightest are drain.rs (`>= 50/150 ms` lower bounds only, immune to load) and client.rs (`< 2 s` disconnect after a 600 ms keepalive).

Most assertions are upper bounds on a wait, which fail only under starvation, so by-habit applies to the module as a whole: SKILL.md:31 states the rule as a standing policy, not a per-test measurement. Whole-`--workspace` critical also holds the lock during linking/compiling of test binaries (a warm run is ~22.5 s held).

Not timing tests but on the critical list by filter only: none; the filter is `timing::` name-based.

## 3. Measured use (journal)

Window: first event 2026-10-08T01:03:03Z, last 2026-10-09T04:38:19Z (27.6 h; events.1.jsonl starts the window, events.jsonl ends it). 5558 takes, 97 critical.

- ops-tower critical runs: **36** (33x `cargo test --workspace --profile test-release -- timing::`, 2x `cargo test --profile test-release --test http -- timing::`, 1x `--manifest-path X:\dev\ops-tower.wt\ops-233\Cargo.toml --workspace ... -- timing::`). Sources: main checkout and worktrees (ops-235 etc.), i.e. mostly prek pre-push.
- Held: **832.8 s** total (typically 22.4-23.3 s each; the two `--test http` runs 35.8 s and 27.1 s). The other 61 critical runs (6212 s) are gate self-tests from ~/.claude worktrees (cc-140, cc-145, cc-150, cc-155, feat-gate-remote-slot), not ops-tower.
- The critical runs' own wait to take: **4457.9 s** total (mean 124 s; max 714.9 s at 2026-10-08T16:35Z->16:46Z, 692.6 s and 571.7 s the next); 11 of 36 took under 1 s.
- Others' waiting: 143 non-critical gates (heavy/light) had a queue-to-take interval overlapping an ops-tower critical hold; their overlapping wait seconds during the holds total **~3012 s** (mean 21 s each, bounded by 23 s hold).

  Gates that arrived after an ops-tower critical queued and waited inside its [queue, release] window: 143 gates, **~17054 s** of waiting (upper bound on the barring effect, since part of that wait is ordinary slot contention; the critical ticket bars later gates from the moment it queues, so the bar is the drain time plus hold).
- Context: ops-tower's own non-critical gates in the window: 775 heavy + 555 light, 40675 s held; all non-critical gates 5137 runs, 213215 s held (heavy 176697, light 36518). So ops-tower critical is 0.4% of held seconds but its queue-to-release windows cost others ~4.7 h of waiting at most.
- Pattern: each critical run is a tiny 22 s hold preceded by a long drain (mean 124 s) in which every later gate is barred.

## 4. Could each run reliably on the box?

Single command class, so one verdict plus the browser/other heavies for completeness.

Gate rule today (tools/gate/README.md:120): "Light and critical gates ... stay local". The routing file `C:/Users/someone/AppData/Local/gate/remote.json` allowlists only `delve-the-dungeon` (dotnet commands); ops-tower has no entry, so none of its commands route today, heavy ones included (no `cargo` pattern). So even the heavy `-- --skip timing::` run is local.

What the critical command needs:

- Windows only: crates use `std::os::windows::fs::OpenOptionsExt` (locks.rs:283), `cmd.exe`/`ping.exe` (run.rs:422-541, grandchild search by process-table snapshot), `GATE_TEST_*` env and a sandboxed `%LOCALAPPDATA%` (http/gate.rs:103-121). Box is Windows 11 Pro, MSVC build tools, Rust stable + 1.98.1 + 1.89.0 (install-toolchains.ps1:77-90): fine. ops-tower has no `rust-toolchain.toml`, so stable is used; `Cargo.toml` is edition 2024 (needs rust >= 1.85).
- No desktop, window station, GPU or browser needed by `timing::` tests. Ports: tests bind `127.0.0.1:0` (client.rs:777, http/main.rs:795), not 8765. `ops_tower::bind(... 0)` in pair.rs is outside timing::. No files outside the worktree: scratch dirs under `.tmp` (CARGO_MANIFEST_DIR) or the OS temp; no credentials. `claude` CLI is not invoked by timing tests (the real-claude test is `#[ignore]`d and excluded).
- Ignored inputs: `target/` does not travel, so the first run in a box work folder is a cold compile of the whole workspace (minutes; ignored `target` then persists in the work folder: README says ignored `bin/obj` survive; same for `target`). `Cargo.lock` is tracked. No `.env`.
- Edge: `cargo test --workspace` also builds `window/` (ops-tower-window); it compiles without a display. Not run (no timing tests there), unverified on the box (no probe run; I ran nothing).
- Box notes: README says i9-13900, 24 cores / 32 threads (the task text says 32 cores).

Verdicts:

| Command | Verdict |
|---|---|
| `cargo test --workspace --profile test-release -- timing::` (critical) | Not routable as is: gate never routes critical (README:120) and `remote.json` has no ops-tower entry. Routable with named changes: (a) a `remote.json` `ops-tower` allowlist with a `cargo test *` pattern, (b) either demote the hook/skill to heavy, or teach the gate to route critical onto a box-critical (draining the box's 4 slots). Then it needs no other change. Must stay local only if the owner wants OS-level jitter on the PC measured. |
| `cargo test ... -- --skip timing::` (heavy) | Routable with (a) only; needs a first cold build. Some non-timing tests use `%LOCALAPPDATA%`/claim files from `gate-state` with sandboxes; not checked for fixtures outside git. |
| clippy / fmt / deny (heavy/light) | Out of scope for critical; clippy heavy is routable with (a); fmt/deny light stay local. |
| Browser suite (`pnpm --dir tests/browser test`, Chromium headed/headless via Playwright, heavy) | Not on critical. Would need: `tests/browser/node_modules` and Playwright's Chromium (ignored, do not travel; `playwright install chromium` is not in install-toolchains.ps1), the `fixture_server.exe` built in `target/release/examples` (ignored), and a headless run in session 0 (Playwright config has no `headless: false`, so headless default). Routable only with a named change (a separate install/build step on the box); unverified. |

## 5. Reliability of timing tests on the box

Not measured; reasoning only.

- Upside: the box runs commands at its own load, not the PC's other sessions; 32 threads; the critical hold itself is 22 s of mostly test wall time (sleeps), not CPU.
- Risk: up to 4 routed heavies share the box (README "Concurrent runs": 4 concurrent runs ~40% slower each than 2), so without a box-critical that drains them, timing tests would run under the same kind of contention that critical exists to avoid, with a different CPU (24 cores).

  With a box critical draining the 4 slots, the box would behave like a quiet 32-thread machine: the tightest bounds (client.rs `< 2 s`, http/gate.rs `DEADLINE + 500 ms`, run.rs `< 5 s`) get more headroom than on the PC where critical drains only this PC. A cold-start snag: the first run in a work folder rebuilds, and ping/cmd process-table scans (grandchild_ping) run slower in session 0 under an unusual process table; unmeasured.
- The tests that assert only lower bounds (drain.rs, switch.rs `waited >= LOCK_WAIT`) are load-immune either way. Candidate for moving off critical entirely: all tests with bounds >= 2 s (agents.rs:1002, briefing.rs 5 s, locks.rs 5 s, switch.rs wait_for 5 s) are tolerant of heavy-slot load; only run.rs:546 (200 ms limit), client.rs (300+300 ms keepalive) and http/gate.rs (deadline) are tight. This is a reading, not a measurement.

## Options to put in front of the owner (not decided here)

1. Demote prek.toml:94 hook to heavy (`-Slot heavy`), keep critical only for an on-demand `timing::` run; frees ~832 s of hold and the drain windows. Needs SKILL.md:29-35 and :49 edited.
2. Run the same command on the box (allowlist + critical routing), per section 4.
3. Narrow critical to the tight tests (run.rs, client.rs, http/gate.rs) with a second filter module name, leaving the 5 s-bound tests heavy.

## Rulings

- Routing a critical gate to the box is opt-in per repo: each repo's entry in `%LOCALAPPDATA%\gate\remote.json` says whether its critical gates may use the box's critical pool (and, by its allowlist patterns, which commands). A repo that does not opt in keeps every critical gate on the PC.
- The repo that uses critical gates most first narrows its own critical runs to the timing tests that need the machine alone. The box critical pool (GATE-21) is re-judged on that repo's critical share measured after that split, and is built only if what remains still obstructs.

## DOC DRIFT

- `X:/dev/ops-tower/CLAUDE.md:12` says gate slots are `heavy|light` and omits critical; `.claude/skills/ops-tower-nextup/SKILL.md:29` and `prek.toml:94` use critical. Which side is wrong: CLAUDE.md is incomplete. Owner: CLAUDE.md.
- `X:/dev/ops-tower/prek.toml:92-98` states the hook's reason only as "(timing::, critical slot)"; the "owner ruling" lives in the skill (SKILL.md:29) with no measured justification. No code mismatch.
- `tools/build-box/README.md` line 3 says i9-13900 (24 cores, 32 threads); the task brief says 32 cores. Owner doc: build-box/README.md (already says 32 threads; ok).

## TASK INDEX GAP

Task: "which ops-tower gates use the critical slot and could they route to the build box". Needed several reads beyond the row (skill, prek.toml, 8 timing modules, gate README, journal).

A row should read: "Change which ops-tower runs use the critical gate" -> `prek.toml` (pre-push hooks), `.claude/skills/ops-tower-nextup/SKILL.md` (Gates), `CLAUDE.md:12`, the 8 `mod timing` files listed in section 2, and `C:/Users/someone/AppData/Local/gate/remote.json` (routing). Belongs in `docs/ARCHITECTURE.md` (Task Index) of ops-tower.

## MISSING ENTRY POINT

- ops-tower has `docs/ARCHITECTURE.md` and `docs/README.md` with a glossary ("Critical run" defined at docs/README.md:43). The "timing:: module" convention (tests that must live in `mod timing`) has no glossary entry or docs/ARCHITECTURE.md Test locations mention (only the skill describes it); grep found none in ARCHITECTURE.md.
