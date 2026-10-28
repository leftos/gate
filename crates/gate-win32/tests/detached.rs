//! A command started detached runs on its own, its output in the one file it was given, and nothing but its pid comes
//! back.

#![cfg(windows)]
#![expect(clippy::expect_used, reason = "a test that cannot start its command or read its output must fail loudly")]

use std::ffi::OsString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::{Duration, Instant};

use gate_win32::{EnvironmentBlock, start_detached};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// The longest the command may take to write its line: a guard against a hang, never a timing bound.
const HANG_GUARD: Duration = Duration::from_secs(600);

static BREAKAWAY: Once = Once::new();

/// Puts this test process in a fresh job that allows breakaway, nested in the job it already runs in, as the piped
/// tests do: the gate's own job refuses breakaway, and a detached start asks for it.
fn allow_breakaway() {
    BREAKAWAY.call_once(|| {
        // SAFETY: no security attributes and no name are allowed; a null handle is a failure.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        assert!(!job.is_null(), "the breakaway job is created: {}", std::io::Error::last_os_error());
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_BREAKAWAY_OK;
        let size = u32::try_from(size_of_val(&limits)).expect("the limits fit a u32");
        // SAFETY: the job is open and `limits` is a whole extended limit structure of `size` bytes.
        let set = unsafe { SetInformationJobObject(job, JobObjectExtendedLimitInformation, (&raw const limits).cast(), size) };
        assert_ne!(set, 0, "breakaway is allowed: {}", std::io::Error::last_os_error());
        // SAFETY: the job is open and the pseudo-handle of this process is always open.
        let assigned = unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) };
        assert_ne!(assigned, 0, "this process enters the job: {}", std::io::Error::last_os_error());
    });
}

/// A fresh folder of this test's own.
fn case_dir(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("gate-win32-detached-{}-{test}", std::process::id()));
    // A folder left by an earlier run of the same pid is cleared first.
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("the case folder is made");
    dir
}

/// Waits, within the hang guard, until the file at `path` holds `needle`; what it holds then.
fn wait_for_text(path: &Path, needle: &str) -> String {
    let started = Instant::now();
    loop {
        let text = fs::read_to_string(path).unwrap_or_default();
        if text.contains(needle) {
            return text;
        }
        assert!(
            started.elapsed() < HANG_GUARD,
            "hung: {} never held {needle:?}; it holds {text:?}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_detached_command_writes_its_output_to_the_given_file_and_its_pid_comes_back() {
    allow_breakaway();
    let dir = case_dir("output");
    let log = dir.join("detached.log");
    let output = File::create(&log).expect("the output file is made");
    let system = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot is set")).join("System32");
    let args: Vec<OsString> = ["/d", "/c", "echo detached line& echo to stderr 1>&2"].map(OsString::from).to_vec();
    let environment = EnvironmentBlock::new(std::env::vars_os()).expect("the block is built");

    let pid = start_detached(&system.join("cmd.exe"), &args, &environment, &dir, &output).expect("the command starts");
    drop(output);

    assert_ne!(pid, 0, "a pid comes back");
    let text = wait_for_text(&log, "to stderr");
    assert!(text.contains("detached line"), "stdout went to the file: {text:?}");
}

/// The variable that makes [`detached_console_probe`] say what console it runs with, rather than pass at once.
const PROBE_VARIABLE: &str = "GATE_WIN32_DETACHED_PROBE";

#[link(name = "user32")]
unsafe extern "system" {
    /// `IsWindowVisible`, which windows-sys gives only under a feature this crate does not take.
    fn IsWindowVisible(window: *mut std::ffi::c_void) -> i32;
}

/// Run by this test binary itself as the detached command of
/// [`a_detached_command_has_a_hidden_console_of_its_own`]: says whether it is attached to a console, which the console
/// programs it starts share rather than each opening a window of its own, and whether that console has a visible window.
/// Run any other way, it does nothing.
#[test]
fn detached_console_probe() {
    use std::io::Write as _;

    use windows_sys::Win32::System::Console::{GetConsoleProcessList, GetConsoleWindow};

    if std::env::var_os(PROBE_VARIABLE).is_none() {
        return;
    }
    let mut attached = [0_u32; 1];
    // SAFETY: the list is writable for its one entry; a process with no console is answered with 0.
    let console = unsafe { GetConsoleProcessList(attached.as_mut_ptr(), 1) } != 0;
    // SAFETY: the call takes no argument.
    let window = unsafe { GetConsoleWindow() };
    // SAFETY: a null or stale window handle is answered with false.
    let visible = !window.is_null() && unsafe { IsWindowVisible(window) } != 0;
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "probe: console={console} visible={visible}").expect("the probe's line is written");
    stdout.flush().expect("the probe's line is flushed");
}

/// A console program the detached command starts must not open a window on the user's desktop: the command has a
/// console of its own, hidden, for its children to share.
#[test]
fn a_detached_command_has_a_hidden_console_of_its_own() {
    allow_breakaway();
    let dir = case_dir("console");
    let log = dir.join("probe.log");
    let output = File::create(&log).expect("the output file is made");
    let args: Vec<OsString> = ["detached_console_probe", "--exact", "--nocapture"].map(OsString::from).to_vec();
    let variables = std::env::vars_os().chain([(OsString::from(PROBE_VARIABLE), OsString::from("1"))]);
    let environment = EnvironmentBlock::new(variables).expect("the block is built");
    let exe = std::env::current_exe().expect("this test binary's path is read");

    start_detached(&exe, &args, &environment, &dir, &output).expect("the probe starts");
    drop(output);

    let text = wait_for_text(&log, "probe: ");
    assert!(
        text.contains("probe: console=true visible=false"),
        "a hidden console of its own: {text:?}"
    );
}
