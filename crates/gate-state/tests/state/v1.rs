//! The formats `gate.exe` writes under a state root: claims, tickets and the counts file, read strictly, plus the count
//! formula and the paths they sit at.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use gate_state::v1::{self, Claim, ClaimKind, Counts, CoveredMarker, FormatError, PoolSetting, Ticket, TicketKind};
use gate_state::{KernelObject, SlotKind, beside_allowance};
use jiff::{SignedDuration, Timestamp};
use serde_json::json;

use crate::support::{Machine, ts};

const CLAIM_TEXT: &str = include_str!("fixtures/v1-claim.json");
const TICKET_TEXT: &str = include_str!("fixtures/v1-ticket.json");
const COVERED_TEXT: &str = include_str!("fixtures/v1-covered.json");
/// The name gate.exe gives the fixture marker: `<pid>-<start ms>.json`.
const COVERED_NAME: &str = "54000-1790852400500.json";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("state")
        .join("fixtures")
        .join(name)
}

/// A fresh folder for the test.
fn scratch(test: &str) -> PathBuf {
    Machine::new(&format!("v1-{test}")).dir
}

/// The claim `v1-claim.json` holds.
fn claim() -> Claim {
    Claim {
        kind: ClaimKind::Heavy,
        index: 0,
        pid: 51000,
        process_start: ts("2026-10-02T21:16:00.5711853Z"),
        started: ts("2026-10-02T21:16:02.0157648Z"),
        cwd: r"C:\work\repo".to_owned(),
        command: "cargo test -p gate-state".to_owned(),
        log: r"C:\work\repo\.tmp\state-test.log".to_owned(),
        timeout_seconds: 900.0,
        remote: None,
        host: None,
        key: None,
        reason: None,
        expected: None,
    }
}

/// A local light ticket; with pid 60080 and arrival 1759439762015 it is the one `v1-ticket.json` holds.
fn ticket(pid: u32, arrived: u64) -> Ticket {
    Ticket {
        kind: TicketKind::Light,
        arrived,
        pid,
        start_ms: 1_759_439_760_571,
        holds: false,
        lends_closed: false,
        alone: false,
        remote: None,
        pool: None,
        pinned: None,
        reason: None,
        expected: None,
    }
}

/// The covered marker `v1-covered.json` holds.
fn covered() -> CoveredMarker {
    CoveredMarker {
        pid: 54000,
        process_start: 1_790_852_400_500,
        parent_pid: 52000,
        command: "cargo fmt --check".to_owned(),
        cwd: r"C:\work\repo".to_owned(),
        since: 1_790_852_402_000,
        kind: None,
    }
}

/// The markers [`v1::read_covered`] reads from the covered folder under `root`, by file name.
fn covered_names(root: &Path) -> Vec<(String, CoveredMarker)> {
    v1::read_covered(&v1::covered_dir(root))
        .expect("the covered folder lists")
        .into_iter()
        .map(|(path, marker)| {
            let name = path.file_name().expect("a marker has a name").to_string_lossy().into_owned();
            (name, marker)
        })
        .collect()
}

/// `text` written as a claim file in a fresh folder, then read.
fn claim_from(test: &str, text: &str) -> Result<Claim, FormatError> {
    let path = scratch(test).join("gate-slot-0.json");
    fs::write(&path, text).expect("the claim can be written");
    v1::read_claim(&path)
}

/// The fixture claim's text with `from` replaced by `to`, which must occur in it.
fn claim_text(from: &str, to: &str) -> String {
    assert!(CLAIM_TEXT.contains(from), "the fixture claim holds {from}");
    CLAIM_TEXT.replacen(from, to, 1)
}

/// The names of `path`'s parts under `root`.
fn parts_under(root: &Path, path: &Path) -> Vec<String> {
    let relative = path.strip_prefix(root).expect("the path is under the root");
    relative.iter().map(|part| part.to_string_lossy().into_owned()).collect()
}

fn setting(count: u32, ignored: Option<String>) -> PoolSetting {
    PoolSetting { count, ignored }
}

#[test]
fn default_counts_follow_the_thread_budgets() {
    let rows = [(1, 1, 1), (2, 1, 1), (5, 1, 2), (8, 1, 3), (9, 2, 4), (17, 4, 8)];

    for (processors, heavy, light) in rows {
        assert_eq!(SlotKind::Heavy.default_count(processors), heavy, "heavy on {processors} processors");
        assert_eq!(SlotKind::Light.default_count(processors), light, "light on {processors} processors");
    }
}

#[test]
fn the_beside_allowance_is_two_light_gates_or_the_whole_pool() {
    let plain = Ticket {
        kind: TicketKind::Critical,
        ..ticket(60080, 1)
    };
    assert_eq!(beside_allowance(1, []), 1);
    assert_eq!(beside_allowance(2, []), 2);
    assert_eq!(beside_allowance(5, [&plain]), 2);
}

#[test]
fn the_beside_allowance_is_none_while_a_request_to_hold_the_machine_alone_is_live() {
    let alone = Ticket {
        kind: TicketKind::Critical,
        alone: true,
        ..ticket(60081, 2)
    };
    let light = ticket(60080, 1);
    let not_critical = Ticket {
        alone: true,
        ..ticket(60082, 3)
    };

    assert_eq!(beside_allowance(5, [&light, &alone]), 0);
    assert_eq!(beside_allowance(1, [&alone]), 0);
    assert_eq!(
        beside_allowance(5, [&light, &not_critical]),
        2,
        "only a critical ticket holds the machine alone"
    );
}

#[test]
fn a_ticket_writes_alone_only_when_true() {
    let dir = scratch("alone-ticket");
    let plain = Ticket {
        kind: TicketKind::Critical,
        ..ticket(60080, 1)
    };
    let path = v1::ticket_path(&dir, plain.pid, plain.start_ms);

    v1::write_ticket(&path, &plain).expect("the plain critical ticket is written");
    let text = fs::read_to_string(&path).expect("the plain critical ticket is there");
    assert!(!text.contains("alone"), "{text}");

    let alone = Ticket { alone: true, ..plain };
    v1::write_ticket(&path, &alone).expect("the alone ticket is written");
    let text = fs::read_to_string(&path).expect("the alone ticket is there");
    assert!(text.ends_with(r#""holds":false,"alone":true}"#), "{text}");
    assert_eq!(v1::read_ticket(&path).expect("the written alone ticket reads"), alone);
}

#[test]
fn paths_sit_under_the_root_as_design_md_names_them() {
    let root = Path::new("state-root");

    assert_eq!(parts_under(root, &v1::slots_dir(root)), ["slots"]);
    assert_eq!(
        parts_under(root, &v1::claim_path(root, KernelObject::LightSlot(3)).expect("a light slot has a claim")),
        ["slots", "gate-light-slot-3.json"]
    );
    assert_eq!(
        parts_under(
            root,
            &v1::claim_path(root, KernelObject::Critical).expect("the critical gate has a claim")
        ),
        ["slots", "gate-critical.json"]
    );
    assert_eq!(v1::claim_path(root, KernelObject::Journal), None);
    assert_eq!(parts_under(root, &v1::tickets_dir(root)), ["waiting"]);
    assert_eq!(
        parts_under(root, &v1::ticket_path(root, 4242, 1_760_000_000_123)),
        ["waiting", "4242-1760000000123.json"]
    );
    assert_eq!(parts_under(root, &v1::covered_dir(root)), ["covered"]);
    assert_eq!(
        parts_under(root, &v1::covered_path(root, 4242, 1_760_000_000_123)),
        ["covered", "4242-1760000000123.json"]
    );
    assert_eq!(parts_under(root, &v1::counts_path(root)), ["slot-counts.json"]);
    assert_eq!(parts_under(root, &v1::paused_dir(root)), ["paused"]);
    assert_eq!(
        parts_under(root, &v1::pause_path(root, 4242, 1_760_000_000_000, 5151, 1_760_000_001_000)),
        ["paused", "4242-1760000000000-5151-1760000001000.pause"]
    );
}

#[test]
fn the_outputs_pending_marker_sits_in_the_remote_folder() {
    let root = Path::new("state-root");

    assert_eq!(parts_under(root, &v1::remote_dir(root)), ["remote"]);
    assert_eq!(
        parts_under(root, &v1::outputs_pending_path(root, "app-1a2b3c4d")),
        ["remote", "app-1a2b3c4d.outputs-pending"]
    );
}

#[test]
fn the_outputs_from_record_sits_in_the_remote_folder() {
    let root = Path::new("state-root");

    assert_eq!(
        parts_under(root, &v1::outputs_from_path(root, "app-1a2b3c4d")),
        ["remote", "app-1a2b3c4d.outputs-from"]
    );
}

#[test]
fn a_builder_s_cache_folder_is_the_remote_folder_for_host_and_its_id_folder_for_an_entry() {
    let root = Path::new("state-root");

    assert_eq!(parts_under(root, &v1::builder_cache_dir(root, None)), ["remote"]);
    assert_eq!(parts_under(root, &v1::builder_cache_dir(root, Some("1f0efc62"))), ["remote", "1f0efc62"]);
    let id = Some("1f0efc62");
    let key = "app-1a2b3c4d";
    for (path, name) in [
        (v1::outputs_hashes_path(root, id, key), "app-1a2b3c4d.outputs-hashes"),
        (v1::outputs_capped_path(root, id, key), "app-1a2b3c4d.outputs-capped"),
        (v1::inputs_hashes_path(root, id, key), "app-1a2b3c4d.inputs-hashes"),
        (v1::sent_build_hashes_path(root, id, key), "app-1a2b3c4d.sent-build-hashes"),
    ] {
        assert_eq!(parts_under(root, &path), ["remote", "1f0efc62", name]);
    }
    assert_eq!(
        parts_under(root, &v1::nothing_back_path(root, key)),
        ["remote", "app-1a2b3c4d.nothing-back"],
        "nothing-back stays PC-wide"
    );
    assert_eq!(
        parts_under(root, &v1::outputs_pending_path(root, key)),
        ["remote", "app-1a2b3c4d.outputs-pending"],
        "the outputs-pending marker stays PC-wide"
    );
    assert_eq!(
        parts_under(root, &v1::outputs_from_path(root, key)),
        ["remote", "app-1a2b3c4d.outputs-from"],
        "the outputs-from record stays PC-wide"
    );
    assert_eq!(parts_under(root, &v1::warm_cap_lock_path(root)), ["remote", "warm.lock"]);
}

#[test]
fn the_outputs_hash_cache_sits_in_the_remote_folder() {
    let root = Path::new("state-root");

    assert_eq!(
        parts_under(root, &v1::outputs_hashes_path(root, None, "app-1a2b3c4d")),
        ["remote", "app-1a2b3c4d.outputs-hashes"]
    );
}

#[test]
fn the_nothing_back_record_sits_in_the_remote_folder() {
    let root = Path::new("state-root");

    assert_eq!(
        parts_under(root, &v1::nothing_back_path(root, "app-1a2b3c4d")),
        ["remote", "app-1a2b3c4d.nothing-back"]
    );
}

#[test]
fn a_nothing_back_record_is_written_whole_over_any_other_and_read_back() {
    let root = scratch("nothing-back");
    let path = v1::nothing_back_path(&root, "app-1a2b3c4d");

    v1::write_nothing_back(&path, &record("builder", None)).expect("the record can be written");
    let first = fs::read(&path).expect("the record can be read");
    let other = v1::NothingBackRecord {
        framework: Some("net10.0".to_owned()),
        runtime: Some("win-x64".to_owned()),
        ..record("other", Some("Release"))
    };
    v1::write_nothing_back(&path, &other).expect("a record can be written over another");

    assert_eq!(
        String::from_utf8_lossy(&first),
        r#"{"v":1,"host":"builder"}"#,
        "an absent setting is left out"
    );
    let bytes = fs::read(&path).expect("the record can be read");
    assert_eq!(
        String::from_utf8_lossy(&bytes),
        r#"{"v":1,"host":"other","configuration":"Release","framework":"net10.0","runtime":"win-x64"}"#
    );
    assert_eq!(v1::nothing_back_record(&bytes).expect("the record reads"), other);
    let names: Vec<_> = fs::read_dir(v1::remote_dir(&root))
        .expect("the remote folder can be listed")
        .map(|entry| entry.expect("an entry can be read").file_name())
        .collect();
    assert_eq!(names, ["app-1a2b3c4d.nothing-back"], "no temp file is left beside it");
}

/// A nothing-back record naming `host`, with `configuration` and no framework or runtime.
fn record(host: &str, configuration: Option<&str>) -> v1::NothingBackRecord {
    v1::NothingBackRecord {
        host: host.to_owned(),
        configuration: configuration.map(str::to_owned),
        framework: None,
        runtime: None,
        built: Vec::new(),
    }
}

#[test]
fn a_nothing_back_record_of_another_shape_names_no_builder() {
    let rows: [(&str, &[u8]); 8] = [
        ("no JSON", b"{\"v\":1,"),
        ("another version", br#"{"v":2,"host":"builder"}"#),
        ("no version", br#"{"host":"builder"}"#),
        ("no host", br#"{"v":1}"#),
        ("a host that is no string", br#"{"v":1,"host":7}"#),
        ("a duplicate key", br#"{"v":1,"host":"a","host":"b"}"#),
        ("not UTF-8", b"{\"v\":1,\"host\":\"\xff\"}"),
        ("a setting that is no string", br#"{"v":1,"host":"builder","runtime":7}"#),
    ];
    for (label, bytes) in rows {
        assert!(v1::nothing_back_record(bytes).is_err(), "{label}");
    }
    assert_eq!(
        v1::nothing_back_record(br#"{"v":1,"host":"builder"}"#).expect("a record from before the settings"),
        record("builder", None)
    );
    assert_eq!(
        v1::nothing_back_record(br#"{"v":1,"host":"builder","configuration":"Debug"}"#).expect("a record"),
        record("builder", Some("Debug"))
    );
}

#[test]
fn the_outputs_cap_record_sits_in_the_remote_folder() {
    let root = Path::new("state-root");

    assert_eq!(
        parts_under(root, &v1::outputs_capped_path(root, None, "app-1a2b3c4d")),
        ["remote", "app-1a2b3c4d.outputs-capped"]
    );
}

#[test]
fn an_outputs_cap_record_is_written_whole_over_any_other_and_read_back() {
    let root = scratch("outputs-capped");
    let path = v1::outputs_capped_path(&root, None, "app-1a2b3c4d");

    v1::write_outputs_capped(&path, &v1::OutputsCappedRecord { hits: 1 }).expect("the record can be written");
    let first = fs::read(&path).expect("the record can be read");
    v1::write_outputs_capped(&path, &v1::OutputsCappedRecord { hits: 2 }).expect("a record can be written over another");

    assert_eq!(String::from_utf8_lossy(&first), r#"{"v":1,"hits":1}"#);
    let bytes = fs::read(&path).expect("the record can be read");
    assert_eq!(String::from_utf8_lossy(&bytes), r#"{"v":1,"hits":2}"#);
    assert_eq!(
        v1::outputs_capped_record(&bytes).expect("the record reads"),
        v1::OutputsCappedRecord { hits: 2 }
    );
    let names: Vec<_> = fs::read_dir(v1::remote_dir(&root))
        .expect("the remote folder can be listed")
        .map(|entry| entry.expect("an entry can be read").file_name())
        .collect();
    assert_eq!(names, ["app-1a2b3c4d.outputs-capped"], "no temp file is left beside it");
}

#[test]
fn an_outputs_cap_record_of_another_shape_holds_no_count() {
    let rows: [(&str, &[u8]); 8] = [
        ("no JSON", b"{\"v\":1,"),
        ("another version", br#"{"v":2,"hits":1}"#),
        ("no version", br#"{"hits":1}"#),
        ("no count", br#"{"v":1}"#),
        ("a count that is no number", br#"{"v":1,"hits":"1"}"#),
        ("a negative count", br#"{"v":1,"hits":-1}"#),
        ("a duplicate key", br#"{"v":1,"hits":1,"hits":2}"#),
        ("not UTF-8", b"{\"v\":1,\"hits\":1}\xff"),
    ];
    for (label, bytes) in rows {
        assert!(v1::outputs_capped_record(bytes).is_err(), "{label}");
    }
}

/// The moment the worktree record tests count from.
fn worktree_now() -> Timestamp {
    "2026-10-21T12:00:00Z".parse().expect("a time")
}

/// [`worktree_now`] less `age`.
fn worktree_ago(age: SignedDuration) -> Timestamp {
    worktree_now().checked_sub(age).expect("a moment after the epoch")
}

/// A worktree record of the worktree `X:/dev/repo.wt/a` of `repo`, last routed at `last_routed`.
fn worktree(last_routed: Timestamp) -> v1::WorktreeRecord {
    v1::WorktreeRecord {
        root: "X:/dev/repo.wt/a".to_owned(),
        common_dir: r"X:\dev\repo\.git".to_owned(),
        repo: "repo".to_owned(),
        last_routed,
        gone_since: None,
    }
}

#[test]
fn the_worktree_record_sits_in_the_remote_folder() {
    let root = Path::new("state-root");

    assert_eq!(
        parts_under(root, &v1::worktree_record_path(root, "app-1a2b3c4d")),
        ["remote", "app-1a2b3c4d.worktree"]
    );
}

#[test]
fn a_worktree_record_is_written_with_v_and_its_four_keys_and_read_back() {
    let root = scratch("worktree-record");
    let path = v1::worktree_record_path(&root, "app-1a2b3c4d");
    let written = worktree(worktree_now());

    v1::write_worktree_record(&path, &written).expect("the record can be written");

    let text = fs::read_to_string(&path).expect("the record can be read");
    let value: serde_json::Value = serde_json::from_str(&text).expect("the record is JSON");
    let keys: BTreeSet<&str> = value.as_object().expect("an object").keys().map(String::as_str).collect();
    assert_eq!(keys, BTreeSet::from(["v", "root", "commonDir", "repo", "lastRouted"]), "{text}");
    assert_eq!(value["v"], 1, "{text}");
    assert_eq!(value["lastRouted"], "2026-10-21T12:00:00.0000000Z", "{text}");
    assert_eq!(v1::worktree_record(text.as_bytes()).expect("the record reads"), written);
    let names: Vec<_> = fs::read_dir(v1::remote_dir(&root))
        .expect("the remote folder can be listed")
        .map(|entry| entry.expect("an entry can be read").file_name())
        .collect();
    assert_eq!(names, ["app-1a2b3c4d.worktree"], "no temp file is left beside it");
}

#[test]
fn a_worktree_record_with_a_field_it_does_not_know_reads() {
    let text = r#"{"v":1,"root":"X:/dev/repo.wt/a","commonDir":"X:\\dev\\repo\\.git","repo":"repo","lastRouted":"2026-10-21T12:00:00Z","laterField":"2026-10-21T13:00:00Z"}"#;

    assert_eq!(v1::worktree_record(text.as_bytes()).expect("the record reads"), worktree(worktree_now()));
}

#[test]
fn a_worktree_record_of_another_shape_is_refused() {
    let fields = r#""root":"X:/a","commonDir":"X:\\a\\.git","repo":"a","lastRouted":"2026-10-21T12:00:00Z""#;
    let rows: [(&str, Vec<u8>); 6] = [
        ("no version", format!("{{{fields}}}").into_bytes()),
        ("another version", format!(r#"{{"v":2,{fields}}}"#).into_bytes()),
        ("a duplicate key", format!(r#"{{"v":1,{fields},"repo":"b"}}"#).into_bytes()),
        ("fields missing", br#"{"v":1,"root":"X:/a"}"#.to_vec()),
        (
            "a time that is no time",
            format!(r#"{{"v":1,{}}}"#, fields.replace("2026-10-21T12:00:00Z", "noon")).into_bytes(),
        ),
        ("not UTF-8", b"\xff".to_vec()),
    ];
    for (label, bytes) in rows {
        assert!(v1::worktree_record(&bytes).is_err(), "{label}");
    }
}

#[test]
fn needs_write_when_the_worktree_record_is_missing_or_unreadable() {
    assert!(v1::needs_write(None, &worktree(worktree_now())));
}

#[test]
fn needs_write_when_the_root_changed() {
    let existing = v1::WorktreeRecord {
        root: "X:/dev/repo.wt/b".to_owned(),
        ..worktree(worktree_ago(SignedDuration::from_mins(1)))
    };

    assert!(v1::needs_write(Some(&existing), &worktree(worktree_now())));
}

#[test]
fn needs_write_when_the_common_dir_changed() {
    let existing = v1::WorktreeRecord {
        common_dir: r"X:\dev\other\.git".to_owned(),
        ..worktree(worktree_ago(SignedDuration::from_mins(1)))
    };

    assert!(v1::needs_write(Some(&existing), &worktree(worktree_now())));
}

#[test]
fn needs_write_when_the_repo_changed() {
    let existing = v1::WorktreeRecord {
        repo: "other".to_owned(),
        ..worktree(worktree_ago(SignedDuration::from_mins(1)))
    };

    assert!(v1::needs_write(Some(&existing), &worktree(worktree_now())));
}

#[test]
fn needs_write_not_for_the_same_record_routed_59_minutes_ago() {
    let existing = worktree(worktree_ago(SignedDuration::from_mins(59)));

    assert!(!v1::needs_write(Some(&existing), &worktree(worktree_now())));
}

#[test]
fn needs_write_for_a_record_routed_exactly_an_hour_ago() {
    let existing = worktree(worktree_ago(SignedDuration::from_hours(1)));

    assert!(v1::needs_write(Some(&existing), &worktree(worktree_now())));
}

#[test]
fn needs_write_for_a_record_routed_after_now() {
    let existing = worktree(worktree_now().checked_add(SignedDuration::from_mins(5)).expect("a time"));

    assert!(v1::needs_write(Some(&existing), &worktree(worktree_now())));
}

const WARM_HASH: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

/// A warm-up record in `state`, failed ones by their commands.
fn warm(state: v1::WarmState) -> v1::WarmRecord {
    let failed = state == v1::WarmState::Failed;
    v1::WarmRecord {
        host: "builder".to_owned(),
        state,
        at: ts("2026-10-16T08:00:00.5000000Z"),
        hash: WARM_HASH.to_owned(),
        gate: "0.1.0+abc1234".to_owned(),
        pid: 4242,
        started: ts("2026-10-16T07:59:00Z"),
        failure: failed.then_some(v1::WarmFailure::Commands),
        reason: failed.then(|| "command 1 exited with status 1".to_owned()),
    }
}

#[test]
fn the_warm_up_files_sit_in_the_remote_folder() {
    let root = Path::new("state-root");

    assert_eq!(parts_under(root, &v1::warm_dir(root)), ["remote", "warm"]);
    assert_eq!(
        parts_under(root, &v1::warm_record_path(root, "app-1a2b3c4d")),
        ["remote", "warm", "app-1a2b3c4d.json"]
    );
    assert_eq!(
        parts_under(root, &v1::warm_log_path(root, "app-1a2b3c4d")),
        ["remote", "warm", "app-1a2b3c4d.log"]
    );
    assert_eq!(
        parts_under(root, &v1::warm_lock_path(root, "app-1a2b3c4d")),
        ["remote", "warm", "app-1a2b3c4d.lock"]
    );
    assert_eq!(parts_under(root, &v1::warm_cap_lock_path(root)), ["remote", "warm.lock"]);
}

#[test]
fn a_warm_up_record_of_each_state_is_written_whole_and_read_back() {
    let root = scratch("warm-record");
    let path = v1::warm_record_path(&root, "app-1a2b3c4d");
    let refused = v1::WarmRecord {
        failure: Some(v1::WarmFailure::Refused),
        reason: Some("the builder is busy".to_owned()),
        ..warm(v1::WarmState::Failed)
    };
    let records = [
        warm(v1::WarmState::Warming),
        warm(v1::WarmState::Warm),
        warm(v1::WarmState::Failed),
        refused,
    ];

    for record in records {
        v1::write_warm_record(&path, &record).expect("the record can be written");
        let bytes = fs::read(&path).expect("the record can be read");
        assert_eq!(v1::warm_record(&bytes).expect("the record reads"), record);
    }
    let bytes = fs::read(&path).expect("the record can be read");
    assert_eq!(
        String::from_utf8_lossy(&bytes),
        format!(
            concat!(
                r#"{{"v":1,"host":"builder","state":"failed","at":"2026-10-16T08:00:00.5000000Z","hash":"{WARM_HASH}","gate":"0.1.0+abc1234","#,
                r#""pid":4242,"started":"2026-10-16T07:59:00.0000000Z","failure":"refused","reason":"the builder is busy"}}"#
            ),
            WARM_HASH = WARM_HASH
        )
    );
    v1::write_warm_record(&path, &warm(v1::WarmState::Warm)).expect("the record can be written");
    let bytes = fs::read(&path).expect("the record can be read");
    assert!(!String::from_utf8_lossy(&bytes).contains("failure"), "a warm record has no failure");
    let names: Vec<_> = fs::read_dir(v1::warm_dir(&root))
        .expect("the warm folder can be listed")
        .map(|entry| entry.expect("an entry can be read").file_name())
        .collect();
    assert_eq!(names, ["app-1a2b3c4d.json"], "no temp file is left beside it");
}

/// The JSON of a warm-up record that reads, for a test to spoil.
fn good_warm_json() -> serde_json::Value {
    json!({
        "v": 1, "host": "builder", "state": "warm", "at": "2026-10-16T08:00:00Z", "hash": WARM_HASH,
        "gate": "0.1.0", "pid": 4242, "started": "2026-10-16T07:59:00Z"
    })
}

#[test]
fn a_warm_up_record_of_another_shape_is_refused() {
    let good = good_warm_json();
    let with = |key: &str, value: serde_json::Value| {
        let mut record = good.clone();
        record[key] = value;
        record.to_string().into_bytes()
    };
    let without = |key: &str| {
        let mut record = good.clone();
        record.as_object_mut().expect("an object").remove(key);
        record.to_string().into_bytes()
    };
    let failed_with = |key: &str, value: &str| {
        let mut record = good.clone();
        record["state"] = json!("failed");
        record[key] = json!(value);
        record.to_string().into_bytes()
    };
    let rows: Vec<(&str, Vec<u8>)> = vec![
        ("an unknown state", with("state", json!("cold"))),
        ("a state in another case", with("state", json!("Warm"))),
        ("a short hash", with("hash", json!(&WARM_HASH[1..]))),
        ("an upper-case hash", with("hash", json!(WARM_HASH.to_uppercase()))),
        ("a hash that is not hex", with("hash", json!(WARM_HASH.replacen('a', "g", 1)))),
        ("a failure without failed", with("failure", json!("commands"))),
        ("a reason without failed", with("reason", json!("why"))),
        ("failed without a failure", failed_with("reason", "why")),
        ("failed without a reason", failed_with("failure", "commands")),
        ("an unknown failure", {
            let mut record = good.clone();
            record["state"] = json!("failed");
            record["failure"] = json!("other");
            record["reason"] = json!("why");
            record.to_string().into_bytes()
        }),
        ("no host", without("host")),
        ("no time", without("at")),
        ("no gate", without("gate")),
        ("no pid", without("pid")),
        ("no start", without("started")),
        ("a time that is no time", with("at", json!("yesterday"))),
        ("another version", with("v", json!(2))),
        ("no JSON", b"{\"v\":1,".to_vec()),
    ];
    for (label, bytes) in rows {
        assert!(v1::warm_record(&bytes).is_err(), "{label}: {}", String::from_utf8_lossy(&bytes));
    }
    assert_eq!(
        v1::warm_record(good.to_string().as_bytes()).expect("the good record reads").state,
        v1::WarmState::Warm
    );
    let root = scratch("warm-record-refused");
    let path = v1::warm_record_path(&root, "app-1a2b3c4d");
    let bad = v1::WarmRecord {
        failure: Some(v1::WarmFailure::Commands),
        ..warm(v1::WarmState::Warm)
    };
    let error = v1::write_warm_record(&path, &bad).expect_err("a record that would not read back is not written");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{error}");
    assert!(!path.exists(), "nothing is written");
}

#[test]
fn a_warm_up_record_with_an_empty_host_is_refused() {
    let mut record = good_warm_json();
    record["host"] = json!("");

    let error = v1::warm_record(record.to_string().as_bytes()).expect_err("an empty host is refused");
    assert_eq!(error.to_string(), "host is empty");
}

#[test]
fn a_warm_up_record_with_an_empty_gate_is_refused() {
    let mut record = good_warm_json();
    record["gate"] = json!("");

    let error = v1::warm_record(record.to_string().as_bytes()).expect_err("an empty gate is refused");
    assert_eq!(error.to_string(), "gate is empty");
}

#[test]
fn a_warm_up_record_whose_failed_state_gives_no_reason_is_refused() {
    let mut record = good_warm_json();
    record["state"] = json!("failed");
    record["failure"] = json!("commands");
    record["reason"] = json!("");

    let error = v1::warm_record(record.to_string().as_bytes()).expect_err("an empty reason is refused");
    assert_eq!(error.to_string(), "state failed with an empty reason");
}

#[test]
fn a_pause_marker_name_builds_and_parses_and_refuses_anything_else() {
    let marker = v1::PauseMarker {
        parent_pid: 4242,
        parent_start_ms: 1_760_000_000_000,
        pid: 5151,
        start_ms: 1_760_000_001_000,
    };
    let name = v1::pause_name(marker.parent_pid, marker.parent_start_ms, marker.pid, marker.start_ms);

    assert_eq!(name, "4242-1760000000000-5151-1760000001000.pause");
    assert_eq!(v1::parse_pause_name(&name), Some(marker), "a built name parses back to what built it");
    let refused = [
        "4242-1760000000000-5151-176000001000-7.pause",
        "4242-1760000000000-5151-176000001000",
        "4242-1760000000000-5151-1760000001000.pause.bak",
        "4242-1760000000000-5151.pause",
        "4242-x-5151-1760000001000.pause",
        "4242--5151-1760000001000.pause",
        "-4242-1760000000000-5151-1760000001000.pause",
        "4242-1760000000000-4294967296-176000001000.pause",
        "4242-1760000000000-5151-123456789012345678901.pause",
    ];
    for text in refused {
        assert_eq!(v1::parse_pause_name(text), None, "{text}");
    }
}

#[test]
fn the_v1_claim_fixture_reads_with_every_field() {
    let read = v1::read_claim(&fixture("v1-claim.json")).expect("the fixture claim reads");

    assert_eq!(read, claim());
}

#[test]
fn the_v1_ticket_fixture_reads_with_its_queue_order() {
    let read = v1::read_ticket(&fixture("v1-ticket.json")).expect("the fixture ticket reads");

    assert_eq!(read, ticket(60080, 1_759_439_762_015));
    assert_eq!(read.queue_order(), (1_759_439_762_015, 60080));
}

#[test]
fn the_expected_fixtures_read_with_their_expected_and_the_plain_ones_without() {
    let expected = json!({"p50": 240.5, "p90": 380.25, "n": 30, "key": "local::repo | cargo build"})
        .as_object()
        .cloned()
        .expect("expected is an object");

    let claim = v1::read_claim(&fixture("v1-claim-expected.json")).expect("the claim with expected reads");
    let ticket = v1::read_ticket(&fixture("v1-ticket-expected.json")).expect("the ticket with expected reads");

    assert_eq!(claim.expected.as_ref(), Some(&expected));
    assert_eq!(ticket.expected.as_ref(), Some(&expected));
    let plain_claim = v1::read_claim(&fixture("v1-claim.json")).expect("the fixture claim reads");
    let plain_ticket = v1::read_ticket(&fixture("v1-ticket.json")).expect("the fixture ticket reads");
    assert_eq!((plain_claim.expected, plain_ticket.expected), (None, None), "no key reads as none");
}

#[test]
fn the_v1_counts_fixture_sets_both_pools() {
    let counts = v1::read_counts(&fixture("v1-slot-counts.json"), 17);

    assert_eq!(
        counts,
        Counts {
            heavy: setting(3, None),
            light: setting(6, None),
        }
    );
}

#[test]
fn a_written_claim_reads_back_equal_and_starts_with_v() {
    let dir = scratch("written-claim");
    let path = v1::claim_path(&dir, KernelObject::HeavySlot(0)).expect("a heavy slot has a claim");

    v1::write_claim(&path, &claim()).expect("the claim is written");
    let text = fs::read_to_string(&path).expect("the claim is there");

    assert!(text.starts_with(r#"{"v":1,"kind":"#), "{text}");
    assert_eq!(text, CLAIM_TEXT.trim_end(), "the fixture is what the writer writes");
    assert_eq!(v1::read_claim(&path).expect("the written claim reads"), claim());

    let remote = Claim {
        kind: ClaimKind::Remote,
        index: 1,
        remote: Some(true),
        host: Some("builder@build-box".to_owned()),
        key: Some("repo-1a2b".to_owned()),
        ..claim()
    };
    v1::write_claim(&path, &remote).expect("the remote claim is written");
    let text = fs::read_to_string(&path).expect("the remote claim is there");
    assert!(
        text.ends_with(r#","remote":true,"host":"builder@build-box","key":"repo-1a2b"}"#),
        "{text}"
    );
    assert_eq!(v1::read_claim(&path).expect("the written remote claim reads"), remote);
}

#[test]
fn a_written_ticket_reads_back_equal_and_omits_remote_when_local() {
    let dir = scratch("written-ticket");
    let local = ticket(60080, 1_759_439_762_015);
    let path = v1::ticket_path(&dir, local.pid, local.start_ms);

    v1::write_ticket(&path, &local).expect("the ticket is written");
    let text = fs::read_to_string(&path).expect("the ticket is there");

    assert_eq!(text, TICKET_TEXT.trim_end(), "the fixture is what the writer writes");
    assert!(!text.contains("remote"), "{text}");
    assert_eq!(v1::read_ticket(&path).expect("the written ticket reads"), local);

    let not_remote = Ticket {
        remote: Some(false),
        ..local.clone()
    };
    v1::write_ticket(&path, &not_remote).expect("the not-remote ticket is written");
    let text = fs::read_to_string(&path).expect("the not-remote ticket is there");
    assert_eq!(text, TICKET_TEXT.trim_end(), "remote false is not written");

    let remote = Ticket {
        kind: TicketKind::Heavy,
        remote: Some(true),
        ..local
    };
    v1::write_ticket(&path, &remote).expect("the remote ticket is written");
    let text = fs::read_to_string(&path).expect("the remote ticket is there");
    assert!(text.ends_with(r#""holds":false,"remote":true}"#), "{text}");
    assert_eq!(v1::read_ticket(&path).expect("the written remote ticket reads"), remote);
}

#[test]
fn a_ticket_of_a_builders_pool_carries_its_pool_and_one_of_host_s_pool_none() {
    let dir = scratch("pool");
    let path = v1::ticket_path(&dir, 60080, 1);
    let host = Ticket {
        kind: TicketKind::Heavy,
        remote: Some(true),
        ..ticket(60080, 1)
    };
    v1::write_ticket(&path, &host).expect("host's ticket is written");
    let text = fs::read_to_string(&path).expect("host's ticket is there");
    assert!(text.ends_with(r#""holds":false,"remote":true}"#), "host's ticket is as it was: {text}");
    assert!(!text.contains("pool"), "{text}");

    let pooled = Ticket {
        pool: Some("1f0efc62".to_owned()),
        ..host
    };
    v1::write_ticket(&path, &pooled).expect("the pooled ticket is written");
    let text = fs::read_to_string(&path).expect("the pooled ticket is there");
    assert!(text.ends_with(r#""remote":true,"pool":"1f0efc62"}"#), "{text}");
    assert_eq!(v1::read_ticket(&path).expect("the pooled ticket reads"), pooled);
}

#[test]
fn a_ticket_without_pinned_reads_as_not_pinned() {
    let dir = scratch("pinned-absent");
    let path = v1::ticket_path(&dir, 60080, 1_759_439_762_015);
    fs::create_dir_all(path.parent().expect("the ticket has a folder")).expect("the ticket folder can be made");
    fs::write(&path, TICKET_TEXT).expect("the fixture ticket can be written");

    let ticket = v1::read_ticket(&path).expect("the fixture ticket reads");

    assert_eq!(ticket.pinned, None, "a ticket without the key is not pinned");
}

#[test]
fn a_not_pinned_ticket_writes_no_pinned_key() {
    let dir = scratch("pinned-unwritten");
    let path = v1::ticket_path(&dir, 60080, 1_759_439_762_015);
    for pinned in [None, Some(false)] {
        let ticket = Ticket {
            pinned,
            ..ticket(60080, 1_759_439_762_015)
        };
        v1::write_ticket(&path, &ticket).expect("the ticket is written");
        let text = fs::read_to_string(&path).expect("the ticket is there");
        assert!(!text.contains("pinned"), "{pinned:?}: {text}");
        assert_eq!(text, TICKET_TEXT.trim_end(), "{pinned:?}: the fixture is what the writer writes");
    }
}

#[test]
fn a_pinned_ticket_round_trips() {
    let dir = scratch("pinned-roundtrip");
    let path = v1::ticket_path(&dir, 60080, 1_759_439_762_015);
    let pinned = Ticket {
        remote: Some(true),
        pool: Some("1f0efc62".to_owned()),
        pinned: Some(true),
        ..ticket(60080, 1_759_439_762_015)
    };
    v1::write_ticket(&path, &pinned).expect("the ticket is written");
    let text = fs::read_to_string(&path).expect("the ticket is there");

    assert!(text.ends_with(r#""pool":"1f0efc62","pinned":true}"#), "{text}");
    assert_eq!(v1::read_ticket(&path).expect("the written ticket reads"), pinned);
}

#[test]
fn a_reason_is_written_before_expected_and_reads_back() {
    let dir = scratch("reason");
    let expected = json!({"p50": 1.0}).as_object().cloned().expect("expected is an object");
    let claim_with = Claim {
        reason: Some("why".to_owned()),
        expected: Some(expected.clone()),
        ..claim()
    };
    let claim_path = v1::claim_path(&dir, KernelObject::HeavySlot(0)).expect("a heavy slot has a claim");
    v1::write_claim(&claim_path, &claim_with).expect("the claim is written");
    let text = fs::read_to_string(&claim_path).expect("the claim is there");
    assert!(text.ends_with(r#","reason":"why","expected":{"p50":1.0}}"#), "{text}");
    assert_eq!(v1::read_claim(&claim_path).expect("the written claim reads"), claim_with);

    let ticket_with = Ticket {
        reason: Some("why".to_owned()),
        expected: Some(expected),
        ..ticket(60080, 1)
    };
    let ticket_path = v1::ticket_path(&dir, ticket_with.pid, ticket_with.start_ms);
    v1::write_ticket(&ticket_path, &ticket_with).expect("the ticket is written");
    let text = fs::read_to_string(&ticket_path).expect("the ticket is there");
    assert!(text.ends_with(r#""reason":"why","expected":{"p50":1.0}}"#), "{text}");
    assert_eq!(v1::read_ticket(&ticket_path).expect("the written ticket reads"), ticket_with);

    v1::write_claim(&claim_path, &claim()).expect("the claim without one is written");
    assert!(!fs::read_to_string(&claim_path).expect("the claim is there").contains("reason"));
    v1::write_ticket(&ticket_path, &ticket(60080, 1)).expect("the ticket without one is written");
    assert!(!fs::read_to_string(&ticket_path).expect("the ticket is there").contains("reason"));
}

#[test]
fn a_written_file_leaves_no_tmp_beside_it() {
    let dir = scratch("no-tmp");
    let claim_path = v1::claim_path(&dir, KernelObject::Critical).expect("the critical gate has a claim");
    let ticket_path = v1::ticket_path(&dir, 60080, 1_759_439_760_571);

    v1::write_claim(&claim_path, &claim()).expect("the claim is written into a folder the write makes");
    v1::write_ticket(&ticket_path, &ticket(60080, 1)).expect("the ticket is written into a folder the write makes");

    for (folder, file) in [
        (v1::slots_dir(&dir), "gate-critical.json"),
        (v1::tickets_dir(&dir), "60080-1759439760571.json"),
    ] {
        let names: Vec<String> = fs::read_dir(&folder)
            .expect("the folder exists")
            .map(|entry| entry.expect("the entry lists").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [file], "{} holds only the written file", folder.display());
    }
}

#[test]
fn a_missing_v_is_unreadable() {
    let error = claim_from("missing-v", &claim_text(r#""v":1,"#, "")).expect_err("a claim without v is unreadable");

    assert!(matches!(error, FormatError::MissingVersion), "{error:?}");
    assert_eq!(error.to_string(), r#"missing key "v""#);
}

#[test]
fn an_unknown_v_is_unreadable() {
    for (v, shown) in [("2", "2"), (r#""1""#, r#""1""#)] {
        let text = claim_text(r#""v":1"#, &format!(r#""v":{v}"#));
        let error = claim_from("unknown-v", &text).expect_err("a claim with an unknown v is unreadable");

        assert!(matches!(&error, FormatError::UnknownVersion(text) if text == shown), "{v}: {error:?}");
        assert_eq!(error.to_string(), format!("unknown format v {shown}"));
    }
}

#[test]
fn a_duplicate_key_is_unreadable() {
    let top = claim_text(r#""pid":51000,"#, r#""pid":51000,"pid":51001,"#);
    let error = claim_from("duplicate-top", &top).expect_err("a top-level duplicate is unreadable");
    assert!(matches!(&error, FormatError::DuplicateKey(key) if key == "pid"), "{error:?}");
    assert_eq!(error.to_string(), r#"duplicate key "pid""#);

    let nested = claim_text(
        r#""timeoutSeconds":900.0"#,
        r#""timeoutSeconds":900.0,"expected":{"steps":[{"a":1,"a":2}]}"#,
    );
    let error = claim_from("duplicate-nested", &nested).expect_err("a duplicate inside expected is unreadable");
    assert!(matches!(&error, FormatError::DuplicateKey(key) if key == "a"), "{error:?}");
}

#[test]
fn a_wrong_case_key_is_a_missing_key() {
    let error = claim_from("wrong-case", &claim_text(r#""kind""#, r#""Kind""#)).expect_err("a claim with Kind has no kind");

    assert!(
        matches!(&error, FormatError::Invalid(message) if message == "missing field `kind`"),
        "{error:?}"
    );
}

#[test]
fn a_wrong_type_is_unreadable() {
    let error = claim_from("wrong-type", &claim_text(r#""pid":51000"#, r#""pid":"51000""#)).expect_err("a string pid is unreadable");

    assert!(
        matches!(&error, FormatError::Invalid(message) if message.starts_with("invalid type: string \"51000\"")),
        "{error:?}"
    );
}

#[test]
fn an_unknown_key_is_ignored() {
    let text = claim_text(r#""cwd""#, r#""later":{"x":[1,2]},"cwd""#);

    assert_eq!(claim_from("unknown-key", &text).expect("an unknown key is ignored"), claim());
}

#[test]
fn a_bom_is_unreadable() {
    let error = claim_from("bom", &format!("\u{feff}{CLAIM_TEXT}")).expect_err("a claim with a BOM is unreadable");

    assert!(matches!(error, FormatError::Syntax(_)), "{error:?}");
    assert!(error.to_string().starts_with("not JSON: "), "{error}");
}

#[test]
fn a_syntax_error_is_unreadable() {
    let trailing_comma = claim_text("900.0}", "900.0,}");
    let comment = format!("/* held */{CLAIM_TEXT}");

    for (name, text) in [("trailing-comma", trailing_comma), ("comment", comment)] {
        let error = claim_from(name, &text).expect_err("a syntax error is unreadable");
        assert!(matches!(error, FormatError::Syntax(_)), "{name}: {error:?}");
    }
}

#[test]
fn a_value_that_is_not_an_object_is_unreadable() {
    for text in ["3", r#"[{"v":1}]"#, r#""v""#] {
        let error = claim_from("not-object", text).expect_err("a non-object is unreadable");
        assert!(
            matches!(&error, FormatError::Invalid(message) if message == "not a JSON object"),
            "{text}: {error:?}"
        );
    }
}

#[test]
fn a_non_object_expected_is_unreadable() {
    for (name, expected) in [("expected-number", "3"), ("expected-string", r#""x""#), ("expected-list", "[{}]")] {
        let text = claim_text(r#""timeoutSeconds":900.0"#, &format!(r#""timeoutSeconds":900.0,"expected":{expected}"#));
        let error = claim_from(name, &text).expect_err("a non-object expected is unreadable");
        assert!(
            matches!(&error, FormatError::Invalid(message) if message.starts_with("invalid type: ")),
            "{expected}: {error:?}"
        );
    }

    let ticket_path = scratch("expected-ticket").join("60080-1759439760571.json");
    fs::write(&ticket_path, TICKET_TEXT.replacen('}', r#","expected":true}"#, 1)).expect("the ticket is written");
    let error = v1::read_ticket(&ticket_path).expect_err("a ticket with a non-object expected is unreadable");
    assert!(matches!(error, FormatError::Invalid(_)), "{error:?}");

    let null = claim_text(r#""timeoutSeconds":900.0"#, r#""timeoutSeconds":900.0,"expected":null"#);
    assert_eq!(
        claim_from("expected-null", &null).expect("a null expected reads"),
        claim(),
        "a null expected reads as absent"
    );
}

#[test]
fn a_claim_with_an_infinite_timeout_is_not_written() {
    let dir = scratch("infinite-timeout");
    let path = v1::claim_path(&dir, KernelObject::HeavySlot(0)).expect("a heavy slot has a claim");

    for timeout in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
        let refused = Claim {
            timeout_seconds: timeout,
            ..claim()
        };
        let error = v1::write_claim(&path, &refused).expect_err("a non-finite timeout is refused");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{timeout}");
        assert_eq!(error.to_string(), "timeoutSeconds must be finite", "{timeout}");
        assert!(!v1::slots_dir(&dir).exists(), "{timeout}: nothing is written");
    }
}

#[test]
fn a_file_that_is_not_utf8_is_unreadable() {
    let dir = scratch("not-utf8");
    let bytes = b"{\"v\":1,\"cwd\":\"\xff\"}";
    let claim_path = dir.join("gate-slot-0.json");
    fs::write(&claim_path, bytes).expect("the claim is written");

    let error = v1::read_claim(&claim_path).expect_err("a claim that is not UTF-8 is unreadable");

    assert!(matches!(error, FormatError::Syntax(_)), "{error:?}");
    assert_eq!(error.to_string(), "not JSON: not UTF-8");

    let counts_path = v1::counts_path(&dir);
    fs::write(&counts_path, bytes).expect("the counts file is written");
    let shown = counts_path.display();
    assert_eq!(
        v1::read_counts(&counts_path, 1).heavy,
        setting(1, Some(format!("gate: ignored {shown}: not JSON: not UTF-8; using 1 heavy slots")))
    );
}

#[test]
fn expected_passes_through_untouched() {
    let dir = scratch("expected");
    let expected = json!({"seconds": 12.5, "steps": [{"name": "build", "z": null}, 3], "a": false})
        .as_object()
        .cloned()
        .expect("expected is an object");
    let with_expected = Claim {
        expected: Some(expected.clone()),
        ..claim()
    };
    let path = v1::claim_path(&dir, KernelObject::HeavySlot(0)).expect("a heavy slot has a claim");

    v1::write_claim(&path, &with_expected).expect("the claim is written");
    let text = fs::read_to_string(&path).expect("the claim is there");

    assert!(
        text.ends_with(r#","expected":{"seconds":12.5,"steps":[{"name":"build","z":null},3],"a":false}}"#),
        "{text}"
    );
    let read = v1::read_claim(&path).expect("the claim reads");
    assert_eq!(read.expected, Some(expected));
    assert_eq!(read, with_expected);
}

#[test]
fn tickets_order_by_arrival_then_pid() {
    let mut tickets = [ticket(30, 200), ticket(20, 100), ticket(10, 200), ticket(40, 150)];

    tickets.sort_by_key(Ticket::queue_order);

    let pids: Vec<u32> = tickets.iter().map(|ticket| ticket.pid).collect();
    assert_eq!(pids, [20, 40, 10, 30]);
}

#[test]
fn read_tickets_reports_unreadable_files_and_skips_tmp() {
    let dir = scratch("read-tickets");
    let tickets = v1::tickets_dir(&dir);
    assert!(
        v1::read_tickets(&tickets).expect("a missing folder lists").is_empty(),
        "a missing folder holds no tickets"
    );

    let first = ticket(20, 100);
    let second = ticket(10, 200);
    v1::write_ticket(&v1::ticket_path(&dir, 20, 2), &first).expect("the first ticket is written");
    v1::write_ticket(&v1::ticket_path(&dir, 10, 1), &second).expect("the second ticket is written");
    fs::write(tickets.join("9-9.json"), "{").expect("the torn ticket is written");
    fs::write(tickets.join("8-8.json.77.tmp"), TICKET_TEXT).expect("the temporary file is written");

    let read = v1::read_tickets(&tickets).expect("the folder lists");

    let names: Vec<String> = read
        .iter()
        .map(|(path, _)| path.file_name().expect("a ticket has a name").to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["10-1.json", "20-2.json", "9-9.json"]);
    assert_eq!(read[0].1.as_ref().expect("the second ticket reads"), &second);
    assert_eq!(read[1].1.as_ref().expect("the first ticket reads"), &first);
    assert!(matches!(read[2].1, Err(FormatError::Syntax(_))), "{:?}", read[2].1);
}

#[test]
fn a_written_covered_marker_reads_back_equal_and_is_the_fixture() {
    let dir = scratch("written-covered");
    let marker = covered();
    let path = v1::covered_path(&dir, marker.pid, marker.process_start);

    v1::write_covered(&path, &marker).expect("the marker is written into a folder the write makes");
    let text = fs::read_to_string(&path).expect("the marker is there");

    assert_eq!(text, COVERED_TEXT.trim_end(), "the fixture is what the writer writes");
    assert_eq!(covered_names(&dir), [(COVERED_NAME.to_owned(), marker)]);
    let names: Vec<String> = fs::read_dir(v1::covered_dir(&dir))
        .expect("the covered folder exists")
        .map(|entry| entry.expect("the entry lists").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, [COVERED_NAME], "no temporary file is left beside the marker");
}

#[test]
fn the_v1_covered_fixture_reads_under_its_name() {
    let dir = scratch("covered-fixture");
    let folder = v1::covered_dir(&dir);
    fs::create_dir_all(&folder).expect("the covered folder can be made");
    fs::copy(fixture("v1-covered.json"), folder.join(COVERED_NAME)).expect("the fixture can be copied");

    assert_eq!(covered_names(&dir), [(COVERED_NAME.to_owned(), covered())]);
}

#[test]
fn a_covered_marker_that_is_unreadable_or_misnamed_is_skipped() {
    let missing_field = COVERED_TEXT.replacen(r#""cwd":"C:\\work\\repo","#, "", 1);
    let wrong_v = COVERED_TEXT.replacen(r#""v":1"#, r#""v":2"#, 1);
    assert_ne!(missing_field, COVERED_TEXT, "the fixture holds cwd");
    assert_ne!(wrong_v, COVERED_TEXT, "the fixture holds v");
    let files = [
        ("missing-field", COVERED_NAME, missing_field.as_str()),
        ("wrong-v", COVERED_NAME, wrong_v.as_str()),
        ("another-pid", "54001-1790852400500.json", COVERED_TEXT),
        ("another-start", "54000-1790852400501.json", COVERED_TEXT),
        ("extra-part", "54000-1790852400500-1.json", COVERED_TEXT),
        ("no-numbers", "not-a-marker.json", COVERED_TEXT),
        ("signed", "+54000-1790852400500.json", COVERED_TEXT),
        ("tmp", "54000-1790852400500.json.77.tmp", COVERED_TEXT),
        ("txt", "54000-1790852400500.txt", COVERED_TEXT),
    ];
    for (case, name, text) in files {
        let dir = scratch(&format!("covered-skipped-{case}"));
        let folder = v1::covered_dir(&dir);
        fs::create_dir_all(&folder).expect("the covered folder can be made");
        fs::write(folder.join(name), text).expect("the file can be written");

        assert_eq!(covered_names(&dir), [], "{case}: {name} is no marker");
        assert!(folder.join(name).exists(), "{case}: the reader deletes nothing");
    }
}

#[test]
fn an_empty_or_missing_covered_folder_holds_no_markers() {
    let dir = scratch("covered-empty");

    assert_eq!(covered_names(&dir), [], "a missing folder holds none");
    fs::create_dir_all(v1::covered_dir(&dir)).expect("the covered folder can be made");
    assert_eq!(covered_names(&dir), [], "an empty folder holds none");
}

#[test]
fn a_missing_counts_file_gives_defaults_silently() {
    let path = v1::counts_path(&scratch("counts-missing"));

    assert_eq!(
        v1::read_counts(&path, 9),
        Counts {
            heavy: setting(2, None),
            light: setting(4, None),
        }
    );
}

#[test]
fn a_counts_file_without_v_ignores_both_pools_with_one_line_each() {
    let path = v1::counts_path(&scratch("counts-without-v"));
    fs::write(&path, r#"{"heavy":3,"light":6}"#).expect("the counts file is written");
    let shown = path.display();

    assert_eq!(
        v1::read_counts(&path, 1),
        Counts {
            heavy: setting(1, Some(format!(r#"gate: ignored {shown}: missing key "v"; using 1 heavy slots"#))),
            light: setting(1, Some(format!(r#"gate: ignored {shown}: missing key "v"; using 1 light slots"#))),
        }
    );
}

#[test]
fn an_invalid_count_ignores_that_pool_only() {
    let path = v1::counts_path(&scratch("counts-invalid"));
    let shown = path.display();

    for value in ["3.0", "0", "65", r#""3""#, "null"] {
        fs::write(&path, format!(r#"{{"v":1,"heavy":{value},"light":6}}"#)).expect("the counts file is written");

        assert_eq!(
            v1::read_counts(&path, 9),
            Counts {
                heavy: setting(
                    2,
                    Some(format!(
                        r#"gate: ignored {shown}: "heavy" must be a whole number from 1 to 64; using 2 heavy slots"#
                    ))
                ),
                light: setting(6, None),
            },
            "heavy {value}"
        );
    }
}

#[test]
fn a_counts_key_in_another_case_is_ignored() {
    let path = v1::counts_path(&scratch("counts-case"));
    fs::write(&path, r#"{"v":1,"Heavy":3,"LIGHT":6}"#).expect("the counts file is written");

    assert_eq!(
        v1::read_counts(&path, 9),
        Counts {
            heavy: setting(2, None),
            light: setting(4, None),
        }
    );
}
