//! A fake machine: a folder for the gate's files, a process table, open slot mutexes and working directories.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use gate_state::{CwdReader, DashboardState, Generation, MutexProbe, PoolState, SlotKind, StateInputs, dashboard_state};
use jiff::Timestamp;
use proc_table::{ProcessInfo, ProcessTable};
use serde_json::{Value, json};

/// The moment every state describes.
pub const NOW: &str = "2026-10-01T12:00:00.123456789Z";
/// The start time the claims record.
pub const START: &str = "2026-10-01T11:00:00.5000000Z";
/// 1.5 s after [`START`]: the same process.
pub const START_CLOSE: &str = "2026-10-01T11:00:02Z";
/// 3 s after [`START`]: another process that reused the pid.
pub const START_FAR: &str = "2026-10-01T11:00:03.5Z";
/// A waiting light gate's command line.
pub const LIGHT_GATE: &str = r"pwsh -NoProfile -File C:\Users\me\.claude\tools\gate\gate.ps1 -Log x -Slot light -- cargo test";
/// A critical gate's command line.
pub const CRITICAL_GATE: &str = r"pwsh -NoProfile -File C:\Users\me\.claude\tools\gate\gate.ps1 -Log x -Slot critical -- cargo test";
/// A heavy gate's command line, which a gate routed to the remote pool keeps.
pub const HEAVY_GATE: &str = r"pwsh -NoProfile -File C:\Users\me\.claude\tools\gate\gate.ps1 -Log x -Slot heavy -- cargo test";
/// The ssh destination the remote claims name.
pub const HOST: &str = "builder@build-box";
/// .NET ticks (100 ns since 0001-01-01) at the Unix epoch.
const UNIX_EPOCH_TICKS: i128 = 621_355_968_000_000_000;

pub fn ts(text: &str) -> Timestamp {
    text.parse().expect("a test timestamp parses")
}

/// A test timestamp as .NET ticks, the unit of a ticket's `arrived` and `startTicks`.
pub fn ticks(text: &str) -> i64 {
    i64::try_from(ts(text).as_nanosecond().div_euclid(100) + UNIX_EPOCH_TICKS).expect("a test moment fits in ticks")
}

/// A test timestamp as Unix epoch milliseconds, the unit of a v1 ticket's `arrived` and `startTicks`.
pub fn ms(text: &str) -> u64 {
    u64::try_from(ts(text).as_millisecond()).expect("a test moment is after the epoch")
}

pub struct Machine {
    pub dir: PathBuf,
    pub processes: Vec<ProcessInfo>,
    pub open: HashSet<String>,
    pub cwds: HashMap<u32, String>,
    /// The gate's test prefix, empty unless a test sets it.
    pub prefix: String,
}

impl Machine {
    /// A machine with an empty `gate\slots` folder under a fresh folder named after the test.
    pub fn new(test: &str) -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("gate-state").join(test);
        if dir.exists() {
            fs::remove_dir_all(&dir).expect("the test's old folder can be removed");
        }
        fs::create_dir_all(dir.join("gate").join("slots")).expect("the test's slots folder can be made");
        Self {
            dir,
            processes: Vec::new(),
            open: HashSet::new(),
            cwds: HashMap::new(),
            prefix: String::new(),
        }
    }

    /// A machine past the cutover: `gate\cutover.flag` set, and empty `gate\v1\slots` and `gate\v1\waiting` folders.
    pub fn v1(test: &str) -> Self {
        let machine = Self::new(test);
        for folder in ["slots", "waiting"] {
            fs::create_dir_all(machine.v1_root().join(folder)).expect("the test's v1 folders can be made");
        }
        machine.set_flag();
        machine
    }

    /// The inputs `live_inputs` would give on this machine: the generation follows the cutover flag.
    pub fn inputs(&self) -> StateInputs {
        StateInputs {
            now: ts(NOW),
            machine: "TESTBOX".to_owned(),
            processors: 16,
            local_app_data: self.dir.clone(),
            slot_counts_override: None,
            slot_prefix: self.prefix.clone(),
            generation: Generation::detect(&self.dir).expect("the cutover flag can be looked for"),
        }
    }

    /// gate.exe's default state root, `gate\v1`.
    pub fn v1_root(&self) -> PathBuf {
        self.dir.join("gate").join("v1")
    }

    /// Sets the cutover flag, `gate\cutover.flag`.
    pub fn set_flag(&self) {
        fs::write(self.dir.join("gate").join("cutover.flag"), "").expect("the cutover flag can be written");
    }

    /// gate.exe's counts file, `gate\v1\slot-counts.json`.
    pub fn v1_counts_path(&self) -> PathBuf {
        self.v1_root().join("slot-counts.json")
    }

    /// Writes a claim into `gate\v1\slots`.
    pub fn write_v1_claim(&self, file_name: &str, text: &str) {
        let dir = self.v1_root().join("slots");
        fs::create_dir_all(&dir).expect("the v1 slots folder can be made");
        fs::write(dir.join(file_name), text).expect("a v1 claim can be written");
    }

    /// Writes an arrival ticket into `gate\v1\waiting`.
    pub fn write_v1_ticket(&self, file_name: &str, text: &str) {
        let dir = self.v1_root().join("waiting");
        fs::create_dir_all(&dir).expect("the v1 ticket folder can be made");
        fs::write(dir.join(file_name), text).expect("a v1 ticket can be written");
    }

    pub fn state(&self) -> DashboardState {
        dashboard_state(&self.inputs(), self, self, self)
    }

    pub fn counts_path(&self) -> PathBuf {
        self.dir.join("gate").join("slot-counts.json")
    }

    pub fn write_counts(&self, text: &str) {
        fs::write(self.counts_path(), text).expect("the counts file can be written");
    }

    /// Writes the gate's `remote.json`, which sizes the remote pool.
    pub fn write_remote(&self, text: &str) {
        fs::write(self.dir.join("gate").join("remote.json"), text).expect("remote.json can be written");
    }

    /// Opens the mutex of every heavy slot within the default count of 3.
    pub fn hold_heavy_slots(&mut self) {
        for index in 0..3 {
            self.open.insert(format!("gate-slot-{index}"));
        }
    }

    pub fn write_claim(&self, file_name: &str, text: &str) {
        fs::write(self.dir.join("gate").join("slots").join(file_name), text).expect("a claim file can be written");
    }

    /// Writes an arrival ticket into `gate\<folder>`, such as `waiting`.
    pub fn write_ticket(&self, folder: &str, file_name: &str, text: &str) {
        let dir = self.dir.join("gate").join(folder);
        fs::create_dir_all(&dir).expect("the ticket folder can be made");
        fs::write(dir.join(file_name), text).expect("a ticket can be written");
    }

    pub fn add_process(&mut self, pid: u32, parent_pid: u32, name: &str, command_line: Option<&str>, created: Option<&str>) {
        self.processes.push(ProcessInfo {
            pid,
            parent_pid,
            name: name.to_owned(),
            command_line: command_line.map(str::to_owned),
            created: created.map(ts),
        });
    }

    /// Opens the mutex of every light slot from `first` to the last within the default count of 7.
    pub fn hold_light_slots(&mut self, first: u32) {
        for index in first..7 {
            self.open.insert(format!("gate-light-slot-{index}"));
        }
    }
}

impl ProcessTable for Machine {
    fn snapshot(&self) -> Vec<ProcessInfo> {
        self.processes.clone()
    }
}

impl MutexProbe for Machine {
    fn is_open(&self, name: &str) -> bool {
        self.open.contains(name)
    }
}

impl CwdReader for Machine {
    fn current_dir(&self, pid: u32) -> Option<String> {
        self.cwds.get(&pid).cloned()
    }
}

/// A claim as the gate writes it, with the gate's key order.
pub fn claim(pid: u32, log: Option<&str>, cwd: &str) -> Value {
    json!({
        "kind": "heavy",
        "index": 0,
        "pid": pid,
        "processStart": START,
        "started": START,
        "cwd": cwd,
        "command": "cargo test",
        "log": log,
        "timeoutSeconds": 600.0,
    })
}

/// A remote slot's claim as the gate writes it: a heavy claim's keys, then `remote`, `host` and `key`.
pub fn remote_claim(pid: u32, index: u32) -> Value {
    let mut claim = claim(pid, None, "C:\\work");
    let object = claim.as_object_mut().expect("a claim is an object");
    object.insert("index".to_owned(), json!(index));
    object.insert("remote".to_owned(), json!(true));
    object.insert("host".to_owned(), json!(HOST));
    object.insert("key".to_owned(), json!("work-3f2a9c"));
    claim
}

/// The arrival ticket of a gate routed to the remote pool.
pub fn remote_ticket(pid: u32, arrived: i64) -> Value {
    let mut ticket = ticket("heavy", pid, arrived, false);
    ticket["remote"] = json!(true);
    ticket
}

/// An arrival ticket as the gate writes it, with the gate's key order, for a process that started at [`START`].
pub fn ticket(kind: &str, pid: u32, arrived: i64, holds: bool) -> Value {
    json!({
        "kind": kind,
        "arrived": arrived,
        "pid": pid,
        "startTicks": ticks(START),
        "holds": holds,
    })
}

pub fn pool(state: &DashboardState, kind: SlotKind) -> &PoolState {
    state
        .pools
        .iter()
        .find(|pool| pool.kind == kind)
        .expect("the state has a pool of each kind")
}

/// The keys of a JSON object, in order.
pub fn keys(value: &Value) -> Vec<&str> {
    value
        .as_object()
        .expect("the value is a JSON object")
        .keys()
        .map(String::as_str)
        .collect()
}
