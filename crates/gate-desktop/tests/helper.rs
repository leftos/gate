//! The desktop helper, run as its own process against a box built here from `gate_win32`'s pieces: the box makes the
//! control pipe, starts the helper with its `Arg0`, checks the client, reads `Hello`, places two pipe write ends in the
//! helper, sends `Spawn`, reads `Child` and takes the child's handles out of the helper, says `Done` and runs the
//! command. Each case breaks one step and reads the helper's exit code.

#![cfg(windows)]
#![expect(clippy::expect_used, reason = "a test that cannot run the helper or its command must fail loudly")]

use std::ffi::OsStr;
use std::io::Read;
use std::ops::Deref;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsHandle, AsRawHandle, OwnedHandle};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use gate_desktop::protocol::{HELPER_FRAME_LIMIT, Message, Spawn, read_message, write_message};
use gate_desktop::{HelperExit, PIPE_PREFIX, arg0};
use gate_win32::{
    ConnectWait, EndReport, Job, PipeClient, PipeOwner, PipeServer, RemoteProcess, RestrictedToken, StoppableProcess, Suspended, creation_time_of,
    current_user_sid, system_directory, thread_is_suspended,
};
use proc_table::win32::Win32ProcessTable;
use windows_sys::Win32::Foundation::{CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE};
use windows_sys::Win32::Security::{ImpersonateLoggedOnUser, RevertToSelf};
use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::SystemServices::SECURITY_MANDATORY_MEDIUM_RID;
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// The report an adopted child is given: a child a drop could not end fails the test that dropped it.
#[expect(clippy::panic, reason = "a child a test's drop could not end must fail that test loudly")]
fn unended() -> EndReport {
    Box::new(|pid, error| panic!("the adopted child {pid} could not be ended: {error}"))
}

/// How long the drop of a [`LeftChild`] waits for the child it ends.
const END_WAIT: Duration = Duration::from_secs(5);

/// A child a case found left behind, opened to be watched and ended by the case: its drop ends it, so an assertion
/// that fails before the case ends the child itself strands no suspended process.
struct LeftChild {
    process: StoppableProcess,
}

impl LeftChild {
    /// Opens the process `pid` a case found and left behind.
    fn open(pid: u32) -> LeftChild {
        LeftChild {
            process: StoppableProcess::open(pid).expect("the left child opens").expect("the left child exists"),
        }
    }
}

impl Deref for LeftChild {
    type Target = StoppableProcess;

    fn deref(&self) -> &StoppableProcess {
        &self.process
    }
}

impl Drop for LeftChild {
    fn drop(&mut self) {
        // Nothing here may fail a test that is already ending; the wait is the moment the end takes.
        let _ = self.process.terminate(1);
        let _ = self.process.wait(END_WAIT);
    }
}

/// The longest any wait of these tests may take: a guard against a hang, never a timing bound.
const HANG_GUARD: Duration = Duration::from_secs(300);

/// The helper's seam that caps each of its limits, in milliseconds.
const LIMIT_VARIABLE: &str = "GATE_DESKTOP_TEST_LIMIT_MS";

/// The helper's seam that picks one step of its run to break.
const FAULT_VARIABLE: &str = "GATE_DESKTOP_TEST_FAULT";

/// The variable the command echoes, set only in the block the box sends.
const PROBE: &str = "GATE_DESKTOP_PROBE";

/// The seam the helper cases run under: far above any stall of a busy machine between two steps, and below the
/// built 30 s wait for `Done`, so a box that never says it still ends the helper's run.
const ROOMY_LIMIT_MS: u64 = 20_000;

/// `ERROR_FILE_NOT_FOUND`.
const ERROR_FILE_NOT_FOUND: u32 = 2;

/// A deadline that only a hang reaches.
fn guard() -> Instant {
    Instant::now() + HANG_GUARD
}

/// `text` as UTF-16.
fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().collect()
}

/// A fresh control pipe for this user.
fn serve() -> PipeServer {
    PipeServer::create(PIPE_PREFIX, &current_user_sid().expect("the user is read"), PipeOwner::User).expect("the pipe is made")
}

/// The helper's seam that, set to `user`, makes it take a control pipe its user owns, as an unelevated test makes it.
const PIPE_OWNER_VARIABLE: &str = "GATE_DESKTOP_TEST_PIPE_OWNER";

/// The helper's seam that names the input desktop in place of the one it reads.
const INPUT_DESKTOP_VARIABLE: &str = "GATE_DESKTOP_TEST_INPUT_DESKTOP";

/// The helper's command, with `args` and, when given, the seam capping its limits at `limit_ms`, the seam that makes
/// it take the pipe this unelevated test makes, which its user owns, and the step `fault` breaks. It reads the input
/// desktop itself, whatever the test's own environment names.
fn helper_command(args: &[&str], limit_ms: Option<u64>, fault: Option<&str>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gate-desktop-helper"));
    command.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    match limit_ms {
        Some(limit) => command.env(LIMIT_VARIABLE, limit.to_string()),
        None => command.env_remove(LIMIT_VARIABLE),
    };
    command.env(PIPE_OWNER_VARIABLE, "user");
    command.env_remove(INPUT_DESKTOP_VARIABLE);
    match fault {
        Some(mode) => command.env(FAULT_VARIABLE, mode),
        None => command.env_remove(FAULT_VARIABLE),
    };
    command
}

/// The helper, started as [`helper_command`] makes it.
fn start_helper(args: &[&str], limit_ms: Option<u64>) -> Child {
    gate_win32::spawn(&mut helper_command(args, limit_ms, None)).expect("the helper starts")
}

/// The helper, started with the step `fault` breaks.
fn start_helper_fault(args: &[&str], limit_ms: Option<u64>, fault: &str) -> Child {
    gate_win32::spawn(&mut helper_command(args, limit_ms, Some(fault))).expect("the helper starts")
}

/// The helper's exit code, once it has exited.
fn exit_of(helper: &mut Child) -> i32 {
    let guard = guard();
    loop {
        if let Some(status) = helper.try_wait().expect("the helper is waited on") {
            return status.code().expect("the helper exited with a code");
        }
        assert!(Instant::now() < guard, "the helper exits");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The code `exit` is, as the helper's process exit code.
fn code(exit: HelperExit) -> i32 {
    i32::try_from(exit.code()).expect("an exit code fits")
}

/// A box with its pipe served and the helper started on it, connected and past `Hello`.
struct Session {
    server: PipeServer,
    helper: Child,
    remote: RemoteProcess,
}

impl Session {
    /// Serves a pipe, starts the helper on it with the seam at `limit_ms`, checks the client is the helper and reads
    /// its `Hello`.
    fn start(limit_ms: Option<u64>) -> Session {
        Session::start_with_fault(limit_ms, None)
    }

    /// As [`Session::start`], with the helper made to break the step `fault` of its run.
    fn start_with_fault(limit_ms: Option<u64>, fault: Option<&str>) -> Session {
        let server = serve();
        let arg = arg0(std::process::id(), server.name()).expect("the pipe's name makes an Arg0");
        let helper = match fault {
            Some(fault) => start_helper_fault(&[&arg], limit_ms, fault),
            None => start_helper(&[&arg], limit_ms),
        };
        let waited = server.wait_for_client(guard(), &[helper.as_handle()]).expect("the connect is waited on");
        assert_eq!(waited, ConnectWait::Connected, "the helper connects");
        assert_eq!(server.client_pid().expect("the client is read"), helper.id(), "the client is the helper");
        let hello = read_message(&server, guard(), HELPER_FRAME_LIMIT, &[]).expect("the hello is read");
        let (version, desktop, ok) = match hello {
            Message::Hello { version, desktop, ok } => Some((version, desktop, ok)),
            _ => None,
        }
        .expect("the helper's first message is a hello");
        assert_eq!(version, gate_desktop::version::build_version());
        assert_eq!(ok, desktop == "Default", "ok says whether the input desktop is Default ({desktop:?})");
        let remote = RemoteProcess::open(helper.id()).expect("the helper opens");
        Session { server, helper, remote }
    }

    /// Places `handle` in the helper, returning its value there.
    fn push(&self, handle: &impl AsHandle) -> u64 {
        self.remote.push(handle.as_handle()).expect("the handle is placed in the helper") as u64
    }

    /// Sends `message` to the helper.
    fn send(&self, message: &Message) {
        write_message(&self.server, message, guard(), &[]).expect("the message is sent");
    }

    /// The helper's next message.
    fn receive(&self) -> Message {
        read_message(&self.server, guard(), HELPER_FRAME_LIMIT, &[]).expect("the helper's message is read")
    }

    /// The pids of the processes the helper started.
    fn children(&self) -> Vec<u32> {
        let started = creation_time_of(self.remote.handle()).expect("the helper's start is read");
        Win32ProcessTable
            .snapshot_without_command_lines()
            .into_iter()
            .filter(|process| process.parent_pid == self.helper.id() && process.created.is_some_and(|created| created >= started))
            .map(|process| process.pid)
            .collect()
    }
}

/// A command for cmd in the system folder, its environment `PROBE=<probe>` and `SystemRoot`, its output to the two
/// handle values.
fn cmd_spawn(command: &str, probe: &str, stdout: u64, stderr: u64) -> Spawn {
    let system = system_directory().expect("the system folder is read");
    let root = std::env::var_os("SystemRoot").expect("SystemRoot is set");
    // Sorted as Windows sorts names, case-insensitively: `GATE_…` before `SystemRoot`.
    let mut environment = wide(format!("{PROBE}={probe}"));
    environment.push(0);
    environment.extend(wide("SystemRoot="));
    environment.extend(wide(&root));
    environment.extend([0, 0]);
    Spawn {
        application: wide(system.join("cmd.exe")),
        command_line: wide(format!("cmd /d /c {command}")),
        directory: wide(&system),
        environment,
        stdout,
        stderr,
    }
}

/// The child a `Child` message reports: its pid, process handle value and thread handle value.
fn child_of(message: &Message) -> (u32, usize, usize) {
    let (pid, process, thread) = match message {
        &Message::Child { pid, process, thread } => Some((pid, process, thread)),
        _ => None,
    }
    .expect("the helper reports its child");
    let value = |value: u64| usize::try_from(value).expect("a handle value fits");
    (pid, value(process), value(thread))
}

#[test]
fn a_spawned_command_runs_with_its_environment_and_output() {
    let mut session = Session::start(Some(ROOMY_LIMIT_MS));
    let (mut out_read, out_write) = std::io::pipe().expect("the stdout pipe is made");
    let (_err_read, err_write) = std::io::pipe().expect("the stderr pipe is made");
    let stdout = session.push(&out_write);
    let stderr = session.push(&err_write);
    drop((out_write, err_write));

    session.send(&Message::Spawn(cmd_spawn(&format!("echo %{PROBE}%"), "from-the-box", stdout, stderr)));
    let (pid, process, thread) = child_of(&session.receive());
    let process = session.remote.pull(process).expect("the process is taken");
    let thread = session.remote.pull(thread).expect("the thread is taken");
    let child = Suspended::adopt(process, thread, pid, unended()).expect("the child is adopted");
    session.send(&Message::Done);
    let helper_exit = exit_of(&mut session.helper);
    let job = Job::anonymous_kill_on_close().expect("the job is made");
    let running = child.assign_and_resume(&job).expect("the child runs");
    let status = running.wait(HANG_GUARD).expect("the child is waited on");
    let mut printed = String::new();
    out_read.read_to_string(&mut printed).expect("the output is read to its end");

    assert_eq!(helper_exit, code(HelperExit::Done));
    assert_eq!(status, Some(0), "the command exits 0");
    assert_eq!(printed.trim_end(), "from-the-box", "the command saw the sent environment");
}

#[test]
fn the_helper_refuses_a_malformed_arg0() {
    let server = serve();
    let hex = server.name().rsplit('-').next().expect("the name has a random part").to_owned();
    let pid = std::process::id();
    let mut shapes = vec![
        String::new(),
        format!("0:{hex}"),
        format!("0{pid}:{hex}"),
        format!("-{pid}:{hex}"),
        format!("+{pid}:{hex}"),
        format!("{pid}a:{hex}"),
        format!("{pid}{hex}"),
        format!("{pid}:{}", &hex[1..]),
        format!("{pid}:{hex}0"),
        format!("{pid}:{hex}:1"),
        format!("{pid}:{}", server.name()),
        format!("{pid}:{}g", &hex[1..]),
        format!("99999999999:{hex}"),
    ];
    if hex.to_uppercase() != hex {
        shapes.push(format!("{pid}:{}", hex.to_uppercase()));
    }

    for shape in &shapes {
        let mut helper = start_helper(&[shape], None);
        assert_eq!(exit_of(&mut helper), code(HelperExit::BadArgument), "{shape:?}");
    }
    let mut none = start_helper(&[], None);
    assert_eq!(exit_of(&mut none), code(HelperExit::BadArgument), "no argument");
    let good = arg0(pid, server.name()).expect("the pipe's name makes an Arg0");
    let mut two = start_helper(&[&good, &good], None);
    assert_eq!(exit_of(&mut two), code(HelperExit::BadArgument), "two arguments");

    let waited = server.wait_for_client(Instant::now(), &[]).expect("the pipe is looked at");
    assert_eq!(waited, ConnectWait::TimedOut, "no helper touched the pipe");
}

#[test]
fn the_helper_exits_no_pipe_when_no_pipe_appears() {
    let unserved = format!("{}:{}", std::process::id(), "0123456789abcdef0123456789abcdef");

    let mut helper = start_helper(&[&unserved], Some(200));

    assert_eq!(exit_of(&mut helper), code(HelperExit::NoPipe));
}

#[test]
fn the_helper_refuses_a_server_whose_pid_is_not_the_box() {
    let server = serve();
    // Pid 4 is the System process: alive, and never this box.
    let other = arg0(4, server.name()).expect("the pipe's name makes an Arg0");

    let mut helper = start_helper(&[&other], None);

    let waited = server.wait_for_client(guard(), &[helper.as_handle()]).expect("the connect is waited on");
    assert_eq!(waited, ConnectWait::Connected, "the helper reached the pipe before its server check");
    assert_eq!(exit_of(&mut helper), code(HelperExit::ServerRefused));
}

#[test]
#[ignore = "needs an elevated token: run elevated with --ignored"]
fn the_production_pipe_passes_the_helpers_security_check() {
    let user = current_user_sid().expect("the user is read");
    let server = PipeServer::create(PIPE_PREFIX, &user, PipeOwner::Administrators).expect("an elevated process makes the Administrators' pipe");

    let name = server.name().to_owned();

    // The helper connects at medium with Administrators deny-only, as this thread does while it impersonates the token.
    let opened = std::thread::spawn(move || {
        let medium = RestrictedToken::of_this_process().expect("the medium restricted token is made");
        // SAFETY: the token is open with `TOKEN_QUERY | TOKEN_DUPLICATE`, the access the call needs, while it runs.
        let impersonated = unsafe { ImpersonateLoggedOnUser(medium.raw_handle()) } != 0;
        assert!(impersonated, "the thread impersonates the token: {}", std::io::Error::last_os_error());
        let opened = PipeClient::open(&name, &user, PipeOwner::Administrators).map(drop);
        // SAFETY: `RevertToSelf` takes no arguments; it ends this thread's impersonation.
        let reverted = unsafe { RevertToSelf() } != 0;
        assert!(reverted, "the impersonation is reverted: {}", std::io::Error::last_os_error());
        opened
    })
    .join()
    .expect("the client thread ends");

    assert!(opened.is_ok(), "the helper's check takes the pipe the box makes: {:?}", opened.err());
}

#[test]
fn the_helper_refuses_a_pipe_not_owned_by_administrators() {
    let server = serve();
    let arg = arg0(std::process::id(), server.name()).expect("the pipe's name makes an Arg0");
    let mut command = helper_command(&[&arg], Some(ROOMY_LIMIT_MS), None);
    // Without the seam the helper takes only a pipe Administrators own, which this test's pipe, its user's, is not.
    command.env_remove(PIPE_OWNER_VARIABLE);

    let mut helper = gate_win32::spawn(&mut command).expect("the helper starts");

    let waited = server.wait_for_client(guard(), &[helper.as_handle()]).expect("the connect is waited on");
    assert_eq!(waited, ConnectWait::Connected, "the helper opened the pipe before reading its owner");
    assert_eq!(exit_of(&mut helper), code(HelperExit::ServerRefused));
    let after = read_message(&server, guard(), HELPER_FRAME_LIMIT, &[]).expect_err("the refusing helper sent nothing");
    assert!(matches!(after, gate_desktop::protocol::ReadError::Closed), "no hello was sent: {after}");
}

/// The value of the helper's own handle to the control pipe: the one pipe handle in its table whose server is this
/// process, found before any other pipe of this process is placed there.
fn control_value(session: &Session) -> u64 {
    let mut found = Vec::new();
    for value in (4..=0x4000_usize).step_by(4) {
        let mut copy: HANDLE = std::ptr::null_mut();
        // SAFETY: the helper is open with `PROCESS_DUP_HANDLE`, the value is only looked up in its table, this
        // process's pseudo-handle is always open and `copy` is writable. No source handle is closed.
        let copied = unsafe {
            DuplicateHandle(
                session.remote.handle().as_raw_handle(),
                std::ptr::without_provenance_mut(value),
                GetCurrentProcess(),
                &raw mut copy,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if copied == 0 {
            continue;
        }
        let mut server = 0;
        // SAFETY: `copy` is a fresh handle of this process, open for both calls; `server` is writable.
        let ours = unsafe { GetFileType(copy) == FILE_TYPE_PIPE && GetNamedPipeServerProcessId(copy, &raw mut server) != 0 };
        // SAFETY: `copy` is this process's own and closed once.
        unsafe { CloseHandle(copy) };
        if ours && server == std::process::id() {
            found.push(value as u64);
        }
    }
    assert_eq!(found.len(), 1, "the helper holds one pipe of this process: {found:?}");
    found[0]
}

/// Sends the raw frame body `body`, as a box that writes its own JSON would.
fn send_raw(session: &Session, body: &str) {
    let mut frame = u32::try_from(body.len()).expect("the body fits").to_le_bytes().to_vec();
    frame.extend_from_slice(body.as_bytes());
    let written = session.server.write_all(&frame, guard(), &[]).expect("the frame is written");
    assert_eq!(written, gate_win32::PipeWrite::Written);
}

/// A spawn's JSON, its fields valid but for `field`, given as raw JSON.
fn spawn_json(stdout: u64, stderr: u64, field: &str, raw: &str) -> String {
    let good = cmd_spawn("exit 0", "x", stdout, stderr);
    let body = String::from_utf8(gate_desktop::protocol::encode(&Message::Spawn(good))).expect("the body is UTF-8");
    let mut json: serde_json::Value = serde_json::from_str(&body).expect("the body is JSON");
    json[field] = serde_json::from_str(raw).expect("the replacement is JSON");
    json.to_string()
}

/// Sends a bad spawn, given the helper's control pipe value, the values of the two pipes placed in it, and a second
/// value of the stdout pipe.
type SendBad = fn(&Session, u64, u64, u64, u64);

#[test]
fn a_spawn_with_a_bad_field_is_refused() {
    let cases: [(&str, SendBad); 5] = [
        ("bad base64", |session, _, stdout, stderr, _| {
            send_raw(session, &spawn_json(stdout, stderr, "command_line", "\"!!!!\""));
        }),
        ("an odd byte count", |session, _, stdout, stderr, _| {
            send_raw(session, &spawn_json(stdout, stderr, "application", "\"QUJD\""));
        }),
        ("the control pipe as stdout", |session, control, _, stderr, _| {
            session.send(&Message::Spawn(cmd_spawn("exit 0", "x", control, stderr)));
        }),
        ("one value twice", |session, _, stdout, _, _| {
            session.send(&Message::Spawn(cmd_spawn("exit 0", "x", stdout, stdout)));
        }),
        ("two values of one pipe", |session, _, stdout, _, again| {
            session.send(&Message::Spawn(cmd_spawn("exit 0", "x", stdout, again)));
        }),
    ];

    for (case, send) in cases {
        let mut session = Session::start(Some(ROOMY_LIMIT_MS));
        let control = control_value(&session);
        let (_out_read, out_write) = std::io::pipe().expect("the stdout pipe is made");
        let (_err_read, err_write) = std::io::pipe().expect("the stderr pipe is made");
        let (stdout, stderr) = (session.push(&out_write), session.push(&err_write));
        let again = session.push(&out_write);

        send(&session, control, stdout, stderr, again);

        assert_eq!(exit_of(&mut session.helper), code(HelperExit::BadSpawn), "{case}");
        assert_eq!(session.children(), Vec::<u32>::new(), "{case}: the helper started nothing");
    }
}

#[test]
fn a_spawn_whose_program_cannot_start_reports_failed() {
    let mut session = Session::start(Some(ROOMY_LIMIT_MS));
    let (_out_read, out_write) = std::io::pipe().expect("the stdout pipe is made");
    let (_err_read, err_write) = std::io::pipe().expect("the stderr pipe is made");
    let (stdout, stderr) = (session.push(&out_write), session.push(&err_write));
    let mut spawn = cmd_spawn("exit 0", "x", stdout, stderr);
    let missing = system_directory()
        .expect("the system folder is read")
        .join("gate-desktop-no-such-program.exe");
    spawn.application = wide(&missing);

    session.send(&Message::Spawn(spawn));

    assert_eq!(session.receive(), Message::Failed { code: ERROR_FILE_NOT_FOUND });
    assert_eq!(exit_of(&mut session.helper), code(HelperExit::SpawnFailed));
}

/// Starts a session, places two fresh pipes in the helper, sends a spawn of a command that waits on nothing, and
/// reads the reported child.
fn reported_child(limit_ms: Option<u64>) -> (Session, u32, usize, usize) {
    let session = Session::start(limit_ms);
    let (_out_read, out_write) = std::io::pipe().expect("the stdout pipe is made");
    let (_err_read, err_write) = std::io::pipe().expect("the stderr pipe is made");
    let (stdout, stderr) = (session.push(&out_write), session.push(&err_write));
    session.send(&Message::Spawn(cmd_spawn("exit 0", "x", stdout, stderr)));
    let (pid, process, thread) = child_of(&session.receive());
    (session, pid, process, thread)
}

#[test]
fn a_box_that_closes_before_done_has_its_child_ended() {
    let (session, pid, _, _) = reported_child(Some(ROOMY_LIMIT_MS));
    let watch = LeftChild::open(pid);
    let Session { server, mut helper, remote } = session;

    drop(server);

    assert_eq!(exit_of(&mut helper), code(HelperExit::BoxGone));
    assert!(watch.wait(HANG_GUARD).expect("the child is waited on"), "the child was ended");
    assert_eq!(watch.exit_code().expect("the exit code is read"), 1, "ended unstarted");
    drop(remote);
}

#[test]
fn a_box_that_never_says_done_leaves_the_child() {
    let (mut session, pid, process, thread) = reported_child(Some(ROOMY_LIMIT_MS));
    let watch = LeftChild::open(pid);
    let process: OwnedHandle = session.remote.pull(process).expect("the process is taken");
    let thread: OwnedHandle = session.remote.pull(thread).expect("the thread is taken");

    let helper_exit = exit_of(&mut session.helper);

    assert_eq!(helper_exit, code(HelperExit::NoDone));
    assert!(!watch.wait(Duration::ZERO).expect("the child is looked at"), "the child still lives");
    assert!(
        thread_is_suspended(thread.as_handle()).expect("the thread is read"),
        "the child is still suspended"
    );
    drop(Suspended::adopt(process, thread, pid, unended()).expect("the child is adopted"));
    assert!(watch.wait(HANG_GUARD).expect("the child is waited on"), "the test ended the child");
}

#[test]
fn a_launch_that_fails_before_create_process_exits_setup_failed_without_failed() {
    let mut session = Session::start(Some(ROOMY_LIMIT_MS));
    let (_out_read, out_write) = std::io::pipe().expect("the stdout pipe is made");
    let (_err_read, err_write) = std::io::pipe().expect("the stderr pipe is made");
    let (stdout, stderr) = (session.push(&out_write), session.push(&err_write));
    let mut spawn = cmd_spawn("exit 0", "x", stdout, stderr);
    // A relative application is refused before `CreateProcessW` is called.
    spawn.application = wide("cmd.exe");

    session.send(&Message::Spawn(spawn));

    assert_eq!(exit_of(&mut session.helper), code(HelperExit::SetupFailed));
    let after = read_message(&session.server, guard(), HELPER_FRAME_LIMIT, &[]).expect_err("nothing follows the spawn");
    assert!(matches!(after, gate_desktop::protocol::ReadError::Closed), "no Failed was sent: {after}");
    assert_eq!(session.children(), Vec::<u32>::new(), "the helper started nothing");
}

#[test]
fn a_box_that_sends_another_message_after_the_child_leaves_it() {
    let (mut session, pid, process, thread) = reported_child(Some(ROOMY_LIMIT_MS));
    let watch = LeftChild::open(pid);
    let process: OwnedHandle = session.remote.pull(process).expect("the process is taken");
    let thread: OwnedHandle = session.remote.pull(thread).expect("the thread is taken");

    session.send(&Message::Hello {
        version: "stray".to_owned(),
        desktop: "Default".to_owned(),
        ok: true,
    });

    assert_eq!(exit_of(&mut session.helper), code(HelperExit::BadMessage));
    assert!(!watch.wait(Duration::ZERO).expect("the child is looked at"), "the child still lives");
    assert!(
        thread_is_suspended(thread.as_handle()).expect("the thread is read"),
        "the child is still suspended"
    );
    drop(Suspended::adopt(process, thread, pid, unended()).expect("the child is adopted"));
    assert!(watch.wait(HANG_GUARD).expect("the child is waited on"), "the test ended the child");
}

#[test]
fn a_box_that_sends_no_spawn_times_the_helper_out() {
    // The test takes no step of its own between the helper's start and its exit, so no stall of the test can race the
    // seam's 1 s. The pipe exists before the helper starts, so the helper's first try to connect succeeds without the
    // box waiting for it, and its hello fits in the pipe's buffer; the seam then bounds only its wait for `Spawn`. Exit
    // 18 says it got that far: no child is ever made, and a hello that failed would exit 13 or 19.
    let server = serve();
    let arg = arg0(std::process::id(), server.name()).expect("the pipe's name makes an Arg0");

    let mut helper = start_helper(&[&arg], Some(1_000));

    assert_eq!(exit_of(&mut helper), code(HelperExit::TimedOut));
    drop(server);
}

#[test]
fn a_box_that_closes_before_spawn_is_gone() {
    let Session { server, mut helper, remote } = Session::start(Some(ROOMY_LIMIT_MS));

    drop(server);

    assert_eq!(exit_of(&mut helper), code(HelperExit::BoxGone));
    drop(remote);
}

#[test]
fn a_box_that_disconnects_before_spawn_is_gone() {
    let mut session = Session::start(Some(ROOMY_LIMIT_MS));

    session.server.disconnect().expect("the helper is disconnected");

    assert_eq!(exit_of(&mut session.helper), code(HelperExit::BoxGone));
}

#[test]
fn a_box_that_sends_another_message_in_place_of_spawn_is_refused() {
    let mut session = Session::start(Some(ROOMY_LIMIT_MS));

    session.send(&Message::Done);

    assert_eq!(exit_of(&mut session.helper), code(HelperExit::BadSpawn));
    assert_eq!(session.children(), Vec::<u32>::new(), "the helper started nothing");
}

#[test]
fn the_helper_runs_only_at_medium_integrity_or_below() {
    let facts = gate_win32::token_facts().expect("the test's token is read");
    let above_medium = facts.elevated || facts.integrity > SECURITY_MANDATORY_MEDIUM_RID.cast_unsigned();
    let server = serve();
    let arg = arg0(std::process::id(), server.name()).expect("the pipe's name makes an Arg0");

    let mut helper = start_helper(&[&arg], Some(ROOMY_LIMIT_MS));

    if above_medium {
        assert_eq!(
            exit_of(&mut helper),
            code(HelperExit::WrongIntegrity),
            "an elevated helper refuses to run"
        );
        let waited = server.wait_for_client(Instant::now(), &[]).expect("the pipe is looked at");
        assert_eq!(waited, ConnectWait::TimedOut, "the refusing helper touched no pipe");
    } else {
        let waited = server.wait_for_client(guard(), &[helper.as_handle()]).expect("the connect is waited on");
        assert_eq!(waited, ConnectWait::Connected, "a medium helper goes on to the pipe");
        // Past its hello, the helper waits for `Spawn`, so the drop is read as the pipe closed.
        let hello = read_message(&server, guard(), HELPER_FRAME_LIMIT, &[]).expect("the hello is read");
        assert!(matches!(hello, Message::Hello { .. }), "the helper's first message is a hello: {hello:?}");
        drop(server);
        assert_eq!(exit_of(&mut helper), code(HelperExit::BoxGone));
    }
}

#[test]
fn exit_before_hello_mode_exits_without_a_hello() {
    let server = serve();
    let arg = arg0(std::process::id(), server.name()).expect("the pipe's name makes an Arg0");

    let mut helper = start_helper_fault(&[&arg], Some(ROOMY_LIMIT_MS), "exit-before-hello");

    let waited = server.wait_for_client(guard(), &[helper.as_handle()]).expect("the connect is waited on");
    assert_eq!(waited, ConnectWait::Connected, "the helper connects before it exits");
    assert_eq!(
        exit_of(&mut helper),
        code(HelperExit::BoxGone),
        "it exits as a helper whose box went away"
    );
    let after = read_message(&server, guard(), HELPER_FRAME_LIMIT, &[]).expect_err("the helper sent no hello");
    assert!(
        matches!(after, gate_desktop::protocol::ReadError::Closed),
        "its end closed the pipe: {after}"
    );
}

#[test]
fn hang_mode_never_answers_hello() {
    // The seam caps the mode's wait on the pipe at 1 s, so the helper ends by itself and the case needs no bound of
    // its own. The pipe exists before it starts, so its connect succeeds at once and the 1 s bounds only that read.
    let server = serve();
    let arg = arg0(std::process::id(), server.name()).expect("the pipe's name makes an Arg0");

    let mut helper = start_helper_fault(&[&arg], Some(1_000), "hang");

    assert_eq!(
        exit_of(&mut helper),
        code(HelperExit::TimedOut),
        "the hanging helper ends at its own limit, having said nothing"
    );
    let after = read_message(&server, guard(), HELPER_FRAME_LIMIT, &[]).expect_err("the helper sent no hello");
    assert!(
        matches!(after, gate_desktop::protocol::ReadError::Closed),
        "its end closed the pipe with nothing in it: {after}"
    );
}

#[test]
fn wrong_version_mode_says_another_build() {
    let server = serve();
    let arg = arg0(std::process::id(), server.name()).expect("the pipe's name makes an Arg0");

    let mut helper = start_helper_fault(&[&arg], Some(ROOMY_LIMIT_MS), "wrong-version");

    let waited = server.wait_for_client(guard(), &[helper.as_handle()]).expect("the connect is waited on");
    assert_eq!(waited, ConnectWait::Connected, "the helper connects");
    let hello = read_message(&server, guard(), HELPER_FRAME_LIMIT, &[]).expect("the hello is read");
    let (version, desktop, ok) = match hello {
        Message::Hello { version, desktop, ok } => Some((version, desktop, ok)),
        _ => None,
    }
    .expect("the helper's first message is a hello");

    assert_eq!(ok, desktop == "Default", "the desktop half is the helper's own ({desktop:?})");
    assert_eq!(version, "0.0.0+fault", "the build the wrong-version fault names");
    assert_ne!(version, gate_desktop::version::build_version(), "which is no build of this helper");

    drop(server);
    assert_eq!(exit_of(&mut helper), code(HelperExit::BoxGone), "the helper ends with its pipe");
}

#[test]
fn the_input_desktop_seam_names_the_desktop_the_hello_judges() {
    for (named, wanted_ok) in [("Default", true), ("Other", false)] {
        let server = serve();
        let arg = arg0(std::process::id(), server.name()).expect("the pipe's name makes an Arg0");
        let mut command = helper_command(&[&arg], Some(ROOMY_LIMIT_MS), None);
        command.env(INPUT_DESKTOP_VARIABLE, named);

        let mut helper = gate_win32::spawn(&mut command).expect("the helper starts");

        let waited = server.wait_for_client(guard(), &[helper.as_handle()]).expect("the connect is waited on");
        assert_eq!(waited, ConnectWait::Connected, "the helper connects");
        let hello = read_message(&server, guard(), HELPER_FRAME_LIMIT, &[]).expect("the hello is read");
        let (desktop, ok) = match hello {
            Message::Hello { desktop, ok, .. } => Some((desktop, ok)),
            _ => None,
        }
        .expect("the helper's first message is a hello");
        assert_eq!(desktop, named, "the hello names the seam's desktop, whatever desktop the helper is on");
        assert_eq!(ok, wanted_ok, "the helper still judges the name it was given ({named:?})");

        drop(server);
        assert_eq!(exit_of(&mut helper), code(HelperExit::BoxGone), "the helper ends with its pipe");
    }
}

#[test]
fn lie_handle_mode_reports_a_bad_process_value() {
    let mut session = Session::start_with_fault(Some(ROOMY_LIMIT_MS), Some("lie-handle"));
    let (_out_read, out_write) = std::io::pipe().expect("the stdout pipe is made");
    let (_err_read, err_write) = std::io::pipe().expect("the stderr pipe is made");
    let (stdout, stderr) = (session.push(&out_write), session.push(&err_write));
    drop((out_write, err_write));

    session.send(&Message::Spawn(cmd_spawn("exit 0", "x", stdout, stderr)));
    let (pid, process, thread) = child_of(&session.receive());
    let left = LeftChild::open(pid);
    assert_eq!(process, thread, "the lie reports the child's thread where the box reads its process");
    // Taken while the helper still runs: once it has exited, no value of its table can be pulled, which would say
    // nothing about the value it reported. The lie is a live handle, so the box's first pull succeeds, and it holds a
    // thread where it means to hold the child's process.
    let taken = session
        .remote
        .pull(process)
        .expect("the reported process value is a live handle of the helper's table");
    session.send(&Message::Done);
    let helper_exit = exit_of(&mut session.helper);

    assert_eq!(helper_exit, code(HelperExit::Done), "the mode waits for done as normal");
    // The pair the box pulled goes through `Suspended::adopt`, the check every hand-off runs: a thread handle cannot
    // stand for the child's process. The box holds one handle of that value, so the second of the pair is a copy.
    let pair = taken.try_clone().expect("the handle is taken again");
    let refused = Suspended::adopt(taken, pair, pid, unended());

    assert!(refused.is_err(), "the pair is refused as the child's process and thread");
    // The child the mode left behind is the test's to end; a failed assertion above leaves that to the guard.
    left.terminate(1).expect("the left child is ended");
    assert!(left.wait(HANG_GUARD).expect("the child is waited on"), "the left child ended");
}

#[test]
fn exit_after_create_mode_leaves_a_suspended_child() {
    let mut session = Session::start_with_fault(Some(ROOMY_LIMIT_MS), Some("exit-after-create"));
    let (_out_read, out_write) = std::io::pipe().expect("the stdout pipe is made");
    let (_err_read, err_write) = std::io::pipe().expect("the stderr pipe is made");
    let (stdout, stderr) = (session.push(&out_write), session.push(&err_write));
    drop((out_write, err_write));

    session.send(&Message::Spawn(cmd_spawn("exit 0", "x", stdout, stderr)));

    let helper_exit = exit_of(&mut session.helper);
    let started = session.children();
    // One guard per pid, before any assertion: whatever the helper left is ended even by a case that fails here.
    let left: Vec<LeftChild> = started.iter().copied().map(LeftChild::open).collect();

    assert_eq!(helper_exit, code(HelperExit::Internal), "the mode exits without reporting the child");
    assert_eq!(left.len(), 1, "the helper started one child and left it: {started:?}");
    let watch = &left[0];
    assert!(
        !watch.wait(Duration::ZERO).expect("the child is looked at"),
        "the child still lives after the helper exited"
    );
    watch.terminate(1).expect("the left child is ended");
    assert!(watch.wait(HANG_GUARD).expect("the child is waited on"), "the child ended");
}
