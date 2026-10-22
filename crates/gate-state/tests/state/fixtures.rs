//! The files `gate.ps1` writes, captured from a real run (`fixtures/README.md`), read the way the Gates tab reads them.

use std::fs;
use std::path::Path;

use gate_state::{CommandSummary, HistoryRun, JournalCache, RunState, SlotCount, SlotKind, SlotState, Waiting, dashboard_state};
use serde_json::Value;

use crate::support::{Machine, START, pool, remote_claim, ts};

/// A heavy slot's claim file, captured while its gate ran `-Log .tmp/o28-capture/first.log`.
const CLAIM: &str = include_str!("fixtures/claim.json");
/// The arrival ticket of a heavy gate waiting for the only heavy slot.
const TICKET: &str = include_str!("fixtures/ticket.json");
/// The counts file the capture ran under.
const SLOT_COUNTS: &str = include_str!("fixtures/slot-counts.json");
/// The journal of three light gates (statuses 0, 3 and 0) captured from an early gate.exe that launched no command and
/// took no slot, plus a hand-written abandoned one. A re-capture with `capture-journal.ps1` gives takes naming real
/// slots and non-zero held and wall times, so the figures `captured_journal_is_read_into_runs` expects change with it.
const JOURNAL: &str = include_str!("fixtures/journal.jsonl");
/// The PowerShell gate's journal of four light gates under one light slot, captured before the v1 format (no `v`), plus
/// a hand-written abandoned one.
const POWERSHELL_JOURNAL: &str = include_str!("fixtures/journal-powershell.jsonl");
/// The self-test prefix the PowerShell journal was captured under.
const POWERSHELL_PREFIX: &str = "ops72-";
/// A remote slot's claim file, written by hand from the design.
const REMOTE_CLAIM: &str = include_str!("fixtures/remote-claim.json");
/// The arrival ticket of a gate routed to the remote pool, written by hand from the design.
const REMOTE_TICKET: &str = include_str!("fixtures/remote-ticket.json");
/// The gate's remote settings: two slots, plus routing keys the state ignores.
const REMOTE_SETTINGS: &str = include_str!("fixtures/remote.json");
/// A hand-written `remote.json` with `host` and two `builders` entries, the macOS `mac-1` and the Windows `win-a`, which
/// no pick sends a run to.
const REMOTE_BUILDERS: &str = include_str!("fixtures/remote-builders.json");
/// The journal of nine routed and local gates captured under a stub remote: a remote run, a run the full pool sent back,
/// five runs refused after their remote take, one that fell back onto its parent's slot, and a local one.
const REMOTE_JOURNAL: &str = include_str!("fixtures/remote-journal.jsonl");
/// The journal of fifteen routed and local gate.exe runs captured with `capture-remote-run.ps1`: through the scripted
/// client, a remote run, a run the full pool sent back, five runs refused after their remote take, one that fell back
/// onto its parent's slot and a local one; then through the Rust client and fake-ssh, six runs it refused itself.
const REMOTE_RUN_JOURNAL: &str = include_str!("fixtures/remote-run-journal.jsonl");
/// The self-test prefix the capture ran under, in front of the claim's name and the waiting folder's.
const PREFIX: &str = "ops28-";
/// The self-test prefix the remote fixtures are read under.
const REMOTE_PREFIX: &str = "ops225-";
/// The routed gate's command line: a gate routed to the remote pool keeps `-Slot heavy`.
const ROUTED_GATE: &str =
    r"pwsh -NoProfile -File C:\Users\me\.claude\tools\gate\gate.ps1 -Log .tmp/r.log -TimeoutSeconds 900 -Slot heavy -- dotnet test";
/// The neutral working directory the fixtures name in place of the capture's, JSON-escaped as the files have it.
const NEUTRAL_CWD: &str = r"C:\\work\\repo";
/// The waiting gate's command line.
const WAITING_GATE: &str =
    r"pwsh -NoProfile -File C:\Users\me\.claude\tools\gate\gate.ps1 -Log .tmp/o28-capture/second.log -TimeoutSeconds 120 -Slot heavy -- pwsh";

/// A run's slot, remote flag, host, fallback reason and state.
type RunRow<'a> = (Option<u32>, bool, Option<&'a str>, Option<&'a str>, RunState);

fn count(value: u32) -> SlotCount {
    SlotCount::new(value).expect("a test count is 1 to 64")
}

/// A string field of a JSON object.
fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).expect("the field is a string")
}

#[test]
fn captured_claim_is_read_as_a_busy_slot_with_its_log() {
    let mut machine = Machine::new("captured_claim_is_read_as_a_busy_slot_with_its_log");
    machine.prefix = PREFIX.to_owned();
    let sandbox = machine.dir.to_str().expect("the test folder is text").to_owned();
    let escaped = serde_json::to_string(&sandbox).expect("a path serialises");
    let claim_text = CLAIM.replace(NEUTRAL_CWD, escaped.trim_matches('"'));
    let fixture: Value = serde_json::from_str(&claim_text).expect("the fixture is JSON");
    let log = text(&fixture, "log").to_owned();
    assert!(
        log.starts_with(text(&fixture, "cwd")),
        "the gate writes a relative -Log resolved against its cwd: {log}"
    );
    let log_dir = Path::new(&log).parent().expect("the log has a folder");
    fs::create_dir_all(log_dir).expect("the log folder can be made");
    fs::write(&log, "building\nfinished\n").expect("the log can be written");
    machine.write_claim("ops28-gate-slot-0.json", &claim_text);
    machine.add_process(51000, 1, "pwsh.exe", None, Some("2026-10-02T21:16:00.5711853Z"));

    let state = machine.state();
    let heavy = pool(&state, SlotKind::Heavy);
    let row = heavy.slots.iter().find(|slot| slot.index == 0).expect("the heavy pool has slot 0");

    assert_eq!(row.state, SlotState::Busy, "the claim's pid and processStart match the live gate");
    assert_eq!(row.stale, None);
    let claim = row.claim.as_ref().expect("a busy slot shows its claim");
    assert_eq!(claim.get("pid").and_then(Value::as_u64), Some(51000));
    assert_eq!(text(claim, "kind"), "heavy");
    assert_eq!(claim.get("index").and_then(Value::as_u64), Some(0));
    assert_eq!(text(claim, "processStart"), "2026-10-02T21:16:00.5711853Z");
    assert_eq!(text(claim, "started"), "2026-10-02T21:16:02.0157648Z");
    assert_eq!(text(claim, "command"), "pwsh -NoProfile -Command Start-Sleep 20");
    assert_eq!(text(claim, "log"), log);
    assert_eq!(claim.get("timeoutSeconds").and_then(Value::as_f64), Some(120.0));
    assert_eq!(row.last_line.as_deref(), Some("finished"), "the slot reads the log the claim names");
}

#[test]
fn captured_ticket_is_read_into_the_queue() {
    let mut machine = Machine::new("captured_ticket_is_read_into_the_queue");
    machine.prefix = PREFIX.to_owned();
    machine.write_ticket("ops28-waiting", "60080-639265725620427420.json", TICKET);
    machine.add_process(60080, 1, "pwsh.exe", Some(WAITING_GATE), Some("2026-10-02T21:16:02.0427420Z"));
    let mut inputs = machine.inputs();
    inputs.now = ts("2026-10-02T21:16:07.4458960Z");

    let state = dashboard_state(&inputs, &machine, &machine, &machine);

    assert_eq!(state.queue.len(), 1, "the ticket's gate is live");
    let ticket = state.queue.first().expect("one ticket");
    assert_eq!(ticket.kind, "heavy");
    assert_eq!(ticket.pid, 60080);
    assert!(!ticket.holds);
    assert_eq!(ticket.arrived, "2026-10-02T21:16:02.4458960Z");
    assert!((ticket.age_seconds - 5.0).abs() < f64::EPSILON, "age {}", ticket.age_seconds);
    assert_eq!(ticket.command_line.as_deref(), Some(WAITING_GATE));
}

/// `text`, a JSON object, with a reserved `expected` member added last.
fn with_expected(text: &str) -> String {
    let body = text.trim_end().strip_suffix('}').expect("the fixture is a JSON object");
    format!(r#"{body},"expected":{{"p50":12.5,"p90":30.25,"n":8,"key":"cargo test"}}}}"#)
}

#[test]
fn claim_and_ticket_accept_expected() {
    let mut machine = Machine::new("claim_and_ticket_accept_expected");
    machine.prefix = PREFIX.to_owned();
    machine.write_claim("ops28-gate-slot-0.json", &with_expected(CLAIM));
    machine.add_process(51000, 1, "pwsh.exe", None, Some("2026-10-02T21:16:00.5711853Z"));
    machine.write_ticket("ops28-waiting", "60080-639265725620427420.json", &with_expected(TICKET));
    machine.add_process(60080, 1, "pwsh.exe", Some(WAITING_GATE), Some("2026-10-02T21:16:02.0427420Z"));
    let mut inputs = machine.inputs();
    inputs.now = ts("2026-10-02T21:16:07.4458960Z");

    let state = dashboard_state(&inputs, &machine, &machine, &machine);

    let heavy = pool(&state, SlotKind::Heavy);
    let row = heavy.slots.iter().find(|slot| slot.index == 0).expect("the heavy pool has slot 0");
    assert_eq!(row.state, SlotState::Busy, "a claim carrying `expected` is read");
    let expected = row.claim.as_ref().and_then(|claim| claim.get("expected"));
    assert_eq!(expected.and_then(|expected| expected.get("n")).and_then(Value::as_u64), Some(8));
    let pids: Vec<u32> = state.queue.iter().map(|ticket| ticket.pid).collect();
    assert_eq!(pids, [60080], "a ticket carrying `expected` is read");
}

#[test]
fn remote_claim_is_read_as_a_busy_remote_slot_with_its_host() {
    let mut machine = Machine::new("remote_claim_is_read_as_a_busy_remote_slot_with_its_host");
    machine.prefix = REMOTE_PREFIX.to_owned();
    machine.write_remote(REMOTE_SETTINGS);
    machine.write_claim("ops225-gate-remote-slot-1.json", REMOTE_CLAIM);
    machine.add_process(52000, 1, "pwsh.exe", Some(ROUTED_GATE), Some("2026-10-02T21:16:00.5711853Z"));

    let state = machine.state();

    let remote = state.remote.as_ref().expect("remote.json sizes a remote pool");
    assert_eq!((remote.count, remote.problem.as_deref()), (2, None), "the routing keys are ignored");
    let names: Vec<&str> = remote.slots.iter().map(|slot| slot.name.as_str()).collect();
    assert_eq!(names, ["ops225-gate-remote-slot-0", "ops225-gate-remote-slot-1"]);
    let row = remote.slots.get(1).expect("remote slot 1 is listed");
    assert_eq!(row.state, SlotState::Busy);
    let claim = row.claim.as_ref().expect("a busy slot shows its claim");
    assert_eq!(text(claim, "host"), "builder@build-box");
    assert_eq!(text(claim, "key"), "repo-3f2a9c");
    assert_eq!(claim.get("remote"), Some(&Value::Bool(true)));
    assert!(
        pool(&state, SlotKind::Heavy).slots.iter().all(|slot| slot.state == SlotState::Free),
        "a remote claim is no heavy slot's"
    );
    assert_eq!(pool(&state, SlotKind::Heavy).waiting, Vec::<Waiting>::new());
}

#[test]
fn a_remote_json_with_builders_is_read_into_builder_pools() {
    let mut machine = Machine::new("a_remote_json_with_builders_is_read_into_builder_pools");
    machine.prefix = REMOTE_PREFIX.to_owned();
    machine.write_remote(REMOTE_BUILDERS);
    let claim = remote_claim(53000, 1);
    machine.write_claim(&format!("{REMOTE_PREFIX}gate-remote-1f0efc62-slot-1.json"), &claim.to_string());
    machine.add_process(53000, 1, "pwsh.exe", Some(ROUTED_GATE), Some(START));

    let state = machine.state();

    let pools: Vec<(Option<&str>, Option<&str>, bool)> = state
        .builders
        .iter()
        .map(|pool| (pool.name.as_deref(), pool.host.as_deref(), pool.routed))
        .collect();
    assert_eq!(
        pools,
        [(Some("mac-1"), Some("mac-1"), true), (Some("win-a"), Some("win-a"), false)],
        "the file's two builders entries, in its order"
    );
    let mac = state.builders.first().expect("mac-1's pool is listed");
    assert_eq!((mac.id.as_str(), mac.os.as_deref(), mac.count), ("1f0efc62", Some("macos"), 2));
    let busy = mac.slots.get(1).expect("mac-1's slot 1 is listed");
    assert_eq!(busy.state, SlotState::Busy, "the pooled claim holds it");
    let shown = busy.claim.as_ref().expect("a busy slot shows its claim");
    assert_eq!(text(shown, "host"), "builder@build-box");
    assert_eq!(shown.get("remote"), Some(&Value::Bool(true)));
    let remote = state.remote.as_ref().expect("the file sizes host's pool");
    assert_eq!((remote.count, remote.problem.as_deref()), (2, None), "host's pool is as it was");
}

#[test]
fn remote_ticket_is_read_into_the_queue() {
    let mut machine = Machine::new("remote_ticket_is_read_into_the_queue");
    machine.prefix = REMOTE_PREFIX.to_owned();
    machine.write_remote(REMOTE_SETTINGS);
    machine.write_ticket("ops225-waiting", "60090-639265725620427420.json", REMOTE_TICKET);
    machine.add_process(60090, 1, "pwsh.exe", Some(ROUTED_GATE), Some("2026-10-02T21:16:02.0427420Z"));
    let mut inputs = machine.inputs();
    inputs.now = ts("2026-10-02T21:16:07.4458960Z");

    let state = dashboard_state(&inputs, &machine, &machine, &machine);

    let ticket = state.queue.first().expect("the ticket's gate is live");
    assert_eq!((ticket.kind.as_str(), ticket.pid, ticket.remote), ("heavy", 60090, true));
    assert!(state.remote.is_some(), "remote.json sizes a remote pool");
}

#[test]
fn remote_journal_is_read_into_runs_and_a_split_summary() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("gate-state")
        .join("remote_journal_is_read_into_runs_and_a_split_summary");
    if dir.exists() {
        fs::remove_dir_all(&dir).expect("the test's old folder can be removed");
    }
    fs::create_dir_all(&dir).expect("the test's folder can be made");
    fs::write(dir.join("ops225-events.jsonl"), REMOTE_JOURNAL).expect("the journal can be written");
    let not_live = |_pid: u32, _process_start: &str| false;
    let last_segment = |cwd: &str| cwd.rsplit(['\\', '/']).next().map(str::to_owned);

    let history = JournalCache::default()
        .read(&dir, REMOTE_PREFIX, ts("2026-10-03T08:00:00Z"), not_live, last_segment)
        .expect("the journal can be read");

    let rows: Vec<RunRow<'_>> = history
        .runs
        .iter()
        .map(|run| (run.slot, run.remote, run.host.as_deref(), run.fallback.as_deref(), run.state))
        .collect();
    assert_eq!(
        rows,
        [
            (Some(0), false, None, None, RunState::Ok),
            (None, false, None, Some("busy"), RunState::Parent),
            (Some(0), false, None, Some("tools"), RunState::Ok),
            (Some(0), false, None, Some("rebooted"), RunState::Ok),
            (Some(0), false, None, Some("parity"), RunState::Failed),
            (Some(0), false, None, Some("busy"), RunState::Ok),
            (Some(0), false, None, Some("unreachable"), RunState::Ok),
            (Some(0), false, None, Some("full"), RunState::Ok),
            (Some(0), true, Some("builder@build-box"), None, RunState::Ok),
        ],
        "newest first: the local run, the run on its parent's slot, the runs that fell back on their local slot, the remote run"
    );
    let parent = &history.runs[1];
    assert_eq!(parent.parent_pid, Some(70000), "the run names the gate whose slot it ran on");
    assert_eq!(
        (parent.started.as_deref(), parent.status, parent.held_seconds),
        (None, None, None),
        "the parent's run is not in the journal, so there is no end to copy"
    );
    let full = &history.runs[7];
    assert_eq!(
        full.queued.as_deref(),
        Some("2026-10-03T06:00:06.5919052Z"),
        "a run the full pool sent back queues once"
    );
    assert_eq!(
        full.started.as_deref(),
        Some("2026-10-03T06:00:10.4672274Z"),
        "its local take is its start"
    );
    assert_eq!(full.waited_seconds, Some(3.875), "its wait counts from its queue");
    let unreachable = &history.runs[6];
    assert_eq!(
        unreachable.started.as_deref(),
        Some("2026-10-03T06:00:16.8420567Z"),
        "the local take after a remote one"
    );
    assert_eq!(unreachable.waited_seconds, Some(0.065), "the local take's wait, as the gate wrote it");
    let summary: Vec<(bool, usize, f64, f64, f64)> = history
        .summary
        .iter()
        .map(|row| (row.remote, row.count, row.p50_seconds, row.p90_seconds, row.max_seconds))
        .collect();
    assert_eq!(
        summary,
        [
            (false, 5, 1.622, 3.263, 3.263),
            (false, 1, 2.299, 2.299, 2.299),
            (true, 1, 12.558, 12.558, 12.558)
        ],
        "the remote runs of a command are a row of their own; the failed run and the run on its parent's slot are not counted"
    );
}

#[test]
fn remote_run_journal_from_gate_exe_is_read_into_runs_and_a_split_summary() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("gate-state")
        .join("remote_run_journal_from_gate_exe_is_read_into_runs_and_a_split_summary");
    if dir.exists() {
        fs::remove_dir_all(&dir).expect("the test's old folder can be removed");
    }
    fs::create_dir_all(&dir).expect("the test's folder can be made");
    fs::write(dir.join("events.jsonl"), REMOTE_RUN_JOURNAL).expect("the journal can be written");
    let not_live = |_pid: u32, _process_start: &str| false;
    let last_segment = |cwd: &str| cwd.rsplit(['\\', '/']).next().map(str::to_owned);

    let history = JournalCache::default()
        .read(&dir, "", ts("2026-10-12T08:00:00Z"), not_live, last_segment)
        .expect("the journal can be read");

    for line in REMOTE_RUN_JOURNAL.lines() {
        let line: Value = serde_json::from_str(line).expect("a journal line is JSON");
        assert_eq!(line.get("v").and_then(Value::as_u64), Some(1), "gate.exe writes the v1 format: {line}");
    }
    let rows: Vec<RunRow<'_>> = history
        .runs
        .iter()
        .map(|run| (run.slot, run.remote, run.host.as_deref(), run.fallback.as_deref(), run.state))
        .collect();
    assert_eq!(
        rows,
        [
            (Some(0), false, None, Some("version"), RunState::Ok),
            (Some(0), false, None, Some("busy"), RunState::Ok),
            (Some(0), false, None, Some("parity"), RunState::Ok),
            (Some(0), false, None, Some("tools"), RunState::Ok),
            (Some(0), false, None, Some("rebooted"), RunState::Ok),
            (Some(0), false, None, Some("unreachable"), RunState::Ok),
            (Some(0), false, None, None, RunState::Ok),
            (None, false, None, Some("busy"), RunState::Parent),
            (Some(0), false, None, Some("tools"), RunState::Ok),
            (Some(0), false, None, Some("rebooted"), RunState::Ok),
            (Some(0), false, None, Some("parity"), RunState::Failed),
            (Some(0), false, None, Some("busy"), RunState::Ok),
            (Some(0), false, None, Some("unreachable"), RunState::Ok),
            (Some(0), false, None, Some("full"), RunState::Ok),
            (Some(0), true, Some("builder"), None, RunState::Ok),
        ],
        "newest first: the Rust client's six refusals, the local run, the run on its parent's slot, the scripted client's \
         refusals and the full pool's fallback on their local slot, the remote run on the remote slot"
    );
    let parent = &history.runs[7];
    assert_eq!(parent.parent_pid, Some(70000), "the run names the gate whose slot it ran on");
    assert_eq!(
        (parent.started.as_deref(), parent.status, parent.held_seconds),
        (None, None, None),
        "the parent's run is not in the journal, so there is no end to copy"
    );
    let full = &history.runs[13];
    assert_eq!(
        full.queued.as_deref(),
        Some("2026-10-12T06:00:00.1970353Z"),
        "a run the full pool sent back queues once"
    );
    assert_eq!(
        full.started.as_deref(),
        Some("2026-10-12T06:00:01.2235979Z"),
        "its local take is its start"
    );
    assert_eq!(
        full.waited_seconds,
        Some(1.026),
        "its wait counts from the run's own queue line, so the remote pool's wait is in it"
    );
    assert_remote_takes_stand(&history.runs);
    let remote = &history.runs[14];
    assert_eq!(
        (remote.verdict.as_deref(), remote.wall_seconds, remote.adjusted_seconds),
        (Some("passed"), Some(1.31), None),
        "a remote run's release has its wall time and no adjusted time"
    );
    let summary: Vec<(&str, bool, usize, f64, f64, f64)> = history
        .summary
        .iter()
        .map(|row| {
            (
                row.command.as_str(),
                row.remote,
                row.count,
                row.p50_seconds,
                row.p90_seconds,
                row.max_seconds,
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (r#".\tool.exe out "ran here""#, false, 11, 0.035, 0.072, 0.117),
            (r#".\tool.exe out "ran here""#, true, 1, 1.312, 1.312, 1.312),
            ("cmd /c exit 0", false, 1, 0.2, 0.2, 0.2),
        ],
        "the remote run of a command is a row of its own and the runs that fell back count as local; the failed run and \
         the run on its parent's slot are not counted"
    );
}

/// Asserts what the fixture's fallbacks kept: the remote take stands for each run a fallback sent back after it took a
/// remote slot, and its start, wait and fallback time hold together. The runs are newest first.
fn assert_remote_takes_stand(runs: &[HistoryRun]) {
    let kept: Vec<bool> = runs.iter().map(|run| run.remote_take.is_some()).collect();
    assert_eq!(
        kept,
        [
            true, true, true, true, true, true, false, true, true, true, true, true, true, false, false
        ],
        "the remote take stands for each run a fallback sent back after it took one: the eleven refusals and the run on \
         its parent's slot; the local run, the run the full pool sent back before any remote take and the remote run keep none"
    );
    for run in runs {
        let Some(take) = &run.remote_take else {
            continue;
        };
        assert!(
            take.started <= take.fell_back_at,
            "the remote take begins before the fallback that ends it: {take:?}"
        );
        assert!(take.waited_seconds.is_some(), "gate.exe's remote take says how long it waited: {take:?}");
    }
}

#[test]
fn captured_slot_counts_set_both_pools() {
    let machine = Machine::new("captured_slot_counts_set_both_pools");
    machine.write_counts(SLOT_COUNTS);

    let state = machine.state();

    let heavy = pool(&state, SlotKind::Heavy);
    assert_eq!((heavy.count, heavy.default_count, heavy.set), (1, 3, Some(count(1))));
    assert_eq!(heavy.problem, None);
    assert_eq!(heavy.slots.len(), 1);
    let light = pool(&state, SlotKind::Light);
    assert_eq!((light.count, light.default_count, light.set), (2, 7, Some(count(2))));
    assert_eq!(light.problem, None);
    assert_eq!(light.slots.len(), 2);
}

#[test]
fn captured_journal_is_read_into_runs() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("gate-state")
        .join("captured_journal_is_read_into_runs");
    if dir.exists() {
        fs::remove_dir_all(&dir).expect("the test's old folder can be removed");
    }
    fs::create_dir_all(&dir).expect("the test's folder can be made");
    fs::write(dir.join("events.jsonl"), JOURNAL).expect("the journal can be written");
    let not_live = |_pid: u32, _process_start: &str| false;
    let last_segment = |cwd: &str| cwd.rsplit(['\\', '/']).next().map(str::to_owned);

    let history = JournalCache::default()
        .read(&dir, "", ts("2026-10-10T06:00:00Z"), not_live, last_segment)
        .expect("the journal can be read");

    let states: Vec<(Option<&str>, RunState)> = history.runs.iter().map(|run| (run.verdict.as_deref(), run.state)).collect();
    assert_eq!(
        states,
        [
            (Some("passed"), RunState::Ok),
            (Some("failed"), RunState::Failed),
            (Some("passed"), RunState::Ok),
            (None, RunState::Abandoned),
        ],
        "newest first: the second exit 0, exit 3, the first exit 0, then the hand-written abandoned gate"
    );
    assert_eq!(history.runs[2], captured_exit_0_row(), "the captured run took no slot");
    assert_eq!(history.runs[1], captured_exit_3_row());
    assert_eq!(history.runs[3].waited_seconds, Some(1.7), "the abandoned gate's wait");
    assert_eq!(
        history.summary,
        [CommandSummary {
            command: "cmd /c exit 0".to_owned(),
            repo: Some("repo".to_owned()),
            remote: false,
            count: 2,
            p50_seconds: 0.0,
            p90_seconds: 0.0,
            max_seconds: 0.0,
        }]
    );
}

/// The row of the captured run that ran `cmd /c exit 0`: it took no slot.
fn captured_exit_0_row() -> HistoryRun {
    HistoryRun {
        command: "cmd /c exit 0".to_owned(),
        cwd: Some(r"C:\work\repo".to_owned()),
        repo: Some("repo".to_owned()),
        kind: "light".to_owned(),
        slot: None,
        slots: Vec::new(),
        machine: None,
        remote: false,
        host: None,
        fallback: None,
        reason: None,
        pid: 3684,
        process_start: "2026-10-10T00:12:31.4670650Z".to_owned(),
        parent_pid: None,
        state: RunState::Ok,
        running: false,
        queued: Some("2026-10-10T00:12:31.4787619Z".to_owned()),
        started: Some("2026-10-10T00:12:31.4792208Z".to_owned()),
        waited_seconds: Some(0.0),
        held_seconds: Some(0.0),
        status: Some(0),
        verdict: Some("passed".to_owned()),
        wall_seconds: Some(0.0),
        adjusted_seconds: None,
        abandoned: Vec::new(),
        remote_take: None,
        cold: None,
    }
}

/// The row of the captured run that ran `cmd /c exit 3`, pid 71048: a different run, which took no slot either.
fn captured_exit_3_row() -> HistoryRun {
    HistoryRun {
        command: "cmd /c exit 3".to_owned(),
        cwd: Some(r"C:\work\repo".to_owned()),
        repo: Some("repo".to_owned()),
        kind: "light".to_owned(),
        slot: None,
        slots: Vec::new(),
        machine: None,
        remote: false,
        host: None,
        fallback: None,
        reason: None,
        pid: 71048,
        process_start: "2026-10-10T00:12:31.4858610Z".to_owned(),
        parent_pid: None,
        state: RunState::Failed,
        running: false,
        queued: Some("2026-10-10T00:12:31.4965943Z".to_owned()),
        started: Some("2026-10-10T00:12:31.4968993Z".to_owned()),
        waited_seconds: Some(0.0),
        held_seconds: Some(0.0),
        status: Some(3),
        verdict: Some("failed".to_owned()),
        wall_seconds: Some(0.0),
        adjusted_seconds: None,
        abandoned: Vec::new(),
        remote_take: None,
        cold: None,
    }
}

#[test]
fn powershell_journal_is_read_into_runs() {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("gate-state");
    let dir = root.join("powershell_journal_is_read_into_runs");
    if dir.exists() {
        fs::remove_dir_all(&dir).expect("the test's old folder can be removed");
    }
    fs::create_dir_all(&dir).expect("the test's folder can be made");
    fs::write(dir.join("ops72-events.jsonl"), POWERSHELL_JOURNAL).expect("the journal can be written");
    let not_live = |_pid: u32, _process_start: &str| false;
    let last_segment = |cwd: &str| cwd.rsplit(['\\', '/']).next().map(str::to_owned);
    let history = JournalCache::default()
        .read(&dir, POWERSHELL_PREFIX, ts("2026-10-03T06:00:00Z"), not_live, last_segment)
        .expect("the journal can be read");

    let states: Vec<(u32, RunState)> = history.runs.iter().map(|run| (run.slot.unwrap_or(u32::MAX), run.state)).collect();
    assert_eq!(
        states,
        [
            (u32::MAX, RunState::Abandoned),
            (0, RunState::Ok),
            (0, RunState::Ok),
            (0, RunState::Failed),
            (0, RunState::Ok),
        ],
        "newest first: the abandoned gate, the gate that waited, the sleep, exit 3, exit 0; ops72-gate-light-slot-0 is slot 0"
    );
    let exit_0 = HistoryRun {
        command: "cmd /c exit 0".to_owned(),
        cwd: Some(r"C:\work\repo".to_owned()),
        repo: Some("repo".to_owned()),
        kind: "light".to_owned(),
        slot: Some(0),
        slots: vec![r"Local\ops72-gate-light-slot-0".to_owned()],
        machine: None,
        remote: false,
        host: None,
        fallback: None,
        reason: None,
        pid: 40268,
        process_start: "2026-10-03T05:04:38.7424554Z".to_owned(),
        parent_pid: None,
        state: RunState::Ok,
        running: false,
        queued: Some("2026-10-03T05:04:39.1209178Z".to_owned()),
        started: Some("2026-10-03T05:04:39.1866333Z".to_owned()),
        waited_seconds: Some(0.065),
        held_seconds: Some(0.105),
        status: Some(0),
        verdict: None,
        wall_seconds: None,
        adjusted_seconds: None,
        abandoned: Vec::new(),
        remote_take: None,
        cold: None,
    };
    assert_eq!(history.runs[4], exit_0);
    assert_eq!(
        history.runs[3],
        HistoryRun {
            command: "cmd /c exit 3".to_owned(),
            pid: 43740,
            process_start: "2026-10-03T05:04:39.3588284Z".to_owned(),
            state: RunState::Failed,
            queued: Some("2026-10-03T05:04:39.6873680Z".to_owned()),
            started: Some("2026-10-03T05:04:39.7589416Z".to_owned()),
            waited_seconds: Some(0.071),
            held_seconds: Some(0.096),
            status: Some(3),
            ..exit_0
        },
        "on the same slot as exit 0"
    );
    assert_eq!(
        history.runs[1].waited_seconds,
        Some(8.12),
        "the gate that queued behind the sleep waited for it"
    );
    assert_eq!(
        history.summary,
        [
            CommandSummary {
                command: "cmd /c exit 0".to_owned(),
                repo: Some("repo".to_owned()),
                remote: false,
                count: 2,
                p50_seconds: 0.105,
                p90_seconds: 0.132,
                max_seconds: 0.132,
            },
            CommandSummary {
                command: "pwsh -NoProfile -Command Start-Sleep 10".to_owned(),
                repo: Some("repo".to_owned()),
                remote: false,
                count: 1,
                p50_seconds: 10.308,
                p90_seconds: 10.308,
                max_seconds: 10.308,
            },
        ]
    );
}
