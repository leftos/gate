//! The gate slots' state as the Gates tab shows it: each pool's counts, its slots, who holds them and who waits,
//! read from the gates' claim files, the slot counts file, the gate's `remote.json`, a process snapshot and the slot
//! mutexes. The process table,
//! the mutexes and process working directories come in through traits, so a daemon and the tests supply their own.
//! The counts and claim files are read as strict JSON: PowerShell's `ConvertFrom-Json` is more lenient, so a hand-edited
//! `{heavy: 4}` that gate.ps1 accepts shows here as a problem. The gate's journal is read into its recent runs and the
//! held times per command through a [`JournalCache`]; the gate writes its v1 lines through [`journal_line`] and
//! [`append_line`], which rolls the journal over and archives the rolled file's release lines.

mod archive;
mod claims;
mod counts;
mod journal;
mod journal_writer;
mod model;
mod remote;
mod root;
mod state;
pub mod v1;
mod verdict;
#[cfg(all(windows, target_pointer_width = "64"))]
mod win32;

pub use archive::{ARCHIVE_DIR, archive_rolled_journal};
pub use counts::{ApplyError, WriteError, apply_slot_count, read_slot_count_setting, slot_count_path, write_slot_counts};
pub use journal::{CommandSummary, History, HistoryRun, JournalCache, RunState, normalize_command};
pub use journal_writer::{
    Appended, Held, JOURNAL_CAP_BYTES, JOURNAL_FILE, JOURNAL_WAIT, JournalEvent, JournalIdentity, JournalLock, JournalNote, LockError,
    ROLLED_JOURNAL_FILE, RemoteTag, RunFacts, append_line, journal_line,
};
pub use model::{
    CRITICAL_LIGHT_RESERVE, CriticalState, CwdReader, DashboardState, MutexProbe, ParseError, PoolState, QueueTicket, RemotePool, SlotCount,
    SlotCounts, SlotKind, SlotRow, SlotState, StateInputs, Waiting, reserved_light,
};
pub use root::{KernelObject, StateRoot, default_root, log_hash, normalize_root, root_hash};
pub use state::dashboard_state;
pub use verdict::Verdict;
#[cfg(all(windows, target_pointer_width = "64"))]
pub use win32::{LiveHistoryError, LiveInputsError, Win32CwdReader, Win32MutexProbe, live_history, live_inputs};
