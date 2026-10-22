//! Slot rows from claim files, the process table and the slot mutexes.

use std::fs;

use gate_state::{BuilderPool, DashboardState, RemotePool, SlotKind, SlotState};
use serde_json::{Value, json};

use crate::support::{HEAVY_GATE, Machine, START, START_CLOSE, START_FAR, claim, heavy_slot, pool, remote_claim};

#[test]
fn live_claim_is_busy_with_descendants_and_last_line() {
    let mut machine = Machine::new("live_claim_is_busy_with_descendants_and_last_line");
    let log = machine.dir.join("build.log");
    fs::write(&log, "first\r\nsecond line\r\n\r\n   \n").expect("the log can be written");
    let claim = claim(100, Some(log.to_str().expect("the log path is text")), "C:\\elsewhere");
    machine.write_claim("gate-slot-1.json", &claim.to_string());
    machine.add_process(100, 1, "pwsh.exe", None, Some(START_CLOSE));
    machine.add_process(101, 100, "cargo.exe", None, None);
    machine.add_process(102, 101, "rustc.exe", None, None);
    machine.add_process(103, 100, "conhost.exe", None, None);
    machine.add_process(200, 1, "unrelated.exe", None, None);
    machine.open.insert("gate-slot-1".to_owned());

    let row = heavy_slot(&machine, 1);

    assert_eq!(row.state, SlotState::Busy);
    assert_eq!(row.claim, Some(claim));
    assert_eq!(row.stale, None);
    assert_eq!(row.processes, 3);
    assert_eq!(row.last_line.as_deref(), Some("second line"));
    assert_eq!(row.extra, Some(false));
}

/// The heavy slot's last log line for a claim of pid 100 whose log holds `log`.
fn last_log_line(test: &str, log: &str) -> Option<String> {
    let mut machine = Machine::new(test);
    let path = machine.dir.join("build.log");
    fs::write(&path, log).expect("the log can be written");
    let claim = claim(100, Some(path.to_str().expect("the log path is text")), "C:\\elsewhere");
    machine.write_claim("gate-slot-1.json", &claim.to_string());
    machine.add_process(100, 1, "pwsh.exe", None, Some(START_CLOSE));
    machine.open.insert("gate-slot-1".to_owned());

    heavy_slot(&machine, 1).last_line
}

#[test]
fn stamped_log_last_line_is_the_text_after_the_tag() {
    let log = concat!(
        "2026-10-22T11:18:27.735525-07:00 gat gate pid=100 ppid=1\n",
        "2026-10-22T11:18:31.287046-07:00 out [main 58dcf94] docs: a change\n",
        "2026-10-22T11:18:31.287056-07:00 out  1 file changed, 2 insertions(+)\n",
    );

    assert_eq!(
        last_log_line("stamped_log_last_line_is_the_text_after_the_tag", log).as_deref(),
        Some(" 1 file changed, 2 insertions(+)")
    );
}

#[test]
fn stamped_log_last_line_passes_over_process_lines() {
    let log = concat!(
        "2026-10-22T11:18:31.287046-07:00 out [main 58dcf94] docs: a change\n",
        "2026-10-22T11:18:31.287056-07:00 out  1 file changed, 2 insertions(+)\n",
        "2026-10-22T11:18:31.287962-07:00 gat exit pid=10220 code=0 ran=3.418502s\n",
        "2026-10-22T11:18:31.288000-07:00 gat spawn pid=4 ppid=100 git.exe\n",
        "2026-10-22T11:18:31.289000-07:00 gat left pid=4 (still running at the end of the watch)\n",
        "2026-10-22T11:18:31.300000-07:00 out\n",
    );

    assert_eq!(
        last_log_line("stamped_log_last_line_passes_over_process_lines", log).as_deref(),
        Some(" 1 file changed, 2 insertions(+)")
    );
}

#[test]
fn stamped_log_last_line_keeps_a_gate_note() {
    let log = concat!(
        "2026-10-22T11:18:31.287046-07:00 out [main 58dcf94] docs: a change\n",
        "2026-10-22T11:18:31.287056-07:00 out  1 file changed, 2 insertions(+)\n",
        "2026-10-22T11:18:31.295548-07:00 gat gate: passed in 3 s\n",
    );

    assert_eq!(
        last_log_line("stamped_log_last_line_keeps_a_gate_note", log).as_deref(),
        Some("gate: passed in 3 s")
    );
}

#[test]
fn claim_with_mismatched_process_start_is_stale() {
    let mut machine = Machine::new("claim_with_mismatched_process_start_is_stale");
    let held = claim(100, None, "C:\\work");
    let free = claim(101, None, "C:\\work");
    machine.write_claim("gate-slot-0.json", &held.to_string());
    machine.write_claim("gate-slot-1.json", &free.to_string());
    machine.add_process(100, 1, "pwsh.exe", None, Some(START_FAR));
    machine.add_process(101, 1, "pwsh.exe", None, Some(START_FAR));
    machine.open.insert("gate-slot-0".to_owned());

    for (index, stale, state) in [(0, held, SlotState::Held), (1, free, SlotState::Free)] {
        let row = heavy_slot(&machine, index);
        assert_eq!(row.state, state, "slot {index}");
        assert_eq!(row.stale, Some(stale), "slot {index}");
        assert_eq!((row.claim, row.processes, row.last_line), (None, 0, None), "slot {index}");
    }
}

#[test]
fn open_mutex_without_claim_is_held() {
    let mut machine = Machine::new("open_mutex_without_claim_is_held");
    machine.open.insert("gate-light-slot-2".to_owned());
    let state = machine.state();

    for slot in &pool(&state, SlotKind::Light).slots {
        let expected = if slot.index == 2 { SlotState::Held } else { SlotState::Free };
        assert_eq!(slot.state, expected, "{}", slot.name);
        assert_eq!((&slot.claim, &slot.stale), (&None, &None), "{}", slot.name);
    }
    assert!(pool(&state, SlotKind::Heavy).slots.iter().all(|slot| slot.state == SlotState::Free));
}

#[test]
fn claim_index_past_count_is_extra() {
    let machine = Machine::new("claim_index_past_count_is_extra");
    machine.write_claim("gate-slot-5.json", &claim(100, None, "C:\\work").to_string());
    let state = machine.state();

    let heavy = pool(&state, SlotKind::Heavy);
    assert_eq!(heavy.count, 3);
    let rows: Vec<(u32, Option<bool>)> = heavy.slots.iter().map(|slot| (slot.index, slot.extra)).collect();
    assert_eq!(rows, [(0, Some(false)), (1, Some(false)), (2, Some(false)), (5, Some(true))]);
    let extra = heavy.slots.iter().find(|slot| slot.index == 5).expect("the extra row exists");
    assert_eq!(extra.name, "gate-slot-5");
    assert!(extra.stale.is_some());
}

#[test]
fn self_test_prefixed_claim_file_is_ignored() {
    let mut machine = Machine::new("self_test_prefixed_claim_file_is_ignored");
    machine.write_claim("xyz-gate-slot-0.json", &claim(100, None, "C:\\work").to_string());
    machine.add_process(100, 1, "pwsh.exe", None, Some(START));
    let state = machine.state();

    let heavy = pool(&state, SlotKind::Heavy);
    assert_eq!(heavy.slots.len(), 3);
    for slot in &heavy.slots {
        assert_eq!(slot.state, SlotState::Free, "{}", slot.name);
        assert_eq!((&slot.claim, &slot.stale), (&None, &None), "{}", slot.name);
    }
}

#[test]
fn unreadable_claim_is_stale_unreadable() {
    let machine = Machine::new("unreadable_claim_is_stale_unreadable");
    machine.write_claim("gate-slot-0.json", "{\"pid\": 1");

    let row = heavy_slot(&machine, 0);

    assert_eq!(row.state, SlotState::Free);
    assert_eq!(row.claim, None);
    let stale = row.stale.expect("an unreadable claim is stale");
    let object = stale.as_object().expect("the stale claim is an object");
    assert_eq!(object.keys().collect::<Vec<_>>(), ["unreadable"]);
    let message = object.get("unreadable").and_then(Value::as_str).expect("the reason is text");
    assert_ne!(message, "");
}

#[test]
fn claim_missing_pid_is_stale_unreadable() {
    let mut machine = Machine::new("claim_missing_pid_is_stale_unreadable");
    let mut claim = claim(100, None, "C:\\work");
    claim.as_object_mut().expect("a claim is an object").remove("pid");
    machine.write_claim("gate-slot-0.json", &claim.to_string());
    machine.add_process(100, 1, "pwsh.exe", None, Some(START));
    machine.open.insert("gate-slot-0".to_owned());

    let row = heavy_slot(&machine, 0);

    assert_eq!(row.state, SlotState::Held);
    assert_eq!(row.claim, None);
    assert_eq!(row.stale, Some(json!({"unreadable": "the claim has no pid that is a process id"})));
}

#[test]
fn relative_claim_log_resolves_against_claim_cwd() {
    let mut machine = Machine::new("relative_claim_log_resolves_against_claim_cwd");
    fs::create_dir_all(machine.dir.join("logs")).expect("the log folder can be made");
    fs::write(machine.dir.join("logs").join("run.log"), "building\ndone\n").expect("the log can be written");
    let cwd = machine.dir.to_str().expect("the test folder is text").to_owned();
    machine.write_claim("gate-slot-0.json", &claim(100, Some("logs/run.log"), &cwd).to_string());
    machine.add_process(100, 1, "pwsh.exe", None, Some(START));

    let row = heavy_slot(&machine, 0);

    assert_eq!(row.state, SlotState::Busy);
    assert_eq!(row.last_line.as_deref(), Some("done"));
}

#[test]
fn last_line_reads_only_the_final_64_kib() {
    let mut machine = Machine::new("last_line_reads_only_the_final_64_kib");
    let log = machine.dir.join("long.log");
    let line = "x".repeat(70_000);
    fs::write(&log, format!("earlier\n{line}\n")).expect("the log can be written");
    machine.write_claim(
        "gate-slot-0.json",
        &claim(100, Some(log.to_str().expect("the log path is text")), "C:\\work").to_string(),
    );
    machine.add_process(100, 1, "pwsh.exe", None, Some(START));

    let row = heavy_slot(&machine, 0);

    let tail = line.get(line.len() - 65_535..).expect("the line is longer than the tail");
    assert_eq!(
        row.last_line.as_deref(),
        Some(tail),
        "the final 65536 bytes are the line's last 65535 and its newline"
    );
}

fn remote_pool(machine: &Machine) -> RemotePool {
    machine.state().remote.expect("the state has a remote pool")
}

/// Each remote slot's index, state and `extra`.
fn remote_rows(pool: &RemotePool) -> Vec<(u32, SlotState, Option<bool>)> {
    pool.slots.iter().map(|slot| (slot.index, slot.state, slot.extra)).collect()
}

/// A `remote.json` with two `builders` entries: host's same file names `mac-1`, whose pool id is [`MAC_POOL`], and
/// `win-a`, which no pick sends a run to.
const BUILDERS_REMOTE: &str = include_str!("fixtures/remote-builders.json");
/// The pool id of `remote-builders.json`'s macOS builder `mac-1`.
const MAC_POOL: &str = "1f0efc62";
/// The pool id of `remote-builders.json`'s Windows builder `win-a`.
const WIN_POOL: &str = "aa6395b0";
/// A `remote.json` whose `hosts` entry `pin-a` is no `builders` entry, so only a run pinned to it reaches it.
const HOSTS_REMOTE: &str = r#"{"host": "build-a", "slots": 2, "repos": {"repo": ["cargo test *"]}, "hosts": ["pin-a"], "builders": [{"host": "mac-1", "os": "macos", "root": "/Users/builder", "routed": false}]}"#;
/// The pool id of `hosts`'s pinned-only builder `pin-a`.
const PIN_POOL: &str = "6893047c";
/// A `remote.json` the gate refuses: `mac-1`'s `slots` is 0, which refuses the whole file.
const REFUSED_REMOTE: &str = r#"{"host": "build-a", "slots": 2, "repos": {"repo": ["cargo test *"]}, "builders": [{"host": "mac-1", "os": "macos", "root": "/Users/builder", "slots": 0, "repos": {"repo": ["cargo test *"]}}, {"host": "win-a", "os": "windows", "slots": 1, "routed": false}]}"#;
/// A `remote.json` the gate refuses, which names the same builder twice in two letter cases.
const REFUSED_REPEATED: &str = r#"{"host": "build-a", "slots": 2, "repos": {"repo": ["cargo test *"]}, "builders": [{"host": "Mac-1", "os": "macos", "root": "/Users/builder", "slots": 0}, {"host": "mac-1", "os": "macos", "root": "/Users/builder", "slots": 1}]}"#;
/// A `remote.json` the gate refuses, whose `hosts` entry `pin-a` is no `builders` entry.
const REFUSED_HOSTS: &str = r#"{"host": "build-a", "slots": 2, "repos": {"repo": ["cargo test *"]}, "hosts": ["pin-a"], "builders": [{"host": "mac-1", "os": "macos", "root": "/Users/builder", "slots": 0}]}"#;
/// A `remote.json` the gate accepts for routing, whose `hosts` value it refuses: `pin-a` is a host and the empty string
/// is not.
const BAD_HOSTS_REMOTE: &str = r#"{"host": "build-a", "slots": 2, "repos": {"repo": ["cargo test *"]}, "hosts": ["pin-a", ""], "builders": [{"host": "mac-1", "os": "macos", "root": "/Users/builder", "routed": false}]}"#;

/// Each of a builder pool's slot rows: its index, state and `extra`.
fn builder_rows(pool: &BuilderPool) -> Vec<(u32, SlotState, Option<bool>)> {
    pool.slots.iter().map(|slot| (slot.index, slot.state, slot.extra)).collect()
}

/// The `index`th builder pool the state lists.
fn builder(state: &DashboardState, index: usize) -> &BuilderPool {
    state.builders.get(index).expect("the state lists the builder pool")
}

#[test]
fn a_builders_entry_shows_its_pool_with_its_own_slots() {
    let mut machine = Machine::new("a_builders_entry_shows_its_pool_with_its_own_slots");
    machine.write_remote(BUILDERS_REMOTE);
    let claim = remote_claim(100, 1);
    machine.write_claim(&format!("gate-remote-{MAC_POOL}-slot-1.json"), &claim.to_string());
    machine.add_process(100, 1, "pwsh.exe", None, Some(START));
    let state = machine.state();

    assert_eq!(state.builders.len(), 2, "the file's two builders entries");
    let mac = builder(&state, 0);
    assert_eq!(
        (mac.id.as_str(), mac.name.as_deref(), mac.host.as_deref(), mac.os.as_deref()),
        (MAC_POOL, Some("mac-1"), Some("mac-1"), Some("macos"))
    );
    assert_eq!((mac.count, mac.problem.as_deref(), mac.routed), (2, None, true));
    assert_eq!(builder_rows(mac), [(0, SlotState::Free, Some(false)), (1, SlotState::Busy, Some(false))]);
    let busy = mac.slots.get(1).expect("mac-1's slot 1 is listed");
    assert_eq!(busy.name, format!("gate-remote-{MAC_POOL}-slot-1"));
    assert_eq!(busy.claim, Some(claim));
    let win = builder(&state, 1);
    assert_eq!((win.id.as_str(), win.os.as_deref(), win.count), (WIN_POOL, Some("windows"), 1));

    let remote = state.remote.as_ref().expect("remote.json sizes host's pool");
    assert_eq!((remote.count, remote.problem.as_deref()), (2, None), "host's pool is as it was");
    assert_eq!(
        remote_rows(remote),
        [(0, SlotState::Free, Some(false)), (1, SlotState::Free, Some(false))]
    );
}

#[test]
fn a_builder_claim_past_its_count_is_an_extra_row() {
    let machine = Machine::new("a_builder_claim_past_its_count_is_an_extra_row");
    machine.write_remote(BUILDERS_REMOTE);
    machine.write_claim(&format!("gate-remote-{MAC_POOL}-slot-3.json"), &remote_claim(100, 3).to_string());

    let state = machine.state();
    let mac = builder(&state, 0);

    assert_eq!(
        builder_rows(mac),
        [
            (0, SlotState::Free, Some(false)),
            (1, SlotState::Free, Some(false)),
            (3, SlotState::Free, Some(true))
        ]
    );
    assert!(
        mac.slots.get(2).is_some_and(|slot| slot.stale.is_some()),
        "the dead gate's claim is stale"
    );
}

#[test]
fn a_builders_entry_with_routed_false_says_so() {
    let machine = Machine::new("a_builders_entry_with_routed_false_says_so");
    machine.write_remote(BUILDERS_REMOTE);
    let state = machine.state();

    let routed: Vec<(bool, Option<&str>)> = state.builders.iter().map(|pool| (pool.routed, pool.name.as_deref())).collect();
    assert_eq!(routed, [(true, Some("mac-1")), (false, Some("win-a"))]);
}

#[test]
fn a_pinned_only_hosts_entry_shows_only_while_it_holds_a_claim() {
    let machine = Machine::new("a_pinned_only_hosts_entry_shows_only_while_it_holds_a_claim");
    machine.write_remote(HOSTS_REMOTE);
    let state = machine.state();

    let names: Vec<Option<&str>> = state.builders.iter().map(|pool| pool.name.as_deref()).collect();
    assert_eq!(names, [Some("mac-1")], "a hosts entry without a claim is not listed");

    machine.write_claim(&format!("gate-remote-{PIN_POOL}-slot-0.json"), &remote_claim(100, 0).to_string());
    let state = machine.state();
    let pin = builder(&state, 1);

    assert_eq!(
        (pin.id.as_str(), pin.name.as_deref(), pin.host.as_deref(), pin.os.as_deref()),
        (PIN_POOL, Some("pin-a"), Some("pin-a"), Some("windows"))
    );
    assert_eq!(
        (pin.count, pin.routed, pin.problem.as_deref()),
        (2, false, None),
        "the top-level slots, no pick"
    );
    assert_eq!(
        builder_rows(pin),
        [(0, SlotState::Free, Some(false)), (1, SlotState::Free, Some(false))],
        "the top-level slots, with the claim on slot 0"
    );
}

#[test]
fn a_pooled_claim_no_entry_names_is_an_orphan_pool_last() {
    let machine = Machine::new("a_pooled_claim_no_entry_names_is_an_orphan_pool_last");
    machine.write_remote(r#"{"host": "build-a", "slots": 1, "repos": {"repo": ["cargo test *"]}}"#);
    machine.write_claim("gate-remote-0a1b2c3d-slot-2.json", &remote_claim(100, 2).to_string());
    let state = machine.state();

    let orphan = state.builders.last().expect("the orphan pool is listed");
    assert_eq!(
        (orphan.id.as_str(), orphan.name.as_deref(), orphan.host.as_deref(), orphan.os.as_deref()),
        ("0a1b2c3d", None, None, None)
    );
    assert_eq!((orphan.count, orphan.routed), (0, false));
    assert_eq!(orphan.problem.as_deref(), Some("no builders or hosts entry has pool id 0a1b2c3d"));
    assert_eq!(builder_rows(orphan), [(2, SlotState::Free, Some(true))]);
}

#[test]
fn a_remote_json_gate_refuses_lists_each_builder_with_gate_s_refusal() {
    let machine = Machine::new("a_remote_json_gate_refuses_lists_each_builder_with_gate_s_refusal");
    machine.write_remote(REFUSED_REMOTE);
    machine.write_claim(&format!("gate-remote-{MAC_POOL}-slot-2.json"), &remote_claim(100, 2).to_string());
    let reason = gate_router::Config::parse(REFUSED_REMOTE).expect_err("the gate refuses the entry's slots");
    let state = machine.state();

    assert_eq!(state.builders.len(), 2, "each entry of the refused file is still listed");
    for pool in &state.builders {
        assert_eq!(pool.problem.as_deref(), Some(reason.as_str()), "{}", pool.id);
        assert_eq!(pool.count, 0, "{}", pool.id);
    }
    assert_eq!(
        builder_rows(builder(&state, 0)),
        [(2, SlotState::Free, Some(true))],
        "the claim the pool id names is an extra row"
    );
    let remote = state.remote.as_ref().expect("the file still sizes host's pool");
    assert_eq!((remote.count, remote.problem.as_deref()), (2, None), "host's pool is read as it was");
    assert_eq!(
        remote_rows(remote),
        [(0, SlotState::Free, Some(false)), (1, SlotState::Free, Some(false))]
    );
}

#[test]
fn a_bad_hosts_value_still_names_its_pool_s_claims() {
    let machine = Machine::new("a_bad_hosts_value_still_names_its_pool_s_claims");
    machine.write_remote(BAD_HOSTS_REMOTE);
    machine.write_claim(&format!("gate-remote-{PIN_POOL}-slot-0.json"), &remote_claim(100, 0).to_string());
    let config = gate_router::Config::parse(BAD_HOSTS_REMOTE).expect("the gate accepts the file for routing");
    let problem = config.builders().expect_err("the gate refuses the hosts value");
    let state = machine.state();

    assert_eq!(
        state.builders.first().map(|pool| pool.name.as_deref()),
        Some(Some("mac-1")),
        "the builders entries are listed as on a good file"
    );
    let pin = state.builders.iter().find(|pool| pool.id == PIN_POOL).expect("pin-a's pool is listed");
    assert_eq!((pin.name.as_deref(), pin.host.as_deref()), (Some("pin-a"), Some("pin-a")));
    assert_eq!(pin.problem.as_deref(), Some(problem), "the gate's own wording for the hosts value");
    assert_eq!((pin.count, pin.routed), (2, false), "the top-level slots, no pick");
    assert_eq!(
        builder_rows(pin),
        [(0, SlotState::Free, Some(false)), (1, SlotState::Free, Some(false))],
        "the claim on slot 0 is stale, so both rows are free"
    );
}

#[test]
fn a_remote_json_that_is_not_json_says_so_on_every_builder_pool() {
    let machine = Machine::new("a_remote_json_that_is_not_json_says_so_on_every_builder_pool");
    machine.write_remote("{not json");
    machine.write_claim(&format!("gate-remote-{MAC_POOL}-slot-1.json"), &remote_claim(100, 1).to_string());
    let reason = gate_router::Config::parse("{not json").expect_err("the gate refuses text that is not JSON");
    let state = machine.state();

    assert_eq!(state.builders.len(), 1, "the pool the claim shows is listed");
    let mac = state.builders.first().expect("the claim's pool is listed");
    assert_eq!(mac.problem.as_deref(), Some(reason.as_str()), "the refusal, not the orphan text");
    assert_eq!(
        builder_rows(mac),
        [(1, SlotState::Free, Some(true))],
        "the claim is still shown as an extra row"
    );
}

#[test]
fn a_remote_json_that_cannot_be_read_says_so_on_every_builder_pool() {
    let machine = Machine::new("a_remote_json_that_cannot_be_read_says_so_on_every_builder_pool");
    // A folder where the file belongs is no NotFound: the file is there and cannot be read, as a locked one cannot.
    fs::create_dir_all(machine.dir.join("gate").join("remote.json")).expect("the folder can be made");
    machine.write_claim(&format!("gate-remote-{MAC_POOL}-slot-1.json"), &remote_claim(100, 1).to_string());
    machine.write_claim("gate-remote-0a1b2c3d-slot-0.json", &remote_claim(101, 0).to_string());
    let state = machine.state();

    assert_eq!(state.builders.len(), 2, "each pool the claims show is listed");
    for pool in &state.builders {
        let problem = pool.problem.as_deref().expect("an unreadable remote.json is a problem");
        let error = problem
            .strip_prefix("remote.json cannot be read: ")
            .expect("the line names the file and the read");
        assert!(!error.is_empty(), "{}: the line carries the error", pool.id);
        assert!(!error.contains('\n'), "{}: the error stays one line", pool.id);
    }
    let path = machine.dir.join("gate").join("remote.json");
    let remote = state.remote.as_ref().expect("an unreadable file still sizes host's pool");
    assert_eq!(remote.count, 0, "no slot of host's pool can be taken");
    let host_problem = remote.problem.as_deref().expect("an unreadable file is a problem");
    let line = format!("gate: ignored {}: it could not be read: ", path.display());
    let error = host_problem
        .strip_prefix(line.as_str())
        .and_then(|rest| rest.strip_suffix("; using no remote slots"))
        .expect("host's line names the file, the read and what it falls back to");
    assert!(!error.is_empty(), "host's line carries the error");
    assert!(!error.contains('\n'), "host's error stays one line");
    let mac = state
        .builders
        .iter()
        .find(|pool| pool.id == MAC_POOL)
        .expect("mac-1's claim names its pool");
    assert_eq!(
        builder_rows(mac),
        [(1, SlotState::Free, Some(true))],
        "the claim is still shown as an extra row"
    );
}

#[test]
fn a_builders_entry_a_hosts_entry_and_an_orphan_pool_are_in_order() {
    let machine = Machine::new("a_builders_entry_a_hosts_entry_and_an_orphan_pool_are_in_order");
    machine.write_remote(HOSTS_REMOTE);
    machine.write_claim(&format!("gate-remote-{PIN_POOL}-slot-0.json"), &remote_claim(100, 0).to_string());
    machine.write_claim("gate-remote-0a1b2c3d-slot-5.json", &remote_claim(101, 5).to_string());
    let state = machine.state();

    let order: Vec<(String, Option<&str>)> = state.builders.iter().map(|pool| (pool.id.clone(), pool.name.as_deref())).collect();
    assert_eq!(
        order,
        [
            (MAC_POOL.to_owned(), Some("mac-1")),
            (PIN_POOL.to_owned(), Some("pin-a")),
            ("0a1b2c3d".to_owned(), None),
        ],
        "remote.json's builders, then its hosts entry holding a claim, then the pooled claim no entry names"
    );
}

#[test]
fn a_refused_file_lists_one_pool_for_a_builder_it_repeats_in_another_letter_case() {
    let machine = Machine::new("a_refused_file_lists_one_pool_for_a_builder_it_repeats_in_another_letter_case");
    machine.write_remote(REFUSED_REPEATED);
    let state = machine.state();

    let names: Vec<Option<&str>> = state.builders.iter().map(|pool| pool.name.as_deref()).collect();
    assert_eq!(names, [Some("Mac-1")], "the first entry the refused file names, once");
}

#[test]
fn a_hosts_entry_of_a_refused_file_names_its_pool_s_claims() {
    let machine = Machine::new("a_hosts_entry_of_a_refused_file_names_its_pool_s_claims");
    machine.write_remote(REFUSED_HOSTS);
    machine.write_claim(&format!("gate-remote-{PIN_POOL}-slot-0.json"), &remote_claim(100, 0).to_string());
    let reason = gate_router::Config::parse(REFUSED_HOSTS).expect_err("the gate refuses the entry's slots");
    let state = machine.state();

    let pin = state.builders.iter().find(|pool| pool.id == PIN_POOL).expect("pin-a's pool is listed");
    assert_eq!((pin.name.as_deref(), pin.host.as_deref()), (Some("pin-a"), Some("pin-a")));
    assert_eq!(pin.problem.as_deref(), Some(reason.as_str()), "the refusal, not the orphan text");
    assert_eq!(builder_rows(pin), [(0, SlotState::Free, Some(true))]);
}

#[test]
fn no_builders_serialise_as_an_empty_list() {
    let machine = Machine::new("no_builders_serialise_as_an_empty_list");
    let state = machine.state();

    assert!(state.builders.is_empty(), "no remote.json names no builders");
    let value = serde_json::to_value(&state).expect("the state serialises");
    assert_eq!(
        value.get("builders"),
        Some(&json!([])),
        "builders is written, so a reader can tell the new shape from the old"
    );
}

#[test]
fn a_busy_builder_slot_s_pid_is_a_holder() {
    let mut machine = Machine::new("a_busy_builder_slot_s_pid_is_a_holder");
    machine.write_remote(BUILDERS_REMOTE);
    machine.write_claim(&format!("gate-remote-{MAC_POOL}-slot-1.json"), &remote_claim(300, 1).to_string());
    machine.add_process(300, 1, "pwsh.exe", Some(HEAVY_GATE), Some(START));
    machine.add_process(301, 1, "pwsh.exe", Some(HEAVY_GATE), Some(START));
    for index in 0..pool(&machine.state(), SlotKind::Heavy).count {
        machine.open.insert(format!("gate-slot-{index}"));
    }
    let state = machine.state();

    let waiting: Vec<u32> = pool(&state, SlotKind::Heavy).waiting.iter().map(|waiting| waiting.pid).collect();
    assert_eq!(waiting, [301], "the gate holding a builder's slot takes no slot of this PC's heavy pool");
    assert_eq!(
        builder(&state, 0).slots.get(1).map(|slot| slot.state),
        Some(SlotState::Busy),
        "the claim holds the builder's slot 1"
    );
}

#[test]
fn remote_claim_is_busy_in_the_remote_pool_and_in_no_other() {
    let mut machine = Machine::new("remote_claim_is_busy_in_the_remote_pool_and_in_no_other");
    machine.write_remote(r#"{"slots": 2}"#);
    let claim = remote_claim(100, 1);
    machine.write_claim("gate-remote-slot-1.json", &claim.to_string());
    machine.add_process(100, 1, "pwsh.exe", None, Some(START));
    machine.add_process(101, 100, "ssh.exe", None, None);
    machine.open.insert("gate-remote-slot-0".to_owned());
    let state = machine.state();

    let remote = state.remote.as_ref().expect("remote.json sizes a remote pool");
    assert_eq!((remote.count, remote.problem.as_deref()), (2, None));
    assert_eq!(
        remote_rows(remote),
        [(0, SlotState::Held, Some(false)), (1, SlotState::Busy, Some(false))]
    );
    let busy = remote.slots.get(1).expect("remote slot 1 is listed");
    assert_eq!((busy.name.as_str(), busy.processes), ("gate-remote-slot-1", 1));
    assert_eq!(busy.claim, Some(claim));
    for kind in [SlotKind::Heavy, SlotKind::Light] {
        assert!(
            pool(&state, kind)
                .slots
                .iter()
                .all(|slot| slot.state == SlotState::Free && !slot.name.contains("remote")),
            "{kind:?} holds no remote slot"
        );
    }
}

#[test]
fn remote_claim_past_the_count_is_extra() {
    let machine = Machine::new("remote_claim_past_the_count_is_extra");
    machine.write_remote(r#"{"slots": 1}"#);
    machine.write_claim("gate-remote-slot-3.json", &remote_claim(100, 3).to_string());

    let remote = remote_pool(&machine);

    assert_eq!(
        remote_rows(&remote),
        [(0, SlotState::Free, Some(false)), (3, SlotState::Free, Some(true))]
    );
    assert!(
        remote.slots.get(1).is_some_and(|slot| slot.stale.is_some()),
        "the dead gate's claim is stale"
    );
}

#[test]
fn no_remote_json_and_no_remote_claim_is_no_remote_pool() {
    let machine = Machine::new("no_remote_json_and_no_remote_claim_is_no_remote_pool");
    let state = machine.state();
    assert_eq!(state.remote, None);
    let value = serde_json::to_value(&state).expect("the state serialises");
    assert_eq!(value.get("remote"), Some(&Value::Null));
}

#[test]
fn remote_claim_without_remote_json_is_an_extra_slot_with_a_problem() {
    let machine = Machine::new("remote_claim_without_remote_json_is_an_extra_slot_with_a_problem");
    machine.write_claim("gate-remote-slot-0.json", &remote_claim(100, 0).to_string());

    let remote = remote_pool(&machine);

    assert_eq!(remote.count, 0);
    let path = machine.dir.join("gate").join("remote.json");
    assert_eq!(remote.problem, Some(format!("no remote.json at {}", path.display())));
    assert_eq!(remote_rows(&remote), [(0, SlotState::Free, Some(true))]);
}

#[test]
fn remote_json_slots_is_read_in_any_letter_case_up_to_16() {
    let machine = Machine::new("remote_json_slots_is_read_in_any_letter_case_up_to_16");
    machine.write_remote(r#"{"SLOTS": 16.0, "repos": {"x": ["dotnet test *"]}}"#);
    let remote = remote_pool(&machine);
    assert_eq!((remote.count, remote.problem), (16, None));
    assert_eq!(remote.slots.len(), 16);
}

#[test]
fn remote_json_without_slots_has_one_slot() {
    let machine = Machine::new("remote_json_without_slots_has_one_slot");
    for text in [r#"{"repos": {}}"#, "{}"] {
        machine.write_remote(text);
        let remote = remote_pool(&machine);
        assert_eq!((remote.count, remote.problem.as_deref()), (1, None), "{text}");
        assert_eq!(remote_rows(&remote), [(0, SlotState::Free, Some(false))], "{text}");
    }
}

#[test]
fn remote_json_that_cannot_be_used_is_a_problem() {
    let machine = Machine::new("remote_json_that_cannot_be_used_is_a_problem");
    let path = machine.dir.join("gate").join("remote.json");
    let ignored = |reason: &str| format!("gate: ignored {}: {reason}; using no remote slots", path.display());
    for (text, reason) in [
        (r#"{"slots": 0}"#, ignored("slots must be a whole number 1 to 16, got '0'")),
        (r#"{"slots": 17}"#, ignored("slots must be a whole number 1 to 16, got '17'")),
        (r#"{"slots": "2"}"#, ignored("slots must be a whole number 1 to 16, got '2'")),
        (r#"{"slots": 2.5}"#, ignored("slots must be a whole number 1 to 16, got '2.5'")),
        (r#"{"slots": null}"#, ignored("slots must be a whole number 1 to 16, got null")),
        ("[2]", ignored("it is not a JSON object")),
    ] {
        machine.write_remote(text);
        let remote = remote_pool(&machine);
        assert_eq!((remote.count, remote.problem.as_deref()), (0, Some(reason.as_str())), "{text}");
        assert!(remote.slots.is_empty(), "{text}");
    }
    machine.write_remote("{slots: 2}");
    let problem = remote_pool(&machine).problem.expect("malformed JSON is a problem");
    assert!(
        problem.starts_with(&format!("gate: ignored {}: it is not valid JSON: ", path.display())),
        "{problem}"
    );
}

#[test]
fn claim_exactly_two_seconds_off_is_not_live() {
    let mut machine = Machine::new("claim_exactly_two_seconds_off_is_not_live");
    let stale = claim(100, None, "C:\\work");
    machine.write_claim("gate-slot-0.json", &stale.to_string());
    machine.add_process(100, 1, "pwsh.exe", None, Some("2026-10-01T11:00:02.5Z"));

    let row = heavy_slot(&machine, 0);

    assert_eq!(row.state, SlotState::Free);
    assert_eq!((row.claim, row.stale), (None, Some(stale)));
}

/// A claim of a gate whose process started at `process_start`, which the snapshot may not hold.
fn claim_started_at(pid: u32, process_start: &str) -> Value {
    let mut claim = claim(pid, None, "C:\\work");
    claim["processStart"] = json!(process_start);
    claim
}

#[test]
fn a_claim_whose_gate_started_after_the_process_snapshot_is_busy() {
    let mut machine = Machine::new("a_claim_whose_gate_started_after_the_process_snapshot_is_busy");
    let claim = claim_started_at(100, "2026-10-01T11:00:01.5Z");
    machine.write_claim("gate-slot-1.json", &claim.to_string());
    machine.add_process(5, 1, "pwsh.exe", None, Some("2026-10-01T11:00:01Z"));
    machine.open.insert("gate-slot-1".to_owned());

    let row = heavy_slot(&machine, 1);

    assert_eq!(
        (row.state, row.claim.as_ref(), row.stale.as_ref()),
        (SlotState::Busy, Some(&claim), None),
        "the snapshot was taken before the gate started, so its claim is newer than the table"
    );
    assert_eq!(row.processes, 0, "the snapshot holds no descendants of a gate it does not hold");
}

#[test]
fn a_claim_younger_than_the_snapshot_on_a_closed_mutex_is_not_busy() {
    let mut machine = Machine::new("a_claim_younger_than_the_snapshot_on_a_closed_mutex_is_not_busy");
    let claim = claim_started_at(100, "2026-10-01T11:00:01.5Z");
    machine.write_claim("gate-slot-1.json", &claim.to_string());
    machine.add_process(5, 1, "pwsh.exe", None, Some("2026-10-01T11:00:01Z"));

    let row = heavy_slot(&machine, 1);

    assert_eq!(
        (row.state, row.claim.as_ref(), row.stale.as_ref()),
        (SlotState::Free, None, Some(&claim)),
        "a released mutex means the gate is gone, however new its claim is"
    );
}

#[test]
fn a_claim_older_than_the_snapshot_on_an_open_mutex_is_held_and_stale() {
    let mut machine = Machine::new("a_claim_older_than_the_snapshot_on_an_open_mutex_is_held_and_stale");
    let claim = claim(100, None, "C:\\work");
    machine.write_claim("gate-slot-1.json", &claim.to_string());
    machine.add_process(5, 1, "pwsh.exe", None, Some("2026-10-01T11:00:02Z"));
    machine.open.insert("gate-slot-1".to_owned());

    let row = heavy_slot(&machine, 1);

    assert_eq!(
        (row.state, row.claim.as_ref(), row.stale.as_ref()),
        (SlotState::Held, None, Some(&claim)),
        "the snapshot holds a process created after the claim's gate, so the claim is not newer than the table"
    );
}

#[test]
fn a_claim_started_with_the_snapshots_newest_process_is_held() {
    let mut machine = Machine::new("a_claim_started_with_the_snapshots_newest_process_is_held");
    let claim = claim_started_at(100, "2026-10-01T11:00:02Z");
    machine.write_claim("gate-slot-1.json", &claim.to_string());
    machine.add_process(5, 1, "pwsh.exe", None, Some("2026-10-01T11:00:02Z"));
    machine.open.insert("gate-slot-1".to_owned());

    let row = heavy_slot(&machine, 1);

    assert_eq!(
        (row.state, row.claim.as_ref(), row.stale.as_ref()),
        (SlotState::Held, None, Some(&claim)),
        "a claim started with the snapshot's newest process is not strictly later than it"
    );
}

#[test]
fn an_empty_snapshot_never_makes_a_claim_live() {
    let mut machine = Machine::new("an_empty_snapshot_never_makes_a_claim_live");
    let claim = claim(100, None, "C:\\work");
    machine.write_claim("gate-slot-1.json", &claim.to_string());
    machine.open.insert("gate-slot-1".to_owned());

    let row = heavy_slot(&machine, 1);

    assert_eq!(
        (row.state, row.claim.as_ref(), row.stale.as_ref()),
        (SlotState::Held, None, Some(&claim)),
        "a snapshot with no process start holds no gate whose claim is newer than it"
    );
}

#[cfg(windows)]
#[test]
fn a_claim_being_deleted_is_skipped() {
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::ffi::OsStrExt as _;
    use std::ptr;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, DELETE, FILE_ATTRIBUTE_NORMAL, FILE_DISPOSITION_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileDispositionInfo,
        OPEN_EXISTING, SetFileInformationByHandle,
    };

    let machine = Machine::new("a_claim_being_deleted_is_skipped");
    let path = machine.dir.join("gate").join("slots").join("gate-slot-1.json");
    fs::write(&path, claim(100, None, "C:\\work").to_string()).expect("the claim can be written");
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    // The classic mark-for-deletion-on-close, not the POSIX delete `fs::remove_file` takes on this Windows: it leaves
    // the claim's name in the folder and makes a later open of it fail with access denied, which is what a gate
    // deleting its claim as the state reads it does.
    // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the call, the security attributes and template
    // handle are null, and the access and share modes are the constants CreateFileW takes.
    let held = unsafe {
        CreateFileW(
            wide.as_ptr(),
            DELETE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    assert_ne!(held, INVALID_HANDLE_VALUE, "the claim can be opened for its delete");
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    let size = u32::try_from(size_of::<FILE_DISPOSITION_INFO>()).expect("the info fits in a u32");
    // SAFETY: `held` is the handle just opened with DELETE access, `info` is a FILE_DISPOSITION_INFO that outlives the
    // call, and `size` is its size in bytes.
    let set = unsafe { SetFileInformationByHandle(held, FileDispositionInfo, ptr::from_ref(&info).cast::<c_void>(), size) };
    assert_ne!(set, 0, "the claim's delete can be set: {}", io::Error::last_os_error());
    let error = fs::read(&path).expect_err("a claim whose delete is pending cannot be read");
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "the staging this test needs: {error}");

    let row = heavy_slot(&machine, 1);

    assert_eq!(
        (row.state, row.claim.as_ref(), row.stale.as_ref()),
        (SlotState::Free, None, None),
        "a claim its gate is deleting as the state reads it is going away, not stale"
    );
    // SAFETY: `held` is the handle this test opened and has not closed.
    unsafe { CloseHandle(held) };
}
