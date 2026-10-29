//! The options both forms of the command line share, collected word by word into a request, with every problem kept.

use std::ffi::{OsStr, OsString};
use std::iter::Peekable;
use std::path::PathBuf;

use crate::problem::{Problem, Required, Spelling};
use crate::request::{
    BurnRequest, Ceiling, CheckRequest, CheckSeams, CutoverRequest, CutoverSeams, DEFAULT_STALL_SECONDS, DEFAULT_TAIL_LINES, DrainAction,
    DrainRequest, DrainSeams, ExpectRequest, PruneRequest, PruneSeams, PushRequest, PushSeams, RunRequest, Slot, StopRequest, TestSeams, WarmRequest,
};
use crate::values;

/// The values `--test-push-register` takes: each builder's registration succeeds, or fails as the box's seam refusing.
#[cfg(feature = "test-seams")]
const PUSH_REGISTER_RESULTS: &[&str] = &["ok", "fail"];
/// The largest send buffer `--test-remote-channel-send-buffer-bytes` takes: the 16 MiB a put's data channel gets
/// without it.
#[cfg(feature = "test-seams")]
const CHANNEL_SEND_BUFFER_MOST: u32 = 16 * 1024 * 1024;

/// The lookup a bare `--env NAME` word forwards `NAME`'s value from: the gate's own environment, unless a test built the
/// builder with one of its own.
#[derive(Clone, Copy)]
pub(crate) struct Forward(fn(&str) -> Option<OsString>);

impl Default for Forward {
    fn default() -> Forward {
        Forward(|name| std::env::var_os(name))
    }
}

/// An option that takes a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ValueOption {
    Slot,
    Timeout,
    Log,
    Stall,
    Tail,
    StateDir,
    Cd,
    Cores,
    Seconds,
    BringTracked,
    Env,
    Reason,
    Builder,
    #[cfg(feature = "test-seams")]
    SampleMs,
    #[cfg(feature = "test-seams")]
    LoadWindowMs,
    #[cfg(feature = "test-seams")]
    HeartbeatMs,
    #[cfg(feature = "test-seams")]
    PollMs,
    #[cfg(feature = "test-seams")]
    DrainPollMs,
    #[cfg(feature = "test-seams")]
    StallMs,
    #[cfg(feature = "test-seams")]
    DiagnosticsBudgetMs,
    #[cfg(feature = "test-seams")]
    InterruptAfterSamples,
    #[cfg(feature = "test-seams")]
    RemoteClient,
    #[cfg(feature = "test-seams")]
    RemoteWaitMs,
    #[cfg(feature = "test-seams")]
    KeyWaitMs,
    #[cfg(feature = "test-seams")]
    GitLimitMs,
    #[cfg(feature = "test-seams")]
    GitDrainMs,
    #[cfg(feature = "test-seams")]
    RestoreLimitMs,
    #[cfg(feature = "test-seams")]
    BesideClaimWait,
    #[cfg(feature = "test-seams")]
    BuiltHereKeyReached,
    #[cfg(feature = "test-seams")]
    KeyWaits,
    #[cfg(feature = "test-seams")]
    DotnetLimitMs,
    #[cfg(feature = "test-seams")]
    RemoteTransport,
    #[cfg(feature = "test-seams")]
    RemoteSsh,
    #[cfg(feature = "test-seams")]
    RemoteRetryPauseMs,
    #[cfg(feature = "test-seams")]
    RemoteBoundMs,
    #[cfg(feature = "test-seams")]
    RemoteInterruptAt,
    #[cfg(feature = "test-seams")]
    RemoteWatchCall,
    #[cfg(feature = "test-seams")]
    RemoteTimeoutMs,
    #[cfg(feature = "test-seams")]
    RemoteBackstopMs,
    #[cfg(feature = "test-seams")]
    RemoteSnapshotAgeMs,
    #[cfg(feature = "test-seams")]
    OutputsProgressMs,
    #[cfg(feature = "test-seams")]
    LocalSdks,
    #[cfg(feature = "test-seams")]
    RemoteFetchCapBytes,
    #[cfg(feature = "test-seams")]
    RemoteApplyCapBytes,
    #[cfg(feature = "test-seams")]
    RemoteOutputsBudgetBytes,
    #[cfg(feature = "test-seams")]
    RemoteInputsBudgetBytes,
    #[cfg(feature = "test-seams")]
    RemoteLfsBudgetBytes,
    #[cfg(feature = "test-seams")]
    RemoteLogCapBytes,
    #[cfg(feature = "test-seams")]
    RemoteTamper,
    #[cfg(feature = "test-seams")]
    TailscaleStatus,
    #[cfg(feature = "test-seams")]
    TailscaleDelayMs,
    #[cfg(feature = "test-seams")]
    TailscaleWaitMs,
    #[cfg(feature = "test-seams")]
    TailscaleHoldStartMs,
    #[cfg(feature = "test-seams")]
    RemoteChannelFault,
    #[cfg(feature = "test-seams")]
    RemoteChannelSendBufferBytes,
    #[cfg(feature = "test-seams")]
    ConfirmMs,
    #[cfg(feature = "test-seams")]
    PushSsh,
    #[cfg(feature = "test-seams")]
    PushBoundMs,
    #[cfg(feature = "test-seams")]
    PushRegister,
    #[cfg(feature = "test-seams")]
    ProbeLimitMs,
    #[cfg(feature = "test-seams")]
    ProbeDrainMs,
    #[cfg(feature = "test-seams")]
    CheckChannelMs,
    #[cfg(feature = "test-seams")]
    SdkLimitMs,
    #[cfg(feature = "test-seams")]
    NetfxKey,
    #[cfg(feature = "test-seams")]
    WarmWaitMs,
    #[cfg(feature = "test-seams")]
    DrainRaiseMs,
    #[cfg(feature = "test-seams")]
    PruneGoneAgeMs,
}

/// An option that is on or off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SwitchOption {
    NoMarkers,
    Local,
    Alone,
    Background,
    Lift,
    Status,
    Builders,
    Quiet,
    #[cfg(feature = "test-seams")]
    PrintRequest,
    #[cfg(feature = "test-seams")]
    InterruptQueued,
    #[cfg(feature = "test-seams")]
    InterruptBeforeKey,
    #[cfg(feature = "test-seams")]
    InterruptBeforeLater,
    #[cfg(feature = "test-seams")]
    InterruptAfterFallback,
    #[cfg(feature = "test-seams")]
    SamplerFail,
    #[cfg(feature = "test-seams")]
    StallFromOutput,
    #[cfg(feature = "test-seams")]
    InterruptFromOutput,
    #[cfg(feature = "test-seams")]
    OwnStartFails,
    #[cfg(feature = "test-seams")]
    RemoteHintFromPull,
    #[cfg(feature = "test-seams")]
    ParentJobTrusted,
    #[cfg(feature = "test-seams")]
    DiagnosticsFail,
    #[cfg(feature = "test-seams")]
    RemoteSnapshot,
    #[cfg(feature = "test-seams")]
    JobBreakaway,
    #[cfg(feature = "test-seams")]
    PruneSweep,
    #[cfg(feature = "test-seams")]
    CutoverInterruptAfterList,
    #[cfg(feature = "test-seams")]
    DrainInterruptAfterList,
    #[cfg(feature = "test-seams")]
    DrainInterruptBeforeRaise,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunOption {
    Value(ValueOption),
    Switch(SwitchOption),
}

/// The options read so far and the problems found so far.
#[derive(Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each bool is a switch of its own, set or not independently of the others"
)]
pub(crate) struct RunBuilder {
    slot: Option<Slot>,
    timeout: Option<Ceiling>,
    log: Option<PathBuf>,
    stall: Option<u64>,
    tail: Option<u64>,
    state_dir: Option<PathBuf>,
    cd: Option<PathBuf>,
    cores: Option<u32>,
    seconds: Option<u32>,
    no_markers: bool,
    local: bool,
    /// `--alone`, which only a critical run takes.
    alone: bool,
    /// The `--reason` text as given, which a local or critical run must give.
    reason: Option<String>,
    /// `gate warm`'s `--background`.
    background: bool,
    /// `gate drain`'s `--lift`.
    lift: bool,
    /// `gate drain`'s `--status`.
    status: bool,
    /// `gate drain`'s own seams, `--test-drain-raise-ms`, `--test-drain-interrupt-after-list` and
    /// `--test-drain-interrupt-before-raise`; its poll,
    /// still-waiting period and transport are read into `test_seams`, as a run's are.
    drain_seams: DrainSeams,
    /// `gate prune`'s `--builders`.
    builders: bool,
    /// `gate prune`'s `--quiet`.
    quiet: bool,
    /// `gate prune`'s `--test-prune-gone-age-ms`; its transport is read into `test_seams`, as a run's is.
    prune_gone_age_ms: Option<u64>,
    bring_tracked: Vec<String>,
    /// The `--env` pairs the gate can set, in the order given.
    env: Vec<(String, String)>,
    /// How many `--env` words the run has given, whether the pair was accepted or not: the position a word to forward is
    /// refused at.
    env_words: usize,
    /// Where a word to forward reads its value.
    forward: Forward,
    print_request: bool,
    test_seams: TestSeams,
    /// `gate cutover`'s own seams, `--test-confirm-ms` and `--test-cutover-interrupt-after-list`; its poll and
    /// still-waiting period are read into `test_seams`, as a run's are.
    cutover_seams: CutoverSeams,
    /// `gate push`'s own seams, `--test-push-ssh`, `--test-push-bound-ms` and `--test-push-register`.
    push_seams: PushSeams,
    /// `--builder <host>`: `gate check`'s builder, or the one builder a run is pinned to.
    builder: Option<String>,
    /// `gate check`'s own seams (`CheckSeams`); its builder seams are read into `test_seams`, as a run's are.
    check_seams: CheckSeams,
    given: Vec<RunOption>,
    pub(crate) problems: Vec<Problem>,
    /// The form the command line is read in: the CLI's unless [`RunBuilder::compat`] made the builder.
    spelling: Spelling,
}

impl RunBuilder {
    /// A builder of the compatibility layer's command line, whose `-TimeoutSeconds` takes a whole number alone.
    pub(crate) fn compat() -> RunBuilder {
        RunBuilder {
            spelling: Spelling::Compat,
            ..RunBuilder::default()
        }
    }

    /// Whether any option other than the hidden `--test-print-request` was given, valid or not.
    pub(crate) fn has_options(&self) -> bool {
        self.given.iter().any(|option| !is_print_request(*option))
    }

    /// Records `option` as given, or the problem of its being given twice; `--bring-tracked` and `--env` may be given any
    /// number of times.
    fn first_time(&mut self, option: RunOption, spelled: &str) -> bool {
        let repeatable = matches!(option, RunOption::Value(ValueOption::BringTracked | ValueOption::Env));
        if self.given.contains(&option) && !repeatable {
            self.problems.push(Problem::GivenTwice(spelled.to_owned()));
            return false;
        }
        self.given.push(option);
        true
    }

    /// The 1-based position of the run's next `--env` word, counting every word given, an empty one included.
    fn next_env_word(&mut self) -> usize {
        self.env_words += 1;
        self.env_words
    }

    /// Pushes the `--env` word `value` at the position of the run's next `--env` word, or the problem of a pair it
    /// holds, read from the builder's forward lookup.
    fn env_word(&mut self, value: &str) -> Result<(), Problem> {
        let position = self.next_env_word();
        let forward = self.forward.0;
        self.env.push(values::env_pair(value, position, forward)?);
        Ok(())
    }

    /// Reads `option`'s value, spelled `spelled` by the caller; an empty value is no value.
    pub(crate) fn value(&mut self, option: ValueOption, spelled: &str, value: &str) {
        if !self.first_time(RunOption::Value(option), spelled) {
            return;
        }
        if value.is_empty() {
            if option == ValueOption::Env {
                self.next_env_word();
            }
            self.problems.push(Problem::NeedsValue(spelled.to_owned()));
        } else if let Err(problem) = self.apply(option, spelled, value) {
            self.problems.push(problem);
        }
    }

    /// Reads `option`'s value from the next word, unless that word `is_option`. An unreadable value is reported once,
    /// as unreadable.
    pub(crate) fn value_from_next(
        &mut self,
        option: ValueOption,
        spelled: &str,
        words: &mut Peekable<impl Iterator<Item = OsString>>,
        is_option: fn(&str) -> bool,
    ) {
        match next_value(words, is_option) {
            Ok(Some(value)) => self.value(option, spelled, &value),
            Ok(None) => self.value(option, spelled, ""),
            Err(problem) => {
                if self.first_time(RunOption::Value(option), spelled) {
                    match option {
                        // An `--env` word that is not Unicode is refused by its position: the word may be a secret.
                        ValueOption::Env => {
                            let position = self.next_env_word();
                            self.problems.push(Problem::EnvWordNotUnicode(position));
                        }
                        _ => self.problems.push(problem),
                    }
                }
            }
        }
    }

    fn apply(&mut self, option: ValueOption, spelled: &str, value: &str) -> Result<(), Problem> {
        match option {
            ValueOption::Slot => self.slot = Some(values::slot(spelled, value)?),
            ValueOption::Timeout => self.timeout = Some(self.timeout_value(spelled, value)?),
            ValueOption::Log => self.log = Some(PathBuf::from(value)),
            ValueOption::Stall => self.stall = Some(values::above_zero(spelled, value)?),
            ValueOption::Tail => self.tail = Some(values::zero_or_above(spelled, value)?),
            ValueOption::StateDir => self.state_dir = Some(PathBuf::from(value)),
            ValueOption::Cd => self.cd = Some(PathBuf::from(value)),
            ValueOption::Cores => self.cores = Some(values::above_zero_u32(spelled, value)?),
            ValueOption::Seconds => self.seconds = Some(values::in_range(spelled, value, 1, values::MAX_BURN_SECONDS)?),
            ValueOption::BringTracked => self.bring_tracked.push(value.to_owned()),
            ValueOption::Env => self.env_word(value)?,
            ValueOption::Reason => self.reason = Some(Self::reason_value(spelled, value)?),
            ValueOption::Builder => self.builder = Some(value.to_owned()),
            #[cfg(feature = "test-seams")]
            ValueOption::SampleMs
            | ValueOption::LoadWindowMs
            | ValueOption::HeartbeatMs
            | ValueOption::PollMs
            | ValueOption::DrainPollMs
            | ValueOption::StallMs
            | ValueOption::DiagnosticsBudgetMs
            | ValueOption::InterruptAfterSamples
            | ValueOption::RemoteClient
            | ValueOption::RemoteWaitMs
            | ValueOption::KeyWaitMs
            | ValueOption::GitLimitMs
            | ValueOption::GitDrainMs
            | ValueOption::RestoreLimitMs
            | ValueOption::BesideClaimWait
            | ValueOption::BuiltHereKeyReached
            | ValueOption::KeyWaits
            | ValueOption::DotnetLimitMs
            | ValueOption::RemoteTransport
            | ValueOption::RemoteSsh
            | ValueOption::RemoteRetryPauseMs
            | ValueOption::RemoteBoundMs
            | ValueOption::RemoteInterruptAt
            | ValueOption::RemoteWatchCall
            | ValueOption::RemoteTimeoutMs
            | ValueOption::RemoteBackstopMs
            | ValueOption::RemoteSnapshotAgeMs
            | ValueOption::OutputsProgressMs
            | ValueOption::LocalSdks
            | ValueOption::RemoteFetchCapBytes
            | ValueOption::RemoteApplyCapBytes
            | ValueOption::RemoteOutputsBudgetBytes
            | ValueOption::RemoteInputsBudgetBytes
            | ValueOption::RemoteLfsBudgetBytes
            | ValueOption::RemoteLogCapBytes
            | ValueOption::RemoteTamper
            | ValueOption::TailscaleStatus
            | ValueOption::TailscaleDelayMs
            | ValueOption::TailscaleWaitMs
            | ValueOption::TailscaleHoldStartMs
            | ValueOption::RemoteChannelFault
            | ValueOption::RemoteChannelSendBufferBytes
            | ValueOption::ConfirmMs
            | ValueOption::PushSsh
            | ValueOption::PushBoundMs
            | ValueOption::PushRegister
            | ValueOption::ProbeLimitMs
            | ValueOption::ProbeDrainMs
            | ValueOption::CheckChannelMs
            | ValueOption::SdkLimitMs
            | ValueOption::NetfxKey
            | ValueOption::WarmWaitMs
            | ValueOption::DrainRaiseMs
            | ValueOption::PruneGoneAgeMs => self.apply_seam(option, spelled, value)?,
        }
        Ok(())
    }

    /// Sets the test seam `option` names from `value`. The options [`Self::apply`] sets itself are named here too, and
    /// set nothing, so each match names every option and a new one is placed in both.
    #[cfg(feature = "test-seams")]
    fn apply_seam(&mut self, option: ValueOption, spelled: &str, value: &str) -> Result<(), Problem> {
        let seams = &mut self.test_seams;
        match option {
            ValueOption::SampleMs => seams.sample_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::LoadWindowMs => seams.load_window_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::HeartbeatMs => seams.heartbeat_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::PollMs => seams.poll_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::DrainPollMs => seams.drain_poll_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::StallMs => seams.stall_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::DiagnosticsBudgetMs => seams.diagnostics_budget_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::InterruptAfterSamples => seams.interrupt_after_samples = Some(values::above_zero(spelled, value)?),
            ValueOption::RemoteClient => seams.remote_client = Some(PathBuf::from(value)),
            ValueOption::RemoteWaitMs => seams.remote_wait_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::KeyWaitMs => seams.key_wait_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::GitLimitMs => seams.git_limit_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::GitDrainMs => seams.git_drain_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::RestoreLimitMs => seams.restore_limit_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::BesideClaimWait => seams.beside_claim_wait = Some(PathBuf::from(value)),
            ValueOption::BuiltHereKeyReached => seams.built_here_key_reached = Some(PathBuf::from(value)),
            ValueOption::KeyWaits => seams.key_waits = Some(PathBuf::from(value)),
            ValueOption::DotnetLimitMs => seams.dotnet_limit_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::RemoteTransport => seams.remote_transport = Some(value.to_owned()),
            ValueOption::RemoteSsh => seams.remote_ssh = Some(PathBuf::from(value)),
            ValueOption::RemoteRetryPauseMs => seams.remote_retry_pause_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::RemoteBoundMs => seams.remote_bound_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::RemoteInterruptAt => {
                seams.remote_interrupt_at = Some(values::remote_call(spelled, value, values::INTERRUPTED_CALLS)?);
            }
            ValueOption::RemoteWatchCall => seams.remote_watch_call = Some(values::remote_call(spelled, value, values::WATCHED_CALLS)?),
            ValueOption::RemoteTimeoutMs => seams.remote_timeout_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::RemoteBackstopMs => seams.remote_backstop_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::RemoteSnapshotAgeMs => seams.remote_snapshot_age_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::OutputsProgressMs => seams.outputs_progress_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::LocalSdks => seams.local_sdks = Some(value.to_owned()),
            ValueOption::RemoteFetchCapBytes => seams.remote_fetch_cap_bytes = Some(values::above_zero(spelled, value)?),
            ValueOption::RemoteApplyCapBytes => seams.remote_apply_cap_bytes = Some(values::above_zero(spelled, value)?),
            ValueOption::RemoteOutputsBudgetBytes => seams.remote_outputs_budget_bytes = Some(values::above_zero(spelled, value)?),
            ValueOption::RemoteInputsBudgetBytes => seams.remote_inputs_budget_bytes = Some(values::above_zero(spelled, value)?),
            ValueOption::RemoteLfsBudgetBytes => seams.remote_lfs_budget_bytes = Some(values::above_zero(spelled, value)?),
            ValueOption::RemoteLogCapBytes => seams.remote_log_cap_bytes = Some(values::above_zero(spelled, value)?),
            ValueOption::RemoteTamper => seams.remote_tamper = Some(values::remote_call(spelled, value, values::TAMPERS)?),
            ValueOption::TailscaleStatus => seams.tailscale_status = Some(PathBuf::from(value)),
            ValueOption::TailscaleDelayMs => seams.tailscale_delay_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::TailscaleWaitMs => seams.tailscale_wait_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::TailscaleHoldStartMs => seams.tailscale_hold_start_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::RemoteChannelFault => {
                seams.remote_channel_fault = Some(values::remote_call(spelled, value, values::CHANNEL_FAULTS)?);
            }
            ValueOption::RemoteChannelSendBufferBytes => {
                seams.remote_channel_send_buffer_bytes = Some(values::in_range(spelled, value, 1, CHANNEL_SEND_BUFFER_MOST)?);
            }
            ValueOption::WarmWaitMs => seams.warm_wait_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::ConfirmMs => self.cutover_seams.confirm_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::PushSsh => self.push_seams.ssh = Some(PathBuf::from(value)),
            ValueOption::PushBoundMs => self.push_seams.bound_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::PushRegister => self.push_seams.register_ok = Some(values::remote_call(spelled, value, PUSH_REGISTER_RESULTS)? == "ok"),
            ValueOption::ProbeLimitMs => self.check_seams.probe_limit_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::ProbeDrainMs => self.check_seams.probe_drain_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::CheckChannelMs => self.check_seams.check_channel_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::SdkLimitMs => self.check_seams.sdk_limit_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::NetfxKey => self.check_seams.netfx_key = Some(value.to_owned()),
            ValueOption::DrainRaiseMs => self.drain_seams.raise_ms = Some(values::above_zero(spelled, value)?),
            ValueOption::PruneGoneAgeMs => self.prune_gone_age_ms = Some(values::zero_or_above(spelled, value)?),
            ValueOption::Slot
            | ValueOption::Timeout
            | ValueOption::Log
            | ValueOption::Stall
            | ValueOption::Tail
            | ValueOption::StateDir
            | ValueOption::Cd
            | ValueOption::Cores
            | ValueOption::Seconds
            | ValueOption::BringTracked
            | ValueOption::Env
            | ValueOption::Reason
            | ValueOption::Builder => {}
        }
        Ok(())
    }

    /// The `--timeout` ceiling: only the subcommand CLI's `--timeout` takes `auto:<n>`; `-TimeoutSeconds` stays a whole
    /// number.
    fn timeout_value(&self, spelled: &str, value: &str) -> Result<Ceiling, Problem> {
        match self.spelling {
            Spelling::Cli => values::ceiling(spelled, value),
            Spelling::Compat => values::above_zero(spelled, value).map(Ceiling::Given),
        }
    }

    /// The `--reason` text, or the problem of one that is blank.
    fn reason_value(spelled: &str, value: &str) -> Result<String, Problem> {
        if value.trim().is_empty() {
            return Err(Problem::EmptyReason(spelled.to_owned()));
        }
        Ok(value.to_owned())
    }

    /// Sets or clears a switch.
    pub(crate) fn switch(&mut self, option: SwitchOption, spelled: &str, on: bool) {
        if !self.first_time(RunOption::Switch(option), spelled) {
            return;
        }
        match option {
            SwitchOption::NoMarkers => self.no_markers = on,
            SwitchOption::Local => self.local = on,
            SwitchOption::Alone => self.alone = on,
            SwitchOption::Background => self.background = on,
            SwitchOption::Lift => self.lift = on,
            SwitchOption::Status => self.status = on,
            SwitchOption::Builders => self.builders = on,
            SwitchOption::Quiet => self.quiet = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::PrintRequest => self.print_request = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::InterruptQueued => self.test_seams.interrupt_queued = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::InterruptBeforeKey => self.test_seams.interrupt_before_key = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::InterruptBeforeLater => self.test_seams.interrupt_before_later = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::InterruptAfterFallback => self.test_seams.interrupt_after_fallback = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::SamplerFail => self.test_seams.sampler_fail = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::StallFromOutput => self.test_seams.stall_from_output = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::InterruptFromOutput => self.test_seams.interrupt_from_output = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::OwnStartFails => self.test_seams.own_start_fails = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::RemoteHintFromPull => self.test_seams.remote_hint_from_pull = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::ParentJobTrusted => self.test_seams.parent_job_trusted = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::DiagnosticsFail => self.test_seams.diagnostics_fail = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::RemoteSnapshot => self.test_seams.remote_snapshot = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::JobBreakaway => self.test_seams.job_breakaway = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::PruneSweep => self.test_seams.prune_sweep = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::CutoverInterruptAfterList => self.cutover_seams.interrupt_after_list = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::DrainInterruptAfterList => self.drain_seams.interrupt_after_list = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::DrainInterruptBeforeRaise => self.drain_seams.interrupt_before_raise = on,
        }
    }

    /// The run request, or every problem: those found while reading, then the test seams that cannot work together,
    /// then `--alone` with a slot other than critical, then a local or critical run's missing reason, then each missing
    /// input in the order slot, timeout, log, command.
    pub(crate) fn finish_run(mut self, command: Vec<OsString>) -> Result<RunRequest, Vec<Problem>> {
        let spelling = self.spelling;
        #[cfg(feature = "test-seams")]
        self.check_remote_seams();
        self.check_env();
        if self.alone && self.slot.is_some_and(|slot| slot != Slot::Critical) {
            self.problems.push(Problem::AloneNeedsCritical(spelling));
        }
        self.check_builder_routes();
        if !self.given.contains(&RunOption::Value(ValueOption::Reason)) {
            let local = self.local;
            let critical = self.slot == Some(Slot::Critical);
            if local || critical {
                self.problems.push(Problem::ReasonNeeded { local, critical, spelling });
            }
        }
        for (option, required) in [
            (ValueOption::Slot, Required::Slot),
            (ValueOption::Timeout, Required::Timeout),
            (ValueOption::Log, Required::Log),
        ] {
            if !self.given.contains(&RunOption::Value(option)) {
                self.problems.push(Problem::Missing(required, spelling));
            }
        }
        if command.is_empty() {
            self.problems.push(Problem::Missing(Required::Command, spelling));
        }
        match (self.slot, self.timeout, self.log) {
            (Some(slot), Some(timeout), Some(log)) if self.problems.is_empty() => Ok(RunRequest {
                slot,
                timeout,
                log,
                stall: self.stall.unwrap_or(DEFAULT_STALL_SECONDS),
                tail: self.tail.unwrap_or(DEFAULT_TAIL_LINES),
                no_markers: self.no_markers,
                local: self.local,
                alone: self.alone,
                builder: self.builder,
                reason: self.reason,
                bring_tracked: self.bring_tracked,
                env: self.env,
                state_dir: self.state_dir,
                cd: self.cd,
                command,
                test_seams: self.test_seams,
                print_request: self.print_request,
                spelling,
            }),
            _ => Err(self.problems),
        }
    }

    /// Refuses a `--builder` pin on a run that can never route: one given `--local`, or one on a slot other than heavy or
    /// light.
    fn check_builder_routes(&mut self) {
        let Some(host) = &self.builder else {
            return;
        };
        let why = if self.local {
            "--local keeps it here".to_owned()
        } else {
            match self.slot {
                Some(slot @ (Slot::Critical | Slot::None)) => {
                    format!("only a heavy or light run routes, and this one is --slot {}", slot.as_str())
                }
                _ => return,
            }
        };
        self.problems.push(Problem::BuilderCannotRoute { host: host.clone(), why });
    }

    /// Refuses the `--env` pairs: more pairs or bytes than its cap ([`values::over_cap`]), counted over the accepted
    /// pairs only, else a name given twice in any ASCII case, named as its later spelling. Each pair was checked as it
    /// was read. Neither refusal names a word, so each takes the run's whole `--env` count as its position.
    fn check_env(&mut self) {
        let bytes = self.env.iter().map(|(name, value)| name.len() + value.len()).sum();
        let refused = if values::over_cap(self.env.len(), bytes) {
            Some((values::EnvRefusal::OverCap, String::new()))
        } else {
            values::repeated_name(self.env.iter().map(|(name, _)| name.as_str())).map(|name| (values::EnvRefusal::Twice, name.to_owned()))
        };
        if let Some((refusal, name)) = refused {
            self.problems.push(Problem::EnvRefused {
                refusal,
                name,
                position: self.env_words,
            });
        }
    }

    /// Refuses the remote client's seams that cannot work together: `--test-remote-ssh`,
    /// `--test-remote-retry-pause-ms`, `--test-remote-bound-ms`, `--test-remote-interrupt-at`,
    /// `--test-remote-watch-call`, `--test-remote-timeout-ms`, `--test-remote-backstop-ms`,
    /// `--test-remote-snapshot-age-ms`, `--test-outputs-progress-ms`, `--test-remote-hint-from-pull`,
    /// `--test-local-sdks`,
    /// `--test-remote-fetch-cap-bytes`,
    /// `--test-remote-apply-cap-bytes`, `--test-remote-outputs-budget-bytes`, `--test-remote-inputs-budget-bytes`,
    /// `--test-remote-lfs-budget-bytes`, `--test-remote-log-cap-bytes`, `--test-remote-tamper` and
    /// `--test-remote-channel-fault`, `--test-remote-channel-send-buffer-bytes` without
    /// `--test-remote-transport`,
    /// whose client alone reads them, `--test-remote-timeout-ms` with it unless `--test-remote-watch-call` names a call
    /// other than `run`, and `--test-remote-snapshot` with it, as each chooses the client.
    #[cfg(feature = "test-seams")]
    fn check_remote_seams(&mut self) {
        const TRANSPORT: &str = "--test-remote-transport";
        let transport = self.given.contains(&RunOption::Value(ValueOption::RemoteTransport));
        // A seam whose value was refused is already a problem of its own.
        for (read, spelled) in [
            (self.test_seams.remote_ssh.is_some(), "--test-remote-ssh"),
            (self.test_seams.remote_retry_pause_ms.is_some(), "--test-remote-retry-pause-ms"),
            (self.test_seams.remote_bound_ms.is_some(), "--test-remote-bound-ms"),
            (self.test_seams.remote_interrupt_at.is_some(), "--test-remote-interrupt-at"),
            (self.test_seams.remote_watch_call.is_some(), "--test-remote-watch-call"),
            (self.test_seams.remote_timeout_ms.is_some(), "--test-remote-timeout-ms"),
            (self.test_seams.remote_backstop_ms.is_some(), "--test-remote-backstop-ms"),
            (self.test_seams.remote_snapshot_age_ms.is_some(), "--test-remote-snapshot-age-ms"),
            (self.test_seams.outputs_progress_ms.is_some(), "--test-outputs-progress-ms"),
            (self.test_seams.remote_hint_from_pull, "--test-remote-hint-from-pull"),
            (self.test_seams.local_sdks.is_some(), "--test-local-sdks"),
            (self.test_seams.remote_fetch_cap_bytes.is_some(), "--test-remote-fetch-cap-bytes"),
            (self.test_seams.remote_apply_cap_bytes.is_some(), "--test-remote-apply-cap-bytes"),
            (
                self.test_seams.remote_outputs_budget_bytes.is_some(),
                "--test-remote-outputs-budget-bytes",
            ),
            (self.test_seams.remote_inputs_budget_bytes.is_some(), "--test-remote-inputs-budget-bytes"),
            (self.test_seams.remote_lfs_budget_bytes.is_some(), "--test-remote-lfs-budget-bytes"),
            (self.test_seams.remote_log_cap_bytes.is_some(), "--test-remote-log-cap-bytes"),
            (self.test_seams.remote_tamper.is_some(), "--test-remote-tamper"),
            (self.test_seams.remote_channel_fault.is_some(), "--test-remote-channel-fault"),
            (
                self.test_seams.remote_channel_send_buffer_bytes.is_some(),
                "--test-remote-channel-send-buffer-bytes",
            ),
        ] {
            if read && !transport {
                self.problems.push(Problem::SeamNeeds {
                    option: spelled,
                    needs: TRANSPORT,
                });
            }
        }
        if transport {
            self.check_remote_timeout();
        }
        if transport && self.test_seams.remote_snapshot {
            self.problems.push(Problem::SeamsConflict {
                option: "--test-remote-snapshot",
                other: TRANSPORT,
            });
        }
    }

    /// Refuses `--test-remote-timeout-ms` unless `--test-remote-watch-call` names a call other than `run`: the seam is a
    /// transfer ceiling, and the run call is the one call it must not apply to, since the run call's backstop has a seam
    /// of its own, `--test-remote-backstop-ms`. A `--test-remote-watch-call` whose value was refused already said its
    /// problem, so none is added for it here.
    #[cfg(feature = "test-seams")]
    fn check_remote_timeout(&mut self) {
        let watch_call_given = self.given.contains(&RunOption::Value(ValueOption::RemoteWatchCall));
        if watch_call_given && self.test_seams.remote_watch_call.is_none() {
            return;
        }
        let other_call = self.test_seams.remote_watch_call.as_deref().is_some_and(|call| call != "run");
        if self.test_seams.remote_timeout_ms.is_some() && !other_call {
            self.problems.push(Problem::SeamNeeds {
                option: "--test-remote-timeout-ms",
                needs: "--test-remote-watch-call naming a call other than run",
            });
        }
    }

    /// The stop request for the pid read, or every problem found, the pid's last.
    pub(crate) fn finish_stop(mut self, pid: Result<u32, Problem>) -> Result<StopRequest, Vec<Problem>> {
        match pid {
            Ok(pid) if self.problems.is_empty() => Ok(StopRequest {
                pid,
                state_dir: self.state_dir,
                print_request: self.print_request,
            }),
            Ok(_) => Err(self.problems),
            Err(problem) => {
                self.problems.push(problem);
                Err(self.problems)
            }
        }
    }

    /// The cutover request, its seams taken from the poll, the still-waiting period, the confirmation window and the
    /// interrupt switch read, or every problem found.
    pub(crate) fn finish_cutover(self) -> Result<CutoverRequest, Vec<Problem>> {
        if !self.problems.is_empty() {
            return Err(self.problems);
        }
        Ok(CutoverRequest {
            print_request: self.print_request,
            seams: CutoverSeams {
                poll_ms: self.test_seams.poll_ms,
                heartbeat_ms: self.test_seams.heartbeat_ms,
                confirm_ms: self.cutover_seams.confirm_ms,
                interrupt_after_list: self.cutover_seams.interrupt_after_list,
            },
        })
    }

    /// The push request, its seams the ssh, bound and registration read, or every problem found.
    pub(crate) fn finish_push(self) -> Result<PushRequest, Vec<Problem>> {
        if !self.problems.is_empty() {
            return Err(self.problems);
        }
        Ok(PushRequest { seams: self.push_seams })
    }

    /// The drain request, its action from `--lift` or `--status`, its builder and its seams, or every problem found:
    /// those found while reading, then `--lift` with `--status`, then `--test-remote-ssh` without
    /// `--test-remote-transport`, whose local transport alone starts it.
    pub(crate) fn finish_drain(mut self) -> Result<DrainRequest, Vec<Problem>> {
        if self.lift && self.status {
            self.problems.push(Problem::OptionsConflict {
                option: "--lift",
                other: "--status",
            });
        }
        #[cfg(feature = "test-seams")]
        if self.test_seams.remote_ssh.is_some() && self.test_seams.remote_transport.is_none() {
            self.problems.push(Problem::SeamNeeds {
                option: "--test-remote-ssh",
                needs: "--test-remote-transport",
            });
        }
        if !self.problems.is_empty() {
            return Err(self.problems);
        }
        let action = match (self.lift, self.status) {
            (true, _) => DrainAction::Lift,
            (false, true) => DrainAction::Status,
            (false, false) => DrainAction::Raise,
        };
        Ok(DrainRequest {
            action,
            builder: self.builder,
            seams: DrainSeams {
                poll_ms: self.test_seams.poll_ms,
                heartbeat_ms: self.test_seams.heartbeat_ms,
                remote_transport: self.test_seams.remote_transport,
                remote_ssh: self.test_seams.remote_ssh,
                ..self.drain_seams
            },
        })
    }

    /// The prune request, its seams the transport, ssh and least age read, or every problem found: those found while
    /// reading, then a missing `--builders`, then `--test-remote-ssh` without `--test-remote-transport`, whose local
    /// transport alone starts it.
    pub(crate) fn finish_prune(mut self) -> Result<PruneRequest, Vec<Problem>> {
        if !self.builders {
            self.problems.push(Problem::Missing(Required::Builders, Spelling::Cli));
        }
        #[cfg(feature = "test-seams")]
        if self.test_seams.remote_ssh.is_some() && self.test_seams.remote_transport.is_none() {
            self.problems.push(Problem::SeamNeeds {
                option: "--test-remote-ssh",
                needs: "--test-remote-transport",
            });
        }
        if !self.problems.is_empty() {
            return Err(self.problems);
        }
        Ok(PruneRequest {
            seams: PruneSeams {
                remote_transport: self.test_seams.remote_transport,
                remote_ssh: self.test_seams.remote_ssh,
                gone_age_ms: self.prune_gone_age_ms,
            },
            quiet: self.quiet,
        })
    }

    /// The check request, its builder and its seams the probe limit, transport, ssh and bound read, or every problem
    /// found: those found while reading, then the builder seams that cannot work together.
    #[cfg_attr(
        not(feature = "test-seams"),
        expect(unused_mut, reason = "only the test-seams build reads seams that can conflict")
    )]
    pub(crate) fn finish_check(mut self) -> Result<CheckRequest, Vec<Problem>> {
        #[cfg(feature = "test-seams")]
        self.check_builder_seams();
        if !self.problems.is_empty() {
            return Err(self.problems);
        }
        Ok(CheckRequest {
            builder: self.builder,
            seams: CheckSeams {
                remote_transport: self.test_seams.remote_transport,
                remote_ssh: self.test_seams.remote_ssh,
                remote_timeout_ms: self.test_seams.remote_timeout_ms,
                tailscale_status: self.test_seams.tailscale_status,
                ..self.check_seams
            },
        })
    }

    /// The warm request, its folder, background switch, state root and seams read, or every problem: those found while
    /// reading, then the remote client's seams that cannot work together, as a run's.
    #[cfg_attr(
        not(feature = "test-seams"),
        expect(unused_mut, reason = "only the test-seams build reads seams that can conflict")
    )]
    pub(crate) fn finish_warm(mut self) -> Result<WarmRequest, Vec<Problem>> {
        #[cfg(feature = "test-seams")]
        self.check_remote_seams();
        if !self.problems.is_empty() {
            return Err(self.problems);
        }
        Ok(WarmRequest {
            cd: self.cd,
            background: self.background,
            state_dir: self.state_dir,
            test_seams: self.test_seams,
        })
    }

    /// Refuses the builder check's seams without `--builder`, which alone reads them, `--test-remote-ssh` without
    /// `--test-remote-transport`, whose local transport alone starts it, and `--test-probe-limit-ms`,
    /// `--test-sdk-limit-ms` and `--test-netfx-key` with `--builder`, whose probes and SDK list run on the box under its
    /// own seams. `--test-probe-drain-ms` is read either way: with `--builder` it bounds the drain of the builder's call.
    /// A seam whose value was refused is already a problem of its own.
    #[cfg(feature = "test-seams")]
    fn check_builder_seams(&mut self) {
        const BUILDER: &str = "--builder";
        let builder = self.builder.is_some();
        for (read, option) in [
            (self.check_seams.probe_limit_ms.is_some(), "--test-probe-limit-ms"),
            (self.check_seams.sdk_limit_ms.is_some(), "--test-sdk-limit-ms"),
            (self.check_seams.netfx_key.is_some(), "--test-netfx-key"),
        ] {
            if builder && read {
                self.problems.push(Problem::SeamNotUsedWith { option, with: BUILDER });
            }
        }
        for (read, spelled) in [
            (self.test_seams.remote_transport.is_some(), "--test-remote-transport"),
            (self.test_seams.remote_ssh.is_some(), "--test-remote-ssh"),
            (self.test_seams.remote_timeout_ms.is_some(), "--test-remote-timeout-ms"),
            (self.test_seams.tailscale_status.is_some(), "--test-tailscale-status"),
            (self.check_seams.check_channel_ms.is_some(), "--test-check-channel-ms"),
        ] {
            if read && !builder {
                self.problems.push(Problem::SeamNeeds {
                    option: spelled,
                    needs: BUILDER,
                });
            }
        }
        if builder && self.test_seams.remote_ssh.is_some() && self.test_seams.remote_transport.is_none() {
            self.problems.push(Problem::SeamNeeds {
                option: "--test-remote-ssh",
                needs: "--test-remote-transport",
            });
        }
    }

    /// The burn request, its cores and seconds, or every problem: those found while reading, then a missing
    /// `--seconds`.
    pub(crate) fn finish_burn(mut self) -> Result<BurnRequest, Vec<Problem>> {
        if !self.given.contains(&RunOption::Value(ValueOption::Seconds)) {
            self.problems.push(Problem::Missing(Required::Seconds, Spelling::Cli));
        }
        match self.seconds {
            Some(seconds) if self.problems.is_empty() => Ok(BurnRequest { cores: self.cores, seconds }),
            _ => Err(self.problems),
        }
    }

    /// The expect request for `command`, its slot `heavy` when none was given, or every problem: those found while
    /// reading, a slot of `none`, then a missing command. A slot is refused as one of heavy, light or critical, the
    /// slots a run says an expected time in.
    pub(crate) fn finish_expect(mut self, command: Vec<OsString>) -> Result<ExpectRequest, Vec<Problem>> {
        for problem in &mut self.problems {
            if let Problem::BadSlot { option, value } = problem {
                *problem = Problem::BadExpectSlot {
                    option: std::mem::take(option),
                    value: std::mem::take(value),
                };
            }
        }
        if self.slot == Some(Slot::None) {
            self.problems.push(Problem::BadExpectSlot {
                option: "--slot".to_owned(),
                value: Slot::None.as_str().to_owned(),
            });
        }
        if command.is_empty() {
            self.problems.push(Problem::Missing(Required::Command, Spelling::Cli));
        }
        if !self.problems.is_empty() {
            return Err(self.problems);
        }
        Ok(ExpectRequest {
            slot: self.slot.unwrap_or(Slot::Heavy),
            state_dir: self.state_dir,
            command,
        })
    }
}

#[cfg(feature = "test-seams")]
fn is_print_request(option: RunOption) -> bool {
    option == RunOption::Switch(SwitchOption::PrintRequest)
}

#[cfg(not(feature = "test-seams"))]
fn is_print_request(_option: RunOption) -> bool {
    false
}

/// The next gate word that is valid Unicode, each unreadable word before it reported in `builder`'s problems. An inline
/// `--env` word is refused by its position, as the two-word form is, and counts as an `--env` word.
pub(crate) fn next_text(words: &mut impl Iterator<Item = OsString>, builder: &mut RunBuilder) -> Option<String> {
    for word in words {
        match word.into_string() {
            Ok(text) => return Some(text),
            Err(word) if inline_env_word(&word) => {
                let position = builder.next_env_word();
                builder.problems.push(Problem::EnvWordNotUnicode(position));
            }
            Err(word) => builder.problems.push(unreadable(&word)),
        }
    }
    None
}

/// Whether `word`, which is not valid Unicode, is the inline form of an `--env` word: the command line's `--env=<word>`, or
/// the compatibility layer's `-Env:<word>`, its name read in any ASCII case.
fn inline_env_word(word: &OsStr) -> bool {
    let shown = word.to_string_lossy();
    shown.starts_with("--env=") || shown.get(..5).is_some_and(|start| start.eq_ignore_ascii_case("-env:"))
}

/// The next word as an option's value: `None` at the end of the words, before `--`, which is never a value, and
/// before a word that `is_option`, which is left to be read as an option.
pub(crate) fn next_value(words: &mut Peekable<impl Iterator<Item = OsString>>, is_option: fn(&str) -> bool) -> Result<Option<String>, Problem> {
    match words.next_if(|word| word.as_os_str() != "--" && !word.to_str().is_some_and(is_option)) {
        None => Ok(None),
        Some(word) => word.into_string().map(Some).map_err(|word| unreadable(&word)),
    }
}

/// The problem of a gate word that is not valid Unicode.
pub(crate) fn unreadable(word: &OsStr) -> Problem {
    Problem::Unreadable(word.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::ffi::OsString;

    use super::{Forward, RunBuilder, ValueOption};
    use crate::problem::Problem;
    use crate::values::{EnvRefusal, MOST_ENV_BYTES};

    thread_local! {
        /// The environment the tests below forward from, one thread's own so the tests cannot see each other's.
        static FORWARDED: RefCell<HashMap<String, OsString>> = RefCell::new(HashMap::new());
    }

    /// The lookup of the tests: a name of [`FORWARDED`], and nothing else.
    fn forward(name: &str) -> Option<OsString> {
        FORWARDED.with(|table| table.borrow().get(name).cloned())
    }

    /// A builder that forwards from `pairs` alone, with the options a run cannot do without already given.
    fn builder(pairs: &[(&str, &str)]) -> RunBuilder {
        FORWARDED.with(|table| {
            let mut table = table.borrow_mut();
            table.clear();
            for (name, value) in pairs {
                table.insert((*name).to_owned(), OsString::from(value));
            }
        });
        let mut builder = RunBuilder {
            forward: Forward(forward),
            ..RunBuilder::default()
        };
        builder.value(ValueOption::Slot, "--slot", "heavy");
        builder.value(ValueOption::Timeout, "--timeout", "5");
        builder.value(ValueOption::Log, "--log", "out.log");
        builder
    }

    /// `builder` with each of `words` given as an `--env` word.
    fn env_words(builder: &mut RunBuilder, words: &[&str]) {
        for word in words {
            builder.value(ValueOption::Env, "--env", word);
        }
    }

    /// Asserts that `builder` refuses its run of `cmd` with exactly `problems`.
    fn refused(builder: RunBuilder, problems: Vec<Problem>) {
        assert_eq!(builder.finish_run(vec![OsString::from("cmd")]), Err(problems));
    }

    #[test]
    fn a_forwarded_word_names_its_position_when_it_is_not_set() {
        let mut builder = builder(&[("SET", "v")]);
        env_words(&mut builder, &["SET=1", "MISSING", "OTHER=2"]);

        refused(builder, vec![Problem::EnvForwardNotSet(2)]);
    }

    #[test]
    fn an_empty_env_word_still_counts() {
        let mut builder = builder(&[]);
        env_words(&mut builder, &["", "MISSING"]);

        refused(builder, vec![Problem::NeedsValue("--env".to_owned()), Problem::EnvForwardNotSet(2)]);
    }

    #[test]
    fn a_forwarded_pair_and_a_written_one_of_one_name_are_refused_as_twice() {
        for (words, later) in [(["FORWARDED=1", "FORWARDED"], "FORWARDED"), (["FORWARDED", "forwarded=1"], "forwarded")] {
            let mut builder = builder(&[("FORWARDED", "v")]);
            env_words(&mut builder, &words);

            refused(
                builder,
                vec![Problem::EnvRefused {
                    refusal: EnvRefusal::Twice,
                    name: later.to_owned(),
                    position: 2,
                }],
            );
        }
    }

    #[test]
    fn the_cap_counts_a_forwarded_value() {
        let value = "x".repeat(MOST_ENV_BYTES);
        let mut builder = builder(&[("BIG", value.as_str())]);
        env_words(&mut builder, &["BIG"]);

        refused(
            builder,
            vec![Problem::EnvRefused {
                refusal: EnvRefusal::OverCap,
                name: String::new(),
                position: 1,
            }],
        );
    }
}
