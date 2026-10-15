//! The state root's identity: whether a root is the machine's default, `%LOCALAPPDATA%\gate\v1`, and the names of the
//! kernel objects a gate under it uses. The default root keeps the PowerShell gate's names; every other root puts the
//! first 8 hex digits of the SHA-256 of its normalised path into each name, so a test root never touches the machine's
//! slots. Paths are compared as Windows compares them, but by string rules alone, so this module builds and tests on
//! every platform.

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

/// A kernel object a gate names under its state root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelObject {
    /// A heavy slot's mutex, by index.
    HeavySlot(u32),
    /// A light slot's mutex, by index.
    LightSlot(u32),
    /// A remote slot's mutex, by index.
    RemoteSlot(u32),
    /// The critical gates' mutex.
    Critical,
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
            KernelObject::Critical => "critical".to_owned(),
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
            KernelObject::HeavySlot(_) | KernelObject::LightSlot(_) | KernelObject::RemoteSlot(_) | KernelObject::Critical => {
                Some(format!("gate-{}.json", self.suffix()))
            }
            KernelObject::Journal
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
