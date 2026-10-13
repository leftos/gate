//! `gate remote-cutover`: hands this PC's routed runs from the remote.ps1 bridge to the Rust remote client. One drain
//! runs at a time: it holds a mutex named from the state root for its whole life, and a run that cannot take it is
//! refused, naming the pid the drain marker names when that drainer lives. With the mutex held, it writes the marker
//! naming this gate, taking over one whose drainer is dead, then the remote-cutover flag, so a routed run that starts
//! from then on waits for the drain; a marker that stands but cannot be read, or names a drainer that lives under
//! another mutex (another logon session's, or another spelling of the root's), is refused, never overwritten, and a
//! flag that cannot be written takes the marker back. Then it drains: it reads this
//! PC's routed gates ([`remote_gates`]) every poll until every scan over a confirmation window finds none, naming on
//! stderr the gates it waits on and those that finish, as `gate cutover` names them; then it asks every builder
//! `remote.json` names, through `gate box inflight`, how many runs hold its work folders, every round until each says
//! 0; then it scans this PC once more, and starts over when a gate came meanwhile. An entry it cannot read, a builder
//! that cannot say, and a `remote.json` gone since the drain began hold the drain. Once drained it removes the marker
//! unless it names another drainer that lives, and keeps the flag, and never says it is done while a marker it cannot
//! read stands or once the flag is gone; a Ctrl+C, which wins over a drain that ends as it comes, leaves both, and
//! running it again resumes.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use gate_args::{RemoteCutoverRequest, RemoteCutoverSeams};
use gate_router::MAX_SLOTS;
use gate_state::v1::DrainRecord;
use gate_state::{Generation, KernelObject, RemoteGates, StateRootAt, Win32MutexProbe, normalize_root, remote_gates, v1};
use gate_win32::{AcquireError, Interrupt, NamedMutex, OwnedMutexGuard};
use jiff::Timestamp;
use proc_table::{Win32ProcessTable, command_line_of};

use crate::answer::{FAILED_STATUS, USAGE_STATUS};
use crate::boxside::payload::escaped;
use crate::console::Console;
use crate::cutover::{Confirm, Drained, counted};
use crate::environment::Variables;
use crate::push::{Call, End, called};
#[cfg(feature = "test-seams")]
use crate::remote::transport::Local;
use crate::remote::transport::ssh::Ssh;
use crate::remote::transport::{Transport, inflight_call};
use crate::resolve::{self, Environment, ResolvedRoot};
use crate::route::read_remote_json;
use crate::run::{CONTROL_C_EXIT, find_program};
use crate::slots::{Liveness, epoch_ms, gate_liveness};

/// How often the drain reads this PC's routed gates again.
const POLL: Duration = Duration::from_secs(2);
/// How long every scan of this PC must find no routed gate before its drain ends, as `gate cutover`'s window.
const CONFIRM: Duration = Duration::from_secs(5);
/// How often the builders are asked again while one holds a run or cannot say.
const ROUND: Duration = Duration::from_secs(10);
/// How long one builder's `gate box inflight` call may take before it is killed.
const INFLIGHT_BOUND: Duration = Duration::from_secs(30);
/// What starts the first line of `gate box inflight`'s output, before the count of runs in flight.
const COUNT_LINE: &str = "gate: remote in flight ";
/// What a gate's line says for a command its claim does not name and its process does not give.
const UNREADABLE_COMMAND: &str = "(command line unreadable)";
/// How many times the drain marker is read, or removed, while the try fails before the drain gives up on it.
const MARKER_TRIES: u32 = 5;
/// How long the drain waits between two tries at the marker.
const MARKER_RETRY: Duration = Duration::from_millis(200);
/// The one name ssh is looked for by along `PATH`.
const SSH_EXE: &str = "ssh.exe";

/// Runs `gate remote-cutover` and returns its exit status.
pub(crate) fn remote_cutover(request: &RemoteCutoverRequest, environment: &Environment, console: &mut Console) -> u32 {
    let Some(local_app_data) = environment.local_app_data.as_deref().filter(|value| !value.is_empty()) else {
        console.err("gate: remote-cutover needs LOCALAPPDATA");
        return u32::from(USAGE_STATUS);
    };
    let local_app_data = Path::new(local_app_data);
    if !local_app_data.is_absolute() {
        console.err(&format!(
            "gate: remote-cutover needs LOCALAPPDATA to be an absolute folder, not {}",
            local_app_data.display()
        ));
        return u32::from(USAGE_STATUS);
    }
    let root = match resolve::root(request.state_dir.as_deref(), environment) {
        Ok(root) => root,
        Err(line) => {
            console.err(&line);
            return u32::from(USAGE_STATUS);
        }
    };
    // The drain mutex is held until this function returns, so no other run drains meanwhile.
    let (flag, me, _drain) = match begin(&root, console) {
        Ok(begun) => begun,
        Err(status) => return status,
    };
    let interrupt = Interrupt::install()
        .inspect_err(|error| {
            console.err(&format!(
                "gate: cannot install the Ctrl+C handler, so Ctrl+C ends the gate at once: {error}"
            ));
        })
        .ok();
    let machine = Machine::new(&root, local_app_data, &request.seams);
    let drained = machine.drain(interrupt, console);
    // A Ctrl+C that came as the drain ended wins over it, so the marker is never removed after one.
    if drained == Drained::Interrupted || raised(interrupt) {
        console.err("gate: remote-cutover: interrupted; the flag and the drain marker stay, and running gate remote-cutover again resumes the drain");
        return CONTROL_C_EXIT;
    }
    end(&root.path, &flag, me, console)
}

/// This gate as the drain marker names it: its pid and its process start, Unix epoch milliseconds.
#[derive(Clone, Copy, Debug)]
struct Drainer {
    pid: u32,
    start_ms: u64,
}

/// Takes the drain mutex under `root`, which a drain holds for its whole life, and refuses the drain when another run
/// holds it ([`take_drain_mutex`]). With the mutex held, refuses a marker that stands but cannot be read or names a
/// drainer that lives ([`check_marker`]), and takes over one whose drainer is dead. Then writes the marker naming this
/// gate, then the flag, kept as it is when it is already there; a flag that cannot be written removes the marker again.
/// The flag's path, this gate as the marker names it and the mutex held, or the status of the step that failed, which
/// was said.
fn begin(root: &ResolvedRoot, console: &mut Console) -> Result<(PathBuf, Drainer, OwnedMutexGuard), u32> {
    let mut failed = |line: String| {
        console.err(&line);
        u32::from(FAILED_STATUS)
    };
    let start = gate_win32::process_start().map_err(|error| failed(format!("gate: remote-cutover: cannot read this gate's own start: {error}")))?;
    let me = Drainer {
        pid: std::process::id(),
        start_ms: epoch_ms(start),
    };
    let marker = v1::remote_cutover_draining_path(&root.path);
    let drain = take_drain_mutex(&root.root.object_name(KernelObject::RemoteCutover), &marker).map_err(&mut failed)?;
    check_marker(&marker).map_err(&mut failed)?;
    fs::create_dir_all(&root.path)
        .and_then(|()| v1::write_drain_marker(&root.path, me.pid, me.start_ms, epoch_ms(Timestamp::now())))
        .map_err(|error| {
            failed(format!(
                "gate: remote-cutover: cannot write the drain marker {}: {error}",
                marker.display()
            ))
        })?;
    let flag = v1::remote_cutover_flag_path(&root.path);
    if let Err(error) = OpenOptions::new().append(true).create(true).open(&flag) {
        console.err(&format!("gate: remote-cutover: cannot write the flag {}: {error}", flag.display()));
        if let Err(error) = remove_marker(&marker) {
            console.err(&cannot_remove(&marker, &error));
        }
        return Err(u32::from(FAILED_STATUS));
    }
    console.err("gate: remote-cutover: flag written; routed runs now wait for the drain");
    Ok((flag, me, drain))
}

/// Takes the drain mutex `name` without waiting; else the line refusing the drain when another run holds it
/// ([`refusal`]), or saying why it cannot be opened or taken.
fn take_drain_mutex(name: &str, marker: &Path) -> Result<OwnedMutexGuard, String> {
    let mutex = match NamedMutex::open(name) {
        Ok(mutex) => mutex,
        Err(error) if held_elsewhere(&error) => return Err(refusal(marker)),
        Err(error) => return Err(format!("gate: remote-cutover: cannot open the mutex {name}: {error}")),
    };
    match mutex.acquire_owned(Duration::ZERO) {
        Ok(drain) => Ok(drain),
        Err(AcquireError::Failed(error)) if !held_elsewhere(&error) => Err(format!("gate: remote-cutover: cannot take the mutex {name}: {error}")),
        Err(AcquireError::Timeout | AcquireError::Failed(_)) => Err(refusal(marker)),
    }
}

/// Whether a failure to open or take the drain mutex says another run holds it: a refused access does, as a run that is
/// not elevated is refused the mutex of an elevated drain.
fn held_elsewhere(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied
}

/// Refuses the drain marker at `marker`, with the mutex held, when it stands but cannot be read, or names a drainer
/// that lives ([`drains`]): one under another mutex, in another logon session or through another spelling of the root.
/// A marker that names a dead drainer is the caller's to take over.
fn check_marker(marker: &Path) -> Result<(), String> {
    match read_marker(marker, Result::is_err) {
        Ok(Some(record)) if drains(&record) => Err(drains_already(record.pid)),
        Ok(_) => Ok(()),
        Err(error) => Err(format!(
            "gate: remote-cutover: cannot read the drain marker {}: {error}; no other run drains, so delete it and run gate \
             remote-cutover again",
            marker.display()
        )),
    }
}

/// Whether the drainer `record` names lives: its pid runs as a process started at its start to the millisecond. A pid
/// whose start cannot be read names no drainer that lives.
fn drains(record: &DrainRecord) -> bool {
    matches!(gate_liveness(record.pid, record.start), Liveness::Live)
}

/// The line refusing a drain while the drainer `pid` lives.
fn drains_already(pid: u32) -> String {
    format!("gate: remote-cutover: pid {pid} drains already; let it finish, or end it and run gate remote-cutover again")
}

/// The line refusing a drain while another run holds the drain mutex: the pid the marker at `marker` names when that
/// drainer lives, the marker read again while it is missing, cannot be read or names none that lives, as a run that has
/// just taken the mutex is about to write it; else the marker's path.
fn refusal(marker: &Path) -> String {
    match read_marker(marker, |read| !matches!(read, Ok(Some(record)) if drains(record))) {
        Ok(Some(record)) if drains(&record) => drains_already(record.pid),
        Ok(_) | Err(_) => format!(
            "gate: remote-cutover: another run drains already and has not written its marker {} yet; let it finish and \
             run gate remote-cutover again",
            marker.display()
        ),
    }
}

/// The drain marker at `marker`, read up to [`MARKER_TRIES`] times, [`MARKER_RETRY`] apart, while `again` holds for the
/// last read: `None` when there is none, else why it cannot be read.
fn read_marker(marker: &Path, again: impl Fn(&Result<Option<DrainRecord>, String>) -> bool) -> Result<Option<DrainRecord>, String> {
    let mut read = read_marker_once(marker);
    for _ in 1..MARKER_TRIES {
        if !again(&read) {
            break;
        }
        thread::sleep(MARKER_RETRY);
        read = read_marker_once(marker);
    }
    read
}

/// One read of the drain marker at `marker`: `None` when there is none, else why it cannot be read, the system's error
/// or, for a marker whose open is refused, which [`v1::read_drain_record`] reads as none, that it does.
fn read_marker_once(marker: &Path) -> Result<Option<DrainRecord>, String> {
    match v1::read_drain_record(marker) {
        Ok(Some(record)) => Ok(Some(record)),
        Ok(None) if stands(marker) => Err("it exists but reads as none".to_owned()),
        Ok(None) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

/// Whether anything stands at `path`: only a lookup that finds nothing says not, so a marker that cannot be looked up
/// is never taken for gone.
fn stands(path: &Path) -> bool {
    !matches!(fs::symlink_metadata(path), Err(error) if error.kind() == io::ErrorKind::NotFound)
}

/// Removes the drain marker at `marker`, tried up to [`MARKER_TRIES`] times, [`MARKER_RETRY`] apart, while the removal
/// fails, as it does while another process holds the file open; a marker already gone counts as removed. The last
/// failure.
fn remove_marker(marker: &Path) -> io::Result<()> {
    let remove = || match fs::remove_file(marker) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        removed => removed,
    };
    let mut removed = remove();
    for _ in 1..MARKER_TRIES {
        if removed.is_ok() {
            break;
        }
        thread::sleep(MARKER_RETRY);
        removed = remove();
    }
    removed
}

/// The line saying the drain marker at `marker` cannot be removed, for `error`.
fn cannot_remove(marker: &Path, error: &io::Error) -> String {
    format!(
        "gate: remote-cutover: cannot remove the drain marker {}: {error}; delete it by hand",
        marker.display()
    )
}

/// Ends a drained cutover: removes the drain marker unless it names another drainer that lives, which is left to it,
/// and says the drain is done and how to go back to the bridge, the flag at `flag`; a flag deleted during the drain is
/// said instead, and the drain fails. A marker that cannot be read, after [`MARKER_TRIES`] reads, or removed is left
/// and said, and the drain fails. The exit status.
fn end(root: &Path, flag: &Path, me: Drainer, console: &mut Console) -> u32 {
    let marker = v1::remote_cutover_draining_path(root);
    match read_marker(&marker, Result::is_err) {
        Ok(Some(record)) if (record.pid, record.start) != (me.pid, me.start_ms) && drains(&record) => {
            console.err(&format!(
                "gate: remote-cutover: drained, but the drain marker {} now names pid {}, whose drain removes it",
                marker.display(),
                record.pid
            ));
            return 0;
        }
        Ok(Some(_)) => {
            if let Err(error) = remove_marker(&marker) {
                console.err(&cannot_remove(&marker, &error));
                return u32::from(FAILED_STATUS);
            }
        }
        Ok(None) => {}
        Err(error) => {
            console.err(&format!(
                "gate: remote-cutover: cannot read the drain marker {}: {error}; it is left",
                marker.display()
            ));
            return u32::from(FAILED_STATUS);
        }
    }
    if !stands(flag) {
        console.err(&format!(
            "gate: remote-cutover: the flag {} was deleted during the drain; routed runs use the bridge",
            flag.display()
        ));
        return u32::from(FAILED_STATUS);
    }
    console.err("gate: remote-cutover: done; routed runs use the Rust client");
    console.err(&format!("gate: remote-cutover: to go back to the bridge, delete {}", flag.display()));
    0
}

/// The routing file under the LOCALAPPDATA folder `local_app_data`, `<LOCALAPPDATA>\gate\remote.json`.
fn remote_json(local_app_data: &Path) -> PathBuf {
    local_app_data.join("gate").join("remote.json")
}

/// What the drain reads: the state root, the LOCALAPPDATA folder that holds the PowerShell root and the routing file,
/// the seams, and whether the routing file was there when the drain began.
struct Machine<'a> {
    root: &'a ResolvedRoot,
    local_app_data: &'a Path,
    seams: &'a RemoteCutoverSeams,
    had_remote_json: bool,
}

impl<'a> Machine<'a> {
    fn new(root: &'a ResolvedRoot, local_app_data: &'a Path, seams: &'a RemoteCutoverSeams) -> Machine<'a> {
        Machine {
            root,
            local_app_data,
            seams,
            had_remote_json: !matches!(read_remote_json(&remote_json(local_app_data)), Ok(None)),
        }
    }

    /// Drains this PC, then the builders, then checks this PC once more, starting over while a gate came meanwhile.
    fn drain(&self, interrupt: Option<Interrupt>, console: &mut Console) -> Drained {
        let mut said = Said::default();
        loop {
            if self.drain_here(&mut said, interrupt, console) == Drained::Interrupted
                || self.drain_builders(interrupt, console) == Drained::Interrupted
            {
                return Drained::Interrupted;
            }
            if self.entries().is_empty() {
                return Drained::Empty;
            }
        }
    }

    /// Reads this PC's routed gates every poll and says what changed, until every scan over the confirmation window
    /// finds none or the console is interrupted, which wins over a window that ends at the same scan.
    fn drain_here(&self, said: &mut Said, interrupt: Option<Interrupt>, console: &mut Console) -> Drained {
        let mut confirm = Confirm::new(self.seams.confirm_ms.map_or(CONFIRM, Duration::from_millis));
        let start = Instant::now();
        loop {
            let entries = self.entries();
            let listed = said.listed;
            for line in said.scan(&entries, &mut command_line_of) {
                console.err(&line);
            }
            if !listed
                && said.listed
                && self.seams.interrupt_after_list
                && let Some(interrupt) = interrupt
            {
                interrupt.raise();
            }
            if raised(interrupt) {
                return Drained::Interrupted;
            }
            if confirm.over(start.elapsed(), entries.is_empty()) {
                return Drained::Empty;
            }
            thread::sleep(self.poll());
        }
    }

    /// Asks every builder what runs there each round and says each that holds a run or cannot say, until a round finds
    /// every builder at 0 or the console is interrupted, which wins over a round that finds them so.
    fn drain_builders(&self, interrupt: Option<Interrupt>, console: &mut Console) -> Drained {
        let round = self.seams.poll_ms.map_or(ROUND, Duration::from_millis);
        loop {
            let asked = self.ask_builders(interrupt);
            for line in &asked.lines {
                console.err(line);
            }
            if self.seams.interrupt_after_round
                && let Some(interrupt) = interrupt
            {
                interrupt.raise();
            }
            if raised(interrupt) {
                return Drained::Interrupted;
            }
            if asked.drained {
                return Drained::Empty;
            }
            if interrupted_within(round, self.poll(), interrupt) {
                return Drained::Interrupted;
            }
        }
    }

    fn poll(&self) -> Duration {
        self.seams.poll_ms.map_or(POLL, Duration::from_millis)
    }

    /// This PC's routed gates and the entries that could not be read, under the state root and, under the default root,
    /// the PowerShell root, whose names the default root shares; a rehearsal's root reads no PowerShell root. Every
    /// remote slot the routing file may give is probed, whatever its `slots` says, so a slot held under an earlier file
    /// is still seen.
    fn entries(&self) -> Vec<Entry> {
        let powershell = self.root.root.is_default().then(|| Generation::PowerShell.state_dir(self.local_app_data));
        let root = StateRootAt {
            path: &self.root.path,
            root: &self.root.root,
        };
        entries(remote_gates(root, powershell.as_deref(), MAX_SLOTS, &Win32ProcessTable, &Win32MutexProbe))
    }

    /// One round: every builder the routing file names asked, and the line of each that holds a run or cannot say;
    /// once the console is interrupted no further builder is asked, and the round is not drained.
    fn ask_builders(&self, interrupt: Option<Interrupt>) -> Asked {
        let hosts = match self.hosts() {
            Ok(hosts) => hosts,
            Err(asked) => return asked,
        };
        let transport = self.transport();
        let variables: Vec<(OsString, OsString)> = std::env::vars_os().collect();
        let bound = self.seams.bound_ms.map_or(INFLIGHT_BOUND, Duration::from_millis);
        let mut asked = Asked {
            drained: true,
            lines: Vec::new(),
        };
        for host in &hosts {
            // A Ctrl+C that came as the last call exited starts no further call.
            if raised(interrupt) {
                asked.drained = false;
                break;
            }
            let answer = match &transport {
                Ok(transport) => ask(transport.as_ref(), host, &variables, bound, interrupt),
                Err(why) => Answer::Failed(why.clone()),
            };
            if answer == Answer::Interrupted {
                asked.drained = false;
                break;
            }
            if let Some(line) = answer.line(host) {
                asked.drained = false;
                asked.lines.push(line);
            }
        }
        asked
    }

    /// The builders the routing file names; else the round, with its line, that there are none to ask: drained when
    /// there is no routing file and there was none when the drain began, held when it is gone since or cannot be read.
    fn hosts(&self) -> Result<Vec<String>, Asked> {
        let path = remote_json(self.local_app_data);
        let held = |line: String| Asked {
            drained: false,
            lines: vec![line],
        };
        let ignored = |reason: &str| {
            held(format!(
                "gate: remote-cutover: ignored {}: {reason}; no builder can be asked until it is fixed",
                path.display()
            ))
        };
        match read_remote_json(&path) {
            Ok(Some(config)) => config.builders().map(<[String]>::to_vec).map_err(ignored),
            Ok(None) if self.had_remote_json => Err(held(format!(
                "gate: remote-cutover: {} is gone since the drain began; no builder can be asked until it is back",
                path.display()
            ))),
            Ok(None) => Err(Asked {
                drained: true,
                lines: vec![format!("gate: remote-cutover: no remote.json at {}; no builder to ask", path.display())],
            }),
            Err(problem) => Err(ignored(&problem.reason)),
        }
    }

    /// The way to the builders: the ssh found along `PATH` ([`found_ssh`]), or the stand-in `--test-remote-ssh` names;
    /// in a test-seams build, the local transport `--test-remote-transport` names. Else why there is none.
    fn transport(&self) -> Result<Box<dyn Transport>, String> {
        #[cfg(feature = "test-seams")]
        if let Some(transport) = &self.seams.transport {
            return Local::new(transport, self.seams.ssh.as_deref()).map(|local| Box::new(local) as Box<dyn Transport>);
        }
        if let Some(stub) = &self.seams.ssh {
            let program = std::path::absolute(stub).map_err(|error| format!("cannot run {}: {error}", stub.display()))?;
            return Ok(Box::new(Ssh { program }));
        }
        let folder = std::env::current_dir().map_err(|error| format!("cannot read the working directory: {error}"))?;
        Ok(Box::new(found_ssh(&Variables::from_process(), &folder)?))
    }
}

/// `ssh.exe` found along the `PATH` of `variables`, refused when it lies in the working folder `folder` itself, where a
/// planted program would; [`find_program`] skips a relative `PATH` entry, so none leads there. An ssh anywhere else is
/// taken, a system one under the working folder too, as when the drain runs from a drive's root. Else why there is none.
fn found_ssh(variables: &Variables, folder: &Path) -> Result<Ssh, String> {
    let program = find_program(SSH_EXE, variables, folder).map_err(|why| format!("cannot run ssh: {why}"))?;
    if program.parent().is_some_and(|parent| same_folder(parent, folder)) {
        return Err(format!("cannot run ssh: {} lies in the working folder", program.display()));
    }
    Ok(Ssh { program })
}

/// Whether `one` and `other` are the same folder, compared by their real paths when both can be read, else by their
/// paths as given, whatever their case.
fn same_folder(one: &Path, other: &Path) -> bool {
    match (fs::canonicalize(one), fs::canonicalize(other)) {
        (Ok(one), Ok(other)) => one == other,
        _ => normalize_root(&one.to_string_lossy()) == normalize_root(&other.to_string_lossy()),
    }
}

/// Whether the console has been interrupted.
fn raised(interrupt: Option<Interrupt>) -> bool {
    interrupt.is_some_and(Interrupt::is_raised)
}

/// Whether the console was interrupted within `period`, looked at every `step`.
fn interrupted_within(period: Duration, step: Duration, interrupt: Option<Interrupt>) -> bool {
    let start = Instant::now();
    loop {
        if raised(interrupt) {
            return true;
        }
        let left = period.saturating_sub(start.elapsed());
        if left.is_zero() {
            return false;
        }
        thread::sleep(left.min(step));
    }
}

/// What one entry the drain waits on is known by from one scan to the next.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    /// A routed gate: its claims and tickets, by its pid.
    Pid(u32),
    /// A held remote slot whose gate is not known, by its index.
    Unknown(Option<u32>),
    /// A claim, ticket or folder that could not be read, by its path.
    Unreadable(PathBuf),
}

/// One entry the drain waits on, and the command its claims name.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    key: Key,
    command: Option<String>,
}

/// The entries `found` holds: each routed gate's pid once, with the first command its claims name, in the order found;
/// each held remote slot whose gate is not known, by its index; then each path that could not be read.
fn entries(found: RemoteGates) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    for gate in found.gates {
        let Some(pid) = gate.pid else {
            entries.push(Entry {
                key: Key::Unknown(gate.slot),
                command: None,
            });
            continue;
        };
        match entries.iter_mut().find(|entry| entry.key == Key::Pid(pid)) {
            Some(entry) => entry.command = entry.command.take().or(gate.command),
            None => entries.push(Entry {
                key: Key::Pid(pid),
                command: gate.command,
            }),
        }
    }
    entries.extend(found.unreadable.into_iter().map(|path| Entry {
        key: Key::Unreadable(path),
        command: None,
    }));
    entries
}

/// How a line names a held remote slot whose gate is not known: by its index, which [`remote_gates`] gives each.
fn held_slot(slot: Option<u32>) -> String {
    slot.map_or_else(|| "a remote slot".to_owned(), |index| format!("remote slot {index}"))
}

/// How a line names `entry`: a gate by its pid and command (its claims' command, else its process's command line from
/// `read_command`), its control characters escaped, a held slot whose gate is not known, or a path that could not be
/// read, so it can be removed.
fn described(entry: &Entry, read_command: &mut dyn FnMut(u32) -> Option<String>) -> String {
    match &entry.key {
        Key::Pid(pid) => {
            let command = entry
                .command
                .clone()
                .or_else(|| read_command(*pid))
                .map_or_else(|| UNREADABLE_COMMAND.to_owned(), |command| escaped(&command));
            format!("pid {pid}  {command}")
        }
        Key::Unknown(slot) => format!("{}, held by a gate that is not known", held_slot(*slot)),
        Key::Unreadable(path) => format!("{}, which cannot be read; remove it once no gate uses it", path.display()),
    }
}

/// What a line says of an entry no longer found.
fn ended(key: &Key) -> String {
    match key {
        Key::Pid(pid) => format!("pid {pid} finished"),
        Key::Unknown(slot) => format!("{} is free", held_slot(*slot)),
        Key::Unreadable(path) => format!("{} is gone", path.display()),
    }
}

/// What the drain has said so far: whether it has listed the entries, the entries it waits on, and every entry it has
/// named.
#[derive(Debug, Default)]
struct Said {
    listed: bool,
    waiting: BTreeSet<Key>,
    named: BTreeSet<Key>,
}

impl Said {
    /// The lines one scan says: the list of the entries, the first time any is found; afterwards each entry gone since
    /// the last scan, then each new one, named only the first time it is seen.
    fn scan(&mut self, entries: &[Entry], read_command: &mut dyn FnMut(u32) -> Option<String>) -> Vec<String> {
        if self.listed {
            self.changes(entries, read_command)
        } else {
            self.first_list(entries, read_command)
        }
    }

    fn first_list(&mut self, entries: &[Entry], read_command: &mut dyn FnMut(u32) -> Option<String>) -> Vec<String> {
        if entries.is_empty() {
            return Vec::new();
        }
        self.listed = true;
        let mut lines = vec![format!(
            "gate: remote-cutover: waiting for {} to finish:",
            counted(entries.len(), "routed gate")
        )];
        for entry in entries {
            lines.push(format!("gate:   {}", described(entry, read_command)));
            self.waiting.insert(entry.key.clone());
            self.named.insert(entry.key.clone());
        }
        lines
    }

    fn changes(&mut self, entries: &[Entry], read_command: &mut dyn FnMut(u32) -> Option<String>) -> Vec<String> {
        let now: BTreeSet<&Key> = entries.iter().map(|entry| &entry.key).collect();
        let gone: Vec<Key> = self.waiting.iter().filter(|key| !now.contains(key)).cloned().collect();
        let mut lines = Vec::new();
        for key in gone {
            self.waiting.remove(&key);
            // A slot whose gate is not known is known only by its index, so a slot held again once freed is named again.
            if matches!(key, Key::Unknown(_)) {
                self.named.remove(&key);
            }
            lines.push(format!("gate: remote-cutover: {} ({} left)", ended(&key), self.waiting.len()));
        }
        for entry in entries {
            if self.waiting.insert(entry.key.clone()) && self.named.insert(entry.key.clone()) {
                lines.push(format!("gate: remote-cutover: also waiting for {}", described(entry, read_command)));
            }
        }
        lines
    }
}

/// One builder round: whether every builder said 0, and the lines of those that did not.
struct Asked {
    drained: bool,
    lines: Vec<String>,
}

/// What one builder answered.
#[derive(Debug, PartialEq, Eq)]
enum Answer {
    /// It holds this many runs.
    InFlight(usize),
    /// Its call exited with this status, or with 0 and no count line: it cannot say.
    Status(u32),
    /// Its call gave no answer within this bound.
    Silent(Duration),
    /// The console was interrupted while its call ran, which ended the call: it was not asked.
    Interrupted,
    /// Its call could not be made, for this reason.
    Failed(String),
}

impl Answer {
    /// The line said of `host`, `None` when it holds no run or was not asked.
    fn line(&self, host: &str) -> Option<String> {
        let cannot = format!("gate: remote-cutover: {host} cannot say what runs there");
        match self {
            Answer::InFlight(0) | Answer::Interrupted => None,
            Answer::InFlight(runs) => Some(format!("gate: remote-cutover: {host}: {runs} run(s) in flight")),
            Answer::Status(status) => Some(format!("{cannot} (status {status}); run gate push")),
            Answer::Silent(bound) => Some(format!("{cannot} (no answer in {} s)", bound.as_secs_f64())),
            Answer::Failed(why) => Some(format!("{cannot}: {why}")),
        }
    }
}

/// What the builder `host` answers its `gate box inflight` call through `transport`: the call run as `gate push` runs
/// its calls, with `variables` as its environment, killed at `bound` and ended early by `interrupt`, and the count read
/// from the first line of its standard output.
fn ask(transport: &dyn Transport, host: &str, variables: &[(OsString, OsString)], bound: Duration, interrupt: Option<Interrupt>) -> Answer {
    let program = inflight_call(transport, host);
    let heard = called(&Call {
        program: &program.program,
        args: program.args,
        input: b"",
        variables,
        bound,
        interrupt,
    });
    match heard.end {
        End::Exited(0) => heard.first.as_deref().and_then(in_flight).map_or(Answer::Status(0), Answer::InFlight),
        End::Exited(status) => Answer::Status(status),
        End::TimedOut => Answer::Silent(bound),
        End::Interrupted => Answer::Interrupted,
        End::Failed(why) => Answer::Failed(why),
    }
}

/// The count of runs in flight `gate box inflight`'s first line gives; `None` for any other line.
fn in_flight(line: &str) -> Option<usize> {
    line.strip_prefix(COUNT_LINE)?.parse().ok()
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose scratch folder cannot be made must fail loudly")]
mod tests {
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{Answer, Entry, Key, Said, described, found_ssh, held_elsewhere, in_flight};
    use crate::environment::Variables;

    /// `ERROR_ACCESS_DENIED`, which opening an elevated drain's mutex gives a run that is not elevated.
    const ERROR_ACCESS_DENIED: i32 = 5;
    /// `ERROR_INVALID_HANDLE`, a failure that says nothing of who holds the mutex.
    const ERROR_INVALID_HANDLE: i32 = 6;

    #[test]
    fn a_mutex_whose_access_is_denied_is_held_by_another_run() {
        assert!(held_elsewhere(&io::Error::from_raw_os_error(ERROR_ACCESS_DENIED)));
        assert!(!held_elsewhere(&io::Error::from_raw_os_error(ERROR_INVALID_HANDLE)));
    }

    /// A folder of this test's own under the temp folder, emptied.
    fn scratch(test: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("gate-remote-cutover-{}-{test}", std::process::id()));
        // A folder left by an earlier run of the same pid is cleared first.
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).expect("the scratch folder can be made");
        folder
    }

    /// An empty `ssh.exe` made in `folder`, which is made too; its path.
    fn ssh_in(folder: &Path) -> PathBuf {
        fs::create_dir_all(folder).expect("the ssh folder can be made");
        let ssh = folder.join("ssh.exe");
        fs::write(&ssh, "").expect("the ssh can be written");
        ssh
    }

    /// This process's environment with `PATH` set to `path`.
    fn path_of(path: &str) -> Variables {
        Variables::from_process().with("PATH", path)
    }

    #[test]
    fn an_ssh_inside_the_working_folder_on_an_absolute_path_entry_is_taken() {
        let base = scratch("inside");
        let ssh = ssh_in(&base.join("tools"));

        let found = found_ssh(&path_of(&base.join("tools").to_string_lossy()), &base).expect("the ssh is taken");

        assert_eq!(found.program, ssh);
    }

    #[test]
    fn an_ssh_in_an_absolute_path_folder_above_the_working_folder_is_taken() {
        let base = scratch("above");
        let ssh = ssh_in(&base);
        let work = base.join("work");
        fs::create_dir_all(&work).expect("the working folder can be made");

        let found = found_ssh(&path_of(&base.to_string_lossy()), &work).expect("the ssh is taken");

        assert_eq!(found.program, ssh);
    }

    #[test]
    fn an_ssh_in_the_working_folder_itself_is_refused() {
        let base = scratch("itself");
        let ssh = ssh_in(&base);

        let refused = found_ssh(&path_of(&base.to_string_lossy()), &base).err();

        assert_eq!(refused, Some(format!("cannot run ssh: {} lies in the working folder", ssh.display())));
    }

    #[test]
    fn an_ssh_only_a_relative_path_entry_leads_to_is_not_found() {
        let base = scratch("relative");
        ssh_in(&base.join("tools"));

        let refused = found_ssh(&path_of("tools"), &base).err();

        assert!(refused.as_deref().is_some_and(|why| why.starts_with("cannot run ssh: ")), "{refused:?}");
    }

    #[test]
    fn a_gate_s_command_is_named_with_its_control_characters_escaped() {
        let claimed = Entry {
            key: Key::Pid(7),
            command: Some("cargo\u{1b}[31m build\r\ngate: forged".to_owned()),
        };
        let unclaimed = Entry {
            key: Key::Pid(8),
            command: None,
        };

        assert_eq!(described(&claimed, &mut |_| None), r"pid 7  cargo\u{1b}[31m build\r\ngate: forged");
        assert_eq!(described(&unclaimed, &mut |_| Some("a\tb".to_owned())), r"pid 8  a\tb");
        assert_eq!(described(&unclaimed, &mut |_| None), "pid 8  (command line unreadable)");
    }

    /// A held remote slot whose gate is not known, by its index.
    fn slot(index: u32) -> Entry {
        Entry {
            key: Key::Unknown(Some(index)),
            command: None,
        }
    }

    #[test]
    fn a_slot_whose_gate_is_not_known_is_named_by_its_index() {
        let mut said = Said::default();

        assert_eq!(
            said.scan(&[slot(3)], &mut |_| None),
            [
                "gate: remote-cutover: waiting for 1 routed gate to finish:",
                "gate:   remote slot 3, held by a gate that is not known"
            ]
        );
        assert_eq!(said.scan(&[], &mut |_| None), ["gate: remote-cutover: remote slot 3 is free (0 left)"]);
    }

    #[test]
    fn a_slot_held_again_once_freed_is_named_again() {
        let mut said = Said::default();
        said.scan(&[slot(0)], &mut |_| None);
        said.scan(&[], &mut |_| None);

        assert_eq!(
            said.scan(&[slot(0)], &mut |_| None),
            ["gate: remote-cutover: also waiting for remote slot 0, held by a gate that is not known"]
        );
    }

    #[test]
    fn a_gate_seen_again_once_finished_is_not_named_again() {
        let gate = Entry {
            key: Key::Pid(5),
            command: Some("cargo test".to_owned()),
        };
        let mut said = Said::default();
        said.scan(std::slice::from_ref(&gate), &mut |_| None);
        said.scan(&[], &mut |_| None);

        assert_eq!(said.scan(std::slice::from_ref(&gate), &mut |_| None), Vec::<String>::new());
    }

    #[test]
    fn the_count_is_read_only_from_a_count_line() {
        assert_eq!(in_flight("gate: remote in flight 3"), Some(3));
        assert_eq!(in_flight("gate: remote in flight 3 app-1"), None);
        assert_eq!(in_flight("gate: remote in flight x"), None);
        assert_eq!(in_flight("usage: gate box preflight|run"), None);
    }

    #[test]
    fn a_builder_s_answer_says_a_line_unless_it_holds_no_run_or_was_not_asked() {
        let line = |answer: Answer| answer.line("builder");

        assert_eq!(line(Answer::InFlight(0)), None);
        assert_eq!(line(Answer::Interrupted), None);
        assert_eq!(
            line(Answer::InFlight(2)).as_deref(),
            Some("gate: remote-cutover: builder: 2 run(s) in flight")
        );
        assert_eq!(
            line(Answer::Status(9009)).as_deref(),
            Some("gate: remote-cutover: builder cannot say what runs there (status 9009); run gate push")
        );
        assert_eq!(
            line(Answer::Silent(Duration::from_millis(1500))).as_deref(),
            Some("gate: remote-cutover: builder cannot say what runs there (no answer in 1.5 s)")
        );
        assert_eq!(
            line(Answer::Failed("cannot run ssh: why".to_owned())).as_deref(),
            Some("gate: remote-cutover: builder cannot say what runs there: cannot run ssh: why")
        );
    }
}
