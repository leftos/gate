//! The gate slots' state as the Gates tab shows it: each pool's counts, its slots, who holds them and who waits,
//! read from the gates' claim files, the slot counts file, the gate's `remote.json`, a process snapshot and the slot
//! mutexes. The process table,
//! the mutexes and process working directories come in through traits, so a daemon and the tests supply their own.
//! The counts and claim files are read as strict JSON: PowerShell's `ConvertFrom-Json` is more lenient, so a hand-edited
//! `{heavy: 4}` that gate.ps1 accepts shows here as a problem. The gate's journal is read into its recent runs and the
//! held times per command through a [`JournalCache`].

mod claims;
mod counts;
mod journal;
mod model;
mod remote;
mod state;
#[cfg(all(windows, target_pointer_width = "64"))]
mod win32;

pub use counts::{WriteError, apply_slot_count, read_slot_count_setting, slot_count_path, write_slot_counts};
pub use journal::{CommandSummary, History, HistoryRun, JournalCache, RunState, normalize_command};
pub use model::{
    CriticalState, CwdReader, DashboardState, MutexProbe, ParseError, PoolState, QueueTicket, RemotePool, SlotCount, SlotCounts, SlotKind, SlotRow,
    SlotState, StateInputs, Waiting,
};
pub use state::dashboard_state;
#[cfg(all(windows, target_pointer_width = "64"))]
pub use win32::{LiveHistoryError, LiveInputsError, Win32CwdReader, Win32MutexProbe, live_history, live_inputs};
