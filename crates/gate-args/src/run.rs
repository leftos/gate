//! The options both forms of the command line share, collected word by word into a request, with every problem kept.

use std::ffi::{OsStr, OsString};
use std::iter::Peekable;
use std::path::PathBuf;

use crate::problem::{Problem, Required, Spelling};
use crate::request::{
    BurnRequest, CutoverRequest, CutoverSeams, DEFAULT_STALL_SECONDS, DEFAULT_TAIL_LINES, PushRequest, PushSeams, RunRequest, Slot, StopRequest,
    TestSeams,
};
use crate::values;

/// An option that takes a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ValueOption {
    Slot,
    Timeout,
    Log,
    Stall,
    Tail,
    StateDir,
    Cores,
    Seconds,
    #[cfg(feature = "test-seams")]
    SampleMs,
    #[cfg(feature = "test-seams")]
    HeartbeatMs,
    #[cfg(feature = "test-seams")]
    PollMs,
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
    GitLimitMs,
    #[cfg(feature = "test-seams")]
    GitDrainMs,
    #[cfg(feature = "test-seams")]
    ServersUnder,
    #[cfg(feature = "test-seams")]
    ServerImage,
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
    LocalSdks,
    #[cfg(feature = "test-seams")]
    RemoteFetchCapBytes,
    #[cfg(feature = "test-seams")]
    RemoteApplyCapBytes,
    #[cfg(feature = "test-seams")]
    RemoteScript,
    #[cfg(feature = "test-seams")]
    BridgePwsh,
    #[cfg(feature = "test-seams")]
    ConfirmMs,
    #[cfg(feature = "test-seams")]
    PushSsh,
    #[cfg(feature = "test-seams")]
    PushBoundMs,
}

/// An option that is on or off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SwitchOption {
    NoMarkers,
    Local,
    #[cfg(feature = "test-seams")]
    PrintRequest,
    #[cfg(feature = "test-seams")]
    InterruptQueued,
    #[cfg(feature = "test-seams")]
    SamplerFail,
    #[cfg(feature = "test-seams")]
    OwnStartFails,
    #[cfg(feature = "test-seams")]
    ParentJobTrusted,
    #[cfg(feature = "test-seams")]
    DiagnosticsFail,
    #[cfg(feature = "test-seams")]
    RemoteSnapshot,
    #[cfg(feature = "test-seams")]
    JobBreakaway,
    #[cfg(feature = "test-seams")]
    CutoverInterruptAfterList,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunOption {
    Value(ValueOption),
    Switch(SwitchOption),
}

/// The options read so far and the problems found so far.
#[derive(Default)]
pub(crate) struct RunBuilder {
    slot: Option<Slot>,
    timeout: Option<u64>,
    log: Option<PathBuf>,
    stall: Option<u64>,
    tail: Option<u64>,
    state_dir: Option<PathBuf>,
    cores: Option<u32>,
    seconds: Option<u32>,
    no_markers: bool,
    local: bool,
    print_request: bool,
    test_seams: TestSeams,
    /// `gate cutover`'s own seams, `--test-confirm-ms` and `--test-cutover-interrupt-after-list`; its poll and
    /// still-waiting period are read into `test_seams`, as a run's are.
    cutover_seams: CutoverSeams,
    /// `gate push`'s own seams, `--test-push-ssh` and `--test-push-bound-ms`.
    push_seams: PushSeams,
    given: Vec<RunOption>,
    pub(crate) problems: Vec<Problem>,
}

impl RunBuilder {
    /// Whether any option other than the hidden `--test-print-request` was given, valid or not.
    pub(crate) fn has_options(&self) -> bool {
        self.given.iter().any(|option| !is_print_request(*option))
    }

    /// Records `option` as given, or the problem of its being given twice.
    fn first_time(&mut self, option: RunOption, spelled: &str) -> bool {
        if self.given.contains(&option) {
            self.problems.push(Problem::GivenTwice(spelled.to_owned()));
            return false;
        }
        self.given.push(option);
        true
    }

    /// Reads `option`'s value, spelled `spelled` by the caller; an empty value is no value.
    pub(crate) fn value(&mut self, option: ValueOption, spelled: &str, value: &str) {
        if !self.first_time(RunOption::Value(option), spelled) {
            return;
        }
        if value.is_empty() {
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
                    self.problems.push(problem);
                }
            }
        }
    }

    fn apply(&mut self, option: ValueOption, spelled: &str, value: &str) -> Result<(), Problem> {
        match option {
            ValueOption::Slot => self.slot = Some(values::slot(spelled, value)?),
            ValueOption::Timeout => self.timeout = Some(values::above_zero(spelled, value)?),
            ValueOption::Log => self.log = Some(PathBuf::from(value)),
            ValueOption::Stall => self.stall = Some(values::above_zero(spelled, value)?),
            ValueOption::Tail => self.tail = Some(values::zero_or_above(spelled, value)?),
            ValueOption::StateDir => self.state_dir = Some(PathBuf::from(value)),
            ValueOption::Cores => self.cores = Some(values::above_zero_u32(spelled, value)?),
            ValueOption::Seconds => self.seconds = Some(values::in_range(spelled, value, 1, values::MAX_BURN_SECONDS)?),
            #[cfg(feature = "test-seams")]
            ValueOption::SampleMs => self.test_seams.sample_ms = Some(values::above_zero(spelled, value)?),
            #[cfg(feature = "test-seams")]
            ValueOption::HeartbeatMs => self.test_seams.heartbeat_ms = Some(values::above_zero(spelled, value)?),
            #[cfg(feature = "test-seams")]
            ValueOption::PollMs => self.test_seams.poll_ms = Some(values::above_zero(spelled, value)?),
            #[cfg(feature = "test-seams")]
            ValueOption::StallMs => self.test_seams.stall_ms = Some(values::above_zero(spelled, value)?),
            #[cfg(feature = "test-seams")]
            ValueOption::DiagnosticsBudgetMs => {
                self.test_seams.diagnostics_budget_ms = Some(values::above_zero(spelled, value)?);
            }
            #[cfg(feature = "test-seams")]
            ValueOption::InterruptAfterSamples => self.test_seams.interrupt_after_samples = Some(values::above_zero(spelled, value)?),
            #[cfg(feature = "test-seams")]
            ValueOption::RemoteClient => self.test_seams.remote_client = Some(PathBuf::from(value)),
            #[cfg(feature = "test-seams")]
            ValueOption::RemoteWaitMs => self.test_seams.remote_wait_ms = Some(values::above_zero(spelled, value)?),
            #[cfg(feature = "test-seams")]
            ValueOption::GitLimitMs => self.test_seams.git_limit_ms = Some(values::above_zero(spelled, value)?),
            #[cfg(feature = "test-seams")]
            ValueOption::GitDrainMs => self.test_seams.git_drain_ms = Some(values::above_zero(spelled, value)?),
            #[cfg(feature = "test-seams")]
            ValueOption::ServersUnder => self.test_seams.servers_under = Some(PathBuf::from(value)),
            #[cfg(feature = "test-seams")]
            ValueOption::ServerImage => self.test_seams.server_image = Some(value.to_owned()),
            #[cfg(feature = "test-seams")]
            ValueOption::RemoteTransport => self.test_seams.remote_transport = Some(value.to_owned()),
            #[cfg(feature = "test-seams")]
            ValueOption::RemoteSsh => self.test_seams.remote_ssh = Some(PathBuf::from(value)),
            #[cfg(feature = "test-seams")]
            ValueOption::RemoteRetryPauseMs => self.test_seams.remote_retry_pause_ms = Some(values::above_zero(spelled, value)?),
            #[cfg(feature = "test-seams")]
            ValueOption::RemoteBoundMs => self.test_seams.remote_bound_ms = Some(values::above_zero(spelled, value)?),
            #[cfg(feature = "test-seams")]
            ValueOption::RemoteInterruptAt => {
                self.test_seams.remote_interrupt_at = Some(values::remote_call(spelled, value, values::INTERRUPTED_CALLS)?);
            }
            #[cfg(feature = "test-seams")]
            ValueOption::RemoteWatchCall => {
                self.test_seams.remote_watch_call = Some(values::remote_call(spelled, value, values::WATCHED_CALLS)?);
            }
            #[cfg(feature = "test-seams")]
            ValueOption::RemoteTimeoutMs => self.test_seams.remote_timeout_ms = Some(values::above_zero(spelled, value)?),
            #[cfg(feature = "test-seams")]
            ValueOption::LocalSdks => self.test_seams.local_sdks = Some(value.to_owned()),
            #[cfg(feature = "test-seams")]
            ValueOption::RemoteFetchCapBytes => self.test_seams.remote_fetch_cap_bytes = Some(values::above_zero(spelled, value)?),
            #[cfg(feature = "test-seams")]
            ValueOption::RemoteApplyCapBytes => self.test_seams.remote_apply_cap_bytes = Some(values::above_zero(spelled, value)?),
            #[cfg(feature = "test-seams")]
            ValueOption::RemoteScript => self.test_seams.remote_script = Some(PathBuf::from(value)),
            #[cfg(feature = "test-seams")]
            ValueOption::BridgePwsh => self.test_seams.bridge_pwsh = Some(PathBuf::from(value)),
            #[cfg(feature = "test-seams")]
            ValueOption::ConfirmMs => self.cutover_seams.confirm_ms = Some(values::above_zero(spelled, value)?),
            #[cfg(feature = "test-seams")]
            ValueOption::PushSsh => self.push_seams.ssh = Some(PathBuf::from(value)),
            #[cfg(feature = "test-seams")]
            ValueOption::PushBoundMs => self.push_seams.bound_ms = Some(values::above_zero(spelled, value)?),
        }
        Ok(())
    }

    /// Sets or clears a switch.
    pub(crate) fn switch(&mut self, option: SwitchOption, spelled: &str, on: bool) {
        if !self.first_time(RunOption::Switch(option), spelled) {
            return;
        }
        match option {
            SwitchOption::NoMarkers => self.no_markers = on,
            SwitchOption::Local => self.local = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::PrintRequest => self.print_request = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::InterruptQueued => self.test_seams.interrupt_queued = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::SamplerFail => self.test_seams.sampler_fail = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::OwnStartFails => self.test_seams.own_start_fails = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::ParentJobTrusted => self.test_seams.parent_job_trusted = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::DiagnosticsFail => self.test_seams.diagnostics_fail = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::RemoteSnapshot => self.test_seams.remote_snapshot = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::JobBreakaway => self.test_seams.job_breakaway = on,
            #[cfg(feature = "test-seams")]
            SwitchOption::CutoverInterruptAfterList => self.cutover_seams.interrupt_after_list = on,
        }
    }

    /// The run request, or every problem: those found while reading, then the test seams that cannot work together,
    /// then each missing input in the order slot, timeout, log, command.
    pub(crate) fn finish_run(mut self, command: Vec<OsString>, spelling: Spelling) -> Result<RunRequest, Vec<Problem>> {
        #[cfg(feature = "test-seams")]
        self.check_remote_seams();
        #[cfg(feature = "test-seams")]
        self.check_server_seams();
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
                state_dir: self.state_dir,
                command,
                test_seams: self.test_seams,
                print_request: self.print_request,
                spelling,
            }),
            _ => Err(self.problems),
        }
    }

    /// Refuses `--test-server-image` without `--test-servers-under`: an image the watch reads as a server's is only
    /// safe in a scan confined to the case's folder.
    #[cfg(feature = "test-seams")]
    fn check_server_seams(&mut self) {
        // A seam whose value was refused is already a problem of its own.
        if self.test_seams.server_image.is_some() && !self.given.contains(&RunOption::Value(ValueOption::ServersUnder)) {
            self.problems.push(Problem::SeamNeeds {
                option: "--test-server-image",
                needs: "--test-servers-under",
            });
        }
    }

    /// Refuses the remote client's seams that cannot work together: `--test-remote-ssh`,
    /// `--test-remote-retry-pause-ms`, `--test-remote-bound-ms`, `--test-remote-interrupt-at`,
    /// `--test-remote-watch-call`, `--test-remote-timeout-ms`, `--test-local-sdks`, `--test-remote-fetch-cap-bytes` and
    /// `--test-remote-apply-cap-bytes` without `--test-remote-transport`, whose client alone reads them, and
    /// `--test-remote-snapshot` with it, as each chooses the client.
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
            (self.test_seams.local_sdks.is_some(), "--test-local-sdks"),
            (self.test_seams.remote_fetch_cap_bytes.is_some(), "--test-remote-fetch-cap-bytes"),
            (self.test_seams.remote_apply_cap_bytes.is_some(), "--test-remote-apply-cap-bytes"),
        ] {
            if read && !transport {
                self.problems.push(Problem::SeamNeeds {
                    option: spelled,
                    needs: TRANSPORT,
                });
            }
        }
        if transport && self.test_seams.remote_snapshot {
            self.problems.push(Problem::SeamsConflict {
                option: "--test-remote-snapshot",
                other: TRANSPORT,
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

    /// The push request, its seams the ssh and bound read, or every problem found.
    pub(crate) fn finish_push(self) -> Result<PushRequest, Vec<Problem>> {
        if !self.problems.is_empty() {
            return Err(self.problems);
        }
        Ok(PushRequest { seams: self.push_seams })
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
}

#[cfg(feature = "test-seams")]
fn is_print_request(option: RunOption) -> bool {
    option == RunOption::Switch(SwitchOption::PrintRequest)
}

#[cfg(not(feature = "test-seams"))]
fn is_print_request(_option: RunOption) -> bool {
    false
}

/// The next gate word that is valid Unicode, each unreadable word before it reported in `problems`.
pub(crate) fn next_text(words: &mut impl Iterator<Item = OsString>, problems: &mut Vec<Problem>) -> Option<String> {
    for word in words {
        match word.into_string() {
            Ok(text) => return Some(text),
            Err(word) => problems.push(unreadable(&word)),
        }
    }
    None
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
