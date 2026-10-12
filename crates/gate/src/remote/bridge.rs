//! The bridge: the client that runs a routed command through remote.ps1's `Invoke-RemoteCommand`, in a pwsh this gate
//! starts on `remote-bridge.ps1`, the script installed beside gate.exe, with remote.ps1's path and the host as its
//! `-RemoteScript` and `-HostAlias` arguments. The call goes to the script on pwsh's stdin as one JSON request (UTF-8,
//! version 2) carrying a fresh nonce. The script answers on pwsh's stdout with control lines, each one line of JSON
//! carrying that nonce: `started` right before it calls `Invoke-RemoteCommand`, then the attempt's status, kill flag
//! and refusal reason, or the error that stopped it. The script writes a newline before each control line, so one
//! starts a line even after output that did not end its own; empty lines are skipped. Any other line on pwsh's stdout
//! is counted and otherwise ignored, and a line over 64 KiB is read through to its end without being held whole.
//!
//! pwsh shares this gate's console and stderr, so remote.ps1's live lines reach the console and a Ctrl+C reaches
//! remote.ps1. A first Ctrl+C before `started` ends pwsh's job at once, as nothing remote has begun; after it, the gate
//! waits while remote.ps1 stops the command on the builder, and a second Ctrl+C ends pwsh's job at once. pwsh's
//! environment is this gate's less the git variables git is never given. A missing pwsh, script or remote.ps1 falls
//! back here with reason `client` before the run takes a remote slot; a pwsh that cannot start, or one that ends before
//! `started`, falls back so once it has taken one. A pwsh that ends after `started` without its result ends the run
//! with status 1: the command may have run on the builder, so it never runs here as well.

use std::collections::hash_map::RandomState;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::hash::BuildHasher;
use std::io::{self, BufRead, BufReader, Write};
use std::os::windows::ffi::OsStrExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, ScopedJoinHandle};
use std::time::Duration;

use gate_args::{ProgramSearch, resolve_program};
use gate_state::{FallbackReason, KernelObject};
use gate_win32::{EnvironmentBlock, Interrupt, Job, Launch, LaunchError, LaunchOutput, Running, StdioPipes, Waited, cancel_pending_io, cancelled};
use serde_json::{Value, json};

use super::{REFUSED, RemoteCall, RemoteClient, RemoteOutcome, git};
use crate::run::CONTROL_C_EXIT;

/// The bridge script's file name, beside gate.exe.
pub(crate) const SHIM: &str = "remote-bridge.ps1";
/// The program the bridge starts, found by this exact name along `PATH`.
const PWSH: &str = "pwsh.exe";
/// The version of the request the script reads.
const REQUEST_VERSION: u32 = 2;
/// What the gate says once on the first Ctrl+C after the remote run started, while remote.ps1 stops the command on
/// the builder.
pub(crate) const STOPPING: &str = "gate: stopping the remote command; Ctrl+C again to kill it here";
/// The status of a run whose pwsh ended after the remote run started without saying how it ended.
const UNREPORTED: i64 = 1;
/// How long one wait for pwsh lasts before it is waited for again: no limit on the call, only the wait's own bound.
const WAIT_STEP: Duration = Duration::from_secs(3600);
/// How long the gate waits between cancels of a read of pwsh's stdout that outlived pwsh's job.
const RELEASE_STEP: Duration = Duration::from_millis(10);
/// The exit code what pwsh left running in its job is ended with.
const STRAGGLER_EXIT: u32 = 1;
/// The longest line of pwsh's stdout the bridge keeps, in bytes before its `\n`.
pub(super) const LINE_LIMIT: usize = 64 * 1024;

/// The bridge, with the stand-ins the test seams give: `remote_script` for remote.ps1 and `pwsh` for the pwsh on
/// `PATH`.
pub(crate) struct Bridge {
    remote_script: Option<PathBuf>,
    pwsh: Option<PathBuf>,
}

impl Bridge {
    pub(crate) fn new(remote_script: Option<PathBuf>, pwsh: Option<PathBuf>) -> Bridge {
        Bridge { remote_script, pwsh }
    }

    /// The pwsh, the bridge script and the remote.ps1 of a call, or why one is missing or cannot be passed on:
    /// remote.ps1 first, then the script, then pwsh.
    fn parts(&self) -> Result<Parts, String> {
        let script = self.remote_script()?;
        let exe = std::env::current_exe().map_err(|error| format!("cannot find gate.exe's own path: {error}"))?;
        let shim = exe.with_file_name(SHIM);
        if !shim.is_file() {
            return Err(format!("cannot find the bridge script at {}", shim.display()));
        }
        unicode("bridge script", &shim)?;
        let pwsh = self.pwsh()?;
        Ok(Parts { pwsh, shim, script })
    }

    /// remote.ps1: the seam's, else the one under `USERPROFILE`.
    fn remote_script(&self) -> Result<PathBuf, String> {
        let script = match &self.remote_script {
            Some(script) => script.clone(),
            None => remote_script_in(std::env::var_os("USERPROFILE").as_deref())?,
        };
        if !script.is_file() {
            return Err(format!("cannot find remote.ps1 at {}", script.display()));
        }
        unicode("remote.ps1", &script)?;
        Ok(script)
    }

    /// pwsh: the seam's, else `pwsh.exe` along the gate's own `PATH`.
    fn pwsh(&self) -> Result<PathBuf, String> {
        let pwsh = match &self.pwsh {
            Some(pwsh) if pwsh.is_file() => pwsh.clone(),
            Some(pwsh) => return Err(format!("cannot find pwsh at {}", pwsh.display())),
            None => pwsh_along(std::env::var_os("PATH").as_deref())?,
        };
        unicode("pwsh", &pwsh)?;
        Ok(pwsh)
    }
}

impl RemoteClient for Bridge {
    /// remote.ps1, the bridge script and pwsh are there; [`Bridge::run`] looks again, for one gone since.
    fn ready(&self) -> Result<(), String> {
        self.parts().map(drop)
    }

    fn run(&mut self, call: &RemoteCall<'_>) -> RemoteOutcome {
        match self.parts() {
            Ok(parts) => attempt(call, &parts),
            Err(why) => RemoteOutcome::not_started(why),
        }
    }
}

/// What a call starts: pwsh, on the bridge script, which dot-sources remote.ps1.
struct Parts {
    pwsh: PathBuf,
    shim: PathBuf,
    script: PathBuf,
}

/// `%USERPROFILE%\.claude\tools\gate\remote.ps1`, for the `USERPROFILE` value `profile`.
pub(super) fn remote_script_in(profile: Option<&OsStr>) -> Result<PathBuf, String> {
    let profile = Path::new(profile.ok_or("USERPROFILE is not set, so remote.ps1 cannot be found")?);
    if !profile.is_absolute() {
        return Err("USERPROFILE is not an absolute path, so remote.ps1 cannot be found".to_owned());
    }
    Ok(profile.join(r".claude\tools\gate\remote.ps1"))
}

/// `pwsh.exe` in the first absolute folder of the `PATH` value `path` that holds a file of that name; a `pwsh` of any
/// other extension is never taken.
pub(super) fn pwsh_along(path: Option<&OsStr>) -> Result<PathBuf, String> {
    let path = path.map_or_else(String::new, |value| value.to_string_lossy().into_owned());
    // A name with an extension is looked for as written, so neither PATHEXT nor the working folder is read.
    let search = ProgramSearch {
        path: &path,
        pathext: "",
        folder: "",
    };
    resolve_program(PWSH, &search, |candidate| Path::new(candidate).is_file())
        .map(PathBuf::from)
        .ok_or_else(|| "pwsh not found on PATH".to_owned())
}

/// Refuses the `what` path `path` when it is not valid Unicode: pwsh would be given another path than this one.
pub(super) fn unicode(what: &str, path: &Path) -> Result<(), String> {
    match path.to_str() {
        Some(_) => Ok(()),
        None => Err(format!("the {what} path {} is not valid Unicode", path.display())),
    }
}

/// The arguments pwsh is started with after its own name: the script, remote.ps1 and the host, each one word.
pub(super) fn arguments(shim: &Path, script: &Path, host: &str) -> Vec<std::ffi::OsString> {
    let mut words: Vec<std::ffi::OsString> = ["-NoProfile", "-NonInteractive", "-File"].map(Into::into).into();
    words.push(shim.into());
    words.extend(["-RemoteScript".into(), script.into(), "-HostAlias".into(), host.into()]);
    words
}

/// A fresh random 128-bit nonce, as 32 hex digits: two hashes under a `RandomState`, whose keys come from the system's
/// random source.
fn nonce() -> String {
    let state = RandomState::new();
    format!("{:016x}{:016x}", state.hash_one(0_u8), state.hash_one(1_u8))
}

/// The request's JSON for `call`, carrying `nonce`.
pub(super) fn request(call: &RemoteCall<'_>, nonce: &str) -> Value {
    json!({
        "version": REQUEST_VERSION,
        "nonce": nonce,
        "root": call.root,
        "repo": call.repo,
        "key": call.key,
        "subfolder": call.subfolder,
        "outputs": call.outputs,
        "words": call.words,
        "log": call.log.to_string_lossy(),
        "timeoutSeconds": call.timeout,
        "stallSeconds": call.stall,
        "tail": call.tail,
    })
}

/// The outcome of a call a Ctrl+C ended.
fn interrupted() -> RemoteOutcome {
    RemoteOutcome {
        status: i64::from(CONTROL_C_EXIT.cast_signed()),
        ..RemoteOutcome::default()
    }
}

/// Runs one call: starts pwsh in its job, sends it the request and reads its control lines while it runs, and ends
/// the call as they and pwsh's end say.
fn attempt(call: &RemoteCall<'_>, parts: &Parts) -> RemoteOutcome {
    if raised(call.interrupt) {
        return interrupted();
    }
    let (pwsh, pipes) = match start(call, parts) {
        Ok(started) => started,
        Err(Unstarted::Failed(why)) => return RemoteOutcome::not_started(why),
        Err(Unstarted::Interrupted) => return interrupted(),
    };
    let nonce = nonce();
    let body = request(call, &nonce).to_string();
    let started = AtomicBool::new(false);
    let mut notes = Notes::default();
    let (waited, report) = thread::scope(|scope| {
        let (body, nonce, started) = (body.as_str(), nonce.as_str(), &started);
        let reader = scope.spawn(move || converse(pipes, body, nonce, started));
        let waited = wait(&pwsh, call.interrupt, started, &mut notes);
        // With pwsh and what it left in its job ended, its stdout reaches its end and the reader returns.
        end_stragglers(&pwsh.job, &mut notes);
        release_reader(&reader, &pwsh, &mut notes);
        (waited, joined(reader.join(), started))
    });
    concluded(waited, report, call.log, notes, raised(call.interrupt))
}

/// A started pwsh: its job, the process, and a copy of the read end of its stdout, which the gate cancels the reader's
/// read on.
struct Pwsh {
    job: Job,
    running: Running,
    stdout: File,
}

/// The reader's report, or, when the reader panicked, one that keeps whether the remote run started and says the
/// reader failed.
fn joined(result: thread::Result<Report>, started: &AtomicBool) -> Report {
    result.unwrap_or_else(|_| Report {
        started: started.load(Ordering::Acquire),
        fault: Some("the bridge's reader of pwsh's stdout failed".to_owned()),
        ..Report::default()
    })
}

/// Returns once the reader of pwsh's stdout has. Once pwsh's job is empty, every byte its processes wrote is in the
/// pipe, so a read that still waits waits on a holder of the write end outside the job: it is cancelled, and again
/// until the reader returns. Each turn follows [`tick`]: a job that still lists a process is ended again, and its read
/// is cancelled once it lists empty; a job whose processes cannot be listed is a note once, and its read is cancelled
/// all the same. A cancel that fails is a note, and the join then waits as it would; an ending that fails is a note
/// once.
fn release_reader<T>(reader: &ScopedJoinHandle<'_, T>, pwsh: &Pwsh, notes: &mut Notes) {
    let (mut unlisted, mut unended) = (false, false);
    while !reader.is_finished() {
        let listing = pwsh.job.process_ids().map(|ids| ids.is_empty());
        if let Err(error) = &listing
            && !unlisted
        {
            notes.say(format!("gate: cannot list the processes in pwsh's job: {error}"));
            unlisted = true;
        }
        match tick(&listing) {
            Tick::Cancel => {
                if let Err(error) = cancel_pending_io(&pwsh.stdout) {
                    notes.say(format!("gate: cannot cancel the read of pwsh's stdout: {error}"));
                    // The join may then wait until the pipe's last writer closes its end.
                    return;
                }
            }
            Tick::Terminate => {
                if let Err(error) = pwsh.job.terminate(STRAGGLER_EXIT)
                    && !unended
                {
                    notes.say(format!("gate: cannot end what pwsh left in {} again: {error}", pwsh.job.name()));
                    unended = true;
                }
            }
        }
        thread::sleep(RELEASE_STEP);
    }
}

/// What one turn of [`release_reader`] does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tick {
    /// Cancel the reader's pending read: no process left in pwsh's job can write.
    Cancel,
    /// End pwsh's job again, for a process started in it as [`end_stragglers`] ended it, and look again next turn.
    Terminate,
}

/// What a turn of [`release_reader`] does, given whether pwsh's job `listing` is empty or why it cannot be listed. A
/// job that cannot be listed was ended by [`end_stragglers`], so every process left in it is ending and cannot write.
fn tick(listing: &io::Result<bool>) -> Tick {
    match listing {
        Ok(true) | Err(_) => Tick::Cancel,
        Ok(false) => Tick::Terminate,
    }
}

/// Whether the interrupt, when there is one, has been raised.
fn raised(interrupt: Option<Interrupt>) -> bool {
    interrupt.is_some_and(Interrupt::is_raised)
}

/// Why pwsh was not started.
enum Unstarted {
    /// It could not be, for this reason.
    Failed(String),
    /// A Ctrl+C came before it ran a single instruction.
    Interrupted,
}

/// Starts pwsh in this gate's job, its stdin and stdout piped and its environment the gate's less git's location
/// variables: created suspended, put in the job, then resumed unless a Ctrl+C came meanwhile.
fn start(call: &RemoteCall<'_>, parts: &Parts) -> Result<(Pwsh, StdioPipes), Unstarted> {
    let failed = |why: String| Unstarted::Failed(why);
    let mut words: Vec<Vec<u16>> = vec![parts.pwsh.as_os_str().encode_wide().collect()];
    let rest = arguments(&parts.shim, &parts.script, call.host);
    words.extend(rest.iter().map(|word| word.encode_wide().collect()));
    let kept = std::env::vars_os().filter(|(name, _)| !git::is_location_variable(name));
    let environment = EnvironmentBlock::new(kept).map_err(|error| failed(format!("cannot build pwsh's environment: {error}")))?;
    let directory = std::env::current_dir().map_err(|error| failed(format!("cannot read the working folder: {error}")))?;
    let name = call.state.object_name(KernelObject::Job {
        gate_pid: std::process::id(),
    });
    let job = Job::create(&name).map_err(|error| failed(format!("cannot create the job {name}: {error}")))?;
    let launch = Launch {
        application: parts.pwsh.clone(),
        command_line: gate_args::command_line(&words),
        directory,
        environment,
        output: LaunchOutput::Piped,
        priority: None,
    };
    let cannot = |error: LaunchError| failed(format!("cannot start {}: {error}", parts.pwsh.display()));
    let mut suspended = launch.start_suspended().map_err(cannot)?;
    let pipes = suspended
        .take_pipes()
        .ok_or_else(|| failed(format!("cannot start {}: its launch gave no pipes", parts.pwsh.display())))?;
    let stdout = pipes
        .stdout
        .try_clone()
        .map_err(|error| failed(format!("cannot copy the handle to pwsh's stdout: {error}")))?;
    suspended.assign(&job).map_err(cannot)?;
    // Dropped unresumed, pwsh is ended before it ran.
    if raised(call.interrupt) {
        return Err(Unstarted::Interrupted);
    }
    let running = suspended.resume().map_err(cannot)?;
    Ok((Pwsh { job, running, stdout }, pipes))
}

/// The bridge's own lines: each said on stderr as it comes, and appended to the log once pwsh has ended, so none is
/// written to the log while remote.ps1 holds it.
#[derive(Default)]
struct Notes(Vec<String>);

impl Notes {
    fn say(&mut self, line: String) {
        // A console that cannot be written to still gets the line in the log.
        let _ = writeln!(io::stderr().lock(), "{line}");
        self.0.push(line);
    }

    /// Says what went wrong reading pwsh's stdout, `fault`, and how many of its lines were not the script's, `ignored`.
    fn say_reading(&mut self, fault: Option<String>, ignored: u32) {
        if let Some(fault) = fault {
            self.say(format!("gate: {fault}"));
        }
        if ignored > 0 {
            self.say(format!(
                "gate: the bridge ignored {ignored} line(s) on pwsh's stdout that were not its own"
            ));
        }
    }

    /// Appends the lines to `log`, once pwsh has ended; a log that cannot be written to is said on stderr.
    fn append_to(self, log: &Path) {
        if self.0.is_empty() {
            return;
        }
        let appended = OpenOptions::new()
            .append(true)
            .create(true)
            .open(log)
            .and_then(|mut file| self.0.iter().try_for_each(|line| writeln!(file, "{line}")));
        if let Err(error) = appended {
            let _ = writeln!(io::stderr().lock(), "gate: cannot write the bridge's lines to {}: {error}", log.display());
        }
    }
}

/// Waits for pwsh to exit. A first Ctrl+C before the remote run may have started, that is before the reader has read
/// the `started` line, ends pwsh's job at once; one after it, which reached pwsh too, is said once and waited through,
/// and a second ends pwsh's job at once. Either is [`Waited::Interrupted`] once pwsh has ended.
fn wait(pwsh: &Pwsh, interrupt: Option<Interrupt>, started: &AtomicBool, notes: &mut Notes) -> Result<Waited, String> {
    let Some(interrupt) = interrupt else {
        return wait_out(&pwsh.running).map(Waited::Exited);
    };
    loop {
        match pwsh
            .running
            .wait_or_interrupt(WAIT_STEP, interrupt)
            .map_err(|error| cannot_wait(&error))?
        {
            Waited::Running => {}
            Waited::Exited(code) => return Ok(Waited::Exited(code)),
            Waited::Interrupted => break,
        }
    }
    // Only the flag is read: a look at pwsh's stdout would wait behind the reader's pending read of it.
    if !started.load(Ordering::Acquire) {
        end_now(pwsh)?;
        return Ok(Waited::Interrupted);
    }
    notes.say(STOPPING.to_owned());
    wait_for_stop(pwsh, interrupt)
}

/// Waits, after a first Ctrl+C, for remote.ps1 to stop the command on the builder and pwsh to exit, or for a second
/// Ctrl+C, which ends pwsh's job at once; [`Waited::Interrupted`] either way, once pwsh has ended.
fn wait_for_stop(pwsh: &Pwsh, interrupt: Interrupt) -> Result<Waited, String> {
    loop {
        match pwsh
            .running
            .wait_or_interrupt_again(WAIT_STEP, interrupt)
            .map_err(|error| cannot_wait(&error))?
        {
            Waited::Running => {}
            Waited::Exited(_) => return Ok(Waited::Interrupted),
            Waited::Interrupted => {
                end_now(pwsh)?;
                return Ok(Waited::Interrupted);
            }
        }
    }
}

/// Why pwsh could not be waited for.
fn cannot_wait(error: &io::Error) -> String {
    format!("cannot wait for pwsh: {error}")
}

/// Ends pwsh's job at once and waits for pwsh to be gone.
fn end_now(pwsh: &Pwsh) -> Result<(), String> {
    pwsh.job
        .terminate(CONTROL_C_EXIT)
        .map_err(|error| format!("cannot end pwsh's job: {error}"))?;
    wait_out(&pwsh.running).map(drop)
}

/// pwsh's exit code, however long it runs.
fn wait_out(running: &Running) -> Result<u32, String> {
    loop {
        if let Some(code) = running.wait(WAIT_STEP).map_err(|error| cannot_wait(&error))? {
            return Ok(code);
        }
    }
}

/// Ends what pwsh left running in its job, or pwsh itself when it could not be waited for, so the job's name is free
/// for a run here and pwsh's stdout reaches its end; a failure is a note.
fn end_stragglers(job: &Job, notes: &mut Notes) {
    if job.process_ids().is_ok_and(|ids| ids.is_empty()) {
        return;
    }
    if let Err(error) = job.terminate(STRAGGLER_EXIT) {
        notes.say(format!("gate: cannot end what pwsh left in {}: {error}", job.name()));
    }
}

/// Sends pwsh the request and closes its stdin, then reads its stdout to the end into the report. A read that failed or
/// panicked is the report's fault, and the rest of pwsh's stdout is read and discarded, so pwsh does not wait on a full
/// pipe. When [`drain`] stops at a second read error, the gate's kept copy of the read end still holds the pipe open,
/// so pwsh's later writes can block once the pipe's buffer fills; that takes two read errors in a row on an anonymous
/// pipe, and is accepted.
fn converse(pipes: StdioPipes, request: &str, nonce: &str, started: &AtomicBool) -> Report {
    let StdioPipes { mut stdin, stdout } = pipes;
    let sent = stdin.write_all(request.as_bytes());
    drop(stdin);
    let mut stdout = BufReader::new(stdout);
    let read = panic::catch_unwind(AssertUnwindSafe(|| read_stdout(&mut stdout, nonce, started)));
    let mut report = joined(read, started);
    if report.fault.is_some() {
        drain(&mut stdout);
    }
    if let Err(error) = sent {
        report.fault.get_or_insert_with(|| format!("cannot send pwsh the request: {error}"));
    }
    report
}

/// Reads `reader` through to its end or to the read the gate cancels, discarding what it reads. A read that fails ends
/// it too: the fault that started the draining is said already, and a pipe that fails again cannot be drained.
fn drain(reader: &mut impl BufRead) {
    loop {
        let length = match reader.fill_buf() {
            Ok(available) if !available.is_empty() => available.len(),
            Ok(_) | Err(_) => return,
        };
        reader.consume(length);
    }
}

/// Reads pwsh's stdout `reader` to its end, each line into the report; `started` is set the moment the `started` line
/// is read. An empty line is skipped, a line over [`LINE_LIMIT`] is counted with the lines that are not the script's,
/// and a read the gate cancelled ends the reading as the end does: every byte pwsh's job wrote was read before it.
pub(super) fn read_stdout(reader: impl BufRead, nonce: &str, started: &AtomicBool) -> Report {
    let mut report = Report::default();
    let mut lines = Lines::new(reader);
    loop {
        match lines.read_line() {
            Ok(Some(Framed::Line)) => report.take_line(&lines.line, nonce),
            Ok(Some(Framed::Overlong)) => report.ignored = report.ignored.saturating_add(1),
            Ok(None) => break,
            Err(error) if cancelled(&error) => break,
            Err(error) => {
                report.fault = Some(format!("cannot read pwsh's stdout: {error}"));
                break;
            }
        }
        if report.started {
            started.store(true, Ordering::Release);
        }
    }
    report
}

/// What [`Lines::read_line`] read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Framed {
    /// A line, now in [`Lines::line`].
    Line,
    /// A line longer than [`LINE_LIMIT`], read through to its end and dropped.
    Overlong,
}

/// The `\n`-ended lines of a reader, each held only up to [`LINE_LIMIT`] bytes.
pub(super) struct Lines<R> {
    reader: R,
    /// The line [`Lines::read_line`] last read, when it was a [`Framed::Line`], without its `\n`.
    pub line: Vec<u8>,
}

impl<R: BufRead> Lines<R> {
    pub(super) fn new(reader: R) -> Lines<R> {
        Lines {
            reader,
            line: Vec::with_capacity(LINE_LIMIT),
        }
    }

    /// Reads the next line, `None` at the end; a last line with no `\n` is a line, at the end and at a read the gate
    /// cancelled alike.
    pub(super) fn read_line(&mut self) -> io::Result<Option<Framed>> {
        self.line.clear();
        let mut framed = None;
        loop {
            let available = match self.reader.fill_buf() {
                Ok(available) => available,
                // The gate's cancel ends the reading as the end does, so what is held so far is the last line.
                Err(error) if framed.is_some() && cancelled(&error) => return Ok(framed),
                Err(error) => return Err(error),
            };
            if available.is_empty() {
                return Ok(framed);
            }
            let end = available.iter().position(|byte| *byte == b'\n');
            let used = end.map_or(available.len(), |end| end + 1);
            let part = &available[..end.unwrap_or(available.len())];
            framed = Some(kept(&mut self.line, part, framed));
            self.reader.consume(used);
            if end.is_some() {
                return Ok(framed);
            }
        }
    }
}

/// Adds `part` to `line` while the line so far, `framed`, stays within [`LINE_LIMIT`]; past it the line is dropped and
/// [`Framed::Overlong`] to its end.
fn kept(line: &mut Vec<u8>, part: &[u8], framed: Option<Framed>) -> Framed {
    if framed == Some(Framed::Overlong) || line.len() + part.len() > LINE_LIMIT {
        line.clear();
        return Framed::Overlong;
    }
    line.extend_from_slice(part);
    Framed::Line
}

/// How the call ends, once pwsh has: as the control lines say, and as pwsh's end says when they say nothing. A Ctrl+C
/// raised with no result ends it as the interrupt, even when pwsh ended before the wait saw the press. The bridge's own
/// lines are in the log by then, so a refusal and a run that never started gather them with the attempt's.
fn concluded(waited: Result<Waited, String>, report: Report, log: &Path, mut notes: Notes, interrupt_raised: bool) -> RemoteOutcome {
    let Report {
        started,
        answer,
        error,
        ignored,
        fault,
    } = report;
    notes.say_reading(fault, ignored);
    if answer.is_none() && interrupt_raised {
        for why in [error, waited.err()].into_iter().flatten() {
            notes.say(format!("gate: {why}"));
        }
        notes.append_to(log);
        return interrupted();
    }
    let ended = match waited {
        Ok(Waited::Exited(code)) => format!("ended (status {})", code.cast_signed()),
        Ok(_) => {
            notes.append_to(log);
            return interrupted();
        }
        Err(why) => format!("could not be waited for ({why})"),
    };
    match answer {
        Some(answer) => {
            notes.append_to(log);
            outcome_of(&answer, log)
        }
        None => unanswered(&ended, started, error, log, notes),
    }
}

/// How a call whose pwsh `ended` without a result ends: status 1 when the remote run `started`, since the command may
/// have run on the builder, else a fallback for the script's `error` or for pwsh's end.
fn unanswered(ended: &str, started: bool, error: Option<String>, log: &Path, mut notes: Notes) -> RemoteOutcome {
    if started {
        let rest = error.as_deref().unwrap_or("it did not report how the run ended");
        notes.say(format!("gate: the bridge {ended} after the remote run started; {rest}"));
        notes.append_to(log);
        return RemoteOutcome {
            status: UNREPORTED,
            ..RemoteOutcome::default()
        };
    }
    notes.append_to(log);
    let why = error.unwrap_or_else(|| format!("the bridge {ended} before the remote run started"));
    RemoteOutcome {
        lines: attempt_lines(log),
        ..RemoteOutcome::not_started(why)
    }
}

/// What a result line says.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Answer {
    pub status: i64,
    pub killed: bool,
    pub reason: Option<String>,
}

/// A control line of the script's, its nonce checked.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Control {
    /// The script is about to call `Invoke-RemoteCommand`.
    Started,
    /// `Invoke-RemoteCommand` returned this.
    Ended(Answer),
    /// The script stopped on this error.
    Failed(String),
}

/// The control line `line` is when it carries `nonce`; `None` for any other line, a malformed control line included.
pub(super) fn control(line: &str, nonce: &str) -> Option<Control> {
    let value: Value = serde_json::from_str(line).ok()?;
    if value.get("nonce")?.as_str()? != nonce {
        return None;
    }
    if value.get("started").is_some() {
        return value["started"].as_bool().filter(|started| *started).map(|_| Control::Started);
    }
    if let Some(error) = value.get("error") {
        return error.as_str().map(|message| Control::Failed(message.to_owned()));
    }
    let reason = match value.get("reason") {
        None | Some(Value::Null) => None,
        Some(Value::String(reason)) => Some(reason.clone()),
        Some(_) => return None,
    };
    Some(Control::Ended(Answer {
        status: value.get("status")?.as_i64()?,
        killed: value.get("killed")?.as_bool()?,
        reason,
    }))
}

/// What pwsh's stdout said: whether the remote run started, its result, the script's error, how many lines were not
/// the script's control lines, and what went wrong with the pipes.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Report {
    pub started: bool,
    pub answer: Option<Answer>,
    pub error: Option<String>,
    pub ignored: u32,
    pub fault: Option<String>,
}

impl Report {
    /// Takes one line of pwsh's stdout as read, its `\r` ending dropped; an empty line is no line at all.
    pub(super) fn take_line(&mut self, line: &[u8], nonce: &str) {
        let text = String::from_utf8_lossy(line);
        let text = text.trim_end_matches('\r');
        if !text.is_empty() {
            self.take(text, nonce);
        }
    }

    /// Takes one line of pwsh's stdout, the first of each control line kept. A result before `started` is taken as
    /// the start alone, since the run may have begun, and counted with the lines that are not the script's: a result
    /// out of order is not trusted.
    pub(super) fn take(&mut self, line: &str, nonce: &str) {
        match control(line, nonce) {
            Some(Control::Started) => self.started = true,
            Some(Control::Ended(answer)) if self.started => {
                self.answer.get_or_insert(answer);
            }
            Some(Control::Ended(_)) => {
                self.started = true;
                self.ignored = self.ignored.saturating_add(1);
            }
            Some(Control::Failed(message)) => {
                self.error.get_or_insert(message);
            }
            None => self.ignored = self.ignored.saturating_add(1),
        }
    }
}

/// The fallback reason of a status and the reason remote.ps1 gave: one of its five, with status 75 only.
pub(super) fn reason_of(status: i64, reason: Option<&str>) -> Option<FallbackReason> {
    if status != REFUSED {
        return None;
    }
    match reason? {
        "busy" => Some(FallbackReason::Busy),
        "unreachable" => Some(FallbackReason::Unreachable),
        "parity" => Some(FallbackReason::Parity),
        "tools" => Some(FallbackReason::Tools),
        "rebooted" => Some(FallbackReason::Rebooted),
        _ => None,
    }
}

/// The outcome `answer` gives; a refusal carries the lines the attempt wrote to `log`.
fn outcome_of(answer: &Answer, log: &Path) -> RemoteOutcome {
    let reason = reason_of(answer.status, answer.reason.as_deref());
    RemoteOutcome {
        status: answer.status,
        reason,
        killed: answer.killed,
        lines: if reason.is_some() { attempt_lines(log) } else { Vec::new() },
        ..RemoteOutcome::default()
    }
}

/// The lines the attempt wrote to `log`, which a run here replaces: none when it wrote none, else one line saying why
/// they cannot be read.
pub(super) fn attempt_lines(log: &Path) -> Vec<String> {
    match fs::read(log) {
        Ok(bytes) => split_lines(&String::from_utf8_lossy(&bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => vec![format!("gate: could not read the remote run's lines from {} ({error})", log.display())],
    }
}

/// The lines of `text`, ended by `\r\n`, `\n` or a lone `\r`, as .NET's `File.ReadAllLines` reads them: a last line
/// with no ending is a line, and an ending at the very end starts none.
pub(super) fn split_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<String> = text.split("\r\n").flat_map(|part| part.split(['\r', '\n'])).map(str::to_owned).collect();
    if text.ends_with(['\r', '\n']) {
        lines.pop();
    }
    lines
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot write its files must fail loudly")]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::io::{self, BufReader, Read};
    use std::os::windows::ffi::OsStringExt;
    use std::path::{Path, PathBuf};
    use std::sync::LazyLock;
    use std::sync::atomic::{AtomicBool, Ordering};

    use gate_state::{FallbackReason, StateRoot};
    use gate_win32::Waited;
    use serde_json::json;

    use super::{
        Answer, Control, Framed, LINE_LIMIT, Lines, Notes, Report, Tick, arguments, attempt_lines, concluded, control, joined, pwsh_along,
        read_stdout, reason_of, remote_script_in, request, split_lines, tick, unicode,
    };
    use crate::remote::RemoteCall;
    use crate::run::CONTROL_C_EXIT;

    static STATE: LazyLock<StateRoot> = LazyLock::new(|| StateRoot::new(r"C:\gate-bridge-tests", None));
    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    /// A folder of this test's own, emptied.
    fn folder(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("gate-bridge-{}-{name}", std::process::id()));
        if folder.exists() {
            fs::remove_dir_all(&folder).expect("an earlier run's folder can be removed");
        }
        fs::create_dir_all(&folder).expect("the folder can be made");
        folder
    }

    #[test]
    fn pwsh_gets_the_script_remote_ps1_and_the_host_each_as_one_word() {
        let words = arguments(
            Path::new(r"C:\gate bin\remote-bridge.ps1"),
            Path::new(r"C:\my tools\remote.ps1"),
            "builder",
        );

        let expected = [
            "-NoProfile",
            "-NonInteractive",
            "-File",
            r"C:\gate bin\remote-bridge.ps1",
            "-RemoteScript",
            r"C:\my tools\remote.ps1",
            "-HostAlias",
            "builder",
        ];
        assert_eq!(words, expected.map(OsString::from));
    }

    #[test]
    fn the_request_carries_every_input_and_the_nonce_and_words_round_trip() {
        let words = [r#"say "hi""#, "--", r"C:\trailing\", "ünïcødé 名前"].map(str::to_owned);
        let outputs = ["**/bin/**".to_owned()];
        let call = RemoteCall {
            host: "builder",
            key: "app-1a2b3c4d",
            root: r"C:\src\app",
            repo: "app",
            subfolder: "sub",
            words: &words,
            outputs: &outputs,
            log: Path::new(r"C:\logs\out.log"),
            timeout: 3600,
            stall: 120,
            tail: 20,
            no_markers: false,
            interrupt: None,
            state: &STATE,
            state_dir: Path::new(r"C:\gate-bridge-tests"),
        };

        let text = request(&call, NONCE).to_string();

        let read: serde_json::Value = serde_json::from_str(&text).expect("the request is JSON");
        assert_eq!(
            read,
            json!({
                "version": 2,
                "nonce": NONCE,
                "root": r"C:\src\app",
                "repo": "app",
                "key": "app-1a2b3c4d",
                "subfolder": "sub",
                "outputs": ["**/bin/**"],
                "words": [r#"say "hi""#, "--", r"C:\trailing\", "ünïcødé 名前"],
                "log": r"C:\logs\out.log",
                "timeoutSeconds": 3600,
                "stallSeconds": 120,
                "tail": 20,
            })
        );
    }

    #[test]
    fn each_nonce_is_32_hex_digits_and_fresh() {
        let (first, second) = (super::nonce(), super::nonce());
        for nonce in [&first, &second] {
            assert_eq!(nonce.len(), 32, "{nonce}");
            assert!(nonce.chars().all(|digit| digit.is_ascii_hexdigit()), "{nonce}");
        }
        assert_ne!(first, second);
    }

    #[test]
    fn control_lines_with_the_nonce_are_read() {
        let cases = [
            (format!(r#"{{"nonce":"{NONCE}","started":true}}"#), Control::Started),
            (
                format!(r#"{{"nonce":"{NONCE}","status":75,"killed":false,"reason":"busy"}}"#),
                Control::Ended(Answer {
                    status: 75,
                    killed: false,
                    reason: Some("busy".to_owned()),
                }),
            ),
            (
                format!(r#"{{"nonce":"{NONCE}","status":124,"killed":true,"reason":null}}"#),
                Control::Ended(Answer {
                    status: 124,
                    killed: true,
                    reason: None,
                }),
            ),
            (
                format!(r#"{{"nonce":"{NONCE}","error":"remote-bridge: no such host"}}"#),
                Control::Failed("remote-bridge: no such host".to_owned()),
            ),
        ];
        for (line, expected) in cases {
            assert_eq!(control(&line, NONCE), Some(expected), "{line}");
        }
    }

    #[test]
    fn lines_with_another_nonce_or_none_or_garbage_are_not_control_lines() {
        let other = "fedcba9876543210fedcba9876543210";
        for line in [
            format!(r#"{{"nonce":"{other}","started":true}}"#),
            format!(r#"{{"nonce":"{other}","status":0,"killed":false,"reason":null}}"#),
            r#"{"started":true}"#.to_owned(),
            r#"{"nonce":null,"error":"x"}"#.to_owned(),
            "not json".to_owned(),
            String::new(),
            format!(r#"{{"nonce":"{NONCE}","started":false}}"#),
            format!(r#"{{"nonce":"{NONCE}","status":"0","killed":false}}"#),
            format!(r#"{{"nonce":"{NONCE}","status":0,"killed":"no"}}"#),
            format!(r#"{{"nonce":"{NONCE}","status":75,"killed":false,"reason":7}}"#),
            format!(r#"{{"nonce":"{NONCE}","error":7}}"#),
        ] {
            assert_eq!(control(&line, NONCE), None, "{line}");
        }
    }

    /// The report of `lines`, in order.
    fn report_of(lines: &[String]) -> Report {
        let mut report = Report::default();
        for line in lines {
            report.take(line, NONCE);
        }
        report
    }

    #[test]
    fn a_report_keeps_the_start_the_first_result_and_the_first_error_and_counts_the_rest() {
        let report = report_of(&[
            "pwsh said something".to_owned(),
            format!(r#"{{"nonce":"{NONCE}","started":true}}"#),
            r#"{"nonce":"fedcba9876543210fedcba9876543210","status":9,"killed":false}"#.to_owned(),
            format!(r#"{{"nonce":"{NONCE}","status":3,"killed":false,"reason":null}}"#),
            format!(r#"{{"nonce":"{NONCE}","status":4,"killed":false,"reason":null}}"#),
            format!(r#"{{"nonce":"{NONCE}","error":"first"}}"#),
            format!(r#"{{"nonce":"{NONCE}","error":"second"}}"#),
        ]);

        let expected = Report {
            started: true,
            answer: Some(Answer {
                status: 3,
                killed: false,
                reason: None,
            }),
            error: Some("first".to_owned()),
            ignored: 2,
            fault: None,
        };
        assert_eq!(report, expected);
    }

    #[test]
    fn a_result_before_the_start_is_the_start_alone() {
        let report = report_of(&[format!(r#"{{"nonce":"{NONCE}","status":0,"killed":false,"reason":null}}"#)]);

        assert_eq!(
            report,
            Report {
                started: true,
                ignored: 1,
                ..Report::default()
            }
        );
    }

    #[test]
    fn an_error_before_the_start_is_kept_without_a_start() {
        let report = report_of(&[format!(r#"{{"nonce":"{NONCE}","error":"bad request"}}"#)]);

        assert_eq!(
            report,
            Report {
                error: Some("bad request".to_owned()),
                ..Report::default()
            }
        );
    }

    #[test]
    fn concluded_returns_interrupt_when_raised_without_answer() {
        let folder = folder("interrupt-race");
        let log = folder.join("out.log");
        let started = Report {
            started: true,
            ..Report::default()
        };
        // pwsh ended before the gate's wait saw the Ctrl+C: after the remote run started, and before it.
        for report in [started, Report::default()] {
            let outcome = concluded(Ok(Waited::Exited(3)), report, &log, Notes::default(), true);

            assert_eq!(outcome.status, i64::from(CONTROL_C_EXIT.cast_signed()), "{outcome:?}");
            assert_eq!(outcome.fallback(), None, "an interrupted run never runs here: {outcome:?}");
        }
        fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn an_empty_job_has_the_read_cancelled() {
        assert_eq!(tick(&Ok(true)), Tick::Cancel);
    }

    #[test]
    fn a_job_that_cannot_be_listed_has_the_read_cancelled_all_the_same() {
        assert_eq!(tick(&Err(io::Error::other("the job cannot be queried"))), Tick::Cancel);
    }

    #[test]
    fn a_job_that_still_lists_a_process_is_ended_again() {
        assert_eq!(tick(&Ok(false)), Tick::Terminate);
    }

    /// `ERROR_OPERATION_ABORTED`, the error of a read the gate cancelled.
    const OPERATION_ABORTED: i32 = 995;

    /// A reader that gives its bytes, then fails every read as one the gate cancelled.
    struct CancelledAfter<'a>(&'a [u8]);

    impl Read for CancelledAfter<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.0.is_empty() {
                return Err(io::Error::from_raw_os_error(OPERATION_ABORTED));
            }
            self.0.read(buffer)
        }
    }

    #[test]
    fn a_last_line_the_gates_cancel_cut_off_is_read_as_at_the_end() {
        let started = format!(r#"{{"nonce":"{NONCE}","started":true}}"#);
        let result = format!(r#"{{"nonce":"{NONCE}","status":0,"killed":false,"reason":null}}"#);
        let text = format!("{started}\n{result}");

        let report = read_stdout(BufReader::new(CancelledAfter(text.as_bytes())), NONCE, &AtomicBool::new(false));

        let expected = Report {
            started: true,
            answer: Some(Answer {
                status: 0,
                killed: false,
                reason: None,
            }),
            ..Report::default()
        };
        assert_eq!(report, expected, "the line held at the cancel is read, and the cancel is no fault");
    }

    #[test]
    fn a_reader_that_panicked_keeps_whether_the_remote_run_started() {
        for started in [true, false] {
            let report = joined(Err(Box::new("the reader panicked")), &AtomicBool::new(started));

            let expected = Report {
                started,
                fault: Some("the bridge's reader of pwsh's stdout failed".to_owned()),
                ..Report::default()
            };
            assert_eq!(report, expected);
        }
    }

    #[test]
    fn empty_lines_are_not_counted() {
        let text = format!(
            "\n\r\n{{\"nonce\":\"{NONCE}\",\"started\":true}}\r\n\n{{\"nonce\":\"{NONCE}\",\"status\":0,\"killed\":false,\"reason\":null}}\n\r\n"
        );
        let started = AtomicBool::new(false);

        let report = read_stdout(text.as_bytes(), NONCE, &started);

        let answer = Answer {
            status: 0,
            killed: false,
            reason: None,
        };
        let expected = Report {
            started: true,
            answer: Some(answer),
            ..Report::default()
        };
        assert_eq!(report, expected);
        assert!(started.load(Ordering::Acquire), "the start is shared as it is read");
        let other = read_stdout(b"\nnot the script's\n\nlast, with no ending".as_slice(), NONCE, &started);
        assert_eq!(other.ignored, 2, "{other:?}");
    }

    #[test]
    fn overlong_line_is_ignored_and_bounded() {
        // A started line padded with trailing spaces, which JSON allows, to exactly the limit.
        let mut at_limit = format!(r#"{{"nonce":"{NONCE}","started":true}}"#);
        at_limit.push_str(&" ".repeat(LINE_LIMIT - at_limit.len()));
        let mut text = vec![b'x'; 4 * LINE_LIMIT];
        text.extend_from_slice(format!("\n{at_limit}\n{at_limit} \n").as_bytes());
        let mut lines = Lines::new(BufReader::with_capacity(1000, text.as_slice()));

        assert_eq!(lines.read_line().expect("the text can be read"), Some(Framed::Overlong));
        assert!(lines.line.capacity() <= LINE_LIMIT, "the long line was never held whole");
        assert_eq!(lines.read_line().expect("the text can be read"), Some(Framed::Line));
        assert_eq!(lines.line, at_limit.as_bytes(), "a line of exactly the limit is kept");
        assert_eq!(
            lines.read_line().expect("the text can be read"),
            Some(Framed::Overlong),
            "one byte past the limit"
        );
        assert_eq!(lines.read_line().expect("the text can be read"), None);
        assert!(lines.line.capacity() <= LINE_LIMIT, "no line was held whole");

        let report = read_stdout(text.as_slice(), NONCE, &AtomicBool::new(false));
        let expected = Report {
            started: true,
            ignored: 2,
            ..Report::default()
        };
        assert_eq!(report, expected);
    }

    #[test]
    fn a_reason_falls_back_only_with_status_75() {
        for (text, reason) in [
            ("busy", FallbackReason::Busy),
            ("unreachable", FallbackReason::Unreachable),
            ("parity", FallbackReason::Parity),
            ("tools", FallbackReason::Tools),
            ("rebooted", FallbackReason::Rebooted),
        ] {
            assert_eq!(reason_of(75, Some(text)), Some(reason), "{text}");
            assert_eq!(reason_of(3, Some(text)), None, "{text} with another status");
        }
        assert_eq!(reason_of(75, None), None, "a 75 with no reason");
        for unknown in ["full", "client", "version", "tired", ""] {
            assert_eq!(reason_of(75, Some(unknown)), None, "{unknown:?} is no reason remote.ps1 gives");
        }
    }

    #[test]
    fn lines_end_at_crlf_lf_or_a_lone_cr() {
        assert_eq!(split_lines(""), Vec::<String>::new());
        assert_eq!(split_lines("a\r\nb\nc\rd"), ["a", "b", "c", "d"]);
        assert_eq!(split_lines("a\r\n"), ["a"]);
        assert_eq!(split_lines("a\r"), ["a"]);
        assert_eq!(split_lines("a\n\n"), ["a", ""]);
        assert_eq!(split_lines("a\r\rb"), ["a", "", "b"]);
        assert_eq!(split_lines("\n"), [""]);
    }

    #[test]
    fn the_attempts_lines_are_read_from_the_log() {
        let folder = folder("lines");
        let log = folder.join("out.log");
        assert!(attempt_lines(&log).is_empty(), "no log, no lines");
        fs::write(&log, "first\r\nsecond\rthird\n").expect("the log can be written");
        assert_eq!(attempt_lines(&log), ["first", "second", "third"]);
        fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn remote_ps1_is_under_an_absolute_userprofile_only() {
        assert_eq!(
            remote_script_in(Some(OsStr::new(r"C:\Users\me"))),
            Ok(PathBuf::from(r"C:\Users\me\.claude\tools\gate\remote.ps1"))
        );
        let relative = "USERPROFILE is not an absolute path, so remote.ps1 cannot be found".to_owned();
        for profile in [r"Users\me", r"\Users\me", "C:me", ""] {
            assert_eq!(remote_script_in(Some(OsStr::new(profile))), Err(relative.clone()), "{profile:?}");
        }
        assert_eq!(
            remote_script_in(None),
            Err("USERPROFILE is not set, so remote.ps1 cannot be found".to_owned())
        );
    }

    #[test]
    fn pwsh_is_found_as_pwsh_exe_only() {
        let only_cmd = folder("pwsh-cmd");
        fs::write(only_cmd.join("pwsh.cmd"), "").expect("pwsh.cmd can be written");
        fs::write(only_cmd.join("pwsh"), "").expect("an extensionless pwsh can be written");
        let with_exe = folder("pwsh-exe");
        fs::write(with_exe.join("pwsh.exe"), "").expect("pwsh.exe can be written");

        let not_found = Err("pwsh not found on PATH".to_owned());
        assert_eq!(pwsh_along(Some(only_cmd.as_os_str())), not_found);
        assert_eq!(pwsh_along(None), not_found);
        assert_eq!(pwsh_along(Some(OsStr::new(r"pwsh-exe;.\"))), not_found, "a relative folder is skipped");
        let path = format!("{};{}", only_cmd.display(), with_exe.display());
        assert_eq!(pwsh_along(Some(OsStr::new(&path))), Ok(with_exe.join("pwsh.exe")));
        fs::remove_dir_all(&only_cmd).expect("the folder can be removed");
        fs::remove_dir_all(&with_exe).expect("the folder can be removed");
    }

    #[test]
    fn a_part_path_that_is_not_unicode_is_refused() {
        let lone_surrogate = OsString::from_wide(&[u16::from(b'C'), u16::from(b':'), u16::from(b'\\'), 0xD800]);
        let path = Path::new(&lone_surrogate);

        let why = unicode("pwsh", path).expect_err("a lone surrogate is refused");

        assert_eq!(why, format!("the pwsh path {} is not valid Unicode", path.display()));
        assert_eq!(unicode("pwsh", Path::new(r"C:\ünïcødé\pwsh.exe")), Ok(()));
    }
}
