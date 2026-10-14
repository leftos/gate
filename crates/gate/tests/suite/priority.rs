//! The build servers' priority (B2): an ordinary gate lowers a build server outside its job to below normal, leaves one
//! at High while a critical ticket holds the machine and one in a holding critical gate's job, and a critical gate
//! raises the servers outside the other gates' jobs, and its own job's, to High. Every case confines its gate's scan
//! to the case's folder (`--test-servers-under`), so no real build server of the machine is touched, and orders its
//! assertions by `wait_until` on the class it reads itself, never on a clock.
//!
//! A gate sweeps every build server of the machine that is outside its own job — any gate's, not only the one a case
//! runs, since another session's gate may run beside it. So a case's servers are copies of fake-tool named
//! [`SERVER`], an image only a gate told so (`--test-server-image`) reads as a compiler server: no gate elsewhere moves
//! them. A case that asserts a class *stayed* put waits for its own gate to say it has sampled
//! (`--test-interrupt-after-samples`) and reads the class then.

use std::fs;
use std::path::PathBuf;
use std::process::{self, Child, Command};

use gate_state::v1::{self, Claim, ClaimKind, Ticket, TicketKind};
use gate_state::{KernelObject, StateRoot};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
use windows_sys::Win32::System::Threading::{
    BELOW_NORMAL_PRIORITY_CLASS, GetPriorityClass, HIGH_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SET_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE, SetPriorityClass,
};

use crate::nesting::{LOOK_OFTEN, end_parent, let_go, line_with, marked, orphaned_as, spawned, test_marker};
use crate::support::{
    Case, FAKE_TOOL, GATE, critical_words, events_of, heavy_words, run_words, said_pid, shown, start, tool, wait_for_journal_line, wait_until,
    write_counts,
};

/// The status Windows reports for a process Ctrl+C ended, `STATUS_CONTROL_C_EXIT`, as a signed exit code: how a case's
/// run ends when the gate raises its own interrupt and stops the command.
const CONTROL_C_EXIT: i32 = 0xC000_013A_u32.cast_signed();
/// The line a gate says as it raises its own interrupt, after the samples `--test-interrupt-after-samples` counted: a
/// case that asserts a class stayed put waits for it, so the class was read after passes that had the case's setup.
const INTERRUPTED: &str = "gate: interrupted; stopping the command";

/// The image of a case's build servers: one no gate reads as a server's unless told so, so no gate elsewhere on the
/// machine moves them while a case asserts on their class.
const SERVER: &str = "gate-test-server.exe";

/// The words a case's gate runs with: a 20 ms sample, so a class is moved within one; the stall window left at its
/// 120 s default, long past every case's wait; [`SERVER`] read as a compiler server's image; and the gate's own
/// interrupt after `samples` samples, which a case that asserts a class stayed put waits for. The server scan is the
/// run words' own, confined to the case's folder.
fn seams(samples: Option<u64>) -> Vec<String> {
    let mut words = ["--test-sample-ms", "20", "--test-server-image", SERVER].map(str::to_owned).to_vec();
    if let Some(samples) = samples {
        words.extend(["--test-interrupt-after-samples".to_owned(), samples.to_string()]);
    }
    words
}

/// The file whose existence would end a command's `chatter` step.
fn chatter_path(case: &Case) -> PathBuf {
    case.folder().join("chatter")
}

/// The file whose existence lets a case's waiting command, and the servers it started, end.
fn go_path(case: &Case) -> String {
    shown(&case.folder().join("go"))
}

/// The words of a command that keeps its log growing until the case's gate interrupts it, so the watch's stall clock
/// cannot run while the case waits for the gate to say it has sampled.
fn chatter(case: &Case) -> Vec<String> {
    vec![tool(case, "chatter.exe"), "chatter".to_owned(), shown(&chatter_path(case))]
}

/// The words of a command that writes one line and then waits quietly, inside the watch's stall window, until the case
/// writes its go file: a case waits on the class its gate moves while the command still runs, then lets it end.
fn quiet(case: &Case) -> Vec<String> {
    vec![
        tool(case, "quiet.exe"),
        "out".to_owned(),
        "run".to_owned(),
        "wait".to_owned(),
        go_path(case),
    ]
}

/// A process's priority class, read by the case itself through `GetPriorityClass`: what its waits and assertions are
/// ordered on.
fn class_of(pid: u32) -> u32 {
    // SAFETY: the handle is opened here and closed before this returns; neither call takes a pointer.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    assert!(!process.is_null(), "the process {pid} can be opened: {}", std::io::Error::last_os_error());
    let class = unsafe { GetPriorityClass(process) };
    let error = std::io::Error::last_os_error();
    // SAFETY: the handle was opened above and is not used after this.
    unsafe { CloseHandle(process) };
    assert_ne!(class, 0, "the class of {pid} can be read: {error}");
    class
}

/// Sets the class of the process `pid`, without reading it back: a gate may move it again at once.
fn put_class(pid: u32, class: u32) {
    // SAFETY: the handle is opened here and closed before this returns; neither call takes a pointer.
    let process = unsafe { OpenProcess(PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    assert!(!process.is_null(), "the process {pid} can be opened: {}", std::io::Error::last_os_error());
    let set = unsafe { SetPriorityClass(process, class) };
    let error = std::io::Error::last_os_error();
    // SAFETY: the handle was opened above and is not used after this.
    unsafe { CloseHandle(process) };
    assert_ne!(set, 0, "the class of {pid} can be set: {error}");
}

/// Sets the class of the process `pid`: a case stands its servers where the rule reads them, whatever class this test
/// process, and so every child of it, runs at.
fn set_class(pid: u32, class: u32) {
    put_class(pid, class);
    assert_eq!(class_of(pid), class, "the class of {pid} reads back as the case set it");
}

/// This test process's start in Unix epoch milliseconds, which a ticket naming it as a live gate carries.
fn own_start_ms() -> u64 {
    gate_win32::process_start_of(process::id())
        .expect("this process's start can be read")
        .expect("this process runs")
        .as_millisecond()
        .unsigned_abs()
}

/// Writes, in the case's root, a critical ticket that holds the machine, naming this test process as the gate holding
/// it, and gives the ticket's path.
fn write_holding_ticket(case: &Case) -> PathBuf {
    let start_ms = own_start_ms();
    let ticket = Ticket {
        kind: TicketKind::Critical,
        arrived: 0,
        pid: process::id(),
        start_ms,
        holds: true,
        remote: None,
        expected: None,
    };
    let path = v1::ticket_path(&case.root(), process::id(), start_ms);
    v1::write_ticket(&path, &ticket).expect("the holding ticket can be written");
    path
}

/// Writes, in the case's root, a claim of `kind` on the slot `object` naming this test process as the live gate
/// holding it.
fn write_claim(case: &Case, object: KernelObject, kind: ClaimKind) {
    let start = gate_win32::process_start_of(process::id())
        .expect("this process's start can be read")
        .expect("this process runs");
    let index = match object {
        KernelObject::HeavySlot(index) | KernelObject::LightSlot(index) | KernelObject::RemoteSlot(index) => index,
        _ => 0,
    };
    let claim = Claim {
        kind,
        index,
        pid: process::id(),
        process_start: start,
        started: start,
        cwd: shown(&case.folder()),
        command: "cargo build".to_owned(),
        log: shown(&case.folder().join("claimed.log")),
        timeout_seconds: 60.0,
        remote: None,
        host: None,
        key: None,
        expected: None,
    };
    let path = v1::claim_path(&case.root(), object).expect("a slot has a claim");
    v1::write_claim(&path, &claim).expect("the claim can be written");
}

/// The name of the job the gate `holder` runs its command in under the case's root.
fn job_name(case: &Case, holder: u32) -> String {
    StateRoot::new(&shown(&case.root()), None).object_name(KernelObject::Job { gate_pid: holder })
}

/// A job a case made, named as the job of the gate `holder` is named under the case's root, so the guard reads the
/// processes the case puts in it as that gate's. Its handle is closed when this drops.
struct NamedJob {
    handle: HANDLE,
}

impl NamedJob {
    /// Makes the job named as the gate `holder`'s job is named under the case's root.
    fn new(case: &Case, holder: u32) -> NamedJob {
        let name = job_name(case, holder);
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: the name is NUL-terminated and outlives the call, and no security attributes are given.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), wide.as_ptr()) };
        assert!(!handle.is_null(), "the job {name} can be created: {}", std::io::Error::last_os_error());
        NamedJob { handle }
    }

    /// Puts the process `pid` in the job.
    fn assign(&self, pid: u32) {
        // SAFETY: the handle is opened here and closed before this returns; the calls take no pointers.
        let process = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        assert!(!process.is_null(), "the process {pid} can be opened: {}", std::io::Error::last_os_error());
        let assigned = unsafe { AssignProcessToJobObject(self.handle, process) };
        let error = std::io::Error::last_os_error();
        // SAFETY: the handle was opened above and is not used after this.
        unsafe { CloseHandle(process) };
        assert_ne!(assigned, 0, "the process {pid} joins the job: {error}");
    }
}

impl Drop for NamedJob {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by this case and is closed once, here.
        unsafe { CloseHandle(self.handle) };
    }
}

/// A server a case starts itself, in a job named as the job of the gate `holder` is named under the case's root, so
/// the guard reads it as a server of that gate's job. The server is killed, and the job's handle closed, when this
/// drops.
struct InJob {
    server: Child,
    _job: NamedJob,
}

impl InJob {
    /// Starts the case folder's copy at `exe`, waiting for a file no case writes until it is killed, in a job named as
    /// the gate `holder`'s job is named under the case's root.
    fn start(case: &Case, holder: u32, exe: &str) -> InJob {
        let never = shown(&case.folder().join("in-job-never"));
        let server = Command::new(exe).args(["wait", never.as_str()]).spawn().expect("the server starts");
        let job = NamedJob::new(case, holder);
        job.assign(server.id());
        InJob { server, _job: job }
    }

    /// The server's pid.
    fn pid(&self) -> u32 {
        self.server.id()
    }
}

impl Drop for InJob {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

/// A build server a case starts itself, waiting for a file no case writes until the case ends it. It runs from the case
/// folder, so the case's own confined server scan reads it, and uses no CPU: what its case asserts is the class the
/// gate's pass moves the server to, which the rule sets whatever the server is doing. Killed when this drops.
struct Waiter {
    server: Child,
}

impl Waiter {
    /// Starts the case folder's copy at `exe`, waiting for a file no case writes.
    fn start(case: &Case, exe: &str) -> Waiter {
        Waiter::start_naming(case, exe, &[])
    }

    /// Starts the case folder's copy at `exe` as [`Waiter::start`] does, each of `identity` printed by its own `out`
    /// step first, so it stands in the copy's live command line.
    fn start_naming(case: &Case, exe: &str, identity: &[&str]) -> Waiter {
        let never = shown(&case.folder().join("never"));
        let mut words: Vec<&str> = identity.iter().flat_map(|word| ["out", *word]).collect();
        words.extend(["wait", never.as_str()]);
        let mut command = Command::new(exe);
        case.prepare_without_state_dir(&mut command, &words);
        Waiter {
            server: command.spawn().expect("the server starts"),
        }
    }

    /// The server's pid.
    fn pid(&self) -> u32 {
        self.server.id()
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

#[test]
fn an_ordinary_gate_lowers_a_build_server_outside_its_job() {
    let case = Case::new("an_ordinary_gate_lowers_a_build_server_outside_its_job");
    let compiler = tool(&case, SERVER);
    let command = quiet(&case);
    let command: Vec<&str> = command.iter().map(String::as_str).collect();
    let seams = seams(None);
    let seams: Vec<&str> = seams.iter().map(String::as_str).collect();
    let server = Waiter::start(&case, &compiler);
    set_class(server.pid(), NORMAL_PRIORITY_CLASS);

    let running = case.start(&run_words("out.log", &seams, &command));
    wait_until("the server outside the job to be lowered", || {
        class_of(server.pid()) == BELOW_NORMAL_PRIORITY_CLASS
    });
    fs::write(go_path(&case), "").expect("the go file can be written");
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(class_of(server.pid()), BELOW_NORMAL_PRIORITY_CLASS, "and nothing raises it again");
}

#[test]
fn an_ordinary_gate_tells_a_dotnet_server_by_its_live_command_line() {
    let case = Case::new("an_ordinary_gate_tells_a_dotnet_server_by_its_live_command_line");
    let dotnet = tool(&case, "dotnet.exe");
    let command = quiet(&case);
    let command: Vec<&str> = command.iter().map(String::as_str).collect();
    let seams = seams(None);
    let seams: Vec<&str> = seams.iter().map(String::as_str).collect();
    // Two copies of the same `dotnet` host, both started before the gate: the live command line of one names
    // `VBCSCompiler.dll`, so the gate reads a compiler server out of it, and the other's names no server.
    let plain = Waiter::start(&case, &dotnet);
    let named = Waiter::start_naming(&case, &dotnet, &["VBCSCompiler.dll"]);
    set_class(plain.pid(), NORMAL_PRIORITY_CLASS);
    set_class(named.pid(), NORMAL_PRIORITY_CLASS);

    let running = case.start(&run_words("out.log", &seams, &command));
    wait_until("the dotnet server outside the job to be lowered", || {
        class_of(named.pid()) == BELOW_NORMAL_PRIORITY_CLASS
    });
    // The pass that lowered the named host read the plain one in the same snapshot.
    let plain_class = class_of(plain.pid());
    fs::write(go_path(&case), "").expect("the go file can be written");
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        plain_class, NORMAL_PRIORITY_CLASS,
        "a dotnet host whose command line names no server is no server, so its class is left"
    );
}

#[test]
fn an_ordinary_gate_leaves_a_server_at_high_while_a_critical_ticket_holds() {
    let case = Case::new("an_ordinary_gate_leaves_a_server_at_high_while_a_critical_ticket_holds");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let compiler = tool(&case, SERVER);
    let command = chatter(&case);
    let command: Vec<&str> = command.iter().map(String::as_str).collect();
    let seams = seams(Some(4));
    let seams: Vec<&str> = seams.iter().map(String::as_str).collect();
    let server = Waiter::start(&case, &compiler);
    let ticket = write_holding_ticket(&case);
    set_class(server.pid(), HIGH_PRIORITY_CLASS);

    // The gate is waited for to say it has sampled, so the class below was read after passes that had the ticket.
    let held = case.start(&run_words("held.log", &seams, &command));
    held.wait_for_stderr(INTERRUPTED);
    assert_eq!(
        class_of(server.pid()),
        HIGH_PRIORITY_CLASS,
        "a server at High stays while a critical ticket holds"
    );
    let held = held.finish();
    assert_eq!(held.code, Some(CONTROL_C_EXIT), "{held:?}");

    fs::remove_file(&ticket).expect("the holding ticket can be deleted");
    let running = case.start(&run_words("lowered.log", &seams, &command));
    wait_until("the server to be lowered once nothing holds", || {
        class_of(server.pid()) == BELOW_NORMAL_PRIORITY_CLASS
    });
    let lowered = running.finish();

    assert_eq!(lowered.code, Some(CONTROL_C_EXIT), "{lowered:?}");
}

#[test]
fn an_ordinary_gate_leaves_a_server_in_a_holding_critical_gate_s_job() {
    let case = Case::new("an_ordinary_gate_leaves_a_server_in_a_holding_critical_gate_s_job");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let compiler = tool(&case, SERVER);
    let command = chatter(&case);
    let command: Vec<&str> = command.iter().map(String::as_str).collect();
    let seams = seams(Some(4));
    let seams: Vec<&str> = seams.iter().map(String::as_str).collect();
    // A server in the job of the gate the ticket names, and one outside it, which the same passes must still lower.
    let inside = InJob::start(&case, process::id(), &compiler);
    let ticket = write_holding_ticket(&case);
    let outside = Waiter::start(&case, &compiler);
    // The guard reads that job by the name the holding gate's job is named by: it must hold the server this case put
    // in it, or the case asserts on a server the guard never sees.
    let job = gate_win32::OpenedJob::open_to_list(&job_name(&case, process::id()))
        .expect("the job can be opened")
        .expect("the job the case made exists");
    assert!(
        job.process_ids().expect("the job can be listed").contains(&inside.pid()),
        "the server is in the job {:?}",
        job.name()
    );
    set_class(inside.pid(), NORMAL_PRIORITY_CLASS);
    set_class(outside.pid(), NORMAL_PRIORITY_CLASS);

    let running = case.start(&run_words("out.log", &seams, &command));
    running.wait_for_stderr(INTERRUPTED);
    assert_eq!(
        class_of(inside.pid()),
        NORMAL_PRIORITY_CLASS,
        "a server in the job of the gate whose ticket holds stays at that gate's own priority"
    );
    assert_eq!(
        class_of(outside.pid()),
        BELOW_NORMAL_PRIORITY_CLASS,
        "while a server outside it is lowered by the same passes"
    );
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    fs::remove_file(&ticket).expect("the holding ticket can be deleted");
}

#[test]
fn a_critical_gate_raises_a_build_server_outside_other_gates_jobs() {
    let case = Case::new("a_critical_gate_raises_a_build_server_outside_other_gates_jobs");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let compiler = tool(&case, SERVER);
    let command = quiet(&case);
    let command: Vec<&str> = command.iter().map(String::as_str).collect();
    let seams = seams(None);
    let seams: Vec<&str> = seams.iter().map(String::as_str).collect();
    // A class no other gate's rule moves: only a High gate raises one below High, and a gate that is not High leaves
    // below-normal alone, so the server this gate raises to High stays there.
    let server = Waiter::start(&case, &compiler);
    set_class(server.pid(), BELOW_NORMAL_PRIORITY_CLASS);

    let critical = case.start(&critical_words("critical.log", &seams, &command));
    wait_until("the critical gate to raise the server outside its job", || {
        class_of(server.pid()) == HIGH_PRIORITY_CLASS
    });
    fs::write(go_path(&case), "").expect("the go file can be written");
    let outcome = critical.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

#[test]
fn high_gate_leaves_a_server_in_another_gates_job() {
    let case = Case::new("high_gate_leaves_a_server_in_another_gates_job");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let compiler = tool(&case, SERVER);
    let command = chatter(&case);
    let command: Vec<&str> = command.iter().map(String::as_str).collect();
    let seams = seams(Some(4));
    let seams: Vec<&str> = seams.iter().map(String::as_str).collect();
    // Another live gate holding a slot — a light gate on the reserved slot beside the critical one, named by its claim —
    // with a server in its job; and a server outside every gate's job, which the same passes raise. Below normal is a
    // class no gate that is not High moves.
    let inside = InJob::start(&case, process::id(), &compiler);
    write_claim(&case, KernelObject::LightSlot(0), ClaimKind::Light);
    let outside = Waiter::start(&case, &compiler);
    set_class(inside.pid(), BELOW_NORMAL_PRIORITY_CLASS);
    set_class(outside.pid(), BELOW_NORMAL_PRIORITY_CLASS);

    let critical = case.start(&critical_words("critical.log", &seams, &command));
    wait_until("the critical gate to raise the server outside every gate's job", || {
        class_of(outside.pid()) == HIGH_PRIORITY_CLASS
    });
    // The gate is waited for to say it has sampled, so the class below was read after passes that saw the claim.
    critical.wait_for_stderr(INTERRUPTED);
    assert_eq!(
        class_of(inside.pid()),
        BELOW_NORMAL_PRIORITY_CLASS,
        "a server in another live gate's job stays at that gate's priority"
    );
    let outcome = critical.finish();

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
}

#[test]
fn a_gate_covered_by_a_critical_parent_that_ends_lowers_its_command_and_moves_servers_as_its_own_kind() {
    let case = Case::new("a_gate_covered_by_a_critical_parent_that_ends_lowers_its_command_and_moves_servers_as_its_own_kind");
    let compiler = tool(&case, SERVER);
    let go = go_path(&case);
    let release = case.folder().join("outer.release");
    let mut extra = LOOK_OFTEN.to_vec();
    extra.extend(["--test-server-image", SERVER]);
    let inner = heavy_words("inner.log", &extra, &[FAKE_TOOL, "pid", "wait", &go]);
    // The heavy gate runs under the critical outer gate's slot, as critical: its command at High.
    let (outer, orphan) = orphaned_as(&case, "critical", &inner, &release);
    let log = case.folder().join("inner.log");
    wait_until("the command to say its pid", || {
        fs::read_to_string(&log).is_ok_and(|text| text.lines().any(|line| line.starts_with("pid ")))
    });
    let command = said_pid(&fs::read_to_string(&log).expect("the log can be read"), "pid ");
    assert_eq!(class_of(command), HIGH_PRIORITY_CLASS, "the command runs as critical");

    end_parent(outer, &release);
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, orphan.pid));
    wait_until("the command lowered from High at the take", || {
        class_of(command) == BELOW_NORMAL_PRIORITY_CLASS
    });
    // A server in the job of another live gate holding a slot, which a High gate leaves alone and a heavy gate with no
    // critical request holding the machine lowers: only a gate whose servers and guard both moved off High lowers it.
    let other = InJob::start(&case, process::id(), &compiler);
    write_claim(&case, KernelObject::LightSlot(0), ClaimKind::Light);
    // Not read back: the gate may lower it at once.
    put_class(other.pid(), NORMAL_PRIORITY_CLASS);
    wait_until("the server in the other gate's job lowered as a heavy gate's pass lowers it", || {
        class_of(other.pid()) == BELOW_NORMAL_PRIORITY_CLASS
    });

    fs::write(&go, "").expect("the go file can be written");
    assert_eq!(orphan.ended(), 0);
}

#[test]
fn a_light_gate_under_a_critical_parent_that_ends_runs_as_light_under_a_heavy_stand_in() {
    let case = Case::new("a_light_gate_under_a_critical_parent_that_ends_runs_as_light_under_a_heavy_stand_in");
    let compiler = tool(&case, SERVER);
    let paths = ["c.pid", "c.release", "c-go", "outer.release"].map(|name| case.folder().join(name));
    let [c_pid, c_release, c_go, release] = &paths;
    let texts = paths.each_ref().map(|path| shown(path));
    // Only C reads the case's server image as a server's, so only C moves the server outside every job: up to High
    // while it runs as critical, down to below normal once it runs as light.
    let mut c_extra = LOOK_OFTEN.to_vec();
    c_extra.extend(["--test-server-image", SERVER]);
    let mut a_command = vec![FAKE_TOOL, "spawn-pid", &texts[0], &texts[1], GATE];
    a_command.extend(run_words("c.log", &c_extra, &[FAKE_TOOL, "wait", &texts[2]]));
    let a_words = heavy_words("a.log", &LOOK_OFTEN, &a_command);
    let server = Waiter::start(&case, &compiler);
    set_class(server.pid(), BELOW_NORMAL_PRIORITY_CLASS);

    // O (critical) > A (heavy) > C (light): both run as critical.
    let (outer, a) = orphaned_as(&case, "critical", &a_words, release);
    let c = spawned(&case, c_pid);
    wait_until("C raising the server as a critical gate does", || {
        class_of(server.pid()) == HIGH_PRIORITY_CLASS
    });

    let outer_pid = end_parent(outer, release);
    let c_line = line_with(&case.folder().join("a.log.err"), &format!("gate: the parent gate {outer_pid} ended;"));
    assert_eq!(
        c_line,
        format!("gate: the parent gate {outer_pid} ended; running under the heavy slot of gate {}", a.pid),
        "A stands in with the heavy slot it takes, not the critical one O held"
    );
    wait_until("C lowering the server as a light gate does", || {
        class_of(server.pid()) == BELOW_NORMAL_PRIORITY_CLASS
    });

    let_go(c_go);
    assert_eq!(c.ended(), 0);
    let_go(c_release);
    assert_eq!(a.ended(), 0);
    assert_eq!(events_of(&case, c.pid), ["covered"]);
}

#[test]
fn covered_gate_raises_its_own_jobs_server() {
    let case = Case::new("covered_gate_raises_its_own_jobs_server");
    let compiler = tool(&case, SERVER);
    let go = go_path(&case);
    // The command is a server copy that starts a second copy in the gate's job, waiting for the go file, and then keeps
    // its log growing until that file exists, so the watch's stall clock cannot run while the case waits.
    let command = [compiler.as_str(), "spawn-wait", go.as_str(), "chatter", go.as_str()];
    let seams = seams(None);
    let seams: Vec<&str> = seams.iter().map(String::as_str).collect();
    // This test process stands as the gate's critical parent: its marker covers the gate, and the critical claim names
    // it, as a critical parent's own claim would.
    write_claim(&case, KernelObject::Critical, ClaimKind::Critical);
    let words = critical_words("out.log", &seams, &command);
    let running = start(marked(&case, &words, &test_marker(&case, "critical")));
    let log = case.folder().join("out.log");
    wait_until("the command to start its server", || {
        fs::read_to_string(&log).is_ok_and(|text| {
            text.split_inclusive('\n')
                .any(|line| line.starts_with("spawned ") && line.ends_with('\n'))
        })
    });
    let server = said_pid(&fs::read_to_string(&log).expect("the log can be read"), "spawned ");
    // The server is put in the parent's job, as a covered gate's job is nested in its parent's: the critical claim must
    // not make the gate read its own job's server as another gate's. Its class is set after, so only a pass that reads
    // it in that job can raise it.
    let parent_job = NamedJob::new(&case, process::id());
    parent_job.assign(server);
    put_class(server, BELOW_NORMAL_PRIORITY_CLASS);

    wait_until("the covered gate to raise its own job's server", || {
        class_of(server) == HIGH_PRIORITY_CLASS
    });
    fs::write(&go, "").expect("the go file can be written");
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}
