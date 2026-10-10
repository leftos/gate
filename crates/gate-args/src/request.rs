//! What a parsed command line asks the gate to do.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::problem::Spelling;

/// The stall window when `--stall` is not given.
pub const DEFAULT_STALL_SECONDS: u64 = 120;
/// The log lines printed at the end when `--tail` is not given.
pub const DEFAULT_TAIL_LINES: u64 = 20;

/// What the gate was asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
#[expect(
    clippy::large_enum_variant,
    reason = "one invocation is read once a process and moved once; boxing the run request would save nothing"
)]
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

/// A valid `gate box`: the verb, the payload word as given, base64 of UTF-8 JSON that the binary decodes, and the
/// hidden seams given before the verb.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoxRequest {
    pub verb: BoxVerb,
    pub payload: OsString,
    pub seams: BoxSeams,
}

/// What the black-box suite sets on `gate box` through the hidden `--test-*` flags given before the verb; all `None` in
/// a gate built without the `test-seams` feature.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BoxSeams {
    /// The period of `gate box run`'s heartbeat, in milliseconds.
    pub heartbeat_ms: Option<u64>,
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
    /// Make the gathering of a kill's diagnostics fail, so the block is the heading and the failed line.
    pub diagnostics_fail: bool,
    /// The step file of the scripted remote client a routed run calls in place of the real one.
    pub remote_client: Option<PathBuf>,
    /// How long a routed run waits for a remote slot before it falls back with reason `full`.
    pub remote_wait_ms: Option<u64>,
    /// How long each git call of the routing decision may take before git is killed and the run goes ahead here.
    pub git_limit_ms: Option<u64>,
    /// The folder the watched command's build servers must live under, `None` for every process on the machine: in a
    /// suite whose cases run at once, it keeps one case's build server out of another's watch.
    pub servers_under: Option<PathBuf>,
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
    /// The form the command line was given in, so a line names a flag as the caller spelled it.
    pub spelling: Spelling,
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
