//! The formats `gate.exe` writes under a state root: claims, tickets and the counts file, read strictly, plus the count
//! formula and the paths they sit at.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use gate_state::v1::{self, Claim, ClaimKind, Counts, CoveredMarker, FormatError, PoolSetting, Ticket, TicketKind};
use gate_state::{KernelObject, SlotKind, reserved_light};
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
        remote: None,
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
fn reserved_light_is_two_or_the_whole_pool() {
    assert_eq!(reserved_light(1), 1);
    assert_eq!(reserved_light(2), 2);
    assert_eq!(reserved_light(5), 2);
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
