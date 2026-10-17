# Remote inputs (GATE-22)

A routed command can need ignored files only the PC has: an integration-test group loads a helper a local step published into `.tmp/`, and the snapshot sends only tracked and untracked-not-ignored files. This design sends a repo's declared ignored inputs to the builder's work folder before the run, compressed, and sends only what changed.

The map is `.tmp/map-gate-22.md` (not committed; its facts are quoted here where a decision rests on them). Every decision below is an orchestrator decision under the `gate-nextup` profile ("Decisions"), open to the owner.

Large: a routing-file key, a client phase, a new box verb, a box install step and record, two seams, a fallback reason. It builds on GATE-282's stdin requests.

## Decisions

**I1. Key.** `inputs` in `remote.json` is a table beside `outputs`, keyed by repo name, each value a list of globs, read by the same `read_table` with the same `[`/`]` ban (`gate-router/src/config.rs:89`, `:444`). No default list. A dotnet `--no-build` command still sends its inputs (the drop withholds outputs, not inputs).

Worst case: a typo'd repo name sends nothing; `gate check` does not report it (I17).

**I2. Globs and exclusions.** Relative to the worktree root, `/`-separated, case-insensitive, as outputs (`outputs/glob.rs`). Inputs have their own exclusion set, so `.tmp/` is reachable.

Never sent: any path with a `.git` component, `.lfsconfig`, `.env`, `*.env`, `.env.<name>` and `.envrc` at any depth, matched without regard to case (secrets do not go to a builder; no opt-in), the gate's own archives and stages under `.tmp/` (named for the work folder's key: the outputs' and the inputs' own), and the run's own log and `.err` files, found by canonical path.

Worst case: a repo whose command needs a `.env` on the builder cannot route it; the run says nothing is wrong and fails on the builder, so the repo keeps that command local.

**I3. Which files.** Only files git reports ignored in the PC's worktree (`git check-ignore`, which includes the global excludes and `.git/info/exclude`). A match that is not ignored is already in the snapshot: it is skipped with a log-only line, `gate: inputs: <path> is in the snapshot; not sent as an input`.

**I4. Ignore rules only the PC has.** The box keeps every installed input out of its own git: after an install it rewrites `<Work>\.git\info\exclude` from the `<Work>.inputs` record (I12), with the no-link handling `preflight::ready_tree` gives `.git`. So `clean -fdq` keeps the inputs and the box's back commit never picks one up, whichever ignore rule made it ignored on the PC.

A folder found at `.git\info\exclude` (or at `.git\objects\info\alternates`, which `ready_tree` writes) is never walked or deleted: it is renamed beside itself to `<name>.gate-moved-<n>` (`n` from 1 to 100, never replacing an entry), since another account may have filled it, and the box says `gate: remote moved aside the folder <old> to <new>`. Anything else at the name is deleted as itself.

Worst case: a box-side `info/exclude` the user's command edits is overwritten at the next install.

**I5. Transport.** A new box verb `inputs`, two calls per routed run that has an `inputs` list: a diff call (request + manifest of path, size, SHA-256; the box answers the paths whose copy differs or is missing), then a put call with an archive of exactly those files, skipped when nothing differs. Both after `push` and before the run call (`remote/client.rs`, the `outputs::marked` seam), so a refused route uploads nothing and the run's clocks do not count the upload.

Worst case: one more ssh setup (about 2 s) per routed run of a repo that declares inputs, even when nothing changed. Only repos with an `inputs` list pay it. The rejected fold-into-run shape trusts a PC record of what the box holds, and a stale record turns into refusals.

**I6. Box verb contract.** `gate box inputs --stdin`. Stdin: the request line (JSON: `Key`, `Work`, `Lock`, `Nonce`, `Step` = `diff` or `put`, `Pair` when GATE-249 sends one), then the manifest line, then for `put` the raw archive bytes to end of stdin. The verb takes the work folder's lock exactly as `run` does (busy refusal, exit 75) and runs the same layout step `run` runs, so a later run finds the layout unchanged and clears nothing.

Stdout:
- diff: `gate-remote-inputs: need <n>` then `gate-remote-inputs: path <escaped path>` per file;
- put: `gate-remote-inputs: installed <n>`.

The run call's request carries `InputsDigest` (SHA-256 of the manifest line). After its layout step the box compares it with the `<Work>.inputs` record's digest; a mismatch (another run changed the work folder between the calls) is the refusal tag `inputs` and the client runs the command here (I10).

**I7. Caps.** 50,000 members, a 64 MiB manifest, 4 GiB decompressed per put (a seam on each side: `--test-remote-inputs-budget-bytes` on the client, `--test-inputs-budget-bytes` on the box, with the five refusal forms the outputs seams have). The members and manifest caps count what the globs match, the 4 GiB what the diff named. A set past a cap is not sent: the route is refused with reason `inputs` (I10).

Worst case: a repo whose inputs pass 4 GiB never routes those commands, and the line names the cap.

**I8. Watch.** The diff call is watched as a fetch is: a stall on its output and its own transfer ceiling (30 min) counted from its own start. The put is watched while the archive is fed by the later of the last byte fed and the last byte of output, and once it is fully fed by its transfer ceiling alone, since the box installs without a word.

A cut call says its reason in the `inputs` fallback line and no `STALLED` or `TRANSFER TIMED OUT` line, which the log scan would fail the run here for. `--test-remote-watch-call` names the diff `inputs-diff` and the put `inputs`.

**I9. A glob that matches nothing** gets a log-only line, `gate: inputs: <glob> matched nothing`. A file that changes while it is packed fails the pack (the archive's own rule), which is a failed step (I10).

**I10. A failed step runs the command here.** Any failure of either inputs call (refusal, cap, transfer ceiling, stall, a pack failure, the run's `inputs` digest refusal) refuses the route with a new fallback reason `inputs`: console `gate: remote <host> inputs; running here`, log `gate: remote inputs not sent: <reason>`. The journal's fallback reasons gain `inputs`, so ops-tower gets a repo inbox request when the item lands (Docs-owner map, "a file format ops-tower reads").

Worst case: a builder that always fails the step sends every run of that repo local; the journal's reason shows it.

**I11. PC hash cache.** Its own file `remote\<key>.inputs-hashes` (a `v1.rs` path function beside `outputs_hashes_path`), never the outputs cache, whose saves drop what the outputs walk no longer finds. A cached hash that the pack finds wrong is dropped from it. No PC record of what the box holds: the diff call is the truth.

**I12. Box record.** `<Work>.inputs` beside `<Work>.tools` and `<Work>.layout`, opened the way `.tools` is (refuses a link, a folder, more than one name): per installed file its path, size, SHA-256, and the last-write time and file id the box left it with, plus the manifest digest (empty while an install is under way, written last once the work folder holds every file of the manifest).

A copy a put left in place because it changed on the box is a kept entry (`{"kept": true}`), listed in the exclude file and never trusted. The diff trusts a copy whose size, last-write time and file id match the record, and hashes any other. The layout step's clear removes the record with the work folder's children.

**I13. Stale inputs.** A put deletes a box copy that left the manifest only when the record installed it and the copy still hashes to the record's value; it deletes nothing else.

**I14. Destination safety.** Member paths are plain and relative; the put unpacks into a stage `<root>\inputs-stage\<key>-<nonce>` (`inputs-stage-medium` for an unelevated box), outside `Work` and on its volume, protected from the instant it exists (owner, DACL and label given at creation); a stages folder the box did not make refuses the put (`cannot use <path>: another account made it; ...`), and the route then falls back with reason `inputs` (the fallback is step (c)).

The put then moves each file in with `check_folder` on every parent under `Work` and a final-path check, as `remote/install.rs` installs outputs on the PC. An existing target that is not a plain file, or has more than one link, refuses the put. No path rewriting. The box writes inputs with the same token and owner as the checkout, so whatever a command can read of the checkout it can read of its inputs (GATE-231's desktop runs included).

**I15. Pairs.** Each repo's own `inputs` list applies to its own tree; the manifest's paths are prefixed with the tree's folder name (`{leaf}/…`, `{sibling}/…`). Reached only once GATE-332 builds pairs; until then a pair whose repos declare inputs routes, is refused with reason `inputs` before any inputs call and runs here.

**I16. Lines.** Console, when a put sent files: `gate: inputs: <n> file(s), <X.X> MB (<Y.Y> MB compressed) in <Z.Z> s`. Log only, when nothing differed: `gate: inputs: <n> file(s) unchanged`. Builder text is escaped as every builder line is. The `passed` split gains `inputs <n> s` when an inputs call ran.

**I17. `gate check`** reports nothing about inputs.

## Landing

Steps (a), (b) and (c) are built and on main.

Steps in order: (a) router key and `RemoteRun`/`RemoteCall` field; (b) box verb with diff and put, record, exclude rewrite, stage and install, seams; (c) client phase, cache, lines, fallback reason, the run's digest check; (d) docs: design.md Routing and The box side (the Inputs bullet), ARCHITECTURE.md Task Index row "Send ignored inputs with a routed run", GLOSSARY.md "input", test-map rows; ops-tower inbox request for the `inputs` fallback reason.
