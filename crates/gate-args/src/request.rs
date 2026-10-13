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
    /// `gate cutover`.
    Cutover(CutoverRequest),
    /// `gate remote-cutover`.
    RemoteCutover(RemoteCutoverRequest),
    /// `gate push`.
    Push(PushRequest),
    /// `gate burn`.
    Burn(BurnRequest),
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

/// What the black-box suite sets on `gate box` through the hidden `--test-*` flags given before the verb; each field
/// keeps its default, as its own line says, in a gate built without the `test-seams` feature.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BoxSeams {
    /// The period of `gate box run`'s heartbeat, in milliseconds; `None` without the feature.
    pub heartbeat_ms: Option<u64>,
    /// Make `gate box run` fail to give its command a UTF-8 console before the command starts, so it starts on the
    /// plain hidden console; `false` without the feature.
    pub fail_console: bool,
    /// Make `gate box run` wait, just before it starts the command, until this path exists; `None` without the
    /// feature.
    pub park_launch: Option<PathBuf>,
    /// The outputs folder's size budget in bytes for `gate box run`, over the built-in 50 GiB; `None` without the
    /// feature.
    pub outputs_budget_bytes: Option<u64>,
    /// How long `gate box preflight`'s `dotnet --list-sdks` may take, in milliseconds, in place of the built-in 30 s;
    /// `None` without the feature.
    pub dotnet_limit_ms: Option<u64>,
}

/// What `gate box` was asked to do on the builder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoxVerb {
    Preflight,
    Run,
    Stop,
    Boot,
    Outputs,
    /// How many of the builder's work folders a run holds, which the remote cutover's drain asks of every builder.
    Inflight,
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
            BoxVerb::Inflight => "inflight",
        }
    }
}

/// A slot kind a run asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    Heavy,
    Light,
    Critical,
    /// No slot: the run takes nothing from any pool and never waits.
    None,
}

impl Slot {
    /// The kind as the CLI spells it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Slot::Heavy => "heavy",
            Slot::Light => "light",
            Slot::Critical => "critical",
            Slot::None => "none",
        }
    }
}

/// What the black-box suite sets through the hidden `--test-*` flags: the timing knobs, in milliseconds, and the
/// interrupts the gate raises on itself; all `None` and off in a gate built without the `test-seams` feature.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each bool is a hidden flag of its own, set or not independently of the others"
)]
pub struct TestSeams {
    pub sample_ms: Option<u64>,
    pub heartbeat_ms: Option<u64>,
    pub poll_ms: Option<u64>,
    pub stall_ms: Option<u64>,
    /// How long a kill's diagnostics gathering may take before it is cut, in milliseconds, in place of the gate's own
    /// budget.
    pub diagnostics_budget_ms: Option<u64>,
    /// Raise the console interrupt after this many samples of the running command.
    pub interrupt_after_samples: Option<u64>,
    /// Raise the console interrupt once the gate has said it is waiting for a slot.
    pub interrupt_queued: bool,
    /// Raise the console interrupt once the gate has said it is waiting for a slot here, after the remote pool sent the
    /// run back with reason `full`, so the wait counts from the run's own queue line.
    pub interrupt_after_fallback: bool,
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
    /// How long the readers of a routing-decision git call that has exited may take to reach their pipes' ends before
    /// the call fails, in place of 2 s.
    pub git_drain_ms: Option<u64>,
    /// How long the remote client's `dotnet --list-sdks` of this PC may take before it is killed and the SDK list
    /// counts as unreadable, in place of 30 s.
    pub dotnet_limit_ms: Option<u64>,
    /// The folder the watched command's build servers must live under, `None` for every process on the machine: in a
    /// suite whose cases run at once, it keeps one case's build server out of another's watch.
    pub servers_under: Option<PathBuf>,
    /// One more image file name the watch reads as a compiler server's, as it reads `VBCSCompiler.exe`: a case's server
    /// that no PowerShell gate elsewhere on the machine classifies, and so none moves. Only with `servers_under`.
    pub server_image: Option<String>,
    /// Make a routed run stop once its client has made the snapshot and bridge commits and said them, with status 0
    /// and nothing pushed.
    pub remote_snapshot: bool,
    /// The transport of the Rust remote client a routed run calls, as given: `local:<folder>`, the builder's root a
    /// folder here and every ssh call this gate's own `box`.
    pub remote_transport: Option<String>,
    /// The program each ssh call of the local transport starts with ssh's own words, in place of this gate's `box`.
    pub remote_ssh: Option<PathBuf>,
    /// The pause before the one retry of a call that failed before the ssh banner, in milliseconds.
    pub remote_retry_pause_ms: Option<u64>,
    /// How long the remote client's `stop` call may take before its ssh is killed, in milliseconds.
    pub remote_bound_ms: Option<u64>,
    /// The call of the remote client, `preflight`, `run`, `outputs` or `install`, after whose first output byte (for
    /// `install`, its first moved file) the client raises the console interrupt itself; or `sdks`, before it reads this
    /// PC's SDK list.
    pub remote_interrupt_at: Option<String>,
    /// The one call of the remote client, `preflight`, `push`, `run`, `ls-remote` or `fetch`, that `--test-stall-ms`
    /// and `--test-remote-timeout-ms` apply to; every other call keeps the run's own stall window and ceiling, and
    /// `ls-remote` its git limit unless named with a ceiling, which is then its deadline.
    pub remote_watch_call: Option<String>,
    /// The remote client's ceiling in milliseconds, counted from the start of the call `--test-remote-watch-call`
    /// names, or of the run when it names none.
    pub remote_timeout_ms: Option<u64>,
    /// This PC's SDK list as the remote client's parity check reads it, in place of `dotnet --list-sdks`: versions
    /// separated by commas, or `none`.
    pub local_sdks: Option<String>,
    /// The most bytes the job of the remote client's sync-back fetch may write, in place of 4 GiB.
    pub remote_fetch_cap_bytes: Option<u64>,
    /// The most bytes the files a back commit of the remote client's sync-back writes may total, in place of 4 GiB.
    pub remote_apply_cap_bytes: Option<u64>,
    /// The box's outputs budget the remote client reads, in place of 50 GiB: the outputs fetch is capped at zstd's
    /// worst case for it, and the install unpacks its archive to it.
    pub remote_outputs_budget_bytes: Option<u64>,
    /// The most bytes of one remote call's output the remote client writes to the gate's log, in place of 256 MiB.
    pub remote_log_cap_bytes: Option<u64>,
    /// The script the remote.ps1 bridge dot-sources in place of `%USERPROFILE%\.claude\tools\gate\remote.ps1`.
    pub remote_script: Option<PathBuf>,
    /// The program the remote.ps1 bridge starts in place of the pwsh found on `PATH`.
    pub bridge_pwsh: Option<PathBuf>,
    /// Make this gate's own start unreadable both ways, so the run refuses before it writes anything.
    pub own_start_fails: bool,
    /// Count every live parent a slot marker names as one this gate runs inside the command of, so a marker naming the
    /// test process is trusted as a real parent gate's is.
    pub parent_job_trusted: bool,
    /// Let a process in the command's job create one outside it (`JOB_OBJECT_LIMIT_BREAKAWAY_OK`), so a case can stand
    /// a process that left the job in for a packaged app the command's descendant starts.
    pub job_breakaway: bool,
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
    /// The `--bring-tracked` globs, in the order given: a fetched output git tracks is installed only when one matches it.
    pub bring_tracked: Vec<String>,
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

/// A valid `gate cutover`, which takes no words: only the hidden switches the black-box suite gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CutoverRequest {
    /// The hidden `--test-print-request` switch.
    pub print_request: bool,
    /// The hidden `--test-*` flags that set the drain's timing and interrupt.
    pub seams: CutoverSeams,
}

/// What the black-box suite sets on `gate cutover` through its hidden `--test-*` flags; each field keeps its default,
/// `None` or off, in a gate built without the `test-seams` feature.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CutoverSeams {
    /// The drain's poll, in milliseconds (`--test-poll-ms`).
    pub poll_ms: Option<u64>,
    /// The period of the drain's still-waiting line, in milliseconds (`--test-heartbeat-ms`).
    pub heartbeat_ms: Option<u64>,
    /// How long every scan must find no PowerShell gate before the drain ends, in milliseconds (`--test-confirm-ms`).
    pub confirm_ms: Option<u64>,
    /// Raise the console interrupt right after the first list of the gates waited on is said
    /// (`--test-cutover-interrupt-after-list`).
    pub interrupt_after_list: bool,
}

/// A valid `gate remote-cutover`: the state root it drains, and the hidden seams the black-box suite gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteCutoverRequest {
    /// `--state-dir` as given.
    pub state_dir: Option<PathBuf>,
    /// The hidden `--test-*` flags that set the drain's timing, its interrupt and the way to the builders.
    pub seams: RemoteCutoverSeams,
}

/// What the black-box suite sets on `gate remote-cutover` through its hidden `--test-*` flags; each field keeps its
/// default, `None` or off, in a gate built without the `test-seams` feature.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteCutoverSeams {
    /// The drain's poll of this PC and the period of its builder rounds, in milliseconds (`--test-poll-ms`).
    pub poll_ms: Option<u64>,
    /// How long every scan of this PC must find no routed gate before its drain ends, in milliseconds
    /// (`--test-confirm-ms`).
    pub confirm_ms: Option<u64>,
    /// Raise the console interrupt right after the first list of the routed gates waited on is said
    /// (`--test-cutover-interrupt-after-list`).
    pub interrupt_after_list: bool,
    /// Raise the console interrupt once the first round of builder calls has answered, before the round is judged
    /// (`--test-cutover-interrupt-after-round`).
    pub interrupt_after_round: bool,
    /// How long one builder's `gate box inflight` call may take before it is killed, in milliseconds
    /// (`--test-remote-bound-ms`).
    pub bound_ms: Option<u64>,
    /// The transport the builders are asked through, as given: `local:<folder>` or `local-ssh:<folder>`
    /// (`--test-remote-transport`).
    pub transport: Option<String>,
    /// The program each builder's call runs in place of the ssh found along `PATH` (`--test-remote-ssh`).
    pub ssh: Option<PathBuf>,
}

/// A valid `gate push`, which takes no words: only the hidden seams the black-box suite gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushRequest {
    /// The hidden `--test-*` flags that set the push's ssh and bound.
    pub seams: PushSeams,
}

/// A valid `gate burn`: the threads to spin, `None` for the machine's logical processors, and how many seconds to spin
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BurnRequest {
    pub cores: Option<u32>,
    pub seconds: u32,
}

/// What the black-box suite sets on `gate push` through its hidden `--test-*` flags; each field keeps its default,
/// `None`, in a gate built without the `test-seams` feature.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PushSeams {
    /// The program each builder's call runs in place of the ssh found along `PATH` (`--test-push-ssh`).
    pub ssh: Option<PathBuf>,
    /// How long each builder's call may take before it is killed, in milliseconds (`--test-push-bound-ms`).
    pub bound_ms: Option<u64>,
}
