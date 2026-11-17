# GATE-1237: why a builder work folder grew to 664 GB, and how to bound it

Measured 2026-11-17 (UTC), read only: directory walks and fingerprint reads in the Windows builder's work root over ssh, the builder's sccache statistics and prune event log, the PC's gate journal (release lines back to 2026-10-11), and the caller repos' git history and Cargo manifests. Nothing on the builder was written or removed.

## Short answer

A cargo target directory only ever grows. Every output file and incremental cache folder is named by a hash of the unit's package version, features, profile, target kind, rustc version and its dependencies' hashes, and cargo never deletes the ones a newer hash replaces. rustc's own incremental garbage collection only removes old sessions inside one crate's folder, never the folder of a hash that is no longer built.

The main checkout's work folder never goes idle for a day and its worktree is never gone, so neither prune ever removes it. Every release commit renames every workspace crate (the workspace version is in each hash), and every lock change renames everything above the changed package. On rustling-tulip's main that happened about 24 times in 17 days, and each time left a full copy of the workspace's test-release and debug outputs behind.

`incremental = true` in rustling-tulip's `test-release` profile makes each copy expensive. The incremental caches are 59% of the bytes in the live rustling-tulip worktree folders and about two thirds of each test-release copy.

The main folder was not the only thing filling the volume. At the last prune before the fill, the 218 other work folders held about 910 GB. The Windows prune has no free-space floor (the Unix box's has one), so nothing stops the sum of many day-old worktree folders from reaching the volume's size.

## What a warm cargo work folder holds

The largest live rustling-tulip worktree folder, `<root>\work\rt-1869-8e090e17`, was 30.97 GB after 13 routed runs in 46 minutes. The runs were six `cargo nextest run -p rustling-tulip-native --cargo-profile test-release`, three `cargo clippy -p rustling-tulip-native --all-targets --all-features`, three `cargo run -p rustling-tulip-native` and one `rt.ps1` run, which runs nextest over `--workspace` and again with `--features driver`. Every file in it was written that day.

| Part | test-release | debug |
| --- | --- | --- |
| `incremental\` | 14.39 GB, 114 crate folders | 3.99 GB, 48 crate folders |
| `deps\` | 5.94 GB, 2761 files | 5.04 GB, 2746 files |
| `build\` (build script outputs) | 0.26 GB | 0.28 GB |
| `examples\` | 0.10 GB | 0.77 GB |
| `.fingerprint\` | under 0.01 GB, 4004 files | under 0.01 GB, 4628 files |
| Profile total | 20.84 GB | 10.08 GB |

In test-release `deps\`, the bytes were 2.00 GB of `.rlib`, 1.27 GB of `.rmeta`, 1.76 GB of `.exe` (73 test and binary executables) and 0.84 GB of `.pdb` (111 files). The `.pdb` files are 4% of the profile, so debug info is not where the space goes. The debug profile's `.pdb` files totalled 0.10 GB.

Most crates in `deps\` had more than one hash. In test-release, 255 of 573 crates had two or more (`rustling_tulip_native` had 9, `windows_sys` 6). Only 2.31 GB of the 5.94 GB belonged to each crate's newest hash. In debug, 448 of 502 crates had two or more, and 4.49 GB of the 5.04 GB was in the older hashes. Those are not stale yet: in a folder an hour old, each hash belongs to a shape that is still being run.

Reading each hash's fingerprint JSON shows why one crate has several hashes. All 1037 fingerprints carried one rustc hash, so the toolchain played no part here. The builder's stable toolchain is 1.99.1, installed 2026-10-28, and has not changed since.

- Feature sets. `-p rustling-tulip-native`, `--workspace` and `--features driver` each resolve different features for shared dependencies, so `windows-sys` was built with six different feature lists. `rustling-tulip-native`'s own lib, test and integration-test units each exist with `[]` and `["driver"]`.
- Target kinds. Each of a package's lib, lib-test, bin, bin-test and integration tests is its own unit with its own hash.
- Host or target. Build-script and proc-macro dependencies are built under the host's `build-override` profile, which gives a different profile hash than the same crate built for the target.
- Check against build. In debug, clippy's metadata-only units hash differently from `cargo run`'s, because cargo mixes the workspace wrapper into a workspace member's hash. Clippy and build also differ by mode. That is why 398 debug crates have exactly two hashes.
- Dependency hashes. Two `rustling_tulip_native` lib units, written 22 minutes apart, had the same features, profile and target hashes but different unit hashes. A dependency below them had changed hash between the runs, and each unit's hash includes its dependencies' hashes.

Cargo documents this behaviour in `compute_metadata` (`src/compiler/build_runner/compilation_files.rs` in rust-lang/cargo). It hashes the features, profile, mode, LTO, host or target kind, target name and kind, and `rustc -vV` on stable. It adds the package id (name, version, source) and the sorted hashes of every dependency. Its own comment says a value wrongly included gives "the correct output but take[s] more space than needed", because old artefacts are never reused or removed.

Incremental caches multiply the same way. rustc names each crate's incremental folder by the crate's `-C metadata`, so every new unit hash gets a new folder. rustc's documented garbage collection (`compiler/rustc_incremental/src/persist/fs.rs`) deletes "any finalized session directories for a given crate except for the most recent one". It never deletes another hash's folder. The measured folders hold one or two sessions each, matching that.

Logical sizes overstate disk use only slightly. rustc hardlinks unchanged files between an incremental folder's two sessions, and cargo hardlinks uplifted binaries. Deduplicated by file id, `rt-1869-8e090e17` is 29.61 GB against 30.97 GB logical (1632 multi-link files), and the whole root summed logically (777 GB of work folders plus 158 GB of sccache) is about 3% above the volume's used bytes. ReFS block cloning plays no part.

The PC's own `target` in rustling-tulip is no long-lived comparison. It holds 5.9 GB, all written that day: 5.55 GB of debug (clippy's metadata, plus 3.21 GB of incremental caches) and 0.15 GB of test-release, because that repo's heavy runs route.

## The environment a routed cargo run gets

The gate sets no cargo variable on a routed run. The command's environment is the box's own process environment from the ssh session (machine and builder-user variables), the caller's `--env` pairs, and the gate's build-server and git settings. No code under `crates/` names `CARGO_INCREMENTAL`, `CARGO_TARGET_DIR`, `RUSTC_WRAPPER` or `SCCACHE_*` for a run. `gate push` empties `RUSTC_WRAPPER` and `RUSTFLAGS` for its own Unix-box build only.

On the Windows builder the machine environment has no `CARGO_*`, `RUST*` or `SCCACHE_*` variable. The builder user's environment has `SCCACHE_CLIENT_SIDE=1` and `SCCACHE_IGNORE_SERVER_IO_ERROR=1`, and its cargo config has `[build] rustc-wrapper` pointing at sccache 0.18.0, all written by `tools/builder/install-toolchains.ps1`. The sccache disk cache's cap is 469,224,652,800 bytes (437 GiB), and it holds 158 GiB.

`CARGO_TARGET_DIR` is unset, so `target\` lives inside each work folder. `CARGO_INCREMENTAL` is unset, so each profile's own `incremental` setting applies. Cargo's default is on for `dev` (debug and clippy) and off for `release`. rustling-tulip's `Cargo.toml` turns it on for `test-release`, with the comment that the profile "rebuilds an edit in ~15 seconds".

Debug info is cargo's defaults: full in debug, none in release and test-release. On MSVC, link.exe still writes a `.pdb` for each linked binary.

sccache cannot cache an incremental compilation. Its `docs/Rust.md` (v0.18.0) says "rustc's incremental compilation needs to be disabled". Its statistics on the builder count 12,404 calls refused as `incremental`, next to 40,357 refused for `crate-type`. Per the same page, `bin`, `dylib`, `cdylib` and `proc-macro` crates "cannot be cached", which includes every test executable.

The Rust hit rate is 68.43%. So sccache warms external dependencies, but every workspace crate in debug and test-release is compiled by rustc and its outputs kept in the work folder.

## Rate of growth

The main checkout's folder, `<root>\work\rustling-tulip-c8301624`, cannot be measured any more. It was deleted, and at the time of measurement it had been recreated as a bare checkout of 19 files with no `target\`. Its growth is reconstructed from the journal and git instead, so the figures below are inference, not a measurement of the folder.

- Lifetime: the builder's toolchains were installed, and its oldest kept lock file written, on 2026-10-29 (UTC). The key's first routed run after that started on 2026-10-30 at 01:57. From then on no gap between its runs reached 20 hours, so the age prune never removed it. The folder lived about 17 days, to its last run on 2026-11-16 at 22:03 (UTC).
- Use: about 690 routed runs in that time, from 15 to 80 a day in the last two weeks. The most common were `cargo clippy --workspace --all-targets --all-features` (171 runs), `cargo nextest run --workspace --cargo-profile test-release` (167), `cargo run -p rustling-tulip-native` (145) and nextest of `rustling-tulip-native` with `--features driver` (98).
- Average growth: 664 GB in about 17 days, about 39 GB a day or 0.96 GB per routed run. Test-release alone grew about 30 GB a day.

The growth comes in steps, not per run. A warm run of an unchanged shape rewrites its own files under the same hashes, and rustc drops the previous incremental session, so it adds nothing lasting. What adds a lasting copy is a change to a hash input. Between 2026-10-30 and 2026-11-16, rustling-tulip's main had 53 commits touching `Cargo.lock` and 71 touching a `Cargo.toml`:

- 12 were release commits (`chore: release v0.12.0-alpha` and so on). Each bumps `[workspace.package] version` and with it the package id, and so the hash, of every workspace member in every profile and shape.
- 12 changed external packages. One, on 2026-11-13, changed 504 of them.
- The rest added workspace members or dependency edges, which re-hash the crates above them.

A copy costs about what a fresh folder's first runs of the same shapes cost: 20.8 to 21.1 GB of test-release in the three fresh folders that ran the main's three nextest shapes (`rt-1869`, `rt-1871`, `rt-1872`). That is roughly 14.4 GB of incremental caches and 6 GB of `deps\`, and debug adds about 10 GB more. 506 GB of test-release over 24 renaming changes is 21 GB each, which is consistent with each renaming change leaving one full test-release copy behind.

A fresh worktree folder grows quickly too, but only up to the cost of its shapes. The 35 live rustling-tulip folders were 1.75 to 30.97 GB after 2 to 28 runs within one to two hours, 422 GB in all: 250.1 GB of incremental caches, 146.7 GB of `deps\`, 14.2 GB of `build\` and 6.9 GB of `examples\`.

Day-old folders are pruned, and the PC's keyed prune removes a worktree's folder an hour after the worktree is gone. Even so, the 2026-11-16 17:20 prune (builder local time) counted 219 work folders, and 255 at its busiest pass on 2026-11-11. At that 17:20 pass, 123.4 GB was free. That leaves about 910 GB for the 218 folders other than the main one.

The prune's event log gives the volume's free space at each six-hourly pass. The lowest point of each day fell from about 1,156 GB on 2026-10-30 to 373 GB on 2026-11-09, 279 GB on 2026-11-15 and 123 GB on 2026-11-16, before the fill. Between two passes, free space fell by as much as 344 GB (2026-11-03, 11:17 to 17:17).

## The .NET folders

The yaat and yaat-server folders are large but bounded. The biggest, `<root>\work\yaat-68817460`, is 31.35 GB: 16.82 GB in `bin\Release`, 13.12 GB in `bin\Debug` and 0.55 GB in `obj`. `yaat-b19da5fc`, in use since 2026-11-09 with outputs written on every day from then to 2026-11-16, is 29.69 GB, so a .NET folder does not grow with age. MSBuild writes each project's output to the same path for each configuration and target framework, and overwrites it.

What dominates is native runtime assets that NuGet packages copy into every test and tool project's `bin\<Configuration>\net10.0\runtimes\`, for every runtime identifier. In `yaat-68817460` that is 9.68 GB of `.dll`, 8.78 GB of Linux `.so`, 3.68 GB of macOS `.dylib` and 4.34 GB of `.pdb`. The single largest files are CUDA libraries (`cublasLt64_13.dll`, 0.45 GB, in four places).

Debug and Release each hold a copy. yaat-server folders are 0.84 to 12.13 GB, also two configurations of the same set.

## Recommended bounds

| Cause | Bound | Disk effect | Speed cost |
| --- | --- | --- | --- |
| Stale hash copies in a folder that never ages out | Per-folder size cap in the Windows age prune: an idle work folder over the cap is removed like an aged one | Main folder held at the cap; healthy folders (31 GB at most today) untouched | One cold first run after each removal: median 163 s against 61 s warm for a first test-release nextest of a folder (140 cold, 621 warm runs) |
| Many day-old worktree folders | Free-space floor pass in the Windows age prune, as the Unix box has, oldest use first, and a prune more often than every six hours | Volume kept above the floor whatever the folder count | Cold runs for the oldest idle folders only when the floor is hit |
| Incremental caches | `CARGO_INCREMENTAL=0` (or `build.incremental = false` in the builder user's cargo config) for routed cargo runs | About 59% of a rustling-tulip folder, and about two thirds of each stale test-release copy; workspace library crates become cacheable by sccache | Slower warm edit rebuilds of workspace crates, unmeasured; test-release builds at opt-level 3, where it may be large |
| Hash churn from toolchain updates | None needed now; a rustup update re-hashes every unit and is caught by the cap | Not a cause in this window | None |
| Debug info | None | `.pdb` is 4% of test-release and 1% of debug | None |
| .NET native runtime assets | Caller repo's choice (copy only the `win-x64` runtime assets on the Windows builder) | Up to about 12 GB a yaat folder | None measured |
| sccache disk cache | Already capped at 437 GiB, least recently used evicted | Must be counted in the volume's budget beside the work folders | None |

**Adopt the per-folder cap first.** It is the only bound that touches the folder that broke the volume, and it leaves the caller's build settings alone. The prune already removes folders under their lock and renames them aside, so the cap is one more reason to take the same path, judged by a walk that takes about 8 s over the whole root.

A cap of 60 GB fits the main checkout's own warm set with headroom. That set is about 21 GB of test-release for its three nextest shapes, about 10 GB of debug for clippy and `cargo run`, and the 11 GB release build. At the growth measured, the cap would remove that folder once or twice a day. Each removal costs the first run of each shape about 100 s more than a warm one, a few minutes a day in all. No live folder today would be touched.

A finer trim, removing only the units of stale hashes, would keep the main folder warm through a reset. But it needs a last-used signal. The volume reports last-access updates as enabled, yet no fingerprint or rlib in two folders used over 46 and 94 minutes showed a read after its write. cargo-sweep's `--time` judges by that access time, so it is not safe here without more proof. File write times are no substitute: a dependency that is fresh but unchanged for a week is still in use.

**Adopt the free-space floor second.** It is a parity gap: `crates/gate-box-unix/src/prune.rs` removes the oldest-used idle folders while under 100 GiB is free, until 150 GiB is, and `crates/gate/src/boxside/prune.rs` has no floor at all. On the Windows builder a floor checked every six hours must sit above the 344 GB measured drop between two passes. Running the prune hourly, or reading free space at run admission, lets the floor sit nearer the Unix box's.

The main folder is the most recently used one, so the floor alone would not have saved the volume. That is why the cap comes first.

**Trial `CARGO_INCREMENTAL=0` before adopting it.** It cuts each stale copy by about two thirds and the live worktree folders by more than half, and it lets sccache serve unchanged workspace library crates. But it reverses a speed choice rustling-tulip made deliberately for test-release, and the cost was not measured here, since a measurement needs builds on the builder.

Once the cap and floor bound the disk, it is a trade of speed for space, not a fix. It should be decided per repo, by timing a warm edit-rebuild of `rustling-tulip-native` in test-release both ways. A per-call `--env CARGO_INCREMENTAL=0` or the repo's own profile can carry it without the gate overriding every repo's profile.
