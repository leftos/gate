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
use gate_router::{Builder, CrossBuild, FolderId, SendPlan, Target, escaped};
use gate_state::v1::WarmFailure;
use gate_state::{FallbackReason, StateRoot, Verdict};
use gate_win32::{Interrupt, OwnedMutexGuard};
use gate_wire::stamp::{self, Tag};
use gate_wire::{ROOT_BUSY, ROOT_STATUS, ROOT_UNSAFE};

use crate::durations::Usual;

#[cfg(feature = "test-seams")]
pub(crate) use client::local_transport;
pub(crate) use client::{
    CREATED, NEW_WORK_TREE, Warm, local_sdk_listing, no_box, other_build, outside_worktree, sdks_of, shell_said, their_version, warm_of,
};
pub(crate) use retry::SSH_FAILED;

pub(crate) use carried::{Sibling, carried};

mod carried;
mod client;
pub(crate) mod drain;

pub(crate) use client::KEY_WAIT_SLICE;
pub(crate) mod git;
mod held;
pub(crate) mod inputs;
mod install;
pub(crate) mod launch;
pub(crate) mod lfs;
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
/// The status of a run whose command exited 0 but whose transfer after it failed every attempt.
const TRANSFER_FAILED: i64 = 74;
/// What a client failure says: `gate: the remote run failed: <why>`.
const FAILED_RUN: &str = "gate: the remote run failed";

// The protocol lines the box says and the client matches, built once in `gate-wire` for every box.
pub(crate) use gate_wire::{BoxKill, Refusal, adjusted_line, adjusted_prefix, busy_line, killed_line, layout_line, refused_line, started_line};

/// What the box says on standard error, before why, when it refuses to act because the builder's gate install is not
/// trusted: its own program, or the desktop helper a desktop run hands its command to, is not installed where only
/// administrators can change it. The tripwire over the box's own program says it and exits [`INSTALL_STATUS`] before
/// any command starts; the hand-off of a desktop run whose helper is not trusted says it after the run's `install`
/// refusal tag, and the box exits 75. The client and `gate check --builder` read the refusal by this prefix.
pub(crate) const INSTALL_UNTRUSTED: &str = "gate: the builder's gate install is not trusted: ";

/// The exit status of a box that refused to act with [`INSTALL_UNTRUSTED`].
pub(crate) const INSTALL_STATUS: u8 = 1;

/// The box's line refusing to act because its install is not trusted for `why`, shown escaped ([`INSTALL_UNTRUSTED`]).
pub(crate) fn install_untrusted_line(why: &str) -> String {
    format!("{INSTALL_UNTRUSTED}{}", gate_router::escaped(why))
}

/// Why the box refused a call for the builder's gate install, cut to [`git::FAILURE_BYTES`]: the call exited
/// [`INSTALL_STATUS`] and one of `said`, the lines it wrote before any start marker, starts [`INSTALL_UNTRUSTED`];
/// `None` for any other call. The client and `gate check --builder` both read the refusal through it.
pub(crate) fn install_why<'a>(status: i32, said: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    refusal_why(status, INSTALL_STATUS, INSTALL_UNTRUSTED, said)
}

/// Why the box refused a call with the line starting `prefix`, cut to [`git::FAILURE_BYTES`]: the call exited
/// `expected_status` and one of `said`, the lines it wrote, starts `prefix`; `None` for any other call.
fn refusal_why<'a>(status: i32, expected_status: u8, prefix: &str, said: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    if status != i32::from(expected_status) {
        return None;
    }
    said.into_iter()
        .find_map(|line| line.strip_prefix(prefix))
        .map(|why| git::first_bytes(why, git::FAILURE_BYTES))
}

/// Why the box refused its work root, cut to [`git::FAILURE_BYTES`]: the call exited [`gate_wire::ROOT_STATUS`] and one of
/// `said`, the lines it wrote, starts [`gate_wire::ROOT_UNSAFE`]; `None` for any other call, as [`install_why`] reads its
/// own.
pub(crate) fn root_why<'a>(status: i32, said: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    refusal_why(status, ROOT_STATUS, ROOT_UNSAFE, said)
}

/// Why the box found its work root busy, cut to [`git::FAILURE_BYTES`]: the call exited [`gate_wire::ROOT_STATUS`] and one
/// of `said`, the lines it wrote, starts [`gate_wire::ROOT_BUSY`]; `None` for any other call, as [`root_why`] reads an
/// unsafe root.
pub(crate) fn root_busy_why<'a>(status: i32, said: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    refusal_why(status, ROOT_STATUS, ROOT_BUSY, said)
}

/// The verdict a run the box killed for `kill` is journalled with.
pub(crate) fn box_kill_verdict(kill: BoxKill) -> Verdict {
    match kill {
        BoxKill::TimedOut => Verdict::TimedOut,
        BoxKill::Backstop => Verdict::Backstop,
    }
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

    /// Polls the builder's drain for the call of a run waiting behind it, on the run's thread: its preflight alone,
    /// with the run's session and no needs. A draining outcome ([`RemoteOutcome::draining`]) while the drain still
    /// holds the run back, an `unreachable` fallback for a builder the poll could not reach or a poll the client
    /// killed, and any other end once the builder admits the run, which then starts a full attempt ([`RemoteClient::run`]).
    fn poll_drain(&mut self, call: &RemoteCall<'_>) -> RemoteOutcome;
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
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is an independent fact of the route or the request that the client reads by name"
)]
pub(crate) struct RemoteCall<'a> {
    /// The builder the route picked, whose alias is [`RemoteCall::host`]: its profile ([`transport::Transport::profile`]) lays
    /// out the run's paths and words on it, and its id names the PC folder of its per-worktree caches.
    pub builder: &'a Builder,
    pub key: &'a str,
    /// The run's drain session (`Session`), 32 lower-case hex digits made once per run and kept across its attempts
    /// and polls.
    pub session: &'a str,
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
    /// The failure output globs a failed run brings back beside its test reports
    /// ([`gate_router::RemoteRun::failure_outputs`]).
    pub failure_outputs: &'a [String],
    /// Whether [`RemoteCall::words`] are a dotnet command's with `--no-build` dropped ([`gate_router::RemoteRun::dropped_no_build`]).
    pub dropped_no_build: bool,
    /// The route's send of the PC's build, which lets the builder keep a `dotnet test`'s `--no-build`
    /// ([`gate_router::RemoteRun::send`]); `None` for every other run.
    pub send: Option<&'a SendPlan>,
    /// The route's cross build, which a macOS builder runs after the passing command so its outputs come back built for
    /// Windows ([`gate_router::RemoteRun::cross_build`]); `None` for every other run, a warm-up among them.
    pub cross_build: Option<&'a CrossBuild>,
    /// The input globs the run sends: the repo's `inputs` list, none when it has no entry.
    pub inputs: &'a [String],
    /// Whether the run asks for the builder's desktop, as the route's decision judged it; a warm-up never does.
    pub desktop: bool,
    /// The `--bring-tracked` globs: a fetched output git tracks is installed only when one matches it.
    pub bring_tracked: &'a [String],
    /// The variables sent in the run call's `Env` for the command on the builder: the caller's `--env` pairs, then the
    /// gate's `SourceRevisionId` when the route read the worktree's `HEAD`.
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

impl<'a> RemoteCall<'a> {
    /// The alias the call reaches its builder by, the picked builder's own, so the box line and the paths and caches
    /// the call lays out by never name two builders.
    pub(crate) fn host(&self) -> &'a str {
        let builder: &'a Builder = self.builder;
        &builder.alias
    }
}

/// What a call knows of or asks of the work folder's warm-up: nothing, for a repo without warm-up consent or without a
/// warm-up file that parsed; that the repo has both, with the file's hash, which words the cold note of a new work
/// folder; or that the call be the warm-up itself, which runs `commands` from the warm-up file of `hash` in place of a
/// command and brings nothing back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Warmth<'a> {
    Unasked,
    Consented(&'a str),
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
    /// Whether the command exited 0 but a transfer after it failed every attempt, as the TRANSFER FAILED line says: the
    /// run's verdict is `transfer-failed`.
    pub transfer_failed: bool,
    /// How a warm-up ended; `None` for a call that is no warm-up.
    pub warm: Option<WarmEnd>,
    /// The ceiling, in whole seconds, the run call was sent once the builder's preflight found the run's work tree new,
    /// raised from the call's; `None` for a run whose ceiling was its own.
    pub raised: Option<u64>,
    /// Whether the builder was sent the words with `--no-build` dropped ([`RemoteCall::dropped_no_build`]), a run whose
    /// send of the PC's build did not land among them: it built there, and brings nothing back.
    pub dropped_no_build: bool,
    /// What the builder's `drain` refusal said, a status 75 with no reason that is no fallback: the run waits behind
    /// the drain. `None` for any other end.
    pub draining: Option<drain::Drain>,
}

/// How long a routed run's parts took: the run call, the command's own time, which its ceiling counts; the push; and the
/// inputs calls, the LFS calls, the sync-back's fetch and the outputs fetch, each `None` when the run made none. A
/// client that times none of them leaves them all at zero and `None`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Split {
    pub command: Duration,
    pub push: Duration,
    pub inputs: Option<Duration>,
    pub lfs: Option<Duration>,
    /// The send of the PC's build of one project, for a routed `dotnet test --no-build` that has one.
    pub sent_build: Option<Duration>,
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

/// Appends each of `lines` to `log`, stamped now and tagged `gat`, and says them on `stream` as they are: the one
/// log-and-console writer the clients say their lines with.
fn say<S: AsRef<str>>(log: &Path, lines: &[S], mut stream: impl Write) -> Result<(), String> {
    let cannot = |error: io::Error| format!("cannot write to {}: {error}", log.display());
    let mut file = OpenOptions::new().append(true).create(true).open(log).map_err(cannot)?;
    for line in lines {
        writeln!(file, "{}", stamp::stamped(&stamp::now(), Tag::Gat, line.as_ref())).map_err(cannot)?;
        writeln!(stream, "{}", line.as_ref()).map_err(|error| format!("cannot say a line: {error}"))?;
    }
    stream.flush().map_err(|error| format!("cannot say a line: {error}"))
}

/// The texts of the log at `path` as a case asserts on them: each line without its stamp and tag, ended with a line
/// feed. A log not written yet, or not UTF-8, reads as nothing.
#[cfg(all(test, feature = "test-seams"))]
pub(crate) fn log_texts(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut texts = String::with_capacity(text.len());
    for line in text.lines() {
        texts.push_str(stamp::unstamped(line).text);
        texts.push('\n');
    }
    texts
}

/// The client `seams` name: the one of `--test-remote-snapshot`, else the Rust client of `--test-remote-transport`,
/// else the scripted one of `--test-remote-client`, else the Rust client over ssh; or the line saying why its
/// transport or its step file cannot be used.
#[cfg(feature = "test-seams")]
pub(crate) fn chosen(seams: &TestSeams) -> Result<Box<dyn RemoteClient>, String> {
    if seams.remote_snapshot {
        let bounds = git::GitBounds::of(seams);
        return Ok(Box::new(snapshot_only::SnapshotOnly { bounds }));
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

/// Each tree a routed run of `target` is checked out in on its builder, laid out by the profile the remote transport
/// `transport` reaches the builder by ([`transport::builder_profile`]), with the PC root it stands for, paired and
/// spelled as the outputs' install pairs and spells them: the main repo's tree first, then each carried sibling's in
/// carried order. Else why they cannot be told, or why the first PC root holding a character the word cannot carry
/// ([`uncarried`]) leaves out every tree.
pub(crate) fn box_trees(transport: Option<&str>, target: &Target) -> Result<Vec<(String, String)>, String> {
    let profile = transport::builder_profile(&target.builder, transport)?;
    let siblings: Vec<(&str, &str)> = target
        .siblings
        .iter()
        .map(|sibling| (sibling.name.as_str(), sibling.root.as_str()))
        .collect();
    let work = profile.work(&target.key);
    let (main, siblings) = outputs::trees_of(&work, &target.leaf, &target.place.root, &siblings, profile.form());
    std::iter::once(main)
        .chain(siblings)
        .map(|(tree, pc_root)| {
            let spelled = install::pc_spelling(pc_root)
                .map(|spelled| spelled.display().to_string())
                .map_err(|error| format!("cannot read the root {}: {}", escaped(pc_root), escaped(&error.to_string())))?;
            match uncarried(&spelled) {
                Some(held) => Err(format!(
                    "the root {} holds '{}', which the builder's command line cannot carry",
                    escaped(&spelled),
                    escaped(&held.to_string())
                )),
                None => Ok((tree, spelled)),
            }
        })
        .collect()
}

/// The first character of the PC root `root` that the path-map word cannot carry to the builder's compiler unescaped,
/// or `None` when it holds only ASCII letters and digits, `.`, `_`, `-`, `~` and `\`, past a leading drive letter and
/// its colon, or a leading `\\` of a share. A verbatim (`\\?\`) or device (`\\.\`) prefix is refused by its `?` or `.`.
fn uncarried(root: &str) -> Option<char> {
    let rest = if let Some(share) = root.strip_prefix(r"\\") {
        if let Some(prefix @ ('?' | '.')) = share.chars().next() {
            return Some(prefix);
        }
        share
    } else {
        let mut chars = root.chars();
        match (chars.next(), chars.next()) {
            (Some(drive), Some(':')) if drive.is_ascii_alphabetic() => chars.as_str(),
            _ => root,
        }
    };
    rest.chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '~' | '\\')))
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

    use super::git::{GitBounds, GitError};
    use super::snapshot::{self, Snapshot};
    use super::{FAILED, FAILED_RUN, RemoteCall, RemoteClient, RemoteOutcome};
    use crate::run::CONTROL_C_EXIT;

    pub(crate) struct SnapshotOnly {
        /// The bounds of each git call.
        pub(crate) bounds: GitBounds,
    }

    impl RemoteClient for SnapshotOnly {
        /// No builder is called, so none is ever drained: every poll admits the run.
        fn poll_drain(&mut self, _call: &RemoteCall<'_>) -> RemoteOutcome {
            RemoteOutcome::default()
        }

        fn run(&mut self, call: &RemoteCall<'_>) -> RemoteOutcome {
            let roots: Vec<PathBuf> = std::iter::once(call.root)
                .chain(call.siblings.iter().map(super::Sibling::root))
                .map(PathBuf::from)
                .collect();
            let (status, lines) = match snapshot::take(&roots, call.key, call.builder.id.as_deref(), self.bounds, call.interrupt) {
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
                    dropped_no_build: call.dropped_no_build,
                    ..RemoteOutcome::default()
                };
            }
            RemoteOutcome {
                status,
                lines,
                dropped_no_build: call.dropped_no_build,
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
        use crate::remote::git::GIT_BOUNDS;
        use crate::remote::snapshot;
        use crate::remote::{RemoteCall, RemoteClient};
        use crate::run::CONTROL_C_EXIT;

        /// A state root no gate uses, which the snapshot-only client never reads.
        static STATE: LazyLock<StateRoot> = LazyLock::new(|| StateRoot::new(r"C:\gate-snapshot-client-tests", None));

        /// The call the client is given; `root` is never reached by the test of an interrupt, which ends the first git
        /// call before it starts.
        fn call<'a>(root: &'a str, log: &'a Path, interrupt: Option<Interrupt>) -> RemoteCall<'a> {
            RemoteCall {
                cross_build: None,
                builder: &crate::test_support::HOST,
                key: "app-1a2b3c4d",
                session: "0123456789abcdef0123456789abcdef",
                root,
                root_id: gate_router::FolderId { volume: 0, file: [0; 16] },
                leaf: "app",
                siblings: Vec::new(),
                repo: "app",
                subfolder: "",
                words: &[],
                outputs: &[],
                outputs_withheld: false,
                failure_outputs: &[],
                dropped_no_build: false,
                send: None,
                desktop: false,
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

            let outcome = SnapshotOnly { bounds: GIT_BOUNDS }.run(&call(r"C:\no-such-worktree", &log, Some(interrupt)));

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

            let outcome = SnapshotOnly { bounds: GIT_BOUNDS }.run(&call(&shown, &log, None));

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

            let outcome = SnapshotOnly { bounds: GIT_BOUNDS }.run(&call(&shown, &log, None));

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
/// `warm-failed commands|refused <reason>` and `warm-deleted` set how a warm-up ended. `draining <calls> <place|none>
/// <raised> <lapses>` ends each of the client's first `<calls>` calls, runs and drain polls counted together, as the
/// builder's drain refusal with that place and barrier, and nothing else of such a call runs; `draining-at <call>
/// <place|none> <raised> <lapses>` so ends the one call of that number, counted from 1; `wait-at <call> <path>` waits
/// until the path exists at the start of the call of that number, before its drain refusal; a drain poll no drain step
/// ends performs only the `wait`, `interrupt`, `refuse` and `fail-start` steps, and admits the run when none refuses it. `on
/// <alias>` starts a block of steps that run only for a call to that builder; the steps before the first `on` line run
/// for every builder without a block of its own, and an alias has one block at most.
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

    use super::drain::Drain;
    use super::{BoxKill, FolderId, REFUSED, RemoteCall, RemoteClient, RemoteOutcome, Sibling, WarmEnd, Warmth};
    use crate::run::CONTROL_C_EXIT;

    /// How often `wait` looks for its path and the interrupt.
    const WAIT_STEP: Duration = Duration::from_millis(20);
    /// Every reason a `refuse` step may give.
    const REASONS: [FallbackReason; 15] = [
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
        FallbackReason::Desktop,
        FallbackReason::Install,
        FallbackReason::Lfs,
        FallbackReason::Root,
    ];

    #[derive(Debug, Clone, PartialEq)]
    pub(super) enum Step {
        /// Starts the block of steps that run only for a call to this builder alias.
        On(String),
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
        /// Ends this many of the client's first calls as the builder's drain refusal says it.
        Draining(usize, Drain),
        /// Ends the client's call of this number, counted from 1, as the builder's drain refusal says it.
        DrainingAt(usize, Drain),
        /// Waits, before anything else of it, in the client's call of this number until the path exists.
        WaitAt(usize, PathBuf),
    }

    pub(crate) struct ScriptedClient {
        steps: Vec<Step>,
        /// The calls made so far, runs and drain polls together.
        calls: usize,
    }

    impl ScriptedClient {
        /// The client of the step file at `path`, or the line saying why it cannot be read or parsed.
        pub(crate) fn load(path: &Path) -> Result<ScriptedClient, String> {
            let cannot = |why: String| format!("gate: cannot use the remote client's steps {}: {why}", path.display());
            let text = fs::read_to_string(path).map_err(|error| cannot(error.to_string()))?;
            parse(&text).map(|steps| ScriptedClient { steps, calls: 0 }).map_err(cannot)
        }

        /// Counts a call of `call`, performs the `wait-at` steps of its number, and ends it as the builder's drain
        /// refusal when it is among the first calls the host's `draining` step answers or the one a `draining-at` step
        /// names; `None` for any other call whose waits came.
        fn drained(&mut self, call: &RemoteCall<'_>) -> Option<RemoteOutcome> {
            self.calls += 1;
            let calls = self.calls;
            let steps = steps_for(&self.steps, call.host());
            for step in steps {
                if let Step::WaitAt(at, path) = step
                    && *at == calls
                {
                    let mut outcome = RemoteOutcome::default();
                    match perform(&Step::Wait(path.clone()), call, &mut outcome) {
                        Ok(true) => {}
                        Ok(false) => return Some(outcome),
                        Err(why) => return Some(RemoteOutcome::not_started(why)),
                    }
                }
            }
            steps
                .iter()
                .find_map(|step| match step {
                    Step::Draining(drained, drain) if calls <= *drained => Some(*drain),
                    Step::DrainingAt(at, drain) if calls == *at => Some(*drain),
                    _ => None,
                })
                .map(|drain| RemoteOutcome {
                    status: REFUSED,
                    draining: Some(drain),
                    ..RemoteOutcome::default()
                })
        }
    }

    impl RemoteClient for ScriptedClient {
        fn poll_drain(&mut self, call: &RemoteCall<'_>) -> RemoteOutcome {
            if let Some(drained) = self.drained(call) {
                return drained;
            }
            let mut outcome = RemoteOutcome::default();
            for step in steps_for(&self.steps, call.host()) {
                let goes_on = match step {
                    Step::Wait(_) => match perform(step, call, &mut outcome) {
                        Ok(came) => came,
                        Err(why) => return RemoteOutcome::not_started(why),
                    },
                    Step::Interrupt | Step::Refuse(_) | Step::FailStart(_) => settle(step, call, &mut outcome),
                    _ => true,
                };
                if !goes_on {
                    break;
                }
            }
            outcome
        }

        fn run(&mut self, call: &RemoteCall<'_>) -> RemoteOutcome {
            if let Some(drained) = self.drained(call) {
                return drained;
            }
            let mut outcome = RemoteOutcome {
                dropped_no_build: call.dropped_no_build,
                ..RemoteOutcome::default()
            };
            // No script sends the PC's build: a run with a send takes the drop, as the client's does when its send fails,
            // the line kept among the outcome's so a fallback does not say it again.
            if call.send.is_some() {
                if let Err(why) = super::say(call.log, &[gate_router::DROPPED_NO_BUILD_LINE], io::stderr().lock()) {
                    return RemoteOutcome::not_started(why);
                }
                outcome.lines.push(gate_router::DROPPED_NO_BUILD_LINE.to_owned());
            }
            for step in steps_for(&self.steps, call.host()) {
                match perform(step, call, &mut outcome) {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(why) => return RemoteOutcome::not_started(why),
                }
            }
            outcome
        }
    }

    /// The steps of `text`, blank lines skipped, or the first line that is no step, by its number: an `on` line whose
    /// alias an earlier one named is none.
    pub(super) fn parse(text: &str) -> Result<Vec<Step>, String> {
        let mut steps = Vec::new();
        for (index, line) in text.lines().enumerate().filter(|(_, line)| !line.trim().is_empty()) {
            let step = step(line).map_err(|why| format!("line {}: {why}", index + 1))?;
            if let Step::On(alias) = &step
                && steps.contains(&step)
            {
                return Err(format!("line {}: {alias} already has its own steps", index + 1));
            }
            steps.push(step);
        }
        Ok(steps)
    }

    /// The steps of `steps` a call to `host` runs: those after its own `on` line, up to the next `on` line, when it has
    /// one; else those before the first `on` line.
    pub(super) fn steps_for<'s>(steps: &'s [Step], host: &str) -> &'s [Step] {
        let own = steps.iter().position(|step| matches!(step, Step::On(alias) if alias == host));
        let start = own.map_or(0, |at| at + 1);
        let end = steps[start..]
            .iter()
            .position(|step| matches!(step, Step::On(_)))
            .map_or(steps.len(), |at| start + at);
        &steps[start..end]
    }

    fn step(line: &str) -> Result<Step, String> {
        match line.split_once(' ') {
            None => bare_step(line),
            Some((word, operand)) => operand_step(word, operand),
        }
    }

    fn bare_step(word: &str) -> Result<Step, String> {
        match word {
            "on" => Err("on takes a builder's alias".to_owned()),
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
            "draining" => drain_of("draining", operand).map(|(calls, drain)| Step::Draining(calls, drain)),
            "draining-at" => drain_of("draining-at", operand).map(|(call, drain)| Step::DrainingAt(call, drain)),
            "wait-at" => match operand.split_once(' ') {
                Some((call, path)) if !path.is_empty() => call
                    .parse()
                    .map(|call| Step::WaitAt(call, PathBuf::from(path)))
                    .map_err(|_| format!("wait-at takes a call's number and a path, got '{operand}'")),
                _ => Err(format!("wait-at takes a call's number and a path, got '{operand}'")),
            },
            "on" if !operand.is_empty() && !operand.contains(' ') => Ok(Step::On(operand.to_owned())),
            "on" => Err(format!("on takes one builder's alias, got '{operand}'")),
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

    /// The `draining` or `draining-at` step `name` of `operand`: the number of calls it answers or of the call it
    /// answers, then the place, `none` for none, and the barrier's raise and lapse times in Unix seconds.
    fn drain_of(name: &str, operand: &str) -> Result<(usize, Drain), String> {
        let wrong = || format!("{name} takes calls, a place or none, and the raise and lapse seconds, got '{operand}'");
        let words: Vec<&str> = operand.split(' ').collect();
        let [calls, place, raised, lapses] = words[..] else {
            return Err(wrong());
        };
        let place = match place {
            "none" => None,
            place => Some(place.parse().map_err(|_| wrong())?),
        };
        let drain = Drain {
            place,
            raised: raised.parse().map_err(|_| wrong())?,
            lapses: lapses.parse().map_err(|_| wrong())?,
        };
        Ok((calls.parse().map_err(|_| wrong())?, drain))
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
            Step::Out(_)
            | Step::Err(_)
            | Step::Record(_)
            | Step::Wait(_)
            | Step::On(_)
            | Step::Draining(..)
            | Step::DrainingAt(..)
            | Step::WaitAt(..) => {}
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
            "host": call.host(),
            "key": call.key,
            "session": call.session,
            "root": call.root,
            "rootId": recorded_id(call.root_id),
            "leaf": call.leaf,
            "siblings": call.siblings.iter().map(recorded_sibling).collect::<Vec<Value>>(),
            "repo": call.repo,
            "subfolder": call.subfolder,
            "words": call.words,
            "outputs": call.outputs,
            "inputs": call.inputs,
            "desktop": call.desktop,
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
            Warmth::Consented(hash) => json!({"consented": hash}),
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

        use super::{Step, parse, steps_for};

        /// The steps the file `text` runs for a call to `host`.
        fn run_for(text: &str, host: &str) -> Result<Vec<Step>, String> {
            parse(text).map(|steps| steps_for(&steps, host).to_vec())
        }

        #[test]
        fn an_on_block_runs_only_for_a_call_to_its_host() {
            let text = "on mac-1\nrefuse version\non host-a\nstatus 0\n";
            assert_eq!(run_for(text, "mac-1"), Ok(vec![Step::Refuse(FallbackReason::Version)]));
            assert_eq!(run_for(text, "host-a"), Ok(vec![Step::Status(0)]));
            assert_eq!(run_for(text, "host-b"), Ok(Vec::new()), "no shared steps and no block of its own");
        }

        #[test]
        fn a_file_without_an_on_line_runs_every_step_for_every_host() {
            assert_eq!(run_for("refuse full\n", "mac-1"), Ok(vec![Step::Refuse(FallbackReason::Full)]));
            assert_eq!(run_for("refuse full\n", "host-a"), Ok(vec![Step::Refuse(FallbackReason::Full)]));
        }

        #[test]
        fn the_steps_before_the_first_on_line_run_for_a_host_without_a_block() {
            let text = "out hi\non mac-1\nrefuse install\n";
            assert_eq!(run_for(text, "mac-1"), Ok(vec![Step::Refuse(FallbackReason::Install)]), "no out hi");
            assert_eq!(run_for(text, "host-a"), Ok(vec![Step::Out("hi".to_owned())]));
        }

        #[test]
        fn an_on_line_takes_exactly_one_alias_never_twice() {
            assert_eq!(parse("out a\non\n"), Err("line 2: on takes a builder's alias".to_owned()));
            assert_eq!(
                parse("on mac-1 extra\n"),
                Err("line 1: on takes one builder's alias, got 'mac-1 extra'".to_owned())
            );
            assert_eq!(
                parse("on mac-1\nstatus 0\non mac-1\n"),
                Err("line 3: mac-1 already has its own steps".to_owned())
            );
        }

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

#[cfg(test)]
mod tests {
    use gate_wire::{ROOT_BUSY, ROOT_STATUS, ROOT_UNSAFE, root_busy_line, root_unsafe_line};

    use super::{INSTALL_UNTRUSTED, git, root_busy_why, root_why};
    use crate::boxside::WORK_ROOT_STEP;

    #[test]
    fn the_root_line_names_why_escaped_and_the_installer_step() {
        assert_eq!(
            root_unsafe_line("D:\\gate-remote lets S-1-5-11 add files to it\n", WORK_ROOT_STEP),
            "gate: the builder's work root is not locked down: D:\\gate-remote lets S-1-5-11 add files to it\\n; run the \
             installer's \"Gate work root\" step (install-toolchains.ps1 -Only \"Gate work root\")"
        );
    }

    #[test]
    fn the_busy_root_line_names_why_escaped_and_no_installer_step() {
        assert_eq!(
            root_busy_line("D:\\gate-remote is open in another process; retry when it is closed\n"),
            "gate: the builder's work root is busy: D:\\gate-remote is open in another process; retry \
             when it is closed\\n"
        );
    }

    #[test]
    fn a_root_refusal_is_read_from_its_line_at_its_status() {
        let line = root_unsafe_line(r"D:\gate-remote has no access list", WORK_ROOT_STEP);
        let said = ["gate: remote version 1.0.0", line.as_str()];

        assert_eq!(root_why(i32::from(ROOT_STATUS), said), line.strip_prefix(ROOT_UNSAFE));
    }

    #[test]
    fn another_status_or_no_root_line_is_no_root_refusal() {
        let line = root_unsafe_line(r"D:\gate-remote is a reparse point", WORK_ROOT_STEP);
        assert_eq!(root_why(2, [line.as_str()]), None, "another status");
        assert_eq!(root_why(0, [line.as_str()]), None, "a pass");
        let install = format!("{INSTALL_UNTRUSTED}the helper is owned by S-1-5-21-1");
        assert_eq!(root_why(i32::from(ROOT_STATUS), [install.as_str()]), None, "the install refusal");
        assert_eq!(root_why(i32::from(ROOT_STATUS), Vec::<&str>::new()), None, "nothing said");
    }

    #[test]
    fn a_root_why_is_cut_to_the_failure_bytes() {
        let long = format!("{ROOT_UNSAFE}{}", "x".repeat(git::FAILURE_BYTES + 10));
        assert_eq!(root_why(1, [long.as_str()]).map(str::len), Some(git::FAILURE_BYTES));
    }

    #[test]
    fn a_busy_root_is_read_from_its_line_at_its_status() {
        let line = root_busy_line(r"D:\gate-remote is open in another process; retry when it is closed");
        let said = ["gate: remote version 1.0.0", line.as_str()];

        assert_eq!(root_busy_why(i32::from(ROOT_STATUS), said), line.strip_prefix(ROOT_BUSY));
    }

    #[test]
    fn another_status_or_no_busy_line_is_no_busy_root() {
        let line = root_busy_line(r"D:\gate-remote is open in another process; retry when it is closed");
        assert_eq!(root_busy_why(2, [line.as_str()]), None, "another status");
        assert_eq!(root_busy_why(0, [line.as_str()]), None, "a pass");
        let unsafe_root = root_unsafe_line(r"D:\gate-remote is a reparse point", WORK_ROOT_STEP);
        assert_eq!(root_busy_why(i32::from(ROOT_STATUS), [unsafe_root.as_str()]), None, "the unsafe root");
        assert_eq!(
            root_why(i32::from(ROOT_STATUS), [line.as_str()]),
            None,
            "a busy root is not an unsafe one"
        );
        assert_eq!(root_busy_why(i32::from(ROOT_STATUS), Vec::<&str>::new()), None, "nothing said");
    }

    #[test]
    fn a_busy_why_is_cut_to_the_failure_bytes() {
        let long = format!("{ROOT_BUSY}{}", "x".repeat(git::FAILURE_BYTES + 10));
        assert_eq!(root_busy_why(1, [long.as_str()]).map(str::len), Some(git::FAILURE_BYTES));
    }
}
