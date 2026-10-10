//! The Rust remote client, in the phases the PowerShell client runs: the snapshot; the run call built and checked
//! against the most cmd takes before anything is sent; the preflight and the builder's version; the push and the saved
//! ref; and the run call, its output passed through to the log and the console. A call that failed before the ssh
//! banner is made once more. A Ctrl+C ends the attempt between its calls, during the retry pause, and during a git
//! call. Each call goes through the [`Transport`]; every process starts through [`gate_win32::spawn`]. The run takes
//! the work-folder key's lock, [`hold_key`], before its remote slot, so a second routed run of one worktree falls back
//! `busy` at once.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use gate_args::RunRequest;
use gate_state::{FallbackReason, KernelObject, key_hash};
use gate_win32::{AcquireError, Interrupt, NamedMutex, OwnedMutexGuard};

use super::git::{self, Git, GitError};
use super::payload::{self, Location, RunCall};
use super::retry::{self, Ended, GIT_SSH_FAILED, SSH_FAILED};
use super::snapshot::{self, Snapshot};
use super::transport::{Local, Program, Transport};
use super::{FAILED, FAILED_RUN, REFUSED, RemoteCall, RemoteClient, RemoteOutcome, busy_line};
use crate::run::CONTROL_C_EXIT;

/// The statuses of a preflight whose builder has no `gate box` that takes the run: cmd's for a program it cannot find,
/// and a gate's usage error for a payload it does not read.
const NOT_RECOGNIZED: i32 = 9009;
const USAGE: i32 = 2;
/// How often the retry pause looks at the console interrupt.
const PAUSE_STEP: Duration = Duration::from_millis(20);
/// The box's heartbeat, `gate-remote-heartbeat: <digits>` on standard error: progress, kept out of the log.
const HEARTBEAT: &str = "gate-remote-heartbeat: ";
/// The preflight's lines the client reads.
const VERSION: &str = "gate: remote version ";
const CREATED: &str = "gate: remote created ";
/// This gate's version, which the builder's must be.
const OURS: &str = env!("CARGO_PKG_VERSION");

/// The client of `--test-remote-transport`.
pub(crate) struct RustClient {
    transport: Box<dyn Transport>,
    /// How long each git call of the snapshot and the saved ref may take.
    limit: Duration,
    /// The pause before a retry.
    pause: Duration,
}

impl RustClient {
    /// The client of the local transport `transport` names, with the request's seams; else the line saying why the
    /// transport cannot be used.
    pub(crate) fn local(request: &RunRequest, transport: &str) -> Result<RustClient, String> {
        let seams = &request.test_seams;
        let local = Local::new(transport, seams.remote_ssh.as_deref())?;
        Ok(RustClient {
            transport: Box::new(local),
            limit: git::limit(seams),
            pause: retry::pause(seams),
        })
    }
}

/// The lock of `call`'s work-folder key: a root-hashed kernel mutex taken without waiting, which a routed run holds from
/// before it takes a remote slot until its remote attempt ends. A lock its last holder left without releasing is taken.
/// Else `None` when another routed run holds it, or the line saying why it could not be opened or taken.
pub(crate) fn hold_key(call: &RemoteCall<'_>) -> Result<OwnedMutexGuard, Option<String>> {
    let name = call.state.object_name(KernelObject::Routed { hash: key_hash(call.key) });
    let mutex = NamedMutex::open(&name).map_err(|error| Some(format!("gate: cannot open the mutex {name}: {error}")))?;
    match mutex.acquire_owned(Duration::ZERO) {
        Ok(held) => Ok(held),
        Err(AcquireError::Timeout) => Err(None),
        Err(AcquireError::Failed(error)) => Err(Some(format!("gate: cannot take the mutex {name}: {error}"))),
    }
}

impl RemoteClient for RustClient {
    fn hold_key(&self, call: &RemoteCall<'_>) -> Result<Option<OwnedMutexGuard>, Option<String>> {
        hold_key(call).map(Some)
    }

    fn run(&mut self, call: &RemoteCall<'_>) -> RemoteOutcome {
        let mut attempt = Attempt {
            call,
            transport: self.transport.as_ref(),
            limit: self.limit,
            pause: self.pause,
            lines: Vec::new(),
            problem: None,
        };
        let end = match attempt.phases() {
            Ok(end) | Err(end) => end,
        };
        attempt.outcome(end)
    }
}

/// How the attempt ended.
#[derive(Debug)]
enum End {
    /// The run call ended with this status, the command's own.
    Status(i64),
    /// The builder cannot take the run, for this reason: status 75, and the run falls back here.
    Refused(FallbackReason),
    /// The client failed, for this reason: status 1, with no fallback.
    Failed(String),
    /// The console interrupt ended it.
    Interrupted,
}

/// Where a call's lines go besides the log.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Echo {
    /// To the console stream each came on, as the command's output is.
    Console,
    /// Nowhere else: a preflight's lines.
    LogOnly,
    /// Nowhere else, each line as its carriage-return updates leave it: a push's progress.
    Progress,
}

/// One call's process as it ended: its exit status, whether it wrote on standard output, its first standard error
/// line (a heartbeat included) and its last one (none), and every line it wrote but heartbeats.
struct Called {
    status: i32,
    output_seen: bool,
    first_error: Option<String>,
    last_error: Option<String>,
    lines: Vec<String>,
}

impl Called {
    /// Whether the call failed before the banner, for a program that exits `fail_status` when ssh failed.
    fn before_the_banner(&self, fail_status: i32) -> bool {
        retry::before_the_banner(&Ended {
            status: self.status,
            fail_status,
            output_seen: self.output_seen,
            first_error: self.first_error.as_deref(),
        })
    }
}

/// One run of the client: the call, the way to the builder, the limits, every line the attempt wrote to the log, and
/// the first line that could not be written.
struct Attempt<'c, 'a> {
    call: &'c RemoteCall<'a>,
    transport: &'c dyn Transport,
    limit: Duration,
    pause: Duration,
    lines: Vec<String>,
    problem: Option<String>,
}

impl Attempt<'_, '_> {
    /// The phases in order; `Err` for an end before the run call.
    fn phases(&mut self) -> Result<End, End> {
        let snapshot = self.snapshot()?;
        let location = Location::under(self.transport.root(), self.call.repo, self.call.key, &payload::job_nonce());
        let nonce = payload::run_nonce();
        let run = RunCall {
            words: self.call.words,
            subfolder: self.call.subfolder,
            nonce: &nonce,
            snapshot: &snapshot.commit,
        };
        let run_line = payload::capped("run", payload::line("run", &payload::run_word(&location, &run))).map_err(End::Failed)?;
        let created = self.preflight(&location)?;
        self.push(&snapshot, created)?;
        self.run_call(&run_line)
    }

    /// Goes on unless the console interrupt is raised, which ends the attempt as an interrupt.
    fn go_on(&self) -> Result<(), End> {
        if self.call.interrupt.is_some_and(Interrupt::is_raised) {
            return Err(End::Interrupted);
        }
        Ok(())
    }

    /// The snapshot of the call's worktree.
    fn snapshot(&self) -> Result<Snapshot, End> {
        let roots = [PathBuf::from(self.call.root)];
        snapshot::take(&roots, self.call.key, self.limit, self.call.interrupt)
            .map_err(git_end)?
            .pop()
            .ok_or_else(|| End::Failed("the snapshot made no commit".to_owned()))
    }

    /// The preflight: readies the work folder, and checks the builder runs this gate's version. Whether it created the
    /// bare repository just now. A builder with no `gate box` that takes the run is not routed, as one of another
    /// version is not.
    fn preflight(&mut self, location: &Location) -> Result<bool, End> {
        self.go_on()?;
        let line = payload::capped("preflight", payload::line("preflight", &payload::location_word(location))).map_err(End::Failed)?;
        let (mut called, _) = self.with_retry(SSH_FAILED, |attempt| attempt.ssh(&line, Echo::LogOnly))?;
        let theirs = their_version(&called.lines).map(str::to_owned);
        let created = called.lines.iter().any(|line| line.starts_with(CREATED));
        self.lines.append(&mut called.lines);
        let host = self.call.host;
        match called.status {
            0 => {}
            SSH_FAILED => {
                let last = called.last_error.as_deref().unwrap_or_default();
                self.say(format!("gate: remote {host} unreachable: {last}"));
                return Err(End::Refused(FallbackReason::Unreachable));
            }
            status @ (NOT_RECOGNIZED | USAGE) => {
                self.say(format!(
                    "gate: not routed: {host} has no gate box that takes this run (preflight status {status})"
                ));
                return Err(End::Refused(FallbackReason::Version));
            }
            status => {
                self.say(format!("gate: preparing {} on {host} failed (status {status})", self.transport.root()));
                return Err(End::Status(i64::from(status)));
            }
        }
        if let Some(line) = version_refusal(host, theirs.as_deref(), OURS) {
            self.say(line);
            return Err(End::Refused(FallbackReason::Version));
        }
        Ok(created)
    }

    /// Pushes the snapshot, and its bridge unless the bare repository was `created` just now, with no hooks and no
    /// submodules, then saves the local ref and says the push. A push that fails is `unreachable`.
    fn push(&mut self, snapshot: &Snapshot, created: bool) -> Result<(), End> {
        self.go_on()?;
        let (root, key) = (Path::new(self.call.root), self.call.key);
        let target = self.transport.push_target(self.call.host, self.call.repo);
        let mut args: Vec<String> = ["push", "--no-verify", "--no-recurse-submodules", "--progress"].map(str::to_owned).into();
        args.extend(target.receive_pack.iter().map(|pack| format!("--receive-pack={pack}")));
        args.extend(["--".to_owned(), target.url.clone(), format!("+{}:refs/gate/{key}", snapshot.commit)]);
        let bridge = snapshot.bridge.as_deref().filter(|_| !created);
        args.extend(bridge.map(|bridge| format!("+{bridge}:refs/gate-bridge/{key}")));
        let git = Git::find(root).map_err(End::Failed)?;
        let started = Instant::now();
        let ssh_command = target.ssh_command.as_deref();
        let (mut called, _) = self.with_retry(GIT_SSH_FAILED, |attempt| attempt.git(&git, &args, ssh_command))?;
        let size = push_size(&called.lines);
        self.lines.append(&mut called.lines);
        if called.status != 0 {
            self.say(format!("gate: pushing the snapshot to {} failed (status {})", target.url, called.status));
            return Err(End::Refused(FallbackReason::Unreachable));
        }
        snapshot::save(root, key, &snapshot.commit, self.limit, self.call.interrupt).map_err(git_end)?;
        let short = snapshot.commit.get(..8).unwrap_or(&snapshot.commit);
        self.say(format!("gate: pushed snapshot {short} in {} s ({size})", seconds(started.elapsed())));
        Ok(())
    }

    /// The run call: the command's own status; `unreachable` when it failed before the banner twice, as the command
    /// never started; a refusal when its 75 comes with the box's own refusal line.
    fn run_call(&mut self, line: &str) -> Result<End, End> {
        self.go_on()?;
        let (mut called, again) = self.with_retry(SSH_FAILED, |attempt| attempt.ssh(line, Echo::Console))?;
        let reason = refusal(self.call.key, &called.lines);
        self.lines.append(&mut called.lines);
        if again {
            return Ok(End::Refused(FallbackReason::Unreachable));
        }
        let status = i64::from(called.status);
        match reason {
            Some(reason) if status == REFUSED => Ok(End::Refused(reason)),
            _ => Ok(End::Status(status)),
        }
    }

    /// Makes a call with `make` and, when it failed before the banner, says so, pauses and makes it once more; a Ctrl+C
    /// before the retry or during its pause ends the attempt instead. The call that counts, its lines not yet kept, and
    /// whether the retry failed before the banner too.
    fn with_retry(&mut self, fail_status: i32, mut make: impl FnMut(&mut Self) -> Result<Called, End>) -> Result<(Called, bool), End> {
        let mut first = make(self)?;
        if !first.before_the_banner(fail_status) {
            return Ok((first, false));
        }
        self.lines.append(&mut first.lines);
        self.go_on()?;
        self.say(retry::retry_line(self.call.host));
        paused(self.pause, self.call.interrupt)?;
        self.go_on()?;
        let second = make(self)?;
        let again = second.before_the_banner(fail_status);
        Ok((second, again))
    }

    /// One call of `gate box` on the builder carrying `line`.
    fn ssh(&mut self, line: &str, echo: Echo) -> Result<Called, End> {
        let Program { program, args } = self.transport.call(self.call.host, line);
        let mut command = Command::new(program);
        command.args(args);
        self.started(command, echo)
    }

    /// git with `args` in the worktree, given `ssh_command` as the ssh it runs when there is one.
    fn git(&mut self, git: &Git, args: &[String], ssh_command: Option<&str>) -> Result<Called, End> {
        let mut command = git.command(Path::new(self.call.root));
        command.args(args);
        if let Some(ssh) = ssh_command {
            command.env("GIT_SSH_COMMAND", ssh);
        }
        self.started(command, Echo::Progress)
    }

    /// Starts `command` with no standard input, reads its two streams line by line into the log, and onto the console
    /// as `echo` says, until both end, then waits for it.
    fn started(&mut self, mut command: Command, echo: Echo) -> Result<Called, End> {
        let shown = Path::new(command.get_program()).display().to_string();
        let log = OpenOptions::new()
            .append(true)
            .create(true)
            .open(self.call.log)
            .map_err(|error| End::Failed(format!("cannot write to {}: {error}", self.call.log.display())))?;
        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = gate_win32::spawn(&mut command).map_err(|error| End::Failed(format!("cannot start {shown}: {error}")))?;
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(End::Failed(format!("{shown} was started without its output pipes")));
        };
        let heard = Mutex::new(Heard::new(log));
        thread::scope(|scope| {
            scope.spawn(|| relay(stdout, false, &heard, echo));
            scope.spawn(|| relay(stderr, true, &heard, echo));
        });
        let status = child
            .wait()
            .map_err(|error| End::Failed(format!("cannot wait for {shown}: {error}")))?
            .code()
            .ok_or_else(|| End::Failed(format!("{shown} ended with no exit status")))?;
        let heard = heard.into_inner().unwrap_or_else(PoisonError::into_inner);
        if let Some(problem) = heard.problem {
            return Err(End::Failed(problem));
        }
        Ok(Called {
            status,
            output_seen: heard.output_seen,
            first_error: heard.first_error,
            last_error: heard.last_error,
            lines: heard.lines,
        })
    }

    /// Appends the gate's own `line` to the log and says it on stderr; the first failure to is kept.
    fn say(&mut self, line: String) {
        if let Err(why) = super::say(self.call.log, &[&line], io::stderr().lock()) {
            self.problem.get_or_insert(why);
        }
        self.lines.push(line);
    }

    /// The outcome of an attempt that ended at `end`; a line that could not be written fails the run.
    fn outcome(mut self, end: End) -> RemoteOutcome {
        let (status, reason) = match end {
            End::Status(status) => (status, None),
            End::Refused(reason) => (REFUSED, Some(reason)),
            End::Interrupted => (i64::from(CONTROL_C_EXIT.cast_signed()), None),
            End::Failed(why) => {
                self.say(format!("{FAILED_RUN}: {why}"));
                (FAILED, None)
            }
        };
        let (status, reason) = if self.problem.is_some() { (FAILED, None) } else { (status, reason) };
        RemoteOutcome {
            status,
            reason,
            killed: false,
            lines: self.lines,
            detail: self.problem,
        }
    }
}

/// Waits out `pause` in short steps, ending it when the console `interrupt` is raised.
fn paused(pause: Duration, interrupt: Option<Interrupt>) -> Result<(), End> {
    let deadline = Instant::now() + pause;
    loop {
        if interrupt.is_some_and(Interrupt::is_raised) {
            return Err(End::Interrupted);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        thread::sleep(left.min(PAUSE_STEP));
    }
}

/// A git failure as the attempt's end: an interrupt is the run's interrupt, any other a client failure.
fn git_end(error: GitError) -> End {
    match error {
        GitError::Interrupted => End::Interrupted,
        GitError::Failed(why) => End::Failed(why),
    }
}

/// What one call's two streams said so far, and the log they are written to.
struct Heard {
    log: File,
    lines: Vec<String>,
    output_seen: bool,
    first_error: Option<String>,
    last_error: Option<String>,
    problem: Option<String>,
}

impl Heard {
    fn new(log: File) -> Heard {
        Heard {
            log,
            lines: Vec::new(),
            output_seen: false,
            first_error: None,
            last_error: None,
            problem: None,
        }
    }

    /// Takes one `line` of standard error when `error`, else of standard output: a heartbeat only marks the first error
    /// line; any other is written to the log, said on its stream when `echo` says so, and kept.
    fn take(&mut self, line: &str, error: bool, echo: Echo) {
        if error {
            self.first_error.get_or_insert_with(|| line.to_owned());
        } else {
            self.output_seen = true;
        }
        if error && is_heartbeat(line) {
            return;
        }
        if error {
            self.last_error = Some(line.to_owned());
        }
        if let Err(error) = writeln!(self.log, "{line}") {
            self.problem.get_or_insert_with(|| format!("cannot write to the log: {error}"));
        }
        if echo == Echo::Console {
            let said = if error {
                say_on(io::stderr().lock(), line)
            } else {
                say_on(io::stdout().lock(), line)
            };
            if let Err(error) = said {
                self.problem.get_or_insert_with(|| format!("cannot say a line: {error}"));
            }
        }
        self.lines.push(line.to_owned());
    }
}

fn say_on(mut stream: impl Write, line: &str) -> io::Result<()> {
    writeln!(stream, "{line}")?;
    stream.flush()
}

fn held(heard: &Mutex<Heard>) -> MutexGuard<'_, Heard> {
    heard.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Reads `stream`, standard error when `error`, line by line into `heard` until it ends; each line without its line
/// end, read as UTF-8 with U+FFFD for what is not.
fn relay(stream: impl Read, error: bool, heard: &Mutex<Heard>, echo: Echo) {
    let mut reader = BufReader::new(stream);
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        match reader.read_until(b'\n', &mut bytes) {
            Ok(0) => return,
            Ok(_) => {}
            Err(failure) => {
                held(heard)
                    .problem
                    .get_or_insert_with(|| format!("cannot read a call's output: {failure}"));
                return;
            }
        }
        let text = String::from_utf8_lossy(&bytes);
        let line = text.strip_suffix('\n').unwrap_or(&text);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let line = if echo == Echo::Progress { last_update(line) } else { line };
        held(heard).take(line, error, echo);
    }
}

/// What a progress line shows once its carriage-return updates are done: the text after its last carriage return.
fn last_update(line: &str) -> &str {
    line.rsplit('\r').next().unwrap_or(line)
}

/// Whether `line` is exactly the box's heartbeat: its prefix and one or more digits.
fn is_heartbeat(line: &str) -> bool {
    line.strip_prefix(HEARTBEAT)
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
}

/// The builder's version, from the last `gate: remote version <v>` line of its preflight; `None` when it said none.
fn their_version(lines: &[String]) -> Option<&str> {
    lines.iter().rev().find_map(|line| line.strip_prefix(VERSION)).map(str::trim)
}

/// The line refusing a builder `host` whose version, `theirs`, is not `ours`; `None` when they are the same. A
/// builder that said no version runs one unknown.
fn version_refusal(host: &str, theirs: Option<&str>, ours: &str) -> Option<String> {
    (theirs != Some(ours)).then(|| format!("gate: not routed: {host} runs gate {}, this PC {ours}", theirs.unwrap_or("unknown")))
}

/// The reason of the box's own refusal line for the work folder `key` among `lines`; `None` when there is none, as
/// for a command that exits 75 itself.
fn refusal(key: &str, lines: &[String]) -> Option<FallbackReason> {
    let busy = busy_line(key);
    let tools = format!("gate: remote work folder {key} could not restore its dotnet tools");
    let has = |wanted: &str| lines.iter().any(|line| line.trim() == wanted);
    if has(&busy) {
        Some(FallbackReason::Busy)
    } else if has(&tools) {
        Some(FallbackReason::Tools)
    } else {
        None
    }
}

/// The size git's push reports writing, from the last `Writing objects: 100% (…), <size>` it said; `up to date` when
/// it wrote nothing.
fn push_size(lines: &[String]) -> String {
    const WRITING: &str = "Writing objects: 100% (";
    lines
        .iter()
        .rev()
        .find_map(|line| {
            let rest = &line[line.rfind(WRITING)? + WRITING.len()..];
            let size = rest.split_once("), ")?.1.split(['|', ',', '\r']).next()?.trim();
            (!size.is_empty()).then(|| size.to_owned())
        })
        .unwrap_or_else(|| "up to date".to_owned())
}

/// `elapsed` in seconds to one decimal, rounded half to even as PowerShell's `[math]::Round` rounds, with no `.0`.
fn seconds(elapsed: Duration) -> String {
    let tenths = (elapsed.as_secs_f64() * 10.0).round_ties_even() / 10.0;
    format!("{tenths}")
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose interrupt or transport cannot be made must fail loudly")]
mod tests {
    use std::path::Path;
    use std::sync::LazyLock;
    use std::time::Duration;

    use gate_state::{FallbackReason, StateRoot};
    use gate_win32::Interrupt;

    use super::{Attempt, Called, End, SSH_FAILED, is_heartbeat, last_update, paused, push_size, refusal, seconds, their_version, version_refusal};
    use crate::remote::git::GIT_LIMIT;
    use crate::remote::payload::Location;
    use crate::remote::snapshot::Snapshot;
    use crate::remote::transport::Local;
    use crate::remote::{RemoteCall, busy_line};
    use crate::run::CONTROL_C_EXIT;

    /// A state root no gate uses; no test here takes its key's lock.
    static STATE: LazyLock<StateRoot> = LazyLock::new(|| StateRoot::new(r"C:\gate-client-tests", None));
    /// A pause no test could wait out: one that ends, ends on the interrupt.
    const LONG: Duration = Duration::from_secs(600);

    /// The console interrupt, raised: it is never lowered, so every test here that reads it sees it raised.
    fn raised() -> Interrupt {
        let interrupt = Interrupt::install().expect("the console handler is installed");
        interrupt.raise();
        interrupt
    }

    /// A call whose interrupt is raised, logging to `log`, which no test here writes.
    fn call(log: &Path) -> RemoteCall<'_> {
        RemoteCall {
            host: "builder",
            key: "app-1a2b3c4d",
            root: r"C:\no-such-worktree",
            repo: "app",
            subfolder: "",
            words: &[],
            outputs: &[],
            log,
            timeout: 60,
            stall: 60,
            tail: 0,
            no_markers: false,
            interrupt: Some(raised()),
            state: &STATE,
        }
    }

    fn attempt<'c, 'a>(call: &'c RemoteCall<'a>, transport: &'c Local) -> Attempt<'c, 'a> {
        Attempt {
            call,
            transport,
            limit: GIT_LIMIT,
            pause: LONG,
            lines: Vec::new(),
            problem: None,
        }
    }

    fn unwritten_log() -> std::path::PathBuf {
        std::env::temp_dir().join("gate-client-tests-no-such-folder").join("out.log")
    }

    #[test]
    fn a_ctrl_c_ends_the_retry_pause_at_once() {
        assert!(matches!(paused(LONG, Some(raised())), Err(End::Interrupted)));
        assert!(paused(Duration::from_millis(1), None).is_ok(), "no interrupt: the pause runs out");
    }

    #[test]
    fn a_ctrl_c_after_a_banner_drop_makes_no_retry() {
        let log = unwritten_log();
        let call = call(&log);
        let transport = Local::new(r"local:C:\builder", None).expect("a local transport");
        let mut attempt = attempt(&call, &transport);
        let mut made = 0;

        let ended = attempt.with_retry(SSH_FAILED, |_| {
            made += 1;
            Ok(Called {
                status: SSH_FAILED,
                output_seen: false,
                first_error: Some("Connection timed out during banner exchange".to_owned()),
                last_error: Some("Connection timed out during banner exchange".to_owned()),
                lines: vec!["Connection timed out during banner exchange".to_owned()],
            })
        });

        assert!(matches!(ended, Err(End::Interrupted)));
        assert_eq!(made, 1, "the call is not made again");
        assert!(!log.exists(), "no retry line is said");
    }

    #[test]
    fn a_ctrl_c_before_the_preflight_the_push_or_the_run_call_ends_the_attempt_as_an_interrupt() {
        let log = unwritten_log();
        let call = call(&log);
        let transport = Local::new(r"local:C:\builder", None).expect("a local transport");
        let mut attempt = attempt(&call, &transport);
        let snapshot = Snapshot {
            commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            bridge: None,
        };
        let location = Location::under(r"C:\builder", "app", "app-1a2b3c4d", "0123456789ab");

        assert!(matches!(attempt.preflight(&location), Err(End::Interrupted)), "the preflight");
        assert!(matches!(attempt.push(&snapshot, false), Err(End::Interrupted)), "the push");
        assert!(matches!(attempt.run_call("line"), Err(End::Interrupted)), "the run call");
        let outcome = attempt.outcome(End::Interrupted);
        assert_eq!(outcome.status, i64::from(CONTROL_C_EXIT.cast_signed()), "{outcome:?}");
        assert!(outcome.lines.is_empty(), "an interrupt says no failure line: {outcome:?}");
        assert_eq!(outcome.fallback(), None, "an interrupt is no fallback");
        assert!(!log.exists(), "nothing is said");
    }

    #[test]
    fn the_box_s_busy_line_is_the_one_its_refusal_is_read_from() {
        assert_eq!(busy_line("app-1a2b3c4d"), "gate: remote work folder app-1a2b3c4d is busy");
        assert_eq!(refusal("app-1a2b3c4d", &lines(&[&busy_line("app-1a2b3c4d")])), Some(FallbackReason::Busy));
    }

    #[test]
    fn a_progress_line_is_what_its_last_update_shows() {
        assert_eq!(
            last_update("Writing objects:  50% (1/2)\rWriting objects: 100% (2/2), 1 KiB | 1 KiB/s, done."),
            "Writing objects: 100% (2/2), 1 KiB | 1 KiB/s, done."
        );
        assert_eq!(last_update("To C:\\builder\\app.git"), "To C:\\builder\\app.git", "a line with no update");
    }

    fn lines(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| (*text).to_owned()).collect()
    }

    #[test]
    fn only_the_box_s_own_line_makes_a_75_a_refusal() {
        let key = "app-1a2b3c4d";
        assert_eq!(
            refusal(key, &lines(&["out", "gate: remote work folder app-1a2b3c4d is busy"])),
            Some(FallbackReason::Busy)
        );
        assert_eq!(
            refusal(
                key,
                &lines(&["gate: remote work folder app-1a2b3c4d could not restore its dotnet tools  "])
            ),
            Some(FallbackReason::Tools)
        );
        assert_eq!(refusal(key, &lines(&["the command exits 75 itself"])), None, "a command's own 75");
        assert_eq!(
            refusal(key, &lines(&["gate: remote work folder other-00000000 is busy"])),
            None,
            "another work folder's line"
        );
        assert_eq!(
            refusal(key, &lines(&["x gate: remote work folder app-1a2b3c4d is busy"])),
            None,
            "the whole line"
        );
    }

    #[test]
    fn a_builder_of_another_version_or_none_is_refused() {
        let preflight = lines(&[
            "gate: remote created D:\\gate-remote\\app.git",
            "gate: remote version 0.1.0",
            "gate: remote boot 2026-10-10T08:00:00.0000000Z",
        ]);
        assert_eq!(their_version(&preflight), Some("0.1.0"));
        assert_eq!(version_refusal("builder", Some("0.1.0"), "0.1.0"), None, "the same version");
        assert_eq!(
            version_refusal("builder", Some("0.2.0"), "0.1.0"),
            Some("gate: not routed: builder runs gate 0.2.0, this PC 0.1.0".to_owned())
        );
        assert_eq!(their_version(&lines(&["gate: remote boot x"])), None);
        assert_eq!(
            version_refusal("builder", None, "0.1.0"),
            Some("gate: not routed: builder runs gate unknown, this PC 0.1.0".to_owned())
        );
    }

    #[test]
    fn a_heartbeat_is_its_prefix_and_digits_only() {
        assert!(is_heartbeat("gate-remote-heartbeat: 1250"));
        assert!(!is_heartbeat("gate-remote-heartbeat: "), "no digits");
        assert!(!is_heartbeat("gate-remote-heartbeat: 12x"), "not only digits");
        assert!(!is_heartbeat("gate-remote-heartbeat:12"), "no space");
    }

    #[test]
    fn the_push_size_is_the_last_writing_line_s_size() {
        let pushed = lines(&[
            "Enumerating objects: 3, done.",
            "Writing objects:  33% (1/3)\rWriting objects: 100% (3/3), 268 bytes | 268.00 KiB/s, done.",
            "Total 3 (delta 0), reused 0 (delta 0), pack-reused 0",
        ]);
        assert_eq!(push_size(&pushed), "268 bytes");
        assert_eq!(push_size(&lines(&["Writing objects: 100% (9/9), 1.20 MiB, done."])), "1.20 MiB");
        assert_eq!(push_size(&lines(&["Everything up-to-date"])), "up to date");
    }

    #[test]
    fn the_push_s_seconds_have_one_decimal_rounded_half_to_even() {
        assert_eq!(seconds(Duration::from_millis(250)), "0.2");
        assert_eq!(seconds(Duration::from_millis(1340)), "1.3");
        assert_eq!(seconds(Duration::from_secs(2)), "2");
    }
}
