//! The gate slots' state as the gate dashboard shows it: each pool's counts, its slots, who holds them and who waits,
//! read from the gates' claim files, the slot counts file, a process snapshot and the slot mutexes. The process table,
//! the mutexes and process working directories come in through traits, so a daemon and the tests supply their own.
//! The counts and claim files are read as strict JSON: PowerShell's `ConvertFrom-Json` is more lenient, so a hand-edited
//! `{heavy: 4}` that gate.ps1 accepts shows here as a problem.

mod claims;
mod counts;
mod model;
mod state;

pub use counts::{WriteError, apply_slot_count, read_slot_count_setting, slot_count_path, write_slot_counts};
pub use model::{
    CwdReader, DashboardState, MutexProbe, ParseError, PoolState, ProcessInfo, ProcessTable, SlotCount, SlotCounts, SlotKind, SlotRow, SlotState,
    StateInputs, Waiting,
};
pub use state::dashboard_state;
