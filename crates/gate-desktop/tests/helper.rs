//! The desktop helper, run as its own process against a box built here from `gate_win32`'s pieces: the box makes the
//! control pipe, starts the helper with its `Arg0`, checks the client, reads `Hello`, places two pipe write ends in the
//! helper, sends `Spawn`, reads `Child` and takes the child's handles out of the helper, says `Done` and runs the
//! command. Each case breaks one step and reads the helper's exit code.

#![cfg(windows)]
#![expect(clippy::expect_used, reason = "a test that cannot run the helper or its command must fail loudly")]

use std::ffi::OsStr;
use std::io::Read;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsHandle, AsRawHandle, OwnedHandle};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use gate_desktop::protocol::{HELPER_FRAME_LIMIT, Message, Spawn, read_message, write_message};
use gate_desktop::{HelperExit, PIPE_PREFIX, arg0};
use gate_win32::{
    ConnectWait, Job, PipeServer, RemoteProcess, StoppableProcess, Suspended, creation_time_of, current_user_sid, system_directory,
    thread_is_suspended,
};
use proc_table::win32::Win32ProcessTable;
use windows_sys::Win32::Foundation::{CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// The longest any wait of these tests may take: a guard against a hang, never a timing bound.
const HANG_GUARD: Duration = Duration::from_secs(300);

/// The helper's seam that caps each of its limits, in milliseconds.
const LIMIT_VARIABLE: &str = "GATE_DESKTOP_TEST_LIMIT_MS";

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
    PipeServer::create(PIPE_PREFIX, &current_user_sid().expect("the user is read")).expect("the pipe is made")
}

/// The helper, started with `args` and, when given, the seam capping its limits at `limit_ms`.
fn start_helper(args: &[&str], limit_ms: Option<u64>) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gate-desktop-helper"));
    command.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    match limit_ms {
        Some(limit) => command.env(LIMIT_VARIABLE, limit.to_string()),
        None => command.env_remove(LIMIT_VARIABLE),
    };
    gate_win32::spawn(&mut command).expect("the helper starts")
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
        let server = serve();
        let arg = arg0(std::process::id(), server.name()).expect("the pipe's name makes an Arg0");
        let helper = start_helper(&[&arg], limit_ms);
        let waited = server.wait_for_client(guard(), &[helper.as_handle()]).expect("the connect is waited on");
        assert_eq!(waited, ConnectWait::Connected, "the helper connects");
        assert_eq!(server.client_pid().expect("the client is read"), helper.id(), "the client is the helper");
        let hello = read_message(&server, guard(), HELPER_FRAME_LIMIT).expect("the hello is read");
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
        write_message(&self.server, message, guard()).expect("the message is sent");
    }

    /// The helper's next message.
    fn receive(&self) -> Message {
        read_message(&self.server, guard(), HELPER_FRAME_LIMIT).expect("the helper's message is read")
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
    let child = Suspended::adopt(process, thread, pid).expect("the child is adopted");
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
    let watch = StoppableProcess::open(pid).expect("the child opens").expect("the child exists");
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
    let watch = StoppableProcess::open(pid).expect("the child opens").expect("the child exists");
    let process: OwnedHandle = session.remote.pull(process).expect("the process is taken");
    let thread: OwnedHandle = session.remote.pull(thread).expect("the thread is taken");

    let helper_exit = exit_of(&mut session.helper);

    assert_eq!(helper_exit, code(HelperExit::NoDone));
    assert!(!watch.wait(Duration::ZERO).expect("the child is looked at"), "the child still lives");
    assert!(
        thread_is_suspended(thread.as_handle()).expect("the thread is read"),
        "the child is still suspended"
    );
    drop(Suspended::adopt(process, thread, pid).expect("the child is adopted"));
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
    let after = read_message(&session.server, guard(), HELPER_FRAME_LIMIT).expect_err("nothing follows the spawn");
    assert!(matches!(after, gate_desktop::protocol::ReadError::Closed), "no Failed was sent: {after}");
    assert_eq!(session.children(), Vec::<u32>::new(), "the helper started nothing");
}

#[test]
fn a_box_that_sends_another_message_after_the_child_leaves_it() {
    let (mut session, pid, process, thread) = reported_child(Some(ROOMY_LIMIT_MS));
    let watch = StoppableProcess::open(pid).expect("the child opens").expect("the child exists");
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
    drop(Suspended::adopt(process, thread, pid).expect("the child is adopted"));
    assert!(watch.wait(HANG_GUARD).expect("the child is waited on"), "the test ended the child");
}

#[test]
fn a_box_that_sends_no_spawn_times_the_helper_out() {
    // Only the wait for `Spawn` runs out: the helper's connect and hello are done before the test reads the hello.
    let mut session = Session::start(Some(1_000));

    assert_eq!(exit_of(&mut session.helper), code(HelperExit::TimedOut));
}

#[test]
fn a_box_that_closes_before_spawn_is_gone() {
    let Session { server, mut helper, remote } = Session::start(Some(ROOMY_LIMIT_MS));

    drop(server);

    assert_eq!(exit_of(&mut helper), code(HelperExit::BoxGone));
    drop(remote);
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
    let above_medium = facts.elevated || facts.integrity > 0x2000;
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
        drop(server);
        assert_eq!(exit_of(&mut helper), code(HelperExit::BoxGone));
    }
}
