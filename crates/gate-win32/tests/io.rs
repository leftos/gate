//! A job's I/O accounting: the bytes its members wrote are counted, those of a member that has exited included.

#![cfg(windows)]
#![expect(clippy::expect_used, reason = "a test that cannot start or wait for its command must fail loudly")]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gate_win32::{EnvironmentBlock, Job, Launch, LaunchOutput};

/// The longest the command of these tests may take: a guard against a hang, never a timing bound.
const HANG_GUARD: Duration = Duration::from_secs(600);
/// The bytes the job's member copies: 1 MiB.
const SIZE: usize = 1024 * 1024;

/// An empty folder of this test's own.
fn case_dir(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("gate-win32-io-{}-{test}", std::process::id()));
    if dir.exists() {
        fs::remove_dir_all(&dir).expect("an earlier run's folder is removed");
    }
    fs::create_dir_all(&dir).expect("the case folder is created");
    dir
}

/// The `cmd.exe` command `line`, run in `dir` with this process's environment and its output to `out.log` there.
fn launch(dir: &Path, line: &str) -> Launch {
    let system = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot is set"));
    Launch {
        application: system.join(r"System32\cmd.exe"),
        command_line: line.encode_utf16().collect(),
        directory: dir.to_path_buf(),
        environment: EnvironmentBlock::new(std::env::vars_os()).expect("the block is built"),
        output: LaunchOutput::Files {
            log: dir.join("out.log"),
            err_log: dir.join("out.log.err"),
        },
        priority: None,
    }
}

#[test]
fn the_written_bytes_of_a_job_count_what_its_member_wrote_to_a_file() {
    let dir = case_dir("written");
    fs::write(dir.join("source.bin"), vec![b'x'; SIZE]).expect("the source file is written");
    let job = Job::create(&format!(r"Local\gate-win32-test-{}-written", std::process::id())).expect("the job is created");
    assert_eq!(
        job.written_bytes().expect("an empty job's written bytes are read"),
        0,
        "an empty job has written nothing"
    );

    let running = launch(&dir, "cmd /d /c type source.bin > copy.bin")
        .start_suspended()
        .expect("the command starts")
        .assign_and_resume(&job)
        .expect("the command is assigned and resumed");
    let code = running.wait(HANG_GUARD).expect("the wait answers").expect("the command exits");
    assert_eq!(code, 0);

    let copied = fs::metadata(dir.join("copy.bin")).expect("the copy was made").len();
    assert_eq!(copied, SIZE as u64, "the member wrote the whole copy");
    let written = job.written_bytes().expect("the written bytes are read after the exit");
    assert!(written >= SIZE as u64, "the bytes of the member that exited still count: {written}");
}
