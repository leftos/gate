//! The durations key and its normalised command, the estimate, the `durations.json` store, its rebuild from the journal
//! and the archive with its marker and swap, and the append that runs a caller's step inside the journal hold.

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use flate2::Compression;
use flate2::write::GzEncoder;
use gate_state::{
    DurationKey, DurationStore, Held, JournalEvent, JournalIdentity, JournalLock, KEY_VERSION, KeyInputs, KeyPlace, LockError, RebuildOwner,
    RebuildReads, RebuildReason, Recorded, RemoteTag, RunFacts, StoreError, StoreRead, StoreReadError, Swapped, Verdict, append_line_then,
    archive_rolled_journal, begin_rebuild, duration_key, durations_path, journal_line, normalize_for_durations, place_by_shape, read_store,
    rebuild_store, rebuilding_path, record_run, swap_rebuilt, write_store,
};
use jiff::{SignedDuration, Timestamp};
use proptest::prelude::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::journal_write::{FakeLock, LOCK_NAME, fixed, fresh_dir};
use crate::support::ts;

/// The worktree root every in-git key of these tests strips.
const ROOT: &str = "X:/work/repo.wt/slug";
/// The system temp root the tests pass.
const TEMP: &str = "X:/scratch/Temp";
/// The moment every estimate is made at.
const NOW: &str = "2026-10-03T12:00:00Z";
/// The build box of a routed run.
const HOST: &str = "builder@build-box";

fn in_repo(sub: &str) -> KeyPlace {
    KeyPlace::InGit {
        repo: "repo".to_owned(),
        sub: sub.to_owned(),
        worktree_root: ROOT.to_owned(),
    }
}

/// The key of `command` run locally, in no critical class, from `folder`.
fn key_in(folder: &KeyPlace, command: &str) -> DurationKey {
    duration_key(&KeyInputs {
        folder,
        remote_host: None,
        critical: false,
        temp_root: Some(TEMP),
        command,
    })
}

/// The key of `command` run locally, in no critical class, at the worktree root.
fn local_key(command: &str) -> DurationKey {
    key_in(&in_repo(""), command)
}

fn passed(wall: f64) -> RunFacts {
    RunFacts {
        wall_seconds: wall,
        adjusted_seconds: Some(wall * 2.0),
        avg_cores: Some(1.5),
        peak_cores: Some(3.0),
        free_cores: None,
        verdict: Verdict::Passed,
    }
}

fn failed(wall: f64) -> RunFacts {
    RunFacts {
        verdict: Verdict::Failed,
        ..passed(wall)
    }
}

fn minutes_before_now(minutes: usize) -> Timestamp {
    ts(NOW) - SignedDuration::from_mins(i64::try_from(minutes).expect("a test's minutes fit"))
}

fn days_before_now(days: i64) -> Timestamp {
    ts(NOW) - SignedDuration::from_hours(days * 24)
}

/// Records passing runs of `walls` under `key`, oldest first, one minute apart, the last a minute before [`NOW`].
fn record_walls(store: &mut DurationStore, key: &DurationKey, walls: &[f64]) {
    for (index, wall) in walls.iter().enumerate() {
        store.record(key, minutes_before_now(walls.len() - index), &passed(*wall));
    }
}

/// The store under `root` when it reads as current.
fn current(root: &Path) -> Option<DurationStore> {
    match read_store(root) {
        StoreRead::Current(store) => Some(store),
        StoreRead::NeedsRebuild(_) | StoreRead::Unavailable(_) | StoreRead::Newer => None,
    }
}

/// Why the store under `root` does not read, when it does not.
fn invalid(root: &Path) -> Option<StoreReadError> {
    match read_store(root) {
        StoreRead::NeedsRebuild(RebuildReason::Invalid(error)) => Some(error),
        _ => None,
    }
}

/// The wall seconds of the samples `store` holds for `key`, oldest first.
fn walls_of(store: &DurationStore, key: &str) -> Vec<f64> {
    store
        .history(key)
        .map(|history| history.samples.iter().map(|sample| sample.wall).collect())
        .unwrap_or_default()
}

/// Commands in the shapes the gate's journal holds, each with the command part of its exact and family keys.
const COMMANDS: &[(&str, &str, &str)] = &[
    (
        r#"cargo nextest run --workspace --cargo-profile test-release -E "package(gate-state) or test(/^journal::/)""#,
        r#"cargo nextest run --workspace --cargo-profile test-release -E "package(gate-state) or test(/^journal::/)""#,
        "cargo nextest run --workspace --cargo-profile test-release -E <filter>",
    ),
    (
        "cargo clippy --workspace --all-targets --all-features -- -D warnings",
        "cargo clippy --workspace --all-targets --all-features -- -D warnings",
        "cargo clippy --workspace --all-targets --all-features -- -D warnings",
    ),
    (
        "dotnet build X:/work/repo.wt/slug/src/App/App.csproj -c Release",
        "dotnet build src/App/App.csproj -c Release",
        "dotnet build src/App/App.csproj -c Release",
    ),
    (
        r"dotnet test X:\work\repo.wt\slug\tests\App.Tests -c Release --no-build --filter FullyQualifiedName~Parser",
        "dotnet test tests/App.Tests -c Release --no-build --filter FullyQualifiedName~Parser",
        "dotnet test tests/App.Tests -c Release --no-build --filter <filter>",
    ),
    (
        "pwsh -NoProfile -File tools/x.ps1 -Filter Pipeline",
        "pwsh -NoProfile -File tools/x.ps1 -Filter Pipeline",
        "pwsh -NoProfile -File tools/x.ps1 -Filter <filter>",
    ),
    ("git commit -F .tmp/msg-x.txt", "git commit -F \u{2026}", "git commit -F \u{2026}"),
    ("X:/tools/bin/cargo.exe build --release", "cargo build --release", "cargo build --release"),
    ("X:/work/repo.wt/slug/tools/gate.CMD -Log x", "gate -Log x", "gate -Log x"),
    (
        r#""X:/work/my tools/run.bat" "X:/work/repo.wt/slug/a b/c.txt""#,
        r#"run "a b/c.txt""#,
        r#"run "a b/c.txt""#,
    ),
    ("git show 22abea9", "git show <sha>", "git show <sha>"),
    (
        "git diff 5f524de0c1a2b3d4e5f60718293a4b5c6d7e8f90 HEAD",
        "git diff <sha> HEAD",
        "git diff <sha> HEAD",
    ),
    (
        "tool deadbeef 1234567 abc123 5f524de0c1a2b3d4e5f60718293a4b5c6d7e8f901",
        "tool deadbeef 1234567 abc123 5f524de0c1a2b3d4e5f60718293a4b5c6d7e8f901",
        "tool deadbeef 1234567 abc123 5f524de0c1a2b3d4e5f60718293a4b5c6d7e8f901",
    ),
    (
        "prek run --files a.rs b.rs c.rs --verbose",
        "prek run --files <files> --verbose",
        "prek run --files <files> --verbose",
    ),
    (
        "dotnet csharpier format --include src/A.cs src/B.cs",
        "dotnet csharpier format --include <files>",
        "dotnet csharpier format --include <files>",
    ),
    ("prek run --files=a.rs,b.rs", "prek run --files=a.rs,b.rs", "prek run --files=a.rs,b.rs"),
    (
        "pwsh -File X:/scratch/Temp/agent/run.ps1 -Case One",
        "pwsh -File \u{2026} -Case One",
        "pwsh -File \u{2026} -Case One",
    ),
    (
        r#"npx vitest run -t "parses a line""#,
        r#"npx vitest run -t "parses a line""#,
        "npx vitest run -t <filter>",
    ),
    ("npx vitest run -t --watch", "npx vitest run -t --watch", "npx vitest run -t --watch"),
    ("dotnet test -t --no-build", "dotnet test -t --no-build", "dotnet test -t --no-build"),
    ("dotnet test -t Parser", "dotnet test -t Parser", "dotnet test -t Parser"),
    (
        "cargo test --manifest-path X:/work/repo.wt/slug/crates/a/Cargo.toml",
        "cargo test --manifest-path crates/a/Cargo.toml",
        "cargo test --manifest-path crates/a/Cargo.toml",
    ),
    ("type x:/WORK/repo.wt/slug/README.md", "type README.md", "type README.md"),
    ("cargo run -- X:/work/repo.wt/slug", "cargo run -- .", "cargo run -- ."),
    (
        "diff X:/work/repo.wt/other/a.txt X:/work/repo.wt/slugger/b.txt",
        "diff repo:a.txt repo:b.txt",
        "diff repo:a.txt repo:b.txt",
    ),
    (
        "cargo test --manifest-path X:/work/other.wt/s/Cargo.toml",
        "cargo test --manifest-path other:Cargo.toml",
        "cargo test --manifest-path other:Cargo.toml",
    ),
    (
        r"cargo test --manifest-path=D:\tools.wt\main\Cargo.toml",
        "cargo test --manifest-path=tools:Cargo.toml",
        "cargo test --manifest-path=tools:Cargo.toml",
    ),
    (
        r"cargo test --manifest-path D:\repo.wt\slug\Cargo.toml",
        "cargo test --manifest-path Cargo.toml",
        "cargo test --manifest-path Cargo.toml",
    ),
    (
        "dotnet test --filter=Category=Fast",
        "dotnet test --filter=Category=Fast",
        "dotnet test --filter=<filter>",
    ),
    (
        "uv run pytest tests/test_a.py",
        "uv run pytest tests/test_a.py",
        "uv run pytest tests/test_a.py",
    ),
];

#[test]
fn normalize_table_of_real_commands() {
    for (command, exact, family) in COMMANDS {
        let key = local_key(command);
        assert_eq!(
            (key.exact.as_str(), key.family.as_str()),
            (format!("local::repo | {exact}").as_str(), format!("local::repo | {family}").as_str()),
            "command {command}"
        );
    }
}

#[test]
fn key_over_200_chars_is_cut_with_sha8() {
    let command = format!("echo {}", "a".repeat(300));
    let full = format!("local::repo | {command}");

    let key = local_key(&command);

    assert_eq!(key.exact, format!("{}{}", &full[..200], sha8(&full)));
    assert_eq!(key.family, key.exact, "a key without filters is its own family");

    let edge = format!("echo {}", "a".repeat(200 - "local::repo | echo ".len()));
    assert_eq!(
        local_key(&edge).exact,
        format!("local::repo | {edge}"),
        "a key of exactly 200 characters stays whole"
    );

    let wide = format!("echo {}", "\u{e9}".repeat(300));
    let wide_full = format!("local::repo | {wide}");
    let cut: String = wide_full.chars().take(200).collect();
    assert_eq!(
        local_key(&wide).exact,
        format!("{cut}{}", sha8(&wide_full)),
        "the cut counts characters, not bytes"
    );
}

/// The first 8 hex digits of the SHA-256 of `text`.
fn sha8(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    format!("{:02x}{:02x}{:02x}{:02x}", digest[0], digest[1], digest[2], digest[3])
}

#[test]
fn exact_key_keeps_filter_family_key_replaces_it() {
    let journal = local_key(r#"cargo nextest run -E "test(/^journal::/)""#);
    let slots = local_key(r#"cargo nextest run -E "test(/^slots::/)""#);
    assert_ne!(journal.exact, slots.exact, "each filter has its own exact key");
    assert_eq!(journal.family, slots.family, "the filters share one family key");
    assert_eq!(journal.family, "local::repo | cargo nextest run -E <filter>");

    for (command, family) in [
        ("dotnet test --filter-class Parser.Tests", "dotnet test --filter-class <filter>"),
        ("pwsh -File t.ps1 -filter Pipeline", "pwsh -File t.ps1 -filter <filter>"),
        ("cargo test -T x -t y", "cargo test -T x -t <filter>"),
        ("cargo nextest run -E -E x", "cargo nextest run -E -E <filter>"),
        ("cargo nextest run -E", "cargo nextest run -E"),
    ] {
        let key = local_key(command);
        assert_eq!(
            key.exact,
            format!("local::repo | {command}"),
            "the exact key keeps the filter of {command}"
        );
        assert_eq!(key.family, format!("local::repo | {family}"), "the family key of {command}");
    }
}

#[test]
fn a_cwd_outside_git_keys_on_its_full_path() {
    let folder = KeyPlace::OutsideGit {
        cwd: "X:/scratch/loose".to_owned(),
    };

    let key = key_in(&folder, "tool X:/work/other.wt/s/c.txt X:/scratch/loose/d.txt X:/work/other.wt/s");

    assert_eq!(
        key.exact, "local::x:/scratch/loose | tool other:c.txt X:/scratch/loose/d.txt other:.",
        "outside git the `.wt` component rule shortens a worktree path and keeps its repo"
    );
}

#[test]
fn an_outside_git_folder_keys_case_and_slash_insensitively() {
    let spellings = [r"X:\Scratch\Loose\", "x:/scratch/loose", "X:/SCRATCH/loose/"];

    let keys: Vec<String> = spellings
        .iter()
        .map(|cwd| key_in(&KeyPlace::OutsideGit { cwd: (*cwd).to_owned() }, "cargo build").exact)
        .collect();

    assert_eq!(keys, ["local::x:/scratch/loose | cargo build"; 3]);
}

#[test]
fn a_worktree_path_spelled_through_another_drive_shares_the_key() {
    let through_root = local_key("cargo test --manifest-path X:/work/repo.wt/slug/crates/a/Cargo.toml");

    let through_other_drive = local_key(r"cargo test --manifest-path D:\repo.wt\slug\crates\a\Cargo.toml");

    assert_eq!(through_other_drive, through_root);
    assert_eq!(through_root.exact, "local::repo | cargo test --manifest-path crates/a/Cargo.toml");
}

#[test]
fn a_flag_equals_path_value_loses_the_worktree_root() {
    for (command, exact) in [
        (
            "cargo test --manifest-path=X:/work/repo.wt/slug/crates/a/Cargo.toml",
            "cargo test --manifest-path=crates/a/Cargo.toml",
        ),
        (r"tool --out=D:\repo.wt\slug\target\x", "tool --out=target/x"),
        ("tool --root=X:/work/repo.wt/slug", "tool --root=."),
        ("dotnet test --filter=Category=Fast", "dotnet test --filter=Category=Fast"),
    ] {
        assert_eq!(local_key(command).exact, format!("local::repo | {exact}"), "command {command}");
    }
}

#[test]
fn a_remote_place_and_the_crit_class_are_in_the_key() {
    let folder = in_repo(r"crates\x/");

    let key = duration_key(&KeyInputs {
        folder: &folder,
        remote_host: Some(HOST),
        critical: true,
        temp_root: None,
        command: "cargo build",
    });

    assert_eq!(key.exact, "remote:builder@build-box:crit:repo/crates/x | cargo build");
    let local = local_key("cargo build");
    assert_eq!(local.exact, "local::repo | cargo build", "a local run in no class leaves both empty");
}

#[test]
fn p50_is_median_of_five_most_recent() {
    let key = local_key("cargo build");
    let mut store = DurationStore::new();
    record_walls(&mut store, &key, &[100.0, 100.0, 100.0, 100.0, 100.0, 5.0, 1.0, 4.0, 2.0, 3.0]);

    let estimate = store.estimate(&key, ts(NOW)).expect("ten passing runs give an estimate");

    assert_eq!((estimate.p50, estimate.p90, estimate.p99, estimate.runs), (3.0, 100.0, 100.0, 10));
    assert_eq!(estimate.adjusted_p99, Some(200.0));
    assert!(!estimate.family);
    assert_eq!(estimate.key, key.exact);
}

#[test]
fn p90_p99_raised_to_p50() {
    let key = local_key("cargo build");
    let mut store = DurationStore::new();
    let mut walls = vec![1.0; 25];
    walls.extend([10.0, 1.0, 10.0, 1.0, 10.0]);
    record_walls(&mut store, &key, &walls);

    let estimate = store.estimate(&key, ts(NOW)).expect("thirty passing runs give an estimate");

    assert_eq!(
        (estimate.p50, estimate.p90, estimate.p99),
        (10.0, 10.0, 10.0),
        "p90 by nearest rank is 1 s, raised to the recent median"
    );
}

/// Records five passing runs of 10 wall seconds under `key`, the first `adjusted` of them with 1 adjusted second.
fn record_adjusted(store: &mut DurationStore, key: &DurationKey, adjusted: usize) {
    for minutes in 1..=5 {
        let facts = RunFacts {
            adjusted_seconds: (minutes <= adjusted).then_some(1.0),
            ..passed(10.0)
        };
        store.record(key, minutes_before_now(minutes), &facts);
    }
}

#[test]
fn adjusted_p99_is_not_raised_to_the_wall_p50() {
    let key = local_key("cargo build");
    let mut store = DurationStore::new();
    record_adjusted(&mut store, &key, 5);

    let estimate = store.estimate(&key, ts(NOW)).expect("five passing runs give an estimate");

    assert_eq!(
        (estimate.p50, estimate.adjusted_p90, estimate.adjusted_p99),
        (10.0, Some(1.0), Some(1.0)),
        "the wall p50 counts wall seconds, the adjusted percentiles load-adjusted ones"
    );
}

#[test]
fn adjusted_p99_needs_five_adjusted_samples() {
    let key = local_key("cargo build");
    let mut four = DurationStore::new();
    record_adjusted(&mut four, &key, 4);
    let mut five = DurationStore::new();
    record_adjusted(&mut five, &key, 5);

    let of = |store: &DurationStore| {
        let estimate = store.estimate(&key, ts(NOW)).expect("five passing runs give an estimate");
        (estimate.adjusted_p90, estimate.adjusted_p99)
    };

    assert_eq!(of(&four), (None, None), "four runs with adjusted seconds are too few");
    assert_eq!(of(&five), (Some(1.0), Some(1.0)));
}

#[test]
fn adjusted_p90_and_p99_differ_over_a_wide_window() {
    let key = local_key("cargo build");
    let mut store = DurationStore::new();
    let mut adjusteds = vec![1.0; 18];
    adjusteds.extend([2.0, 9.0]);
    for (index, adjusted) in adjusteds.into_iter().enumerate() {
        let facts = RunFacts {
            adjusted_seconds: Some(adjusted),
            ..passed(10.0)
        };
        store.record(&key, minutes_before_now(20 - index), &facts);
    }

    let estimate = store.estimate(&key, ts(NOW)).expect("twenty passing runs give an estimate");

    assert_eq!(
        (estimate.adjusted_p90, estimate.adjusted_p99),
        (Some(1.0), Some(9.0)),
        "the 90th and 99th percentiles of the adjusted seconds differ"
    );
}

#[test]
fn five_passing_runs_needed() {
    let key = local_key("cargo build");
    let mut store = DurationStore::new();
    record_walls(&mut store, &key, &[2.0, 2.0, 2.0, 2.0]);
    for minutes in 1..=3 {
        store.record(&key, minutes_before_now(minutes), &failed(9.0));
    }
    assert_eq!(store.estimate(&key, ts(NOW)), None, "four passing runs and three failures are not enough");

    store.record(&key, ts(NOW), &passed(2.0));

    assert_eq!(store.estimate(&key, ts(NOW)).map(|estimate| estimate.runs), Some(5));
}

#[test]
fn window_is_30_runs_or_14_days_whichever_is_fewer() {
    let many = local_key("cargo build");
    let mut store = DurationStore::new();
    let mut walls = vec![1000.0; 10];
    walls.extend((1..=30).map(f64::from));
    record_walls(&mut store, &many, &walls);

    let estimate = store.estimate(&many, ts(NOW)).expect("forty runs give an estimate");

    assert_eq!(
        (estimate.runs, estimate.p50, estimate.p90, estimate.p99),
        (30, 28.0, 28.0, 30.0),
        "only the newest thirty runs count; p90 by rank, 27 s, is raised to the recent median"
    );

    let old = local_key("cargo test");
    for _ in 0..10 {
        store.record(&old, days_before_now(20), &passed(1000.0));
    }
    record_walls(&mut store, &old, &[2.0; 6]);

    let recent = store.estimate(&old, ts(NOW)).expect("six recent runs give an estimate");

    assert_eq!((recent.runs, recent.p99), (6, 2.0), "runs more than 14 days old do not count");
    let edge = store.estimate(&old, ts(NOW) + SignedDuration::from_hours(14 * 24));
    assert_eq!(edge, None, "fourteen days on, the newest run has left the window");
}

#[test]
fn the_mean_is_over_the_window() {
    let key = local_key("cargo build");
    let mut store = DurationStore::new();
    for _ in 0..3 {
        store.record(&key, days_before_now(20), &passed(5000.0));
    }
    let mut walls = vec![1000.0; 10];
    walls.extend((1..=30).map(f64::from));
    record_walls(&mut store, &key, &walls);
    store.record(&key, ts(NOW), &failed(9000.0));

    let estimate = store.estimate(&key, ts(NOW)).expect("forty recent runs give an estimate");

    assert_eq!(
        (estimate.runs, estimate.mean),
        (30, 15.5),
        "the mean of the newest thirty passing runs, without older runs or failures"
    );
}

#[test]
fn failures_are_counted_not_timed() {
    let key = local_key("cargo build");
    let mut store = DurationStore::new();
    record_walls(&mut store, &key, &[10.0; 5]);
    for minutes in 1..=3 {
        store.record(&key, minutes_before_now(minutes), &failed(999.0));
    }
    store.record(
        &key,
        ts(NOW),
        &RunFacts {
            verdict: Verdict::TimedOut,
            ..passed(999.0)
        },
    );

    let history = store.history(&key.exact).expect("the key has a history");

    assert_eq!((history.failed, history.samples.len(), history.last), (4, 5, ts(NOW)));
    let estimate = store.estimate(&key, ts(NOW)).expect("five passing runs give an estimate");
    assert_eq!((estimate.runs, estimate.p99), (5, 10.0));
}

#[test]
fn a_transfer_that_timed_out_leaves_the_history_unchanged() {
    let key = local_key("cargo build");
    let mut store = DurationStore::new();
    record_walls(&mut store, &key, &[10.0; 5]);
    let before = store.history(&key.exact).expect("the key has a history").clone();

    store.record(
        &key,
        ts(NOW),
        &RunFacts {
            verdict: Verdict::TransferTimedOut,
            ..passed(999.0)
        },
    );

    assert_eq!(store.history(&key.exact), Some(&before));
}

#[test]
fn a_family_estimate_says_it_is_one() {
    let filtered = |filter: &str| local_key(&format!("cargo nextest run -E test({filter})"));
    let mut store = DurationStore::new();
    for (minutes, filter) in [(5, "a"), (4, "b"), (3, "a"), (2, "c"), (1, "b")] {
        store.record(&filtered(filter), minutes_before_now(minutes), &passed(10.0));
    }
    let unseen = filtered("d");

    let estimate = store.estimate(&unseen, ts(NOW)).expect("the family has five runs");

    assert!(estimate.family);
    assert_eq!((estimate.key.as_str(), estimate.runs), (unseen.family.as_str(), 5));

    let own = filtered("a");
    record_walls(&mut store, &own, &[20.0; 3]);
    let exact = store.estimate(&own, ts(NOW)).expect("the exact key has five runs");
    assert!(!exact.family);
    assert_eq!((exact.key.as_str(), exact.runs), (own.exact.as_str(), 5));
}

#[test]
fn odd_release_values_are_not_samples() {
    let key = local_key("cargo build");
    let mut store = DurationStore::new();
    for wall in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        store.record(&key, ts(NOW), &passed(wall));
    }
    store.record(
        &key,
        ts(NOW),
        &RunFacts {
            verdict: Verdict::Unknown,
            ..passed(1.0)
        },
    );
    assert_eq!(store.history(&key.exact), None, "no run was filed");

    let root = fresh_dir("durations_odd_release_values");
    let line = |verdict: &str, wall: &str| {
        format!(
            r#"{{"v":1,"time":"2026-10-03T01:00:00Z","event":"release","pid":{},"kind":"heavy","cwd":"{ROOT}","command":"cargo build","wallSeconds":{wall},"verdict":"{verdict}"}}"#,
            verdict.len()
        )
    };
    write_lines(
        &root.join("events.jsonl"),
        &[line("bogus", "1.0"), line("passed", "null"), line("unknown", "1.0")],
    );

    let rebuilt = rebuild_store(&root, Some(TEMP), ts(NOW), lookup);

    assert_eq!(
        rebuilt.store.keys().count(),
        0,
        "an unknown verdict word, a missing wall time and `unknown` file nothing"
    );
    assert!(rebuilt.notes.is_empty(), "{:?}", rebuilt.notes);
}

#[test]
fn a_missing_store_reads_as_no_history_silently() {
    let root = fresh_dir("durations_missing_store");

    let read = read_store(&root);

    assert_eq!(read, StoreRead::NeedsRebuild(RebuildReason::Missing));
}

#[test]
fn a_torn_store_asks_for_a_rebuild_with_its_reason() {
    let root = fresh_dir("durations_torn_store");
    let path = durations_path(&root);
    let torn = r#"{"v":1,"keyVersion":1,"keys":{"local::repo | cargo b"#;
    fs::write(&path, torn).expect("a torn store can be seeded");

    let error = invalid(&root).expect("a torn store does not read");

    assert_eq!(error.path, path);
    assert_ne!(error.reason, "", "a torn store says why");
    assert_eq!(error.to_string(), format!("cannot read {} ({})", path.display(), error.reason));
    assert_eq!(
        record_run(&root, &local_key("cargo build"), ts(NOW), &passed(1.0)),
        Ok(Recorded::NeedsRebuild(RebuildReason::Invalid(error)))
    );
    assert_eq!(
        fs::read_to_string(&path).expect("the store can be read"),
        torn,
        "a torn store is left for the rebuild"
    );
}

#[test]
fn an_unknown_format_version_asks_for_a_rebuild() {
    let root = fresh_dir("durations_unknown_format_version");
    fs::write(durations_path(&root), r#"{"v":2,"keyVersion":1,"keys":{}}"#).expect("a newer store can be seeded");

    let error = invalid(&root).expect("a store of another format does not read");

    assert_eq!(error.reason, "format version 2 is not 1");
    let rebuilt = rebuild_store(&root, Some(TEMP), ts(NOW), lookup);
    let swap = swap_rebuilt(&root, &FakeLock::Free, &rebuilt, ts(NOW), ME).expect("the swap runs");
    assert_eq!(swap.outcome, Swapped::Replaced);
    assert_eq!(current(&root), Some(DurationStore::new()), "the rebuild replaces it");
}

#[test]
fn a_key_version_mismatch_asks_for_a_rebuild() {
    let root = fresh_dir("durations_key_version_mismatch");
    let path = durations_path(&root);
    let stale = r#"{"v":1,"keyVersion":0,"keys":{}}"#;
    fs::write(&path, stale).expect("a stale store can be seeded");
    let key = local_key("cargo build");

    assert_eq!(read_store(&root), StoreRead::NeedsRebuild(RebuildReason::Stale));
    assert_eq!(
        record_run(&root, &key, ts(NOW), &passed(1.0)),
        Ok(Recorded::NeedsRebuild(RebuildReason::Stale))
    );
    assert_eq!(
        fs::read_to_string(&path).expect("the store can be read"),
        stale,
        "a stale store is left for the rebuild"
    );

    fs::remove_file(&path).expect("the stale store can be removed");
    assert_eq!(
        record_run(&root, &key, ts(NOW), &passed(1.0)),
        Ok(Recorded::NeedsRebuild(RebuildReason::Missing))
    );
    assert!(!path.exists(), "a missing store is left for the rebuild");

    write_store(&root, &DurationStore::new()).expect("an empty store can be written");
    assert_eq!(record_run(&root, &key, ts(NOW), &passed(1.0)), Ok(Recorded::Written));
    let store = current(&root).expect("the store is current after a run wrote it");
    assert_eq!(walls_of(&store, &key.exact), [1.0]);
}

#[test]
fn a_newer_key_version_is_left_alone() {
    let root = fresh_dir("durations_newer_key_version");
    let path = durations_path(&root);
    let newer = format!(r#"{{"v":1,"keyVersion":{},"keys":{{}}}}"#, KEY_VERSION + 1);
    fs::write(&path, &newer).expect("a newer build's store can be seeded");
    let key = local_key("cargo build");

    assert_eq!(read_store(&root), StoreRead::Newer, "no history, no rebuild and nothing to say");
    assert_eq!(record_run(&root, &key, ts(NOW), &passed(1.0)), Ok(Recorded::Newer));
    write_lines(&root.join("events.jsonl"), &[build_at("2026-10-03T01:00:00Z", 10.0)]);
    let rebuilt = rebuild_store(&root, Some(TEMP), ts(NOW), lookup);
    let swap = swap_rebuilt(&root, &FakeLock::Free, &rebuilt, ts(NOW), ME).expect("the swap runs");

    assert_eq!(
        swap.outcome,
        Swapped::Kept,
        "a rebuild begun before the newer store came does not replace it"
    );
    assert_eq!(
        fs::read_to_string(&path).expect("the store can be read"),
        newer,
        "a newer build's store is never written"
    );
}

#[test]
fn a_key_with_no_release_in_14_days_is_pruned() {
    let root = fresh_dir("durations_key_pruned");
    let old = local_key("cargo old");
    let failing = local_key("cargo failing");
    let new = local_key("cargo new");
    let mut store = DurationStore::new();
    store.record(&old, days_before_now(15), &passed(1.0));
    store.record(&failing, days_before_now(15), &passed(1.0));
    store.record(&failing, days_before_now(1), &failed(1.0));
    write_store(&root, &store).expect("the store can be seeded");

    assert_eq!(record_run(&root, &new, ts(NOW), &passed(2.0)), Ok(Recorded::Written));

    let kept = current(&root).expect("the store is current");
    assert_eq!(
        kept.keys().collect::<Vec<_>>(),
        [failing.exact.as_str(), new.exact.as_str()],
        "a failure within 14 days keeps a key"
    );
    assert_eq!(kept.history(&failing.exact).map(|history| history.last), Some(days_before_now(1)));
}

/// A release line as the gate writes it.
struct Release<'a> {
    time: &'a str,
    cwd: &'a str,
    command: &'a str,
    kind: &'a str,
    host: Option<&'a str>,
    facts: RunFacts,
}

impl Release<'_> {
    fn line(&self) -> String {
        let identity = JournalIdentity {
            pid: 4242,
            process_start: ts("2026-10-01T00:00:00Z"),
            kind: self.kind.to_owned(),
            cwd: self.cwd.to_owned(),
            command: self.command.to_owned(),
        };
        let event = JournalEvent::Release {
            slots: vec![r"Local\gate-heavy-slot-0".to_owned()],
            held_seconds: 99.0,
            status: Some(i64::from(self.facts.verdict != Verdict::Passed)),
            facts: Some(self.facts),
        };
        let tag = self.host.map(|host| RemoteTag {
            host: host.to_owned(),
            key: "repo-1234".to_owned(),
        });
        journal_line(&identity, ts(self.time), &event, tag.as_ref(), None)
    }
}

fn local_release<'a>(time: &'a str, cwd: &'a str, command: &'a str, facts: RunFacts) -> Release<'a> {
    Release {
        time,
        cwd,
        command,
        kind: "heavy",
        host: None,
        facts,
    }
}

/// The line of a local passing `cargo build` at the worktree root, released at `time` after `wall` seconds.
fn build_at(time: &str, wall: f64) -> String {
    local_release(time, ROOT, "cargo build", passed(wall)).line()
}

fn write_lines(path: &Path, lines: &[String]) {
    let text: String = lines.iter().flat_map(|line| [line.as_str(), "\n"]).collect();
    fs::write(path, text).expect("a journal file can be seeded");
}

fn append_journal(root: &Path, line: &str) {
    let mut journal = OpenOptions::new()
        .append(true)
        .create(true)
        .open(root.join("events.jsonl"))
        .expect("the journal can be opened");
    journal.write_all(format!("{line}\n").as_bytes()).expect("a line can be appended");
}

fn write_archive(root: &Path, name: &str, lines: &[String]) {
    let dir = root.join("archive");
    fs::create_dir_all(&dir).expect("the archive folder can be made");
    let mut encoder = GzEncoder::new(File::create(dir.join(name)).expect("an archive can be made"), Compression::default());
    for line in lines {
        encoder.write_all(line.as_bytes()).expect("an archive line can be written");
        encoder.write_all(b"\n").expect("an archive line feed can be written");
    }
    encoder.finish().expect("the archive can be finished");
}

/// The git lookup of the rebuild tests: the worktree root and one folder under it are in the repo; git places nothing
/// else, as for a folder that no longer exists.
fn lookup(cwd: &Path) -> Option<KeyPlace> {
    match cwd.to_string_lossy().replace('\\', "/").as_str() {
        ROOT => Some(in_repo("")),
        "X:/work/repo.wt/slug/crates/a" => Some(in_repo("crates/a")),
        _ => None,
    }
}

#[test]
fn rebuild_reads_the_live_journal_and_the_archive() {
    let root = fresh_dir("durations_rebuild_reads_everything");
    let shared = local_release("2026-10-02T03:00:00Z", ROOT, "cargo build", failed(50.0)).line();
    write_archive(
        &root,
        "events-20261002T040000Z.jsonl.gz",
        &[
            build_at("2026-10-02T01:00:00Z", 10.0),
            build_at("2026-10-02T02:00:00Z", 12.0),
            shared.clone(),
        ],
    );
    fs::write(root.join("archive").join("events-20261002T040000Z.jsonl.gz.77.tmp"), "not gzip").expect("a temp file can be seeded");
    write_lines(
        &root.join("events.1.jsonl"),
        &[
            r#"{"v":1,"time":"2026-10-02T02:59:00Z","event":"take","pid":4242}"#.to_owned(),
            shared,
            build_at("2026-10-02T05:00:00Z", 14.0),
        ],
    );
    let critical_remote = Release {
        time: "2026-10-03T01:00:00Z",
        cwd: ROOT,
        command: "cargo build",
        kind: "critical",
        host: Some(HOST),
        facts: passed(16.0),
    };
    write_lines(
        &root.join("events.jsonl"),
        &[
            critical_remote.line(),
            local_release("2026-10-03T02:00:00Z", "X:/work/repo.wt/slug/crates/a", "cargo test", passed(18.0)).line(),
            r#"{"v":1,"time":"2026-10-03T03:00:00Z","event":"release","pid":"#.to_owned(),
        ],
    );

    let rebuilt = rebuild_store(&root, Some(TEMP), ts(NOW), lookup);

    let build = rebuilt.store.history("local::repo | cargo build").expect("the local build has a history");
    assert_eq!(
        (walls_of(&rebuilt.store, "local::repo | cargo build"), build.failed),
        (vec![10.0, 12.0, 14.0], 1),
        "the wall seconds, the shared line once"
    );
    assert_eq!(build.samples[0].adjusted, Some(20.0));
    assert_eq!((build.samples[0].avg_cores, build.samples[0].peak_cores), (Some(1.5), Some(3.0)));
    assert_eq!(
        (build.samples[0].time, build.last),
        (ts("2026-10-02T01:00:00Z"), ts("2026-10-02T05:00:00Z"))
    );
    assert_eq!(walls_of(&rebuilt.store, "remote:builder@build-box:crit:repo | cargo build"), [16.0]);
    assert_eq!(walls_of(&rebuilt.store, "local::repo/crates/a | cargo test"), [18.0]);
    assert_eq!(
        rebuilt.notes,
        [format!(
            "skipped 1 release line(s) of {} that do not parse",
            root.join("events.jsonl").display()
        )]
    );
}

#[test]
fn rebuild_keys_a_removed_worktree_by_its_path_shape() {
    let root = fresh_dir("durations_rebuild_keys_by_shape");
    write_lines(
        &root.join("events.jsonl"),
        &[
            local_release(
                "2026-10-03T01:00:00Z",
                "X:/work/gone.wt/old-slug/crates/b",
                "cargo build X:/work/gone.wt/old-slug/src/x.rs",
                passed(10.0),
            )
            .line(),
            local_release("2026-10-03T02:00:00Z", r"X:\Scratch\Gone", "cargo build", passed(11.0)).line(),
        ],
    );

    let rebuilt = rebuild_store(&root, Some(TEMP), ts(NOW), lookup);

    assert_eq!(
        rebuilt.store.keys().collect::<Vec<_>>(),
        ["local::gone/crates/b | cargo build src/x.rs", "local::x:/scratch/gone | cargo build"]
    );
    assert!(rebuilt.notes.is_empty(), "{:?}", rebuilt.notes);
    assert_eq!(
        place_by_shape("X:/work/gone.wt/old-slug"),
        KeyPlace::InGit {
            repo: "gone".to_owned(),
            sub: String::new(),
            worktree_root: "X:/work/gone.wt/old-slug".to_owned(),
        }
    );
    assert_eq!(
        place_by_shape("X:/work/gone.wt"),
        KeyPlace::OutsideGit {
            cwd: "X:/work/gone.wt".to_owned()
        },
        "a worktrees folder with no slug after it is not a worktree"
    );
}

#[test]
fn rebuild_skips_archives_older_than_the_window() {
    let root = fresh_dir("durations_rebuild_skips_old_archives");
    let in_old_archive = local_release("2026-10-02T00:00:00Z", ROOT, "cargo old-archive", passed(1.0)).line();
    write_archive(&root, "events-20260910T000000Z.jsonl.gz", &[in_old_archive]);
    let in_recent_archive = local_release("2026-10-02T01:00:00Z", ROOT, "cargo recent-archive", passed(2.0)).line();
    write_archive(&root, "events-20261002T040000Z.jsonl.gz", &[in_recent_archive]);
    write_lines(
        &root.join("events.jsonl"),
        &[local_release("2026-09-18T00:00:00Z", ROOT, "cargo stale-key", passed(3.0)).line()],
    );

    let rebuilt = rebuild_store(&root, Some(TEMP), ts(NOW), lookup);

    assert_eq!(
        rebuilt.store.keys().collect::<Vec<_>>(),
        ["local::repo | cargo recent-archive"],
        "an archive stamped before the window is not read, and a key with no release in it is dropped"
    );
}

#[test]
fn rebuild_survives_a_roll_between_its_reads() {
    let root = fresh_dir("durations_rebuild_survives_a_roll");
    write_archive(&root, "events-20261002T010000Z.jsonl.gz", &[build_at("2026-10-02T00:30:00Z", 5.0)]);
    write_lines(
        &root.join("events.jsonl"),
        &[build_at("2026-10-02T02:00:00Z", 10.0), build_at("2026-10-02T03:00:00Z", 12.0)],
    );

    let mut reads = RebuildReads::journal(&root, ts(NOW));
    // The journal rolls: it moves over the rolled journal, its release lines are archived, and a new journal begins.
    fs::rename(root.join("events.jsonl"), root.join("events.1.jsonl")).expect("the journal can be rolled");
    assert_eq!(archive_rolled_journal(&root, ts(NOW)), Vec::<String>::new());
    write_lines(&root.join("events.jsonl"), &[build_at("2026-10-02T04:00:00Z", 14.0)]);
    reads.rolled();
    reads.archives(ts(NOW));
    let rebuilt = reads.finish(Some(TEMP), ts(NOW), lookup);

    assert_eq!(
        walls_of(&rebuilt.store, "local::repo | cargo build"),
        [5.0, 10.0, 12.0],
        "every line read before the roll, each once"
    );
    let swap = swap_rebuilt(&root, &FakeLock::Free, &rebuilt, ts(NOW), ME).expect("the swap runs");
    assert_eq!(swap.outcome, Swapped::Replaced);
    let swapped = current(&root).expect("the swapped store is current");
    assert_eq!(
        walls_of(&swapped, "local::repo | cargo build"),
        [5.0, 10.0, 12.0, 14.0],
        "the new journal's line is filed at the swap"
    );
}

#[test]
fn rebuild_swap_is_skipped_when_the_store_was_written_meanwhile() {
    let root = fresh_dir("durations_rebuild_swap");
    write_lines(&root.join("events.jsonl"), &[build_at("2026-10-03T01:00:00Z", 10.0)]);
    let rebuilt = rebuild_store(&root, Some(TEMP), ts(NOW), lookup);
    let mut meanwhile = DurationStore::new();
    meanwhile.record(&local_key("cargo test"), ts(NOW), &passed(3.0));
    write_store(&root, &meanwhile).expect("another gate's store can be written");

    let kept = swap_rebuilt(&root, &FakeLock::Free, &rebuilt, ts(NOW), ME).expect("the swap runs");

    assert_eq!((kept.outcome, kept.notes.len()), (Swapped::Kept, 0));
    assert_eq!(current(&root), Some(meanwhile), "the store written meanwhile is kept");

    fs::write(durations_path(&root), r#"{"v":1,"keyVersion":0,"keys":{}}"#).expect("a stale store can be seeded");
    append_journal(&root, &build_at("2026-10-03T02:00:00Z", 11.0));
    let replaced = swap_rebuilt(&root, &FakeLock::Free, &rebuilt, ts(NOW), ME).expect("the swap runs");
    assert_eq!(replaced.outcome, Swapped::Replaced);
    let swapped = current(&root).expect("the rebuilt store is current");
    assert_eq!(
        walls_of(&swapped, "local::repo | cargo build"),
        [10.0, 11.0],
        "a stale store is replaced by the rebuilt one with the runs released since"
    );

    let busy = swap_rebuilt(&root, &FakeLock::Busy, &rebuilt, ts(NOW), ME);
    assert!(matches!(busy, Err(StoreError::Lock(_))), "a lock not taken swaps nothing: {busy:?}");
    assert_eq!(current(&root), Some(swapped), "the store is left as it was");
}

/// Rolls `root`'s journal as an append does at `at`: it moves over the rolled journal and its release lines are
/// archived.
fn roll(root: &Path, at: Timestamp) {
    fs::rename(root.join("events.jsonl"), root.join("events.1.jsonl")).expect("the journal can be rolled");
    assert_eq!(archive_rolled_journal(root, at), Vec::<String>::new());
}

#[test]
fn two_rolls_between_the_rebuild_and_its_swap_lose_nothing() {
    let root = fresh_dir("durations_two_rolls_before_the_swap");
    write_lines(&root.join("events.jsonl"), &[build_at("2026-10-03T01:00:00Z", 10.0)]);
    let rebuilt = rebuild_store(&root, Some(TEMP), ts(NOW), lookup);
    append_journal(&root, &build_at("2026-10-03T02:00:00Z", 11.0));
    roll(&root, ts(NOW) + SignedDuration::from_mins(1));
    write_lines(&root.join("events.jsonl"), &[build_at("2026-10-03T03:00:00Z", 12.0)]);
    roll(&root, ts(NOW) + SignedDuration::from_mins(2));
    write_lines(&root.join("events.jsonl"), &[build_at("2026-10-03T04:00:00Z", 13.0)]);

    let swap = swap_rebuilt(&root, &FakeLock::Free, &rebuilt, ts(NOW) + SignedDuration::from_mins(3), ME).expect("the swap runs");

    assert_eq!(swap.outcome, Swapped::Replaced);
    let swapped = current(&root).expect("the swapped store is current");
    assert_eq!(
        walls_of(&swapped, "local::repo | cargo build"),
        [10.0, 11.0, 12.0, 13.0],
        "the line appended to the journal the rebuild read survives only in an archive, and is read from it"
    );
}

#[test]
fn a_roll_whose_new_journal_outgrows_the_old_offset_is_seen() {
    let root = fresh_dir("durations_roll_outgrows_offset");
    write_lines(&root.join("events.jsonl"), &[build_at("2026-10-03T01:00:00Z", 10.0)]);
    let rebuilt = rebuild_store(&root, Some(TEMP), ts(NOW), lookup);
    roll(&root, ts(NOW) + SignedDuration::from_mins(1));
    let padding = format!(r#"{{"v":1,"event":"queue","pad":"{}"}}"#, "x".repeat(4000));
    write_lines(&root.join("events.jsonl"), &[build_at("2026-10-03T02:00:00Z", 12.0), padding]);

    let swap = swap_rebuilt(&root, &FakeLock::Free, &rebuilt, ts(NOW), ME).expect("the swap runs");

    assert_eq!(swap.outcome, Swapped::Replaced);
    let swapped = current(&root).expect("the swapped store is current");
    assert_eq!(
        walls_of(&swapped, "local::repo | cargo build"),
        [10.0, 12.0],
        "a new journal longer than the old one is still read from its start"
    );
}

#[test]
fn rebuild_store_reads_newest_first() {
    let root = fresh_dir("durations_rebuild_reads_newest_first");
    let torn = r#"{"v":1,"event":"release","pid":"#.to_owned();
    write_archive(&root, "events-20261001T000000Z.jsonl.gz", std::slice::from_ref(&torn));
    write_archive(&root, "events-20261002T000000Z.jsonl.gz", std::slice::from_ref(&torn));
    write_lines(&root.join("events.1.jsonl"), std::slice::from_ref(&torn));
    write_lines(&root.join("events.jsonl"), std::slice::from_ref(&torn));

    let rebuilt = rebuild_store(&root, Some(TEMP), ts(NOW), lookup);

    let archive = root.join("archive");
    let files = [
        root.join("events.jsonl"),
        root.join("events.1.jsonl"),
        archive.join("events-20261002T000000Z.jsonl.gz"),
        archive.join("events-20261001T000000Z.jsonl.gz"),
    ];
    let expected: Vec<String> = files
        .iter()
        .map(|file| format!("skipped 1 release line(s) of {} that do not parse", file.display()))
        .collect();
    assert_eq!(
        rebuilt.notes, expected,
        "the journal, then the rolled journal, then the archives newest first"
    );
}

#[test]
fn a_swap_prunes_keys_outside_the_window() {
    let root = fresh_dir("durations_swap_prunes");
    write_lines(&root.join("events.jsonl"), &[build_at("2026-10-03T01:00:00Z", 10.0)]);
    let rebuilt = rebuild_store(&root, Some(TEMP), ts(NOW), lookup);
    assert_eq!(rebuilt.store.keys().count(), 1);

    let swap = swap_rebuilt(&root, &FakeLock::Free, &rebuilt, days_before_now(-15), ME).expect("the swap runs");

    assert_eq!(swap.outcome, Swapped::Replaced);
    assert_eq!(
        current(&root),
        Some(DurationStore::new()),
        "a key with no release in the 14 days before the swap is dropped"
    );
}

#[test]
fn a_run_released_during_a_rebuild_from_a_main_checkout_subfolder_keys_by_its_repo() {
    let root = fresh_dir("durations_main_checkout_subfolder");
    let checkout = |path: &str, repo: &str| KeyPlace::InGit {
        repo: repo.to_owned(),
        sub: String::new(),
        worktree_root: path.to_owned(),
    };
    let main_checkout = |cwd: &Path| match cwd.to_string_lossy().as_ref() {
        "X:/work/repo" => Some(checkout("X:/work/repo", "repo")),
        "X:/work/repo/vendor/lib" => Some(checkout("X:/work/repo/vendor/lib", "lib")),
        _ => None,
    };
    write_lines(
        &root.join("events.jsonl"),
        &[
            local_release("2026-10-03T01:00:00Z", "X:/work/repo", "cargo build", passed(10.0)).line(),
            local_release("2026-10-03T01:30:00Z", "X:/work/repo/vendor/lib", "cargo build", passed(20.0)).line(),
        ],
    );
    let rebuilt = rebuild_store(&root, Some(TEMP), ts(NOW), main_checkout);
    append_journal(
        &root,
        &local_release("2026-10-03T02:00:00Z", r"x:\Work\Repo\crates\new", "cargo build", passed(11.0)).line(),
    );
    append_journal(
        &root,
        &local_release("2026-10-03T03:00:00Z", "X:/work/repo/vendor/lib/src", "cargo build", passed(21.0)).line(),
    );
    append_journal(
        &root,
        &local_release("2026-10-03T04:00:00Z", "X:/work/repository", "cargo build", passed(31.0)).line(),
    );

    swap_rebuilt(&root, &FakeLock::Free, &rebuilt, ts(NOW), ME).expect("the swap runs");

    let swapped = current(&root).expect("the swapped store is current");
    assert_eq!(walls_of(&swapped, "local::repo/crates/new | cargo build"), [11.0]);
    assert_eq!(
        walls_of(&swapped, "local::lib/src | cargo build"),
        [21.0],
        "the longest worktree root the folder is under places it"
    );
    assert_eq!(
        walls_of(&swapped, "local::x:/work/repository | cargo build"),
        [31.0],
        "a root matches only at a folder boundary"
    );
}

#[test]
fn a_store_that_cannot_be_opened_is_no_history_without_a_rebuild() {
    let root = fresh_dir("durations_store_cannot_be_opened");
    let path = durations_path(&root);
    fs::create_dir_all(&path).expect("a folder can stand where the store goes");

    let read = read_store(&root);

    let error = match read {
        StoreRead::Unavailable(error) => Some(error),
        _ => None,
    }
    .expect("a store that cannot be opened is unavailable");
    assert_eq!(error.path, path);
    assert_ne!(error.reason, "", "it says why");
    assert_eq!(
        record_run(&root, &local_key("cargo build"), ts(NOW), &passed(1.0)),
        Ok(Recorded::Unavailable(error)),
        "the run is not recorded and no rebuild is asked for"
    );
    assert!(path.is_dir(), "nothing was written over it");
}

#[test]
fn a_release_written_during_a_rebuild_is_in_the_swapped_store() {
    let root = fresh_dir("durations_release_during_rebuild");
    write_lines(&root.join("events.jsonl"), &[build_at("2026-10-03T01:00:00Z", 10.0)]);
    let rebuilt = rebuild_store(&root, Some(TEMP), ts(NOW), lookup);
    append_journal(
        &root,
        &local_release("2026-10-03T02:00:00Z", "X:/work/repo.wt/slug/crates/new", "cargo build", passed(11.0)).line(),
    );

    let swap = swap_rebuilt(&root, &FakeLock::Free, &rebuilt, ts(NOW), ME).expect("the swap runs");

    assert_eq!(swap.outcome, Swapped::Replaced);
    let swapped = current(&root).expect("the swapped store is current");
    assert_eq!(walls_of(&swapped, "local::repo | cargo build"), [10.0]);
    assert_eq!(
        walls_of(&swapped, "local::repo/crates/new | cargo build"),
        [11.0],
        "a folder the rebuild did not place is placed by its path's shape"
    );
}

fn marker(root: &Path) -> Value {
    serde_json::from_slice(&fs::read(rebuilding_path(root)).expect("the marker can be read")).expect("the marker is JSON")
}

/// The gate that rebuilds in most tests.
const ME: RebuildOwner = RebuildOwner { pid: 1, start_ms: 10 };

fn gate(pid: u32, start_ms: u64) -> RebuildOwner {
    RebuildOwner { pid, start_ms }
}

#[test]
fn a_second_rebuild_waits_on_a_live_marker() {
    let root = fresh_dir("durations_second_rebuild_waits");
    let first = gate(100, 1_000);
    assert!(begin_rebuild(&root, &FakeLock::Free, first, |_, _| true).expect("the first gate marks the rebuild"));
    let mut asked = None;

    let second = begin_rebuild(&root, &FakeLock::Free, gate(200, 2_000), |pid, start| {
        asked = Some((pid, start));
        true
    })
    .expect("the second gate reads the marker");

    assert!(!second, "a live marker means another gate rebuilds");
    assert_eq!(asked, Some((100, 1_000)));
    assert_eq!(marker(&root), json!({"pid": 100, "start": 1000}));
    let rebuilt = rebuild_store(&root, Some(TEMP), ts(NOW), lookup);
    swap_rebuilt(&root, &FakeLock::Free, &rebuilt, ts(NOW), first).expect("the swap runs");
    assert!(!rebuilding_path(&root).exists(), "the swap removes its own marker");
    assert!(begin_rebuild(&root, &FakeLock::Free, gate(200, 2_000), |_, _| true).expect("a rebuild may begin again"));
}

#[test]
fn a_dead_rebuilders_marker_is_taken_over() {
    let root = fresh_dir("durations_dead_rebuilder");
    fs::write(rebuilding_path(&root), r#"{"pid":100,"start":1000}"#).expect("a dead gate's marker can be seeded");

    assert!(begin_rebuild(&root, &FakeLock::Free, gate(200, 2_000), |_, _| false).expect("the marker is taken over"));
    assert_eq!(marker(&root), json!({"pid": 200, "start": 2000}));

    fs::write(rebuilding_path(&root), "{torn").expect("a torn marker can be seeded");
    assert!(
        begin_rebuild(&root, &FakeLock::Free, gate(300, 3_000), |_, _| true).expect("the marker is taken over"),
        "a marker that does not read is taken over"
    );
    assert_eq!(marker(&root), json!({"pid": 300, "start": 3000}));
}

#[test]
#[cfg(windows)]
fn an_unreadable_marker_held_open_is_not_taken_over() {
    use std::os::windows::fs::OpenOptionsExt;

    let root = fresh_dir("durations_unreadable_marker_held_open");
    let path = rebuilding_path(&root);
    fs::write(&path, r#"{"pid":100,"start":1000}"#).expect("a marker can be seeded");
    // Opened sharing nothing, the marker can be neither read nor removed by another handle.
    let held = OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&path)
        .expect("the marker can be held open");

    let claimed = begin_rebuild(&root, &FakeLock::Free, gate(200, 2_000), |_, _| false);
    drop(held);

    assert_eq!(claimed, Ok(false), "a marker another process holds open means this gate does not rebuild");
    assert_eq!(marker(&root), json!({"pid": 100, "start": 1000}), "the marker is left as it was");
}

/// A journal lock with one holder at a time: a take while it is held is refused at once.
#[derive(Default)]
struct OneHolder(Cell<bool>);

struct OneHolderGuard<'a>(&'a OneHolder);

impl Drop for OneHolderGuard<'_> {
    fn drop(&mut self) {
        self.0.0.set(false);
    }
}

impl JournalLock for OneHolder {
    type Guard<'a> = OneHolderGuard<'a>;

    fn name(&self) -> &str {
        LOCK_NAME
    }

    fn acquire(&self, _timeout: Duration) -> Result<Held<OneHolderGuard<'_>>, LockError> {
        if self.0.replace(true) {
            return Err(LockError(format!("{LOCK_NAME} is held")));
        }
        Ok(Held {
            guard: OneHolderGuard(self),
            abandoned: false,
        })
    }
}

#[test]
fn two_gates_taking_over_one_dead_marker_rebuild_once() {
    let root = fresh_dir("durations_two_gates_take_over");
    fs::write(rebuilding_path(&root), r#"{"pid":1,"start":1}"#).expect("a dead gate's marker can be seeded");
    let lock = OneHolder::default();
    let mut second = None;

    // The second gate tries its takeover while the first is judging the dead marker.
    let first = begin_rebuild(&root, &lock, gate(100, 1_000), |pid, start| {
        second = Some(begin_rebuild(&root, &lock, gate(200, 2_000), |_, _| false));
        (pid, start) != (1, 1)
    })
    .expect("the first gate takes the marker over");

    assert!(first);
    assert!(!matches!(second, Some(Ok(true))), "only one gate rebuilds: {second:?}");
    assert_eq!(marker(&root), json!({"pid": 100, "start": 1000}));
}

#[test]
fn a_marker_is_never_read_empty_by_a_second_gate() {
    const ROUNDS: usize = 200;
    let base = fresh_dir("durations_marker_never_empty");
    let round = AtomicUsize::new(0);
    let reads = thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let mut whole = 0;
            let mut partial = Vec::new();
            loop {
                let at = round.load(Ordering::SeqCst);
                if at >= ROUNDS {
                    return (whole, partial);
                }
                if let Ok(bytes) = fs::read(rebuilding_path(&base.join(at.to_string()))) {
                    if serde_json::from_slice::<Value>(&bytes).is_ok() {
                        whole += 1;
                    } else {
                        partial.push(String::from_utf8_lossy(&bytes).into_owned());
                    }
                }
                thread::yield_now();
            }
        });
        for at in 0..ROUNDS {
            let root = base.join(at.to_string());
            fs::create_dir_all(&root).expect("a state root can be made");
            assert!(begin_rebuild(&root, &FakeLock::Free, ME, |_, _| true).expect("the marker can be made"));
            round.store(at + 1, Ordering::SeqCst);
        }
        reader.join().expect("the reader ends")
    });

    assert_eq!(
        reads.1,
        Vec::<String>::new(),
        "every read of the marker holds its whole content ({} whole reads)",
        reads.0
    );
}

#[test]
fn a_swap_that_cannot_take_the_lock_removes_its_marker() {
    let root = fresh_dir("durations_swap_busy_removes_marker");
    assert!(begin_rebuild(&root, &FakeLock::Free, ME, |_, _| true).expect("the marker can be made"));
    let rebuilt = rebuild_store(&root, Some(TEMP), ts(NOW), lookup);

    let swap = swap_rebuilt(&root, &FakeLock::Busy, &rebuilt, ts(NOW), ME);

    assert!(matches!(swap, Err(StoreError::Lock(_))), "{swap:?}");
    assert!(!rebuilding_path(&root).exists(), "the gate lets the rebuild go to another");
}

#[test]
fn a_swap_leaves_a_marker_another_gate_took_over() {
    let root = fresh_dir("durations_swap_leaves_other_marker");
    let rebuilt = rebuild_store(&root, Some(TEMP), ts(NOW), lookup);
    assert!(begin_rebuild(&root, &FakeLock::Free, gate(2, 20), |_, _| false).expect("another gate marks the rebuild"));

    let swap = swap_rebuilt(&root, &FakeLock::Free, &rebuilt, ts(NOW), ME).expect("the swap runs");

    assert_eq!(swap.outcome, Swapped::Replaced);
    assert_eq!(marker(&root), json!({"pid": 2, "start": 20}), "another gate's marker is left");
}

/// A journal lock that records each take and release with the journal's line count at that moment.
struct OrderLock {
    journal: PathBuf,
    events: RefCell<Vec<String>>,
}

impl OrderLock {
    fn record(&self, what: &str) {
        let lines = fs::read_to_string(&self.journal).map_or(0, |text| text.lines().count());
        self.events.borrow_mut().push(format!("{what} with {lines} line(s)"));
    }
}

struct OrderGuard<'a>(&'a OrderLock);

impl Drop for OrderGuard<'_> {
    fn drop(&mut self) {
        self.0.record("release");
    }
}

impl JournalLock for OrderLock {
    type Guard<'a> = OrderGuard<'a>;

    fn name(&self) -> &str {
        LOCK_NAME
    }

    fn acquire(&self, _timeout: Duration) -> Result<Held<OrderGuard<'_>>, LockError> {
        self.record("acquire");
        Ok(Held {
            guard: OrderGuard(self),
            abandoned: false,
        })
    }
}

#[test]
fn append_line_then_runs_inside_the_journal_hold() {
    let root = fresh_dir("append_line_then_runs_inside_the_journal_hold");
    let line = r#"{"v":1,"event":"release","pid":1}"#;
    let lock = OrderLock {
        journal: root.join("events.jsonl"),
        events: RefCell::new(Vec::new()),
    };

    let (appended, after) = append_line_then(&root, &lock, fixed(line), ts(NOW), || {
        lock.record("after");
        7
    });

    assert!(appended.written && appended.notes.is_empty(), "{appended:?}");
    assert_eq!(after, Some(7));
    assert_eq!(
        lock.events.into_inner(),
        ["acquire with 0 line(s)", "after with 1 line(s)", "release with 1 line(s)"],
        "the step runs after the line is written and before the lock is let go"
    );

    let (appended, after) = append_line_then(&root, &FakeLock::Busy, fixed(line), ts(NOW), || 7);
    assert!(!appended.written);
    assert_eq!(after, None, "a line not written runs no step");
}

/// A word of a command for the normaliser's property: one each rule rewrites, or a short run of path punctuation.
fn durations_word() -> impl Strategy<Value = String> {
    let ruled = prop_oneof![
        Just("cargo.exe"),
        Just("dotnet"),
        Just("a.cmd.exe"),
        Just("X:/work/repo.wt/slug/a/b"),
        Just(r"X:\work\repo.wt\slug"),
        Just("X:/work/other.wt/s/c"),
        Just("--out=X:/work/repo.wt/slug/c"),
        Just("X:/scratch/Temp/x"),
        Just(".tmp/msg.txt"),
        Just("22abea9"),
        Just("--include"),
        Just("--files"),
        Just("-E"),
        Just("-t"),
        Just("-Filter"),
        Just("--filter=a b"),
        Just("<sha>"),
        Just("\u{2026}"),
    ]
    .prop_map(str::to_owned);
    let character = prop_oneof![
        Just('x'),
        Just('a'),
        Just('7'),
        Just(' '),
        Just('\t'),
        Just('"'),
        Just('\\'),
        Just('/'),
        Just('.'),
        Just('-'),
        Just(':'),
        Just('='),
    ];
    // A doubled `/` can make the `.wt` component rule's output an absolute path it shortens again; the rule is shared
    // with `normalize_command`, whose output must not change, so the property leaves such words out.
    let free = prop::collection::vec(character, 0..8)
        .prop_map(|characters| characters.into_iter().collect::<String>())
        .prop_filter("no doubled slash", |word| !word.contains("//"));
    prop_oneof![ruled, free]
}

proptest! {
    // The normaliser never panics, and normalising its own output, exact or family, changes nothing.
    #[test]
    fn durations_normalize_is_idempotent(words in prop::collection::vec(durations_word(), 0..8), in_git in any::<bool>()) {
        let args: Vec<OsString> = words.into_iter().map(OsString::from).collect();
        let text = gate_args::command_text(&args);
        let root = in_git.then_some(ROOT);
        let once = normalize_for_durations(&text, root, Some(TEMP));
        prop_assert_eq!(&normalize_for_durations(&once.exact, root, Some(TEMP)).exact, &once.exact);
        prop_assert_eq!(&normalize_for_durations(&once.family, root, Some(TEMP)).family, &once.family);
    }
}
