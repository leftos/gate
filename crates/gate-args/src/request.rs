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
    /// `gate push`.
    Push(PushRequest),
    /// `gate check`.
    Check(CheckRequest),
    /// `gate burn`.
    Burn(BurnRequest),
    /// `gate expect`.
    Expect(ExpectRequest),
    /// `gate warm`.
    Warm(WarmRequest),
    /// The hidden `gate box <verb> --stdin`.
    Box(BoxRequest),
}

/// A valid `gate box`: the verb and the hidden seams given before it. The request itself, base64 of UTF-8 JSON, is
/// stdin's first line, which the binary reads and decodes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoxRequest {
    pub verb: BoxVerb,
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
    /// Make `gate box run`'s start of its command with the restricted token fail as access denied, so it starts the
    /// command with its own token; `false` without the feature.
    pub refuse_token: bool,
    /// Make `gate box run` wait, just before it starts the command, until this path exists; `None` without the
    /// feature.
    pub park_launch: Option<PathBuf>,
    /// The outputs folder's size budget in bytes for `gate box run`, over the built-in 50 GiB; `None` without the
    /// feature.
    pub outputs_budget_bytes: Option<u64>,
    /// The most bytes `gate box inputs` unpacks from one put, decompressed, over the built-in 4 GiB; `None` without the
    /// feature.
    pub inputs_budget_bytes: Option<u64>,
    /// How long `gate box preflight`'s `dotnet --list-sdks` may take, in milliseconds, in place of the built-in 30 s;
    /// `None` without the feature.
    pub dotnet_limit_ms: Option<u64>,
    /// The period of `gate box run`'s samples of its machine, in milliseconds, in place of the built-in 5 s; `None`
    /// without the feature.
    pub sample_ms: Option<u64>,
    /// Make every sample `gate box run` takes of its machine fail; `false` without the feature.
    pub sampler_fail: bool,
    /// The share of its machine, in whole percent from 1 to 100, that `gate box run`'s samples find free, others
    /// holding the rest, in place of what the machine's times say; `None` without the feature.
    pub free_percent: Option<u32>,
    /// How many processes the ending of a job `gate box run`'s ceiling killed reports left, in place of what the job
    /// holds after it; `None` without the feature.
    pub kill_leaves: Option<u64>,
    /// How long `gate box preflight`'s tool probes may take together, in milliseconds, in place of the built-in 10 s;
    /// `None` without the feature.
    pub probe_limit_ms: Option<u64>,
}

/// What `gate box` was asked to do on the builder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoxVerb {
    Preflight,
    Run,
    Stop,
    Boot,
    Outputs,
    /// How many of the builder's work folders a run holds.
    Inflight,
    /// Which of a routed run's ignored inputs the work folder lacks, or their install.
    Inputs,
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
            BoxVerb::Inputs => "inputs",
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
    /// The file a local light gate waits for, holding the beside mutex, between journalling its take and writing its
    /// claim: it writes the claim once the file exists.
    pub beside_claim_wait: Option<PathBuf>,
    /// How long the remote client's `dotnet --list-sdks` of this PC may take before it is killed and the SDK list
    /// counts as unreadable, in place of 30 s.
    pub dotnet_limit_ms: Option<u64>,
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
    /// Where the remote client raises the console interrupt itself: `preflight`, `run`, `outputs` or `install`, after
    /// that call's first output byte (for `install`, its first moved file); `stop`, during the run call, so the stop it
    /// leads to can be interrupted again; `sdks`, before it reads this PC's SDK list; or `manifest`, before it reads the
    /// snapshot's `.gate/toolchain.toml`.
    pub remote_interrupt_at: Option<String>,
    /// The one call of the remote client, `preflight`, `push`, `run`, `ls-remote`, `fetch` or `outputs`, that
    /// `--test-stall-ms` applies to, and `--test-remote-timeout-ms` too when it names a call other than `run`; every
    /// other call keeps the run's own stall window and ceilings, and `ls-remote` its git limit unless named with a
    /// ceiling, which is then its deadline.
    pub remote_watch_call: Option<String>,
    /// With `--test-remote-watch-call` naming a call other than `run`, that call's transfer ceiling in milliseconds,
    /// counted from its own start.
    pub remote_timeout_ms: Option<u64>,
    /// The remote client's backstop on the run call in milliseconds, counted from the run call's start, in place of
    /// five ceilings and 60 s.
    pub remote_backstop_ms: Option<u64>,
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
    /// The most bytes one put of the remote client's inputs may total, decompressed, in place of 4 GiB: a set past it
    /// is not sent, and the run falls back `inputs`.
    pub remote_inputs_budget_bytes: Option<u64>,
    /// The most bytes of one remote call's output the remote client writes to the gate's log, in place of 256 MiB.
    pub remote_log_cap_bytes: Option<u64>,
    /// A change the remote client makes to its own folders at one point of its run, as `<point>:<step>`: before it
    /// holds the pair (`before-hold`), its first sibling's letter case changed (`case`), its first sibling replaced by a
    /// new folder (`replace`) or its worktree root replaced by one (`replace-root`); or once the main repository's
    /// snapshot is taken (`after-first-snapshot`), `core.worktree` written into its first sibling's git config, naming
    /// the folder beside it whose name is the sibling's with `-canary` after it (`worktree`).
    pub remote_tamper: Option<String>,
    /// Make this gate's own start unreadable both ways, so the run refuses before it writes anything.
    pub own_start_fails: bool,
    /// Count every live parent a slot marker names as one this gate runs inside the command of, so a marker naming the
    /// test process is trusted as a real parent gate's is.
    pub parent_job_trusted: bool,
    /// Let a process in the command's job create one outside it (`JOB_OBJECT_LIMIT_BREAKAWAY_OK`), so a case can stand
    /// a process that left the job in for a packaged app the command's descendant starts.
    pub job_breakaway: bool,
    /// How long `gate warm` waits for the key mutex and for the warm-up cap each, in milliseconds, in place of the
    /// warm-up file's ceiling.
    pub warm_wait_ms: Option<u64>,
}

/// A run's ceiling as the caller gave it, in whole seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ceiling {
    /// `--timeout <n>`: the ceiling is `n`.
    Given(u64),
    /// `--timeout auto:<n>`: a floor of `n`, which the command's learned durations may raise and never lower.
    Auto(u64),
}

impl Ceiling {
    /// The ceiling in whole seconds before any history raises it, which a run that takes no slot of its own runs
    /// under: the given `n`, or an auto ceiling's floor `n`.
    #[must_use]
    pub fn floor(self) -> u64 {
        match self {
            Ceiling::Given(seconds) | Ceiling::Auto(seconds) => seconds,
        }
    }
}

/// A valid `gate run`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunRequest {
    pub slot: Slot,
    pub timeout: Ceiling,
    /// The log path as given; a relative path is read against the working directory by the binary.
    pub log: PathBuf,
    pub stall: u64,
    pub tail: u64,
    pub no_markers: bool,
    pub local: bool,
    /// The `--bring-tracked` globs, in the order given: a fetched output git tracks is installed only when one matches it.
    pub bring_tracked: Vec<String>,
    /// The `--env` pairs, name and value, in the order given: set for the command over the environment it starts in and
    /// under the gate's own settings, on the PC and on a builder alike.
    pub env: Vec<(String, String)>,
    /// `--state-dir` as given.
    pub state_dir: Option<PathBuf>,
    /// `--cd` as given: the folder the run runs and routes from; a relative path is read against the working directory
    /// by the binary, after the log and the state root.
    pub cd: Option<PathBuf>,
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

/// A valid `gate push`, which takes no words: only the hidden seams the black-box suite gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushRequest {
    /// The hidden `--test-*` flags that set the push's ssh and bound.
    pub seams: PushSeams,
}

/// A valid `gate check`, which takes no words: the builder to check in place of this PC, and the hidden seams the
/// black-box suite gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckRequest {
    /// `--builder <host>` as given: the builder to check; `None` checks this PC.
    pub builder: Option<String>,
    /// The hidden `--test-*` flags that set the check's probe limit and the builder check's transport and bound.
    pub seams: CheckSeams,
}

/// What the black-box suite sets on `gate check` through its hidden `--test-*` flags; each field keeps its default,
/// `None`, in a gate built without the `test-seams` feature.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckSeams {
    /// How long the check's tool probes may take together, in milliseconds, in place of the built-in 10 s
    /// (`--test-probe-limit-ms`).
    pub probe_limit_ms: Option<u64>,
    /// The builder check's local transport, `local:<folder>` or `local-ssh:<folder>`, as a routed run's
    /// (`--test-remote-transport`).
    pub remote_transport: Option<String>,
    /// The program the local transport's call starts with ssh's own words (`--test-remote-ssh`).
    pub remote_ssh: Option<PathBuf>,
    /// How long the builder check's call may take, in milliseconds, in place of 120 s (`--test-remote-timeout-ms`).
    pub remote_timeout_ms: Option<u64>,
}

/// A valid `gate warm`, which takes no words: the folder to warm from, whether it runs in the background, the state
/// root, and the hidden seams the black-box suite gives it. The commands come only from the warm-up file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WarmRequest {
    /// `--cd` as given: a folder of the worktree to warm; a relative path is read against the working directory.
    pub cd: Option<PathBuf>,
    /// `--background`: say nothing of a warm-up that does not start, and exit 0 for it.
    pub background: bool,
    /// `--state-dir` as given.
    pub state_dir: Option<PathBuf>,
    /// The remote client's seams, as a routed run's, and the warm-up's own wait.
    pub test_seams: TestSeams,
}

/// A valid `gate burn`: the threads to spin, `None` for the machine's logical processors, and how many seconds to spin
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BurnRequest {
    pub cores: Option<u32>,
    pub seconds: u32,
}

/// A valid `gate expect`: the command whose expected time is asked for, the slot kind it would run in and the state root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpectRequest {
    /// `--slot` as given, `heavy` when it is not given: `critical` reads the critical gates' history, any other kind
    /// the history the heavy and light gates share.
    pub slot: Slot,
    /// `--state-dir` as given.
    pub state_dir: Option<PathBuf>,
    /// The words after `--`, unchanged.
    pub command: Vec<OsString>,
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
