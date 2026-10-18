# Remote LFS (GATE-469)

A repo that keeps assets in Git LFS routes with them: the PC sends the LFS objects the snapshot's pointers name to a protected store beside the repo's bare repository on the builder, and the box's checkout, whose LFS filter is already live, writes their content into the work folder with no network access. Repos with no `filter=lfs` attribute are unaffected: git starts git-lfs only for paths whose attributes name it.

It builds on GATE-22's inputs verb (`docs/plans/remote-inputs.md`), whose diff-then-put request, lock, stage, archive, watch and fallback plumbing it reuses. The maps are `.tmp/map-gate-469.md` (push, checkout, back commit) and the invariant pass in GATE-469's comments; every decision below is an orchestrator decision under the `gate-nextup` profile ("Decisions").

## How git-lfs behaves here

Read in git-lfs's source (`lfs/gitfilter_smudge.go`, `lfs/lfs.go`, `fs/fs.go`, `commands/command_clean.go`, `docs/man/git-lfs-config.adoc`):

- Smudge trusts a local object by its size alone and never hashes it; an object of the wrong size is deleted.
- Before reading, it copies or links `<alternate>\..\lfs\objects\<oid>` when present. A work folder's alternates name `<Bare>\objects` (`crates/gate/src/boxside/preflight.rs:394`), so `<Bare>\lfs\objects` is already a read path.
- Clean keeps an existing object of the same size and renames its temp file in otherwise.
- A tracked `.lfsconfig` may set `lfs.url`, `remote.*.lfsurl`, `lfs.fetchinclude`, `lfs.fetchexclude` and `lfs.skipdownloaderrors`, never `lfs.storage`; git config overrides it. With `fetchexclude` or `GIT_LFS_SKIP_SMUDGE`, smudge writes the pointer and reports success.

## Decisions

**L1. Pointers.** After the snapshot commit, the PC lists the tree's blobs of at most 1024 bytes whose `filter` attribute is `lfs` (`check-attr --source <commit>`, the slices of `remote/syncback.rs:1274`) and reads them in one `cat-file --batch`. A blob that parses as a spec-v1 pointer (`version https://git-lfs.github.com/spec/v1`, `oid sha256:<64 hex>`, `size <n>`) is a pointer; any other blob, a raw file committed under `filter=lfs` included, is left alone. No pointers: no LFS step, no cost.

**L2. PC objects.** Read from the worktree's LFS store, `<common>\lfs\objects\<aa>\<bb>\<oid>`, or `lfs.storage` when the repo sets it, `<common>` from `snapshot::place_of` (`remote/snapshot.rs:100-131`). Each object is hashed while it is packed; a missing object, one whose size or SHA-256 differs from its pointer, or one that changes while it is packed refuses the route with reason `lfs`, the log line naming the path and `git lfs fetch`.

**L3. Store.** One store per repo on the builder, `<Bare>\lfs\objects\<aa>\<bb>\<oid>`, beside the bare repository every work folder of the repo already shares (`remote/payload.rs:96-97`), so a new worktree resends nothing and git-lfs reads it through its alternates path with no config.

- `<Bare>\lfs` and every folder under it are made with `protected_descriptor` and adopted only when `is_protected_folder` passes (`boxside/inputs.rs:633-705`); one another account made refuses with `lfs`. No Medium-integrity process can write it.
- An unelevated box, whose protected folders are labelled Medium (`inputs.rs:664`), refuses an LFS route with `lfs`.
- A store shared across repos was rejected: an unelevated box or a command on the box's own token could then corrupt every repo's assets, a prune would need every repo's refs and locks, and the diff would tell any PC which other repos' content the builder holds.

**L4. Transport.** A new box verb `gate box lfs --stdin`, two calls after push and inputs and before the run call: a `diff` (the request line, then a manifest line of `<oid> <size>` entries; the box answers `gate-remote-lfs: need <n>` then `gate-remote-lfs: oid <oid>` per object it lacks) and a `put` of exactly those objects (`gate-remote-lfs: installed <n>`), skipped when none is missing. It takes the repo's store lock shared and the work folder's lock as inputs does.

The put unpacks into a protected stage, checks each member's SHA-256 against its name and its size against the manifest while unpacking, and installs by a rename that creates the file new: an object already there is kept, never replaced. Caps: 50,000 objects, a 64 MiB manifest, 16 GiB per put (seams `--test-remote-lfs-budget-bytes` and `--test-lfs-budget-bytes`). Watched as inputs' calls are (I8), named `lfs-diff` and `lfs` for `--test-remote-watch-call`.

**L5. Checkout.** The run payload carries `LfsDigest` (SHA-256 of the manifest line). After its fetch, under the work lock and with the store held without delete sharing through the back commit, the box:

1. Derives the pointers from the snapshot itself (L1's rule, on the box's own tree) and confirms the manifest named by `LfsDigest` covers them. Each object must be a plain file with one link and the pointer's size. Anything else is `gate-remote-refused: <nonce> lfs`, and the command runs here.
2. Runs the checkout with `-c lfs.url=<a dead local URL> -c lfs.remote.searchall=false -c lfs.fetchinclude= -c lfs.fetchexclude= -c lfs.skipdownloaderrors=false`, and `GIT_LFS_SKIP_SMUDGE` and `GIT_LFS_SKIP_DOWNLOAD_ERRORS` removed from its environment (add both to `GIT_REDIRECTS`, `preflight.rs:37-46`, which `BackGit` shares, `run.rs:2510`). These pins go through `-c`, never into `.git\config`, where the command's own git would read them.
3. Checks every pointer path is a plain file of the pointer's size, since `checkout --force` leaves a file whose stat is clean, and a pointer an older run left would survive. A mismatch is repaired once (delete the files and check those paths out again), else refused `lfs`.

No re-hash at checkout and no record: the oid name, the verified install and having no lower-integrity writer are the record.

**L6. The command and the back commit.** A run with pointers never falls back to the box's own token when the restricted start fails (design.md:2611, `run.rs:1447-1451`); it fails the start, as the pnpm install does (design.md:2521). The back commit's `add -A` (`run.rs:2456`) runs with `-c lfs.storage=<a per-run scratch folder>`, so a clean adds nothing to the store; a changed LFS path is refused at sync-back as today (`remote/syncback.rs:1237`).

**L7. git-lfs on the builder.** A run with pointers sends a `git-lfs` need (a `probe.rs` row, `git-lfs version`, any version); a builder that lacks it falls back `parity`. `gate check --builder` reports it for a repo whose `.gitattributes` names `filter=lfs`.

**L8. Pairs.** A pair whose trees hold pointers is refused `lfs` before any call, as inputs I15 is.

**L9. Lines.** Console, when a put sent objects: `gate: lfs: <n> object(s), <X.X> MB in <Z.Z> s`. Log only, when none was missing: `gate: lfs: <n> object(s) present`. Fallback: console `gate: remote <host> lfs; running here`, log `gate: remote lfs not sent: <reason>`. The `passed` split gains `lfs <n> s`. The journal's fallback reasons gain `lfs` (ops-tower gets a repo inbox request when it lands).

**L10. Prune.** Not built here: the store grows until a prune exists. Booked as its own issue: remove objects no `refs/gate/*` snapshot names and older than N days, under an exclusive store lock that puts and runs take shared.

## Steps

- (a) PC pointer listing and object reading (L1, L2), with unit tests on fixture repos the suite makes with `git lfs`.
- (b) The box verb, store, stage and install, seams (L3, L4).
- (c) The checkout's check, pins and post-check, the token rule, the back commit's scratch storage, the need, pairs, the client phase, lines and fallback (L5-L9).
- (d) Docs: design.md Routing and The box side, ARCHITECTURE Task Index row "Carry Git LFS objects to the builder", GLOSSARY "LFS store", test-map rows, the ops-tower request.

Each step needs an invariant-pass check in its review: the store's writers (only the put's install), its readers (the checkout through alternates, never a Medium writer), and every git call that runs a filter.
