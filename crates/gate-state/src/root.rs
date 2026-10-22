//! The state root's identity: whether a root is the machine's default, `%LOCALAPPDATA%\gate\v1`, and the names of the
//! kernel objects a gate under it uses. The default root keeps the PowerShell gate's names; every other root puts the
//! first 8 hex digits of the SHA-256 of its normalised path into each name, so a test root never touches the machine's
//! slots. Paths are compared as Windows compares them, but by string rules alone, so this module builds and tests on
//! every platform.

use std::fmt;

use sha2::{Digest, Sha256};

const VERBATIM_PREFIX: &str = r"\\?\";
const VERBATIM_UNC_PREFIX: &str = r"\\?\UNC\";

/// The normalised form of an absolute root path: a leading `\\?\UNC\` read as `\\` and any other leading `\\?\`
/// removed, `/` read as `\`, lower-cased, and trailing separators removed.
#[must_use]
pub fn normalize_root(path: &str) -> String {
    let path = match path.get(..VERBATIM_UNC_PREFIX.len()) {
        Some(prefix) if prefix.eq_ignore_ascii_case(VERBATIM_UNC_PREFIX) => format!(r"\\{}", &path[VERBATIM_UNC_PREFIX.len()..]),
        _ => path.strip_prefix(VERBATIM_PREFIX).unwrap_or(path).to_owned(),
    };
    let normalised = path.replace('/', "\\").to_lowercase();
    normalised.trim_end_matches('\\').to_owned()
}

/// The root hash of a normalised root path: the first 8 hex digits, lower-case, of the SHA-256 of its UTF-8 bytes.
#[must_use]
pub fn root_hash(normalized: &str) -> String {
    const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(normalized.as_bytes());
    let mut hex = String::with_capacity(8);
    for byte in digest.iter().take(4) {
        hex.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
        hex.push(char::from(HEX_DIGITS[usize::from(byte & 0x0f)]));
    }
    hex
}

/// The log hash of an absolute log path: the first 16 hex digits of the SHA-256 of its normalised form
/// ([`normalize_root`]), as a number, which [`KernelObject::Log`] names its mutex by.
#[must_use]
pub fn log_hash(absolute_log: &str) -> u64 {
    let digest = Sha256::digest(normalize_root(absolute_log).as_bytes());
    let mut first = [0_u8; 8];
    first.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(first)
}

/// The key hash of a work-folder key: the first 16 hex digits of the SHA-256 of its lower-cased UTF-8 bytes, as a
/// number, which [`KernelObject::Routed`] names its mutex by.
#[must_use]
pub fn key_hash(key: &str) -> u64 {
    let digest = Sha256::digest(key.to_lowercase().as_bytes());
    let mut first = [0_u8; 8];
    first.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(first)
}

/// The default root for a `%LOCALAPPDATA%` value: `<LOCALAPPDATA>\gate\v1`.
#[must_use]
pub fn default_root(local_app_data: &str) -> String {
    format!(r"{}\gate\v1", local_app_data.trim_end_matches(['\\', '/']))
}

/// The id of a `builders` entry's own remote pool: the first 8 lower-case hex digits of the SHA-256 of the lower-cased
/// alias, as `gate_router::builder_id` computes it. `host`'s pool has none.
/// It shows as those 8 digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PoolId(u32);

impl PoolId {
    /// `text` as a pool id when it is exactly 8 lower-case hex digits; else `None`.
    #[must_use]
    pub fn parse(text: &str) -> Option<PoolId> {
        let lower_hex = text.len() == 8 && text.bytes().all(|digit| matches!(digit, b'0'..=b'9' | b'a'..=b'f'));
        if !lower_hex {
            return None;
        }
        u32::from_str_radix(text, 16).ok().map(PoolId)
    }
}

impl fmt::Display for PoolId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:08x}", self.0)
    }
}

impl fmt::Debug for PoolId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "PoolId({self})")
    }
}

/// A kernel object a gate names under its state root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelObject {
    /// A heavy slot's mutex, by index.
    HeavySlot(u32),
    /// A light slot's mutex, by index.
    LightSlot(u32),
    /// A slot's mutex of `host`'s remote pool, by index.
    RemoteSlot(u32),
    /// A slot's mutex of a `builders` entry's own remote pool, by the pool's id and the slot's index.
    PooledRemoteSlot { pool: PoolId, index: u32 },
    /// The critical gates' mutex.
    Critical,
    /// The mutex a light gate behind a critical request holds while it counts the light gates beside the request, takes
    /// its slot and writes its claim, and a critical gate holds while it counts the light gates and takes its light
    /// slots, so no light gate is between its take and its claim while another counts.
    CriticalBeside,
    /// The journal mutex.
    Journal,
    /// A waiting gate's wake event, by its pid and its process start in Unix epoch milliseconds.
    WakeEvent { pid: u32, start_ms: u64 },
    /// The job a gate runs its command in, by the gate's pid.
    Job { gate_pid: u32 },
    /// The mutex a running gate holds for its log, by the log's [`log_hash`], so two gates never write one log.
    Log { hash: u64 },
    /// The mutex a routed run holds from before its preflight to its end, by its work-folder key's [`key_hash`], so a
    /// second routed run of one worktree finds it taken and runs here.
    Routed { hash: u64 },
    /// The mutex a routed run holds around what its sync-back writes in a worktree, the main one's or a sibling's, by
    /// the worktree folder's id: its volume's serial number and its file id, so two spellings of one folder share it.
    Worktree { volume: u64, file: [u8; 16] },
}

impl KernelObject {
    fn suffix(self) -> String {
        match self {
            KernelObject::HeavySlot(index) => format!("slot-{index}"),
            KernelObject::LightSlot(index) => format!("light-slot-{index}"),
            KernelObject::RemoteSlot(index) => format!("remote-slot-{index}"),
            KernelObject::PooledRemoteSlot { pool, index } => format!("remote-{pool}-slot-{index}"),
            KernelObject::Critical => "critical".to_owned(),
            KernelObject::CriticalBeside => "critical-beside".to_owned(),
            KernelObject::Journal => "journal".to_owned(),
            KernelObject::WakeEvent { pid, start_ms } => format!("wake-{pid}-{start_ms}"),
            KernelObject::Job { gate_pid } => format!("job-{gate_pid}"),
            KernelObject::Log { hash } => format!("log-{hash:016x}"),
            KernelObject::Routed { hash } => format!("routed-{hash:016x}"),
            KernelObject::Worktree { volume, file } => format!("worktree-{volume:016x}-{:032x}", u128::from_be_bytes(file)),
        }
    }

    /// The file name of the claim a gate holding this object writes: the mutex name without `Local\` and without the
    /// root hash, plus `.json`. `None` for an object no gate claims (the journal mutex, a wake event, a job, a log's
    /// mutex).
    #[must_use]
    pub fn claim_file_name(self) -> Option<String> {
        match self {
            KernelObject::HeavySlot(_)
            | KernelObject::LightSlot(_)
            | KernelObject::RemoteSlot(_)
            | KernelObject::PooledRemoteSlot { .. }
            | KernelObject::Critical => Some(format!("gate-{}.json", self.suffix())),
            KernelObject::CriticalBeside
            | KernelObject::Journal
            | KernelObject::WakeEvent { .. }
            | KernelObject::Job { .. }
            | KernelObject::Log { .. }
            | KernelObject::Routed { .. }
            | KernelObject::Worktree { .. } => None,
        }
    }
}

/// A state root, identified by its normalised path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateRoot {
    normalized: String,
    hash: Option<String>,
}

impl StateRoot {
    /// The root at `absolute_path`, judged against `default`, the machine's default root ([`default_root`]) made
    /// absolute the same way; with no default (no `%LOCALAPPDATA%`) no root is the default.
    #[must_use]
    pub fn new(absolute_path: &str, default: Option<&str>) -> StateRoot {
        let normalized = normalize_root(absolute_path);
        let is_default = default.is_some_and(|default| normalize_root(default) == normalized);
        let hash = (!is_default).then(|| root_hash(&normalized));
        StateRoot { normalized, hash }
    }

    /// Whether this is the machine's default root, whose names are the PowerShell gate's.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.hash.is_none()
    }

    /// The root hash in every kernel object name, or `None` for the default root.
    #[must_use]
    pub fn hash(&self) -> Option<&str> {
        self.hash.as_deref()
    }

    /// The normalised path the default test and the hash are taken over.
    #[must_use]
    pub fn normalized(&self) -> &str {
        &self.normalized
    }

    /// The name of `object` under this root: `Local\gate-<name>` for the default root, `Local\gate-<h>-<name>`
    /// otherwise.
    #[must_use]
    pub fn object_name(&self, object: KernelObject) -> String {
        match &self.hash {
            None => format!(r"Local\gate-{}", object.suffix()),
            Some(hash) => format!(r"Local\gate-{hash}-{}", object.suffix()),
        }
    }
}
