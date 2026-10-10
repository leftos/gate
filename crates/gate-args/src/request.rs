//! What a parsed command line asks the gate to do.

use std::ffi::OsString;
use std::path::PathBuf;

/// The stall window when `--stall` is not given.
pub const DEFAULT_STALL_SECONDS: u64 = 120;
/// The log lines printed at the end when `--tail` is not given.
pub const DEFAULT_TAIL_LINES: u64 = 20;

/// What the gate was asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Invocation {
    /// `gate --version`.
    Version,
    /// `gate --help` or `gate help`.
    Help,
    /// `gate run ...`, or the compatibility layer's run form.
    Run(RunRequest),
    /// `gate stop-tree <pid>`, or the compatibility layer's `-StopTree <pid>`.
    StopTree(StopRequest),
    /// The hidden `gate box <verb> <payload>`.
    Box(BoxRequest),
}

/// A valid `gate box`: the verb, and the payload word as given, base64 of UTF-8 JSON that the binary decodes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoxRequest {
    pub verb: BoxVerb,
    pub payload: OsString,
}

/// What `gate box` was asked to do on the builder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoxVerb {
    Preflight,
    Run,
    Stop,
    Boot,
    Outputs,
}

impl BoxVerb {
    /// The verb as the CLI spells it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            BoxVerb::Preflight => "preflight",
            BoxVerb::Run => "run",
            BoxVerb::Stop => "stop",
            BoxVerb::Boot => "boot",
            BoxVerb::Outputs => "outputs",
        }
    }
}

/// A slot kind a run asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    Heavy,
    Light,
    Critical,
}

impl Slot {
    /// The kind as the CLI spells it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Slot::Heavy => "heavy",
            Slot::Light => "light",
            Slot::Critical => "critical",
        }
    }
}

/// What the black-box suite sets through the hidden `--test-*` flags: the timing knobs, in milliseconds, and the
/// interrupts the gate raises on itself; all `None` and off in a gate built without the `test-seams` feature.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TestSeams {
    pub sample_ms: Option<u64>,
    pub heartbeat_ms: Option<u64>,
    pub poll_ms: Option<u64>,
    pub stall_ms: Option<u64>,
    /// Raise the console interrupt after this many samples of the running command.
    pub interrupt_after_samples: Option<u64>,
    /// Raise the console interrupt once the gate has said it is waiting for a slot.
    pub interrupt_queued: bool,
    /// Make every sample of the running command fail, so the watch goes by wall time alone.
    pub sampler_fail: bool,
}

/// A valid `gate run`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunRequest {
    pub slot: Slot,
    pub timeout: u64,
    /// The log path as given; a relative path is read against the working directory by the binary.
    pub log: PathBuf,
    pub stall: u64,
    pub tail: u64,
    pub no_markers: bool,
    pub local: bool,
    /// `--state-dir` as given.
    pub state_dir: Option<PathBuf>,
    /// The words after `--`, unchanged.
    pub command: Vec<OsString>,
    pub test_seams: TestSeams,
    /// The hidden `--test-print-request` switch.
    pub print_request: bool,
}

/// A valid `gate stop-tree`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StopRequest {
    pub pid: u32,
    /// `--state-dir` as given.
    pub state_dir: Option<PathBuf>,
    /// The hidden `--test-print-request` switch.
    pub print_request: bool,
}
