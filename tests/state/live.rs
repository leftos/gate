//! The live state against the user-level gate dashboard's `-Once` on this machine, both reading one sandboxed gate
//! folder under a test prefix.

use std::fs;
use std::os::windows::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use gate_state::{ProcessTable as _, Win32CwdReader, Win32MutexProbe, Win32ProcessTable, dashboard_state, live_inputs};
use jiff::Timestamp;
use serde_json::{Value, json};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Threading::CreateMutexW;

use crate::support::Machine;

/// `CREATE_NO_WINDOW`: the holder's console gets no window.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[test]
#[ignore = "compares against the user-level gate-dashboard.ps1; machine-wide state; run by name"]
fn live_matches_gate_dashboard_once() {
    let mut machine = Machine::new("live_matches_gate_dashboard_once");
    machine.prefix = format!("ops-tower-live-{}-{}-", std::process::id(), Timestamp::now().subsec_nanosecond());
    let idle = compare(&machine, "idle");
    assert_eq!(idle["pools"][0]["slots"][0]["state"], "free");

    // gate.ps1 holds `Local\<prefix>gate-slot-0` for heavy slot 0 and writes its claim to
    // `gate\slots\<prefix>gate-slot-0.json` (claims.rs `read_claims`), keys in the order of support.rs `claim`. The
    // claim names a sleeping child rather than this process, whose children include the dashboard while it runs.
    let holder = Holder::start();
    let _mutex = HeldMutex::create(&format!("{}gate-slot-0", machine.prefix));
    let created = Win32ProcessTable
        .snapshot()
        .into_iter()
        .find(|process| process.pid == holder.pid())
        .and_then(|process| process.created)
        .expect("the holder's start time can be read");
    let log = machine.dir.join("held.log");
    fs::write(&log, "first line\nlast line\n\n").expect("the holder's log can be written");
    let claim = json!({
        "kind": "heavy",
        "index": 0,
        "pid": holder.pid(),
        "processStart": iso(created),
        "started": iso(Timestamp::now()),
        "cwd": machine.dir.to_string_lossy(),
        "command": "cargo test",
        "log": log.to_string_lossy(),
        "timeoutSeconds": 600.0,
    });
    machine.write_claim(&format!("{}gate-slot-0.json", machine.prefix), &claim.to_string());
    let held = compare(&machine, "held");
    assert_eq!(held["pools"][0]["slots"][0]["state"], "busy");
    assert_eq!(held["pools"][0]["slots"][0]["lastLine"], "last line");
}

/// Both sides' state of the sandbox, compared once each is normalised; returns this side's.
fn compare(machine: &Machine, case: &str) -> Value {
    let theirs = dashboard_once(machine);
    let mut inputs = live_inputs().expect("this machine's inputs can be read");
    inputs.local_app_data.clone_from(&machine.dir);
    inputs.slot_prefix.clone_from(&machine.prefix);
    inputs.slot_counts_override = None;
    let ours = serde_json::to_value(dashboard_state(&inputs, &Win32ProcessTable, &Win32MutexProbe, &Win32CwdReader)).expect("the state serialises");
    let (ours, theirs) = (normalise(ours), normalise(theirs));
    assert_eq!(ours, theirs, "the {case} state differs from gate-dashboard.ps1 -Once");
    // `Value` equality ignores key order; the serialised text keeps it, as serde_json's `preserve_order` reads and
    // writes keys in document order.
    assert_eq!(
        serde_json::to_string(&ours).expect("our state serialises"),
        serde_json::to_string(&theirs).expect("their state serialises"),
        "the {case} state's key order differs from gate-dashboard.ps1 -Once"
    );
    ours
}

/// The claim date fields each slot row may carry, in its `claim` and its `stale`.
const CLAIM_DATES: [&str; 2] = ["processStart", "started"];

/// The value with `now` blanked, as each side reads the clock at its own moment, and every claim date in one canonical
/// form. gate-dashboard.ps1 re-prints a claim's dates through `ConvertFrom-Json` and `ConvertTo-Json`, which drops the
/// fraction's trailing zeros, while gate-state keeps the claim file's text on purpose; so the dates are compared as the
/// instants they name. A date that does not parse is left as it is, and the comparison fails on it. Every value is
/// replaced in place, so each key keeps its position.
fn normalise(mut state: Value) -> Value {
    state["now"] = Value::Null;
    if let Some(holder) = state.pointer_mut("/critical/holder") {
        canonical_claim_dates(holder);
    }
    if let Some(pools) = state.get_mut("pools").and_then(Value::as_array_mut) {
        for pool in pools {
            for slot in pool.get_mut("slots").and_then(Value::as_array_mut).into_iter().flatten() {
                canonical_claim_dates(slot);
            }
        }
    }
    state
}

/// Rewrites the row's claim dates that parse as instants into the instant's own text.
fn canonical_claim_dates(row: &mut Value) {
    for side in ["claim", "stale"] {
        for field in CLAIM_DATES {
            if let Some(date) = row.get_mut(side).and_then(|claim| claim.get_mut(field))
                && let Some(instant) = date.as_str().and_then(|text| text.parse::<Timestamp>().ok())
            {
                *date = Value::String(instant.to_string());
            }
        }
    }
}

fn dashboard_once(machine: &Machine) -> Value {
    let output = Command::new("pwsh")
        .args(["-NoProfile", "-File"])
        .arg(dashboard_script())
        .arg("-Once")
        .env("LOCALAPPDATA", &machine.dir)
        .env("GATE_TEST_SLOT_PREFIX", &machine.prefix)
        .env_remove("GATE_TEST_SLOT_COUNTS")
        .stdin(Stdio::null())
        .output()
        .expect("pwsh runs the gate dashboard");
    assert!(
        output.status.success(),
        "gate-dashboard.ps1 -Once failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = output.stdout.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&output.stdout);
    serde_json::from_slice(stdout).expect("gate-dashboard.ps1 -Once prints JSON")
}

/// `$env:CLAUDE_CONFIG_DIR`, else `~/.claude`, then `tools\gate\gate-dashboard.ps1`.
fn dashboard_script() -> PathBuf {
    let config = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|dir| !dir.is_empty()).map_or_else(
        || PathBuf::from(std::env::var_os("USERPROFILE").expect("USERPROFILE is set")).join(".claude"),
        PathBuf::from,
    );
    config.join("tools").join("gate").join("gate-dashboard.ps1")
}

/// A timestamp as .NET's round-trip format in UTC, as gate.ps1 writes `processStart`.
fn iso(timestamp: Timestamp) -> String {
    jiff::fmt::strtime::format("%Y-%m-%dT%H:%M:%S.%7fZ", timestamp).expect("a timestamp formats")
}

/// A sleeping child process the claim names, killed when dropped.
struct Holder(Child);

impl Holder {
    fn start() -> Self {
        let child = Command::new("ping")
            .args(["-n", "120", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("ping starts");
        Self(child)
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for Holder {
    #[expect(clippy::print_stderr, reason = "a drop cannot fail the test, so a holder left running is said on stderr")]
    fn drop(&mut self) {
        if let Err(error) = self.0.kill().and_then(|()| self.0.wait().map(drop)) {
            eprintln!("could not stop the holder process {}: {error}", self.0.id());
        }
    }
}

/// A named mutex this process holds open, closed when dropped.
struct HeldMutex(HANDLE);

impl HeldMutex {
    fn create(name: &str) -> Self {
        let wide: Vec<u16> = format!("Local\\{name}").encode_utf16().chain([0]).collect();
        // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the call; null attributes are allowed.
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
        assert!(!handle.is_null(), "the slot mutex {name} can be created");
        Self(handle)
    }
}

impl Drop for HeldMutex {
    fn drop(&mut self) {
        // SAFETY: the handle was open when the guard was made and is closed only here, once.
        unsafe { CloseHandle(self.0) };
    }
}
