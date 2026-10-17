//! The remote client a routed run calls: the call it is given, the outcome it hands back, and the clients there are.
//! A routed run that no test seam gives a client calls the Rust client over ssh, which looks for ssh when the run first
//! routes; a run whose ssh cannot be found falls back `client`. A gate built with the
//! `test-seams` feature has the scripted client `--test-remote-client` names, which runs a step file in this process,
//! and the client of `--test-remote-snapshot`, which stops once it has made the snapshot, and the Rust client of
//! `--test-remote-transport`, which runs the command on a builder of the local transport. [`git`] runs git for the
//! routing decision and the client; [`snapshot`] makes the snapshot and bridge commits; `client` runs the Rust
//! client's phases, `transport` reaches the builder, `payload` builds what `gate box` is sent, `retry` holds the rule
//! a call is retried on, `outputs` builds the outputs manifest the run call carries, keeps the outputs pending marker
//! and reads the box's outputs line, and `syncback` fetches and applies the changes the command left on the builder.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use gate_args::TestSeams;
use gate_router::FolderId;
use gate_state::v1::WarmFailure;
use gate_state::{FallbackReason, StateRoot, Verdict};
use gate_win32::{Interrupt, OwnedMutexGuard};

use crate::durations::Usual;

#[cfg(feature = "test-seams")]
pub(crate) use client::local_transport;
pub(crate) use client::{CREATED, NOT_RECOGNIZED, USAGE as BOX_USAGE, local_sdk_listing, other_build, outside_worktree, sdks_of, their_version};
pub(crate) use retry::SSH_FAILED;

pub(crate) use carried::{Sibling, carried};

mod carried;
mod client;

pub(crate) use client::KEY_WAIT_SLICE;
pub(crate) mod git;
mod held;
mod inputs;
mod install;
mod launch;
mod outputs;
pub(crate) mod parity;
pub(crate) mod payload;
mod retry;
pub(crate) mod siblings;
mod snapshot;
mod syncback;
#[cfg(feature = "test-seams")]
mod tamper;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "gate check reads the manifest and a routed run reads the Godot SDK pins beside it; nothing outside \
                  the tests reads the dotnet tools read beside it"
    )
)]
pub(crate) mod toolchain;
pub(crate) mod transport;
mod watchdog;
mod worktree_lock;

/// The status a client hands back with a refusal's reason.
pub(crate) const REFUSED: i64 = 75;
/// The status of a run whose client failed.
const FAILED: i64 = 1;
/// What a client failure says: `gate: the remote run failed: <why>`.
const FAILED_RUN: &str = "gate: the remote run failed";

/// The line the box says when another run holds the work folder `key`, just after its refusal tag.
pub(crate) fn busy_line(key: &str) -> String {
    format!("gate: remote work folder {key} is busy")
}

/// Why the box refused a run, as its refusal tag names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// Another run holds the work folder.
    Busy,
    /// The work folder's dotnet tools could not be restored.
    Tools,
    /// The work folder could not be cleared or laid out for the run's repos.
    Layout,
    /// The work folder does not hold the inputs the run's `InputsDigest` names.
    Inputs,
}

impl Refusal {
    /// The word the refusal tag names it by.
    fn word(self) -> &'static str {
        match self {
            Refusal::Busy => "busy",
            Refusal::Tools => "tools",
            Refusal::Layout => "layout",
            Refusal::Inputs => "inputs",
        }
    }
}

/// The line the box says when it could not clear or lay out the work folder of the repo `repo` for `why`, just after
/// its refusal tag; the PC passes it on as it runs the command itself.
pub(crate) fn layout_line(repo: &str, why: &str) -> String {
    format!("gate: not routed: the builder could not clear {repo}'s work folder: {why}")
}

/// The start marker the box says on stderr just before it starts the command of the run whose payload carries `nonce`:
/// `gate-remote-started: <nonce>`. A call that said it is past the banner, so the client never makes it again.
///
/// The nonce only tells runs apart: it is no secret, as it is on the box's and ssh's command lines. That a command
/// cannot forge a refusal rests on the box saying this line before it creates the command's process, and on the
/// client ignoring every refusal tag once it has seen this line, whatever order the two came in.
pub(crate) fn started_line(nonce: &str) -> String {
    format!("gate-remote-started: {nonce}")
}

/// The refusal tag the box says on stderr just before the human line of a refusal of the run whose payload carries
/// `nonce`: `gate-remote-refused: <nonce> busy|tools|layout`. The client reads a refusal from this line alone.
///
/// The nonce is no secret, as [`started_line`] says: a command can say this line too, and the client ignores it
/// because the box's start marker came before anything the command says.
pub(crate) fn refused_line(nonce: &str, refusal: Refusal) -> String {
    format!("gate-remote-refused: {nonce} {}", refusal.word())
}

/// Why the box killed the command of a run itself: its load-adjusted ceiling, or the backstop of five ceilings of wall
/// time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BoxKill {
    /// The command's load-adjusted clock reached its ceiling.
    TimedOut,
    /// The command ran five ceilings of wall time, its clock not having reached the ceiling, or its sampler failed.
    Backstop,
}

impl BoxKill {
    /// The verdict a run the box killed for this is journalled with.
    pub(crate) fn verdict(self) -> Verdict {
        match self {
            BoxKill::TimedOut => Verdict::TimedOut,
            BoxKill::Backstop => Verdict::Backstop,
        }
    }

    /// The word the kill tag names it by, the verdict it gives the run.
    pub(crate) fn word(self) -> &'static str {
        match self {
            BoxKill::TimedOut => "timed-out",
            BoxKill::Backstop => "backstop",
        }
    }
}

/// The kill tag the box says on stderr just after its kill line, when it killed the command of the run whose payload
/// carries `nonce` itself: `gate-remote-killed: <nonce> timed-out|backstop`. The client reads the box's kill from this
/// line alone, as a status 124 is also what a stopped job or the command itself may end with.
pub(crate) fn killed_line(nonce: &str, kill: BoxKill) -> String {
    format!("gate-remote-killed: {nonce} {}", kill.word())
}

/// The line the box says on stderr once its watch of the command of the run whose payload carries `nonce` ended and its
/// readers drained: the load-adjusted seconds it counted, to three decimals, as the journal keeps them,
/// `gate-remote-adjusted: <nonce> <s.sss>`. Never said once its sampler failed.
pub(crate) fn adjusted_line(nonce: &str, seconds: f64) -> String {
    format!("gate-remote-adjusted: {nonce} {seconds:.3}")
}

/// The prefix of the adjusted line ([`adjusted_line`]) of the run whose payload carries `nonce`, its seconds after it.
pub(crate) fn adjusted_prefix(nonce: &str) -> String {
    format!("gate-remote-adjusted: {nonce} ")
}

/// What runs a routed command on the builder.
pub(crate) trait RemoteClient {
    /// Takes the lock of the call's work-folder key, which the run holds from before it takes a remote slot until its
    /// remote attempt ends: `Ok(None)` for a client that takes none. `Err(None)` when another run of the worktree holds
    /// the lock, which the run waits for ([`RemoteClient::hold_key_waiting`]); else the line saying why it could not be
    /// opened or taken.
    fn hold_key(&self, _call: &RemoteCall<'_>) -> Result<Option<OwnedMutexGuard>, Option<String>> {
        Ok(None)
    }

    /// Takes the lock of the call's work-folder key as [`RemoteClient::hold_key`] does, waiting up to `wait` for another
    /// holder to let it go: `Held(None)` for a client that takes none. `Err` with the line saying why it could not be
    /// opened or taken.
    fn hold_key_waiting(&self, _call: &RemoteCall<'_>, _wait: Duration) -> Result<KeyWait, String> {
        Ok(KeyWait::Held(None))
    }

    /// Whether the client can start, checked before the run takes a key lock, a ticket or a remote slot. `Err` with why
    /// a part it starts is missing, which the run's `client` fallback line names as it falls back at once.
    fn ready(&mut self) -> Result<(), String> {
        Ok(())
    }

    /// Runs the call to its end, or to its refusal, on the run's thread.
    fn run(&mut self, call: &RemoteCall<'_>) -> RemoteOutcome;
}

/// How a client's wait for the work-folder key's lock ended.
pub(crate) enum KeyWait {
    /// The lock is held, its guard `None` for a client that takes none.
    Held(Option<OwnedMutexGuard>),
    /// Another holder still had it at the end of the wait.
    Busy,
    /// The console interrupt was raised during the wait.
    Interrupted,
}

/// What the client is asked to run, and where: the route's facts, the run's log and limits, and the interrupt that
/// stops it.
#[cfg_attr(
    not(feature = "test-seams"),
    expect(
        dead_code,
        reason = "the Rust client leaves the tail and the markers to the gate's verdict, and only the scripted client records them"
    )
)]
pub(crate) struct RemoteCall<'a> {
    pub host: &'a str,
    pub key: &'a str,
    pub root: &'a str,
    /// The id the route's decision read for the worktree root's folder, following links: the worktree lock refuses a
    /// root whose folder has another id at use.
    pub root_id: FolderId,
    /// The worktree root's folder name.
    pub leaf: &'a str,
    /// The siblings the run carries, as the route's decision allowed and checked them, in declared order ([`carried`]).
    pub siblings: Vec<Sibling<'a>>,
    pub repo: &'a str,
    pub subfolder: &'a str,
    /// The words the builder runs.
    pub words: &'a [String],
    /// The output globs the run brings back.
    pub outputs: &'a [String],
    /// Whether the route withheld the run's outputs, its command a dotnet `--no-build` one whose word the builder
    /// dropped: the builder builds, and nothing comes back.
    pub outputs_withheld: bool,
    /// The input globs the run sends: the repo's `inputs` list, none when it has no entry.
    pub inputs: &'a [String],
    /// The `--bring-tracked` globs: a fetched output git tracks is installed only when one matches it.
    pub bring_tracked: &'a [String],
    /// The caller's `--env` pairs, sent in the run call's `Env` for the command on the builder.
    pub env: &'a [(String, String)],
    /// The log the client writes the run's output to.
    pub log: &'a Path,
    pub timeout: u64,
    /// The usual p90 of the run's remote take, which the run call's hang hint compares with; `None` without history.
    pub usual: Option<Usual>,
    pub stall: u64,
    pub tail: u64,
    pub no_markers: bool,
    /// The console interrupt, which the client sees raised and stops what it started on; `None` when it could not be
    /// installed.
    pub interrupt: Option<Interrupt>,
    /// The run's state root, whose hash names the work-folder key's lock.
    pub state: &'a StateRoot,
    /// The state root's folder, which holds the work-folder key's outputs pending marker.
    pub state_dir: &'a Path,
    /// What the call asks of the work folder's warm-up.
    pub warmth: Warmth<'a>,
}

/// What a call asks of the work folder's warm-up: nothing; that the builder's preflight say the work folder is warm
/// with the warm-up file of this hash, else the run falls back `cold`; or that the call be the warm-up itself, which
/// runs `commands` from the warm-up file of `hash` in place of a command and brings nothing back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Warmth<'a> {
    Unasked,
    Needs(&'a str),
    WarmUp { commands: &'a [Vec<String>], hash: &'a str },
}

/// How a warm-up ended, as the client classes it: warm, the box having confirmed the hash it was sent; failed, with
/// the class that sets when the next one may start and why, a text built from the end alone and never from what the
/// commands said; or interrupted, which leaves no record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WarmEnd {
    Warm,
    Failed { failure: WarmFailure, reason: String },
    Deleted,
}

/// What the client hands back: the status, a refusal's reason, whether it killed the run, the lines it wrote to the
/// log, why it could not start, and whether the run's outputs were brought back.
#[derive(Debug, Default)]
pub(crate) struct RemoteOutcome {
    pub status: i64,
    pub reason: Option<FallbackReason>,
    pub killed: bool,
    pub lines: Vec<String>,
    /// Why the client could not start its run, or could not say its lines.
    pub detail: Option<String>,
    /// Whether the run's outputs were brought back: `Some(true)` only when the run's outputs step installed its archive
    /// whole, or the box packed nothing for a PC that held outputs, and in either case the box skipped no name this PC
    /// cannot take: a skipped name makes it `Some(false)` on both paths. `None` when the client cannot tell, which
    /// counts as not brought back.
    pub brought_back: Option<bool>,
    /// How long the run's parts took, which a passed line says.
    pub split: Split,
    /// The load-adjusted seconds the box counted for the command, `None` when it said none.
    pub adjusted: Option<f64>,
    /// The kill the box's kill tag named, when the box killed the command itself; the run's verdict comes from it,
    /// never from the log. `None` for any other end, a kill of the client's own included.
    pub box_killed: Option<BoxKill>,
    /// How a warm-up ended; `None` for a call that is no warm-up.
    pub warm: Option<WarmEnd>,
}

/// How long a routed run's parts took: the run call, the command's own time, which its ceiling counts; the push; and the
/// inputs calls, the sync-back's fetch and the outputs fetch, each `None` when the run made none. A client that times
/// none of them leaves them all at zero and `None`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Split {
    pub command: Duration,
    pub push: Duration,
    pub inputs: Option<Duration>,
    pub sync_back: Option<Duration>,
    pub outputs: Option<Duration>,
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
    pub(crate) fn not_started(why: String) -> RemoteOutcome {
        RemoteOutcome {
            status: REFUSED,
            reason: Some(FallbackReason::Client),
            detail: Some(why),
            ..RemoteOutcome::default()
        }
    }
}

/// Appends each of `lines` to `log` and says them on `stream`: the one log-and-console writer the clients say their
/// lines with.
fn say<S: AsRef<str>>(log: &Path, lines: &[S], mut stream: impl Write) -> Result<(), String> {
    let cannot = |error: io::Error| format!("cannot write to {}: {error}", log.display());
    let mut file = OpenOptions::new().append(true).create(true).open(log).map_err(cannot)?;
    for line in lines {
        writeln!(file, "{}", line.as_ref()).map_err(cannot)?;
        writeln!(stream, "{}", line.as_ref()).map_err(|error| format!("cannot say a line: {error}"))?;
    }
    stream.flush().map_err(|error| format!("cannot say a line: {error}"))
}

/// The client `seams` name: the one of `--test-remote-snapshot`, else the Rust client of `--test-remote-transport`,
/// else the scripted one of `--test-remote-client`, else the Rust client over ssh; or the line saying why its
/// transport or its step file cannot be used.
#[cfg(feature = "test-seams")]
pub(crate) fn chosen(seams: &TestSeams) -> Result<Box<dyn RemoteClient>, String> {
    if seams.remote_snapshot {
        let limit = git::limit(seams);
        return Ok(Box::new(snapshot_only::SnapshotOnly { limit }));
    }
    if let Some(transport) = &seams.remote_transport {
        return client::RustClient::local(seams, transport).map(|client| Box::new(client) as Box<dyn RemoteClient>);
    }
    if let Some(path) = &seams.remote_client {
        return scripted::ScriptedClient::load(path).map(|client| Box::new(client) as Box<dyn RemoteClient>);
    }
    Ok(Box::new(client::RustClient::ssh(seams)))
}

/// The client `seams` name: the Rust client over ssh, in a gate built without the test seams.
#[cfg(not(feature = "test-seams"))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the signature of the test-seams build, whose step file can fail to load"
)]
pub(crate) fn chosen(seams: &TestSeams) -> Result<Box<dyn RemoteClient>, String> {
    Ok(Box::new(client::RustClient::ssh(seams)))
}

/// The client of `--test-remote-snapshot`: makes the snapshot and bridge commits of each of the call's worktrees, the
/// main one first, then a pair's siblings in declared order, and says each as `gate: remote snapshot <commit> bridge
/// <commit or none>`, one line a worktree, on stderr and in the log; the run ends with status 0,
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
            let roots: Vec<PathBuf> = std::iter::once(call.root)
                .chain(call.siblings.iter().map(super::Sibling::root))
                .map(PathBuf::from)
                .collect();
            let (status, lines) = match snapshot::take(&roots, call.key, self.limit, call.interrupt) {
                Ok(snapshots) => (0, snapshots.iter().map(said).collect()),
                // An interrupt is no failure, so it says no line: the run is the interrupt, as the client's `wait`.
                Err(GitError::Interrupted) => (i64::from(CONTROL_C_EXIT.cast_signed()), Vec::new()),
                Err(GitError::Failed(why) | GitError::OverCap(why)) => (FAILED, vec![format!("{FAILED_RUN}: {why}")]),
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
                root_id: gate_router::FolderId { volume: 0, file: [0; 16] },
                leaf: "app",
                siblings: Vec::new(),
                repo: "app",
                subfolder: "",
                words: &[],
                outputs: &[],
                outputs_withheld: false,
                inputs: &[],
                log,
                timeout: 3600,
                usual: None,
                stall: 0,
                tail: 0,
                no_markers: false,
                bring_tracked: &[],
                env: &[],
                interrupt,
                state: &STATE,
                state_dir: Path::new(r"C:\gate-snapshot-client-tests"),
                warmth: crate::remote::Warmth::Unasked,
            }
        }

        #[test]
        fn an_interrupted_snapshot_ends_the_run_as_an_interrupt() {
            let interrupt = Interrupt::install().expect("the console handler is installed");
            interrupt.raise();
            let log = crate::test_support::temp_dir().join(format!("gate-snapshot-interrupt-{}.log", std::process::id()));

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
/// <reason>` refuses with status 75; `fail-start <why>` ends the run as a client that could not start; `warmed`,
/// `warm-failed commands|refused <reason>` and `warm-deleted` set how a warm-up ended.
#[cfg(feature = "test-seams")]
mod scripted {
    use std::fs;
    use std::io::{self, Write};
    use std::path::{Path, PathBuf};
    use std::thread;
    use std::time::Duration;

    use gate_state::FallbackReason;
    use gate_state::v1::WarmFailure;
    use gate_win32::Interrupt;
    use serde_json::{Value, json};

    use super::{BoxKill, FolderId, REFUSED, RemoteCall, RemoteClient, RemoteOutcome, Sibling, WarmEnd, Warmth};
    use crate::run::CONTROL_C_EXIT;

    /// How often `wait` looks for its path and the interrupt.
    const WAIT_STEP: Duration = Duration::from_millis(20);
    /// Every reason a `refuse` step may give.
    const REASONS: [FallbackReason; 11] = [
        FallbackReason::Full,
        FallbackReason::Busy,
        FallbackReason::Unreachable,
        FallbackReason::Parity,
        FallbackReason::Rebooted,
        FallbackReason::Tools,
        FallbackReason::Layout,
        FallbackReason::Version,
        FallbackReason::Client,
        FallbackReason::Inputs,
        FallbackReason::Cold,
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
        Adjusted(f64),
        BoxKilled(BoxKill),
        Warmed,
        WarmFailed(WarmFailure, String),
        WarmDeleted,
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
            "warmed" => Ok(Step::Warmed),
            "warm-deleted" => Ok(Step::WarmDeleted),
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
            "adjusted" => operand
                .parse()
                .map(Step::Adjusted)
                .map_err(|_| format!("adjusted takes seconds, got '{operand}'")),
            "box-killed" => [BoxKill::TimedOut, BoxKill::Backstop]
                .into_iter()
                .find(|kill| kill.word() == operand)
                .map(Step::BoxKilled)
                .ok_or_else(|| format!("box-killed takes timed-out or backstop, got '{operand}'")),
            "refuse" => REASONS
                .into_iter()
                .find(|reason| reason.as_str() == operand)
                .map(Step::Refuse)
                .ok_or_else(|| format!("refuse takes a reason, got '{operand}'")),
            "warm-failed" => warm_failed(operand),
            other => Err(format!("unknown step '{other}'")),
        }
    }

    /// The `warm-failed` step of `operand`: `commands` or `refused`, then the reason.
    fn warm_failed(operand: &str) -> Result<Step, String> {
        let (failure, reason) = match operand.split_once(' ') {
            Some(("commands", reason)) if !reason.is_empty() => (WarmFailure::Commands, reason),
            Some(("refused", reason)) if !reason.is_empty() => (WarmFailure::Refused, reason),
            _ => return Err(format!("warm-failed takes commands or refused and a reason, got '{operand}'")),
        };
        Ok(Step::WarmFailed(failure, reason.to_owned()))
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
            Step::Adjusted(seconds) => outcome.adjusted = Some(*seconds),
            Step::BoxKilled(kill) => {
                outcome.killed = true;
                outcome.box_killed = Some(*kill);
            }
            Step::Refuse(reason) => {
                outcome.status = REFUSED;
                outcome.reason = Some(*reason);
            }
            Step::FailStart(why) => {
                *outcome = RemoteOutcome::not_started(why.clone());
                return false;
            }
            Step::Warmed => outcome.warm = Some(WarmEnd::Warm),
            Step::WarmFailed(failure, reason) => {
                outcome.warm = Some(WarmEnd::Failed {
                    failure: *failure,
                    reason: reason.clone(),
                });
            }
            Step::WarmDeleted => outcome.warm = Some(WarmEnd::Deleted),
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
            "rootId": recorded_id(call.root_id),
            "leaf": call.leaf,
            "siblings": call.siblings.iter().map(recorded_sibling).collect::<Vec<Value>>(),
            "repo": call.repo,
            "subfolder": call.subfolder,
            "words": call.words,
            "outputs": call.outputs,
            "inputs": call.inputs,
            "log": call.log.to_string_lossy(),
            "timeout": call.timeout,
            "stall": call.stall,
            "tail": call.tail,
            "noMarkers": call.no_markers,
            "env": call.env,
            "warmth": recorded_warmth(call.warmth),
        });
        fs::write(path, recorded.to_string()).map_err(|error| format!("cannot record the call to {}: {error}", path.display()))?;
        Ok(true)
    }

    /// What the call asks of the warm-up, as the record writes it: `null` for nothing, `{"needs": <hash>}`, or
    /// `{"warmUp": {"commands": [[<word>…]…], "hash": <hash>}}`.
    fn recorded_warmth(warmth: Warmth<'_>) -> Value {
        match warmth {
            Warmth::Unasked => Value::Null,
            Warmth::Needs(hash) => json!({"needs": hash}),
            Warmth::WarmUp { commands, hash } => json!({"warmUp": {"commands": commands, "hash": hash}}),
        }
    }

    /// A carried sibling as the record writes it: its name, its root, and its folder's id ([`recorded_id`]).
    fn recorded_sibling(sibling: &Sibling<'_>) -> Value {
        json!({
            "name": sibling.name(),
            "root": sibling.root(),
            "id": recorded_id(sibling.id()),
        })
    }

    /// A folder's id as the record writes it: the volume serial's 16 hex digits, `-`, and the file id's 32.
    fn recorded_id(id: FolderId) -> String {
        format!("{:016x}-{:032x}", id.volume, u128::from_be_bytes(id.file))
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
        use gate_state::v1::WarmFailure;

        use super::{Step, parse};

        #[test]
        fn each_step_parses_from_its_line() {
            let text = "out ran there\nerr error CS1002: x\n\nrecord C:\\a b\\call.json\r\nwait go\ninterrupt\nkilled\n\
                        status -1\nrefuse rebooted\nfail-start no pwsh\nrefuse cold\nwarmed\n\
                        warm-failed commands command stopped with status 2\nwarm-failed refused the builder refused: busy\n\
                        warm-deleted\n";
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
                    Step::Refuse(FallbackReason::Cold),
                    Step::Warmed,
                    Step::WarmFailed(WarmFailure::Commands, "command stopped with status 2".to_owned()),
                    Step::WarmFailed(WarmFailure::Refused, "the builder refused: busy".to_owned()),
                    Step::WarmDeleted,
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
