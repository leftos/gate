//! The remote client a routed run calls: the call it is given, the outcome it hands back, and the clients there are.
//! With no client, a routed run falls back here with reason `client`. A gate built with the `test-seams` feature has
//! the scripted client `--test-remote-client` names, which runs a step file in this process, and the client of
//! `--test-remote-snapshot`, which stops once it has made the snapshot, and the Rust client of
//! `--test-remote-transport`, which runs the command on a builder. [`git`] runs git for the routing decision and the
//! client; [`snapshot`] makes the snapshot and bridge commits; `client` runs the Rust client's phases, `transport`
//! reaches the builder, `payload` builds what `gate box` is sent, `retry` holds the rule a call is retried on, and
//! `syncback` fetches and applies the changes the command left on the builder.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;

use gate_args::RunRequest;
use gate_state::{FallbackReason, StateRoot};
use gate_win32::{Interrupt, OwnedMutexGuard};

#[cfg(feature = "test-seams")]
mod client;
pub(crate) mod git;
#[cfg(feature = "test-seams")]
mod payload;
#[cfg(feature = "test-seams")]
mod retry;
#[cfg_attr(
    not(any(test, feature = "test-seams")),
    expect(dead_code, reason = "a gate without the test seams has no client that makes a snapshot")
)]
mod snapshot;
#[cfg(feature = "test-seams")]
mod syncback;
#[cfg(feature = "test-seams")]
mod transport;

/// The status a client hands back with a refusal's reason.
pub(crate) const REFUSED: i64 = 75;
/// Why a routed run's remote client cannot start: none is built yet.
const NO_CLIENT: &str = "the bridge to remote.ps1 is not built yet";
/// The status of a run whose client failed.
#[cfg(feature = "test-seams")]
const FAILED: i64 = 1;
/// What a client failure says: `gate: the remote run failed: <why>`.
#[cfg(feature = "test-seams")]
const FAILED_RUN: &str = "gate: the remote run failed";

/// The line the box says when another run holds the work folder `key`, which the client reads its refusal from.
pub(crate) fn busy_line(key: &str) -> String {
    format!("gate: remote work folder {key} is busy")
}

/// What runs a routed command on the builder.
pub(crate) trait RemoteClient {
    /// Takes the lock of the call's work-folder key, which the run holds from before it takes a remote slot until its
    /// remote attempt ends: `Ok(None)` for a client that takes none. `Err` when the run falls back `busy`: `None` when
    /// another routed run holds the lock, else the line saying why it could not be opened or taken.
    fn hold_key(&self, _call: &RemoteCall<'_>) -> Result<Option<OwnedMutexGuard>, Option<String>> {
        Ok(None)
    }

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
    /// The run's state root, whose hash names the work-folder key's lock.
    pub state: &'a StateRoot,
}

/// What the client hands back: the status, a refusal's reason, whether it killed the run, the lines it wrote to the
/// log, and why it could not start.
#[derive(Debug, Default)]
pub(crate) struct RemoteOutcome {
    pub status: i64,
    pub reason: Option<FallbackReason>,
    pub killed: bool,
    pub lines: Vec<String>,
    /// Why the client could not start its run, or could not say its lines.
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

/// Appends each of `lines` to `log` and says them on `stream`: the one log-and-console writer the clients say their
/// lines with.
#[cfg_attr(not(feature = "test-seams"), expect(dead_code, reason = "only the clients the test seams name say lines"))]
fn say<S: AsRef<str>>(log: &Path, lines: &[S], mut stream: impl Write) -> Result<(), String> {
    let cannot = |error: io::Error| format!("cannot write to {}: {error}", log.display());
    let mut file = OpenOptions::new().append(true).create(true).open(log).map_err(cannot)?;
    for line in lines {
        writeln!(file, "{}", line.as_ref()).map_err(cannot)?;
        writeln!(stream, "{}", line.as_ref()).map_err(|error| format!("cannot say a line: {error}"))?;
    }
    stream.flush().map_err(|error| format!("cannot say a line: {error}"))
}

/// The client `request` names: the one of `--test-remote-snapshot`, else the Rust client of `--test-remote-transport`,
/// else the scripted one of `--test-remote-client`, else none; or the line saying why its transport or its step file
/// cannot be used.
#[cfg(feature = "test-seams")]
pub(crate) fn chosen(request: &RunRequest) -> Result<Option<Box<dyn RemoteClient>>, String> {
    if request.test_seams.remote_snapshot {
        let limit = git::limit(&request.test_seams);
        return Ok(Some(Box::new(snapshot_only::SnapshotOnly { limit })));
    }
    if let Some(transport) = &request.test_seams.remote_transport {
        return client::RustClient::local(request, transport).map(|client| Some(Box::new(client) as Box<dyn RemoteClient>));
    }
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

/// The client of `--test-remote-snapshot`: makes the snapshot and bridge commits of the call's worktree and says each
/// as `gate: remote snapshot <commit> bridge <commit or none>`, on stderr and in the log; the run ends with status 0,
/// nothing pushed. A git failure ends it with status 1 and `gate: the remote run failed: <why>`, with no fallback; a
/// Ctrl+C ends it as an interrupt, as the scripted client's `wait` does; and a run whose lines cannot be said ends with
/// status 1, since its commits are already made.
#[cfg(feature = "test-seams")]
mod snapshot_only {
    use std::io;
    use std::path::PathBuf;
    use std::time::Duration;

    use super::git::GitError;
    use super::snapshot::{self, Snapshot};
    use super::{FAILED, FAILED_RUN, RemoteCall, RemoteClient, RemoteOutcome};
    use crate::run::CONTROL_C_EXIT;

    pub(crate) struct SnapshotOnly {
        /// How long each git call may take.
        pub(crate) limit: Duration,
    }

    impl RemoteClient for SnapshotOnly {
        fn run(&mut self, call: &RemoteCall<'_>) -> RemoteOutcome {
            let roots = vec![PathBuf::from(call.root)];
            let (status, lines) = match snapshot::take(&roots, call.key, self.limit, call.interrupt) {
                Ok(snapshots) => (0, snapshots.iter().map(said).collect()),
                // An interrupt is no failure, so it says no line: the run is the interrupt, as the client's `wait`.
                Err(GitError::Interrupted) => (i64::from(CONTROL_C_EXIT.cast_signed()), Vec::new()),
                Err(GitError::Failed(why)) => (FAILED, vec![format!("{FAILED_RUN}: {why}")]),
            };
            if let Err(why) = super::say(call.log, &lines, io::stderr().lock()) {
                // The snapshot is made and cannot be unmade, so a run that cannot say its lines fails where it is.
                return RemoteOutcome {
                    status: FAILED,
                    lines,
                    detail: Some(why),
                    ..RemoteOutcome::default()
                };
            }
            RemoteOutcome {
                status,
                lines,
                ..RemoteOutcome::default()
            }
        }
    }

    fn said(snapshot: &Snapshot) -> String {
        let bridge = snapshot.bridge.as_deref().unwrap_or("none");
        format!("gate: remote snapshot {} bridge {bridge}", snapshot.commit)
    }

    #[cfg(test)]
    #[expect(clippy::expect_used, reason = "a test that cannot make its repository or its log must fail loudly")]
    mod tests {
        use std::fs;
        use std::path::Path;
        use std::sync::LazyLock;

        use gate_state::StateRoot;
        use gate_win32::Interrupt;

        use super::SnapshotOnly;
        use crate::remote::git::GIT_LIMIT;
        use crate::remote::snapshot;
        use crate::remote::{RemoteCall, RemoteClient};
        use crate::run::CONTROL_C_EXIT;

        /// A state root no gate uses, which the snapshot-only client never reads.
        static STATE: LazyLock<StateRoot> = LazyLock::new(|| StateRoot::new(r"C:\gate-snapshot-client-tests", None));

        /// The call the client is given; `root` is never reached by the test of an interrupt, which ends the first git
        /// call before it starts.
        fn call<'a>(root: &'a str, log: &'a Path, interrupt: Option<Interrupt>) -> RemoteCall<'a> {
            RemoteCall {
                host: "builder",
                key: "app-1a2b3c4d",
                root,
                repo: "app",
                subfolder: "",
                words: &[],
                outputs: &[],
                log,
                timeout: 3600,
                stall: 0,
                tail: 0,
                no_markers: false,
                interrupt,
                state: &STATE,
            }
        }

        #[test]
        fn an_interrupted_snapshot_ends_the_run_as_an_interrupt() {
            let interrupt = Interrupt::install().expect("the console handler is installed");
            interrupt.raise();
            let log = std::env::temp_dir().join(format!("gate-snapshot-interrupt-{}.log", std::process::id()));

            let outcome = SnapshotOnly { limit: GIT_LIMIT }.run(&call(r"C:\no-such-worktree", &log, Some(interrupt)));

            assert_eq!(outcome.status, i64::from(CONTROL_C_EXIT.cast_signed()), "{outcome:?}");
            assert!(outcome.lines.is_empty(), "an interrupt says no failure line: {:?}", outcome.lines);
            assert_eq!(outcome.fallback(), None, "an interrupt is no fallback");
            assert_eq!(fs::read_to_string(&log).expect("the log was made"), "", "nothing is said");
            assert!(fs::remove_file(&log).is_ok(), "the log can be removed");
        }

        #[test]
        fn a_snapshot_that_cannot_say_its_lines_ends_the_run_with_status_1() {
            let root = snapshot::repository("client-write");
            let shown = root.to_string_lossy().into_owned();
            let log = root.join("no-such-folder").join("out.log");

            let outcome = SnapshotOnly { limit: GIT_LIMIT }.run(&call(&shown, &log, None));

            assert_eq!(outcome.status, 1, "{outcome:?}");
            assert_eq!(outcome.fallback(), None, "the snapshot's commits are already made");
            assert!(
                outcome.lines[0].starts_with("gate: remote snapshot "),
                "the snapshot is made: {:?}",
                outcome.lines
            );
            fs::remove_dir_all(&root).expect("the repository can be removed");
        }

        #[test]
        fn a_failed_snapshot_says_the_run_failed_with_one_gate_prefix() {
            let root = std::env::temp_dir().join(format!("gate-snapshot-client-{}-not-a-repo", std::process::id()));
            if root.exists() {
                fs::remove_dir_all(&root).expect("a folder left by an earlier run can be removed");
            }
            fs::create_dir_all(&root).expect("a folder that is no repository can be made");
            let shown = root.to_string_lossy().into_owned();
            let log = root.join("out.log");

            let outcome = SnapshotOnly { limit: GIT_LIMIT }.run(&call(&shown, &log, None));

            assert_eq!(outcome.status, 1, "{outcome:?}");
            assert_eq!(outcome.fallback(), None, "a snapshot that cannot be made falls back nowhere");
            let line = &outcome.lines[0];
            assert!(line.starts_with("gate: the remote run failed: git "), "{line}");
            let why = &line["gate: the remote run failed: ".len()..];
            assert!(!why.starts_with("gate: "), "the reason carries no prefix of its own: {line}");
            fs::remove_dir_all(&root).expect("the folder can be removed");
        }
    }
}

/// The scripted client: a step file run in order, one step a line. `out <text>` and `err <text>` append the text to
/// the log and say it on stdout or stderr; `record <path>` writes the call as one JSON object; `wait <path>` returns
/// when the path exists or the interrupt is raised, the latter ending the run with `STATUS_CONTROL_C_EXIT`;
/// `interrupt` raises the call's interrupt; `killed` marks the run killed; `status <n>` sets the status; `refuse
/// <reason>` refuses with status 75; `fail-start <why>` ends the run as a client that could not start.
#[cfg(feature = "test-seams")]
mod scripted {
    use std::fs;
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
            Step::Out(text) => out(call.log, text, io::stdout().lock(), outcome),
            Step::Err(text) => out(call.log, text, io::stderr().lock(), outcome),
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
    fn out(log: &Path, text: &str, stream: impl Write, outcome: &mut RemoteOutcome) -> Result<bool, String> {
        super::say(log, &[text], stream)?;
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
