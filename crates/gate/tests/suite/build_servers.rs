//! The build servers a watch counts: a quiet command whose job holds a build client is kept from stalling by a compiler
//! server burning outside its job, and is left to stall once that server goes quiet, or is not a client at all, or
//! shares the machine only with an old `MSBuild` node.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::support::{Case, Running, assert_in_order, copy_tool, release, run_words, shown, start, wait_until};

/// The status Windows reports for a process Ctrl+C ended, `STATUS_CONTROL_C_EXIT`, as a signed exit code.
const CONTROL_C_EXIT: i32 = 0xC000_013A_u32.cast_signed();

/// Copies fake-tool into the case folder as `name` and returns its absolute path, so a command or a burner names it
/// inside the case's working directory.
pub(crate) fn tool(case: &Case, name: &str) -> String {
    copy_tool(case, name);
    shown(&case.folder().join(name))
}

/// The words a case's gate runs with: a 100 ms sample, a 2 s stall window, and the interrupt at 60 samples, well past
/// it. "Not stalled" is shown by the sample count, and no case waits on a clock. The server scan is the run words' own,
/// confined to the case's folder, so a build server another case — or another session on this machine — started is
/// never counted.
fn seams() -> Vec<String> {
    let mut words = seams_with_stall("2000");
    words.extend(["--test-interrupt-after-samples", "60"].map(str::to_owned));
    words
}

/// The words of a gate that only a stall ends: a 100 ms sample and a stall window of `stall_ms` milliseconds, with no
/// interrupt.
fn seams_with_stall(stall_ms: &str) -> Vec<String> {
    ["--test-sample-ms", "100", "--test-stall-ms", stall_ms].map(str::to_owned).to_vec()
}

/// The words a burner runs: each of `identity` printed by its own `out` step, so it stands in the burner's command
/// line, then `burn <stop>` until the case's stop file exists, then a sleep longer than any case, under the suite's
/// hang guard, that keeps the process alive and quiet for the rest of the case; the burner's drop kills it.
fn burn_words(identity: &[&str], stop: &str) -> Vec<String> {
    let mut words: Vec<String> = identity.iter().flat_map(|word| ["out", *word]).map(str::to_owned).collect();
    words.extend(["burn", stop, "sleep", "590000"].map(str::to_owned));
    words
}

/// A build server a case started outside the job, burning until its stop file exists. The file is created when the
/// burner drops, so a case that fails before its last line still stops it.
pub(crate) struct Burner {
    /// The burner's process id, as the diagnostics block names it.
    pub(crate) pid: u32,
    /// The file whose existence ends the burn.
    stop: PathBuf,
    /// The copy itself, killed when the burner drops, so a case never leaves one behind.
    _running: Running,
}

impl Burner {
    /// Starts the case folder's copy at `exe` with `identity` as the words of its command line, burning until the
    /// case's `stop` file exists, and returns it once it has said `burning`.
    pub(crate) fn spawn(case: &Case, exe: &str, identity: &[&str], stop: &str) -> Burner {
        let stop = case.folder().join(stop);
        let words = burn_words(identity, &shown(&stop));
        let borrowed: Vec<&str> = words.iter().map(String::as_str).collect();
        let mut command = Command::new(exe);
        case.prepare_without_state_dir(&mut command, &borrowed);
        let running = start(command);
        running.wait_for_stdout("burning");
        Burner {
            pid: running.id(),
            stop,
            _running: running,
        }
    }

    /// Ends the burn by creating its stop file.
    pub(crate) fn stop(&self) {
        fs::write(&self.stop, "").expect("the burner's stop file can be written");
    }
}

impl Drop for Burner {
    fn drop(&mut self) {
        let _ = fs::write(&self.stop, "");
    }
}

/// Returns the gate under test, `running`, once the log at `path` holds `needle`; fails with the gate's status and
/// stderr as soon as the gate has ended without the log holding it, rather than after the hang guard.
#[expect(clippy::panic, reason = "a gate that ended before its log held the line fails the case with what it said")]
fn wait_for_log(mut running: Running, path: &Path, needle: &str) -> Running {
    let holds = || fs::read_to_string(path).is_ok_and(|text| text.contains(needle));
    wait_until(&format!("{needle:?} in {}, or the gate ending", path.display()), || {
        holds() || !running.is_running()
    });
    if holds() {
        return running;
    }
    let outcome = running.finish();
    panic!(
        "the gate ended before {needle:?} was in {}: status {:?}, stderr:\n{}",
        path.display(),
        outcome.code,
        outcome.stderr
    );
}

/// The file whose existence ends a case's `chatter` step: while it is missing, the command's log keeps growing, so the
/// watch's stall clock cannot run.
fn chatter_path(case: &Case) -> PathBuf {
    case.folder().join("chatter")
}

/// Ends the command's `chatter` step, letting the stall window start.
fn stop_chatter(case: &Case) {
    fs::write(chatter_path(case), "").expect("the chatter file can be written");
}

#[test]
#[ignore = "needs the machine alone: run with -Slot critical and --ignored"]
fn a_quiet_build_waiting_on_a_busy_shared_compiler_is_not_stalled() {
    let case = Case::new("a_quiet_build_waiting_on_a_busy_shared_compiler_is_not_stalled");
    let msbuild = tool(&case, "MSBuild.exe");
    let compiler = tool(&case, "VBCSCompiler.exe");
    let seams = seams();
    let seams: Vec<&str> = seams.iter().map(String::as_str).collect();
    // The server is created before the gate's command, so it counts as shared; it burns for the whole run.
    let server = Burner::spawn(&case, &compiler, &[], "stop-compiler");

    let outcome = case.run(&run_words("out.log", &seams, &[msbuild.as_str(), "out", "building", "sleep", "30000"]));
    server.stop();

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert!(
        !outcome.stderr.contains("STALLED"),
        "the burning shared compiler is progress for the quiet build:\n{outcome:?}"
    );
    assert_in_order(&outcome.stderr, &["gate: interrupted; stopping the command"]);
}

#[test]
fn a_quiet_command_is_stalled_once_the_shared_compiler_goes_quiet() {
    let case = Case::new("a_quiet_command_is_stalled_once_the_shared_compiler_goes_quiet");
    let msbuild = tool(&case, "MSBuild.exe");
    let compiler = tool(&case, "VBCSCompiler.exe");
    let seams = seams();
    let seams: Vec<&str> = seams.iter().map(String::as_str).collect();
    let log = case.folder().join("out.log");
    let server = Burner::spawn(&case, &compiler, &[], "stop-compiler");

    let running = case.start(&run_words("out.log", &seams, &[msbuild.as_str(), "out", "building", "sleep", "30000"]));
    let running = wait_for_log(running, &log, "building");
    server.stop();
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: STALLED: "), "{outcome:?}");
    assert_eq!(release(&case)["verdict"], "stalled", "{outcome:?}");
}

#[test]
#[ignore = "needs the machine alone: run with -Slot critical and --ignored"]
fn a_server_started_after_the_command_counts_while_the_job_holds_a_client() {
    let case = Case::new("a_server_started_after_the_command_counts_while_the_job_holds_a_client");
    let msbuild = tool(&case, "MSBuild.exe");
    let compiler = tool(&case, "VBCSCompiler.exe");
    let seams = seams();
    let seams: Vec<&str> = seams.iter().map(String::as_str).collect();
    let log = case.folder().join("out.log");
    let chatter = shown(&chatter_path(&case));

    // The command chatters until the case ends it, so the stall clock cannot run while the burner is started. The
    // burner is created only once the command is running, so it counts as new; from the moment the chatter stops it is
    // that server's CPU which keeps the quiet build alive.
    let running = case.start(&run_words(
        "out.log",
        &seams,
        &[msbuild.as_str(), "chatter", chatter.as_str(), "sleep", "30000"],
    ));
    let running = wait_for_log(running, &log, "chatter 1");
    let server = Burner::spawn(&case, &compiler, &[], "stop-compiler");
    stop_chatter(&case);
    let outcome = running.finish();
    server.stop();

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert!(
        !outcome.stderr.contains("STALLED"),
        "the new server's CPU is progress while the job holds a client:\n{outcome:?}"
    );
}

#[test]
#[ignore = "needs the machine alone: run with -Slot critical and --ignored"]
fn a_dotnet_server_is_told_by_its_command_line() {
    let case = Case::new("a_dotnet_server_is_told_by_its_command_line");
    let msbuild = tool(&case, "MSBuild.exe");
    let dotnet = tool(&case, "dotnet.exe");
    let seams = seams();
    let seams: Vec<&str> = seams.iter().map(String::as_str).collect();

    // The copy's command line names VBCSCompiler.dll, so the classifier reads a server out of it.
    let named = Burner::spawn(&case, &dotnet, &["VBCSCompiler.dll"], "stop-named");
    let outcome = case.run(&run_words("named.log", &seams, &[msbuild.as_str(), "out", "building", "sleep", "30000"]));
    named.stop();
    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "the dotnet server counts:\n{outcome:?}");
    assert!(!outcome.stderr.contains("STALLED"), "{outcome:?}");
}

#[test]
fn a_dotnet_host_without_a_server_on_its_command_line_is_no_server() {
    let case = Case::new("a_dotnet_host_without_a_server_on_its_command_line_is_no_server");
    let msbuild = tool(&case, "MSBuild.exe");
    let dotnet = tool(&case, "dotnet.exe");
    let seams = seams();
    let seams: Vec<&str> = seams.iter().map(String::as_str).collect();

    // A burning dotnet copy whose command line names no server is no server, so nothing keeps the quiet build alive.
    let plain = Burner::spawn(&case, &dotnet, &[], "stop-plain");
    let outcome = case.run(&run_words("plain.log", &seams, &[msbuild.as_str(), "out", "building", "sleep", "30000"]));
    plain.stop();

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: STALLED: "), "{outcome:?}");
}

#[test]
fn a_build_with_no_build_is_not_a_client() {
    let case = Case::new("a_build_with_no_build_is_not_a_client");
    let dotnet = tool(&case, "dotnet.exe");
    let compiler = tool(&case, "VBCSCompiler.exe");
    let seams = seams();
    let seams: Vec<&str> = seams.iter().map(String::as_str).collect();
    let server = Burner::spawn(&case, &compiler, &[], "stop-compiler");

    // The command's own line names --no-build, so its dotnet host is no client: the burning shared compiler is no
    // progress for it and the quiet command stalls.
    let outcome = case.run(&run_words("out.log", &seams, &[dotnet.as_str(), "out", "--no-build", "sleep", "30000"]));
    server.stop();

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: STALLED: "), "{outcome:?}");
}

#[test]
fn an_old_msbuild_node_never_counts() {
    let case = Case::new("an_old_msbuild_node_never_counts");
    let msbuild = tool(&case, "MSBuild.exe");
    let dotnet = tool(&case, "dotnet.exe");
    let seams = seams();
    let seams: Vec<&str> = seams.iter().map(String::as_str).collect();
    // An MSBuild node started before the command burns outside the job. It is a build server, but no compiler server,
    // so nothing counts it and the quiet build with a client of its own stalls.
    let node = Burner::spawn(&case, &dotnet, &["MSBuild.dll", "nodemode"], "stop-node");

    let outcome = case.run(&run_words("out.log", &seams, &[msbuild.as_str(), "out", "building", "sleep", "30000"]));
    node.stop();

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: STALLED: "), "{outcome:?}");
}

#[test]
fn a_stall_kill_lists_the_build_server_its_last_sample_counted() {
    let case = Case::new("a_stall_kill_lists_the_build_server_its_last_sample_counted");
    let msbuild = tool(&case, "MSBuild.exe");
    let compiler = tool(&case, "VBCSCompiler.exe");
    // A 60 s stall window and no interrupt: the block lists the server the last sample classified, whether or not its
    // CPU rose, so a window that long keeps the case's meaning while a loaded machine starves the command and the
    // server. The command sleeps past any case, so only the stall ends it. A 1 ms diagnostics budget cuts every block
    // the case gets, so the server line the block must hold cannot depend on the machine's load.
    let mut seams = seams_with_stall("60000");
    seams.extend(["--test-diagnostics-budget-ms", "1"].map(str::to_owned));
    let seams: Vec<&str> = seams.iter().map(String::as_str).collect();
    let log = case.folder().join("out.log");
    let chatter = shown(&chatter_path(&case));

    // The command chatters until the case ends it, so the stall clock cannot run while the server is started. The
    // server is created after the command, so it counts as new; it goes quiet with the chatter and stays alive, so the
    // last sample before the kill counts it and the block must list it.
    let running = case.start(&run_words(
        "out.log",
        &seams,
        &[msbuild.as_str(), "chatter", chatter.as_str(), "sleep", "590000"],
    ));
    let running = wait_for_log(running, &log, "chatter 1");
    let server = Burner::spawn(&case, &compiler, &[], "stop-compiler");
    server.stop();
    stop_chatter(&case);
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    let text = fs::read_to_string(&log).expect("the log was written");
    let line = format!("gate:   build server {} VBCSCompiler.exe: cpu ", server.pid);
    assert_in_order(
        &text,
        &[
            "gate: diagnostics\n",
            &line,
            " (started after the command)\n",
            "gate:   ... diagnostics cut at ",
        ],
    );
    assert!(text.contains("gate: STALLED: "), "{outcome:?}");
}
