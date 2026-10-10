//! The remote client a routed run calls: the call it is given, the outcome it hands back, and the clients there are.
//! With no client, a routed run falls back here with reason `client`. A gate built with the `test-seams` feature has
//! the scripted client `--test-remote-client` names, which runs a step file in this process.

use std::path::Path;

use gate_args::RunRequest;
use gate_state::FallbackReason;
use gate_win32::Interrupt;

/// The status a client hands back with a refusal's reason.
pub(crate) const REFUSED: i64 = 75;
/// Why a routed run's remote client cannot start: none is built yet.
const NO_CLIENT: &str = "the bridge to remote.ps1 is not built yet";

/// What runs a routed command on the builder.
pub(crate) trait RemoteClient {
    /// Runs the call to its end, or to its refusal, on the run's thread.
    fn run(&mut self, call: &RemoteCall<'_>) -> RemoteOutcome;
}

/// What the client is asked to run, and where: the route's facts, the run's log and limits, and the interrupt that
/// stops it.
#[cfg_attr(
    not(feature = "test-seams"),
    expect(dead_code, reason = "a gate without the test seams has only `NoClient`, which reads none of the call")
)]
pub(crate) struct RemoteCall<'a> {
    pub host: &'a str,
    pub key: &'a str,
    pub root: &'a str,
    pub repo: &'a str,
    pub subfolder: &'a str,
    /// The words the builder runs.
    pub words: &'a [String],
    /// The output globs the run brings back.
    pub outputs: &'a [String],
    /// The log the client writes the run's output to.
    pub log: &'a Path,
    pub timeout: u64,
    pub stall: u64,
    pub tail: u64,
    pub no_markers: bool,
    /// The console interrupt, which the client sees raised and stops what it started on; `None` when it could not be
    /// installed.
    pub interrupt: Option<Interrupt>,
}

/// What the client hands back: the status, a refusal's reason, whether it killed the run, the lines it wrote to the
/// log, and why it could not start.
#[derive(Debug, Default)]
pub(crate) struct RemoteOutcome {
    pub status: i64,
    pub reason: Option<FallbackReason>,
    pub killed: bool,
    pub lines: Vec<String>,
    pub detail: Option<String>,
}

impl RemoteOutcome {
    /// Why the run falls back here: a client that could not start, or a refusal, which is status 75 with a reason. A 75
    /// with no reason is the command's own status.
    pub(crate) fn fallback(&self) -> Option<FallbackReason> {
        match self.reason {
            Some(FallbackReason::Client) => Some(FallbackReason::Client),
            reason if self.status == REFUSED => reason,
            _ => None,
        }
    }

    /// A client that could not start, for `why`.
    fn not_started(why: String) -> RemoteOutcome {
        RemoteOutcome {
            status: REFUSED,
            reason: Some(FallbackReason::Client),
            detail: Some(why),
            ..RemoteOutcome::default()
        }
    }
}

/// No client: every call is one that could not start.
pub(crate) struct NoClient;

impl RemoteClient for NoClient {
    fn run(&mut self, _call: &RemoteCall<'_>) -> RemoteOutcome {
        RemoteOutcome::not_started(NO_CLIENT.to_owned())
    }
}

/// The client `request` names: the scripted one of `--test-remote-client`, else none; or the line saying why its step
/// file cannot be used.
#[cfg(feature = "test-seams")]
pub(crate) fn chosen(request: &RunRequest) -> Result<Option<Box<dyn RemoteClient>>, String> {
    match &request.test_seams.remote_client {
        Some(path) => scripted::ScriptedClient::load(path).map(|client| Some(Box::new(client) as Box<dyn RemoteClient>)),
        None => Ok(None),
    }
}

/// The client `request` names: none in a gate built without the test seams.
#[cfg(not(feature = "test-seams"))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the signature of the test-seams build, whose step file can fail to load"
)]
pub(crate) fn chosen(_request: &RunRequest) -> Result<Option<Box<dyn RemoteClient>>, String> {
    Ok(None)
}

/// The scripted client: a step file run in order, one step a line. `out <text>` and `err <text>` append the text to
/// the log and say it on stdout or stderr; `record <path>` writes the call as one JSON object; `wait <path>` returns
/// when the path exists or the interrupt is raised, the latter ending the run with `STATUS_CONTROL_C_EXIT`;
/// `interrupt` raises the call's interrupt; `killed` marks the run killed; `status <n>` sets the status; `refuse
/// <reason>` refuses with status 75; `fail-start <why>` ends the run as a client that could not start.
#[cfg(feature = "test-seams")]
mod scripted {
    use std::fs::{self, OpenOptions};
    use std::io::{self, Write};
    use std::path::{Path, PathBuf};
    use std::thread;
    use std::time::Duration;

    use gate_state::FallbackReason;
    use gate_win32::Interrupt;
    use serde_json::json;

    use super::{REFUSED, RemoteCall, RemoteClient, RemoteOutcome};
    use crate::run::CONTROL_C_EXIT;

    /// How often `wait` looks for its path and the interrupt.
    const WAIT_STEP: Duration = Duration::from_millis(20);
    /// Every reason a `refuse` step may give.
    const REASONS: [FallbackReason; 8] = [
        FallbackReason::Full,
        FallbackReason::Busy,
        FallbackReason::Unreachable,
        FallbackReason::Parity,
        FallbackReason::Rebooted,
        FallbackReason::Tools,
        FallbackReason::Version,
        FallbackReason::Client,
    ];

    #[derive(Debug, PartialEq)]
    pub(super) enum Step {
        Out(String),
        Err(String),
        Record(PathBuf),
        Wait(PathBuf),
        Interrupt,
        Killed,
        Status(i64),
        Refuse(FallbackReason),
        FailStart(String),
    }

    pub(crate) struct ScriptedClient {
        steps: Vec<Step>,
    }

    impl ScriptedClient {
        /// The client of the step file at `path`, or the line saying why it cannot be read or parsed.
        pub(crate) fn load(path: &Path) -> Result<ScriptedClient, String> {
            let cannot = |why: String| format!("gate: cannot use the remote client's steps {}: {why}", path.display());
            let text = fs::read_to_string(path).map_err(|error| cannot(error.to_string()))?;
            parse(&text).map(|steps| ScriptedClient { steps }).map_err(cannot)
        }
    }

    impl RemoteClient for ScriptedClient {
        fn run(&mut self, call: &RemoteCall<'_>) -> RemoteOutcome {
            let mut outcome = RemoteOutcome::default();
            for step in &self.steps {
                match perform(step, call, &mut outcome) {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(why) => return RemoteOutcome::not_started(why),
                }
            }
            outcome
        }
    }

    /// The steps of `text`, blank lines skipped, or the first line that is no step, by its number.
    pub(super) fn parse(text: &str) -> Result<Vec<Step>, String> {
        text.lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(index, line)| step(line).map_err(|why| format!("line {}: {why}", index + 1)))
            .collect()
    }

    fn step(line: &str) -> Result<Step, String> {
        match line.split_once(' ') {
            None => bare_step(line),
            Some((word, operand)) => operand_step(word, operand),
        }
    }

    fn bare_step(word: &str) -> Result<Step, String> {
        match word {
            "interrupt" => Ok(Step::Interrupt),
            "killed" => Ok(Step::Killed),
            other => Err(format!("unknown step, or a step without its value: '{other}'")),
        }
    }

    fn operand_step(word: &str, operand: &str) -> Result<Step, String> {
        match word {
            "out" => Ok(Step::Out(operand.to_owned())),
            "err" => Ok(Step::Err(operand.to_owned())),
            "record" => Ok(Step::Record(PathBuf::from(operand))),
            "wait" => Ok(Step::Wait(PathBuf::from(operand))),
            "fail-start" => Ok(Step::FailStart(operand.to_owned())),
            "status" => operand
                .parse()
                .map(Step::Status)
                .map_err(|_| format!("status takes a whole number, got '{operand}'")),
            "refuse" => REASONS
                .into_iter()
                .find(|reason| reason.as_str() == operand)
                .map(Step::Refuse)
                .ok_or_else(|| format!("refuse takes a reason, got '{operand}'")),
            other => Err(format!("unknown step '{other}'")),
        }
    }

    /// Performs `step` of `call` into `outcome`; whether the run goes on to the next step, or why the client failed.
    fn perform(step: &Step, call: &RemoteCall<'_>, outcome: &mut RemoteOutcome) -> Result<bool, String> {
        match step {
            Step::Out(text) => say(call.log, text, io::stdout().lock(), outcome),
            Step::Err(text) => say(call.log, text, io::stderr().lock(), outcome),
            Step::Record(path) => record(call, path),
            Step::Wait(path) => {
                let came = wait(path, call.interrupt)?;
                if !came {
                    outcome.status = i64::from(CONTROL_C_EXIT.cast_signed());
                }
                Ok(came)
            }
            _ => Ok(settle(step, call, outcome)),
        }
    }

    /// Performs a step that only marks the run; whether the run goes on.
    fn settle(step: &Step, call: &RemoteCall<'_>, outcome: &mut RemoteOutcome) -> bool {
        match step {
            Step::Interrupt => call.interrupt.into_iter().for_each(Interrupt::raise),
            Step::Killed => outcome.killed = true,
            Step::Status(status) => outcome.status = *status,
            Step::Refuse(reason) => {
                outcome.status = REFUSED;
                outcome.reason = Some(*reason);
            }
            Step::FailStart(why) => {
                *outcome = RemoteOutcome::not_started(why.clone());
                return false;
            }
            Step::Out(_) | Step::Err(_) | Step::Record(_) | Step::Wait(_) => {}
        }
        true
    }

    /// Appends `text` to the log, says it on `stream` and keeps it in the outcome's lines.
    fn say(log: &Path, text: &str, mut stream: impl Write, outcome: &mut RemoteOutcome) -> Result<bool, String> {
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(log)
            .map_err(|error| format!("cannot open {}: {error}", log.display()))?;
        writeln!(file, "{text}").map_err(|error| format!("cannot write to {}: {error}", log.display()))?;
        writeln!(stream, "{text}")
            .and_then(|()| stream.flush())
            .map_err(|error| format!("cannot say a line: {error}"))?;
        outcome.lines.push(text.to_owned());
        Ok(true)
    }

    /// Writes the call to `path` as one JSON object.
    fn record(call: &RemoteCall<'_>, path: &Path) -> Result<bool, String> {
        let recorded = json!({
            "host": call.host,
            "key": call.key,
            "root": call.root,
            "repo": call.repo,
            "subfolder": call.subfolder,
            "words": call.words,
            "outputs": call.outputs,
            "log": call.log.to_string_lossy(),
            "timeout": call.timeout,
            "stall": call.stall,
            "tail": call.tail,
            "noMarkers": call.no_markers,
        });
        fs::write(path, recorded.to_string()).map_err(|error| format!("cannot record the call to {}: {error}", path.display()))?;
        Ok(true)
    }

    /// Sleeps until `path` exists, true, or the interrupt is raised, false.
    fn wait(path: &Path, interrupt: Option<Interrupt>) -> Result<bool, String> {
        loop {
            if path.try_exists().map_err(|error| format!("cannot check {}: {error}", path.display()))? {
                return Ok(true);
            }
            if interrupt.is_some_and(Interrupt::is_raised) {
                return Ok(false);
            }
            thread::sleep(WAIT_STEP);
        }
    }

    #[cfg(test)]
    mod tests {
        use std::path::PathBuf;

        use gate_state::FallbackReason;

        use super::{Step, parse};

        #[test]
        fn each_step_parses_from_its_line() {
            let text = "out ran there\nerr error CS1002: x\n\nrecord C:\\a b\\call.json\r\nwait go\ninterrupt\nkilled\n\
                        status -1\nrefuse rebooted\nfail-start no pwsh\n";
            assert_eq!(
                parse(text),
                Ok(vec![
                    Step::Out("ran there".to_owned()),
                    Step::Err("error CS1002: x".to_owned()),
                    Step::Record(PathBuf::from(r"C:\a b\call.json")),
                    Step::Wait(PathBuf::from("go")),
                    Step::Interrupt,
                    Step::Killed,
                    Step::Status(-1),
                    Step::Refuse(FallbackReason::Rebooted),
                    Step::FailStart("no pwsh".to_owned()),
                ])
            );
            assert_eq!(parse(""), Ok(Vec::new()), "no steps");
        }

        #[test]
        fn a_line_that_is_no_step_is_named_by_its_number() {
            assert_eq!(parse("out a\njump 3\n"), Err("line 2: unknown step 'jump'".to_owned()));
            assert_eq!(parse("status x"), Err("line 1: status takes a whole number, got 'x'".to_owned()));
            assert_eq!(parse("refuse tired"), Err("line 1: refuse takes a reason, got 'tired'".to_owned()));
            assert_eq!(
                parse("\nrecord\n"),
                Err("line 2: unknown step, or a step without its value: 'record'".to_owned())
            );
        }
    }
}
