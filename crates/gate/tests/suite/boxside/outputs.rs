//! A routed run's outputs on the box: the manifest a run with output globs reads on stdin once its heartbeat beats, the
//! files it packs after a passing command, the old archives it removes, and the `outputs` verb that sends an archive
//! once and removes it.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

use super::{
    bare_box_command, box_command, field, gate_box, git, git_folder, has_ref, path_for_run, payload, renamed, run_box, run_box_with_path, snapshot,
};
use crate::support::{Case, FAKE_TOOL, Outcome, box_payload, shown, start_with_stdin};

/// The ignore file of an outputs case's snapshot: `bin/` is build output, so no back commit carries it.
const IGNORE_BIN: (&str, &str) = (".gitignore", "bin/\n");

/// The payload of a run of `command` in the work folder `k` asking for the outputs under `globs`.
fn outputs_payload(case: &Case, command: &[&str], globs: &[&str]) -> Value {
    let mut payload = box_payload(case, "k", command);
    payload["Outputs"] = json!(globs);
    payload
}

/// The lower-case hex SHA-256 of `bytes`.
fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// The manifest line the PC sends when it holds `files`, each a `/`-separated path and its text; the box reads only
/// the SHA-256 of each, so the size and last write are placeholders.
fn manifest(files: &[(&str, &str)]) -> String {
    let files: Map<String, Value> = files
        .iter()
        .map(|(path, text)| {
            (
                (*path).to_owned(),
                json!({"size": text.len(), "modified": 1, "sha256": sha256(text.as_bytes())}),
            )
        })
        .collect();
    format!("{}\n", json!({ "files": files }))
}

/// `gate box run` of `payload` with `line` written to its stdin, which stays open until the run has ended.
fn run_with_manifest(case: &Case, payload: &Value, line: &str) -> Outcome {
    finish_with_manifest(box_command(case, &["box", "run", &self::payload(payload)]), line)
}

/// `command` started with `line` written to its stdin, which stays open until it has ended.
fn finish_with_manifest(command: Command, line: &str) -> Outcome {
    let (running, mut stdin) = start_with_stdin(command);
    stdin.write_all(line.as_bytes()).expect("the manifest can be written");
    let outcome = running.finish();
    drop(stdin);
    outcome
}

/// The name of the outputs archive of the run of `payload`.
fn archive_name(payload: &Value) -> String {
    format!("k-{}.tar.zst", field(payload, "Nonce"))
}

/// Where the outputs archive of the run of `payload` is written.
fn archive_path(payload: &Value) -> PathBuf {
    Path::new(field(payload, "OutputFolder")).join(archive_name(payload))
}

/// The line a run of `payload` that packed `count` files says.
fn packed_line(payload: &Value, count: usize) -> String {
    format!("gate-remote-outputs: {count} {}", archive_name(payload))
}

/// Whether `text` holds the whole line `line`.
fn has_line(text: &str, line: &str) -> bool {
    text.lines().any(|said| said == line)
}

/// The files in the outputs archive at `path`, by member path, with their bytes; the archive's own list left out.
fn archive_files(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let file = File::open(path).expect("the archive can be opened");
    let decoder = zstd::stream::read::Decoder::new(file).expect("the archive is zstd");
    let mut archive = tar::Archive::new(decoder);
    let mut files = BTreeMap::new();
    for entry in archive.entries().expect("the archive is a tar stream") {
        let mut entry = entry.expect("a member can be read");
        let name = entry.path().expect("a member has a path").to_string_lossy().replace('\\', "/");
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).expect("a member's bytes can be read");
        if name != ".gate-outputs.json" {
            files.insert(name, bytes);
        }
    }
    files
}

/// `files`, each a path and its text, as [`archive_files`] gives them.
fn expected(files: &[(&str, &str)]) -> BTreeMap<String, Vec<u8>> {
    files.iter().map(|(path, text)| ((*path).to_owned(), text.as_bytes().to_vec())).collect()
}

/// The size each file member of the outputs archive at `path` has in its header, by member path, the archive's own list
/// left out; the members' bytes are passed over, never held.
fn archive_sizes(path: &Path) -> BTreeMap<String, u64> {
    let file = File::open(path).expect("the archive can be opened");
    let decoder = zstd::stream::read::Decoder::new(file).expect("the archive is zstd");
    let mut archive = tar::Archive::new(decoder);
    let mut sizes = BTreeMap::new();
    for entry in archive.entries().expect("the archive is a tar stream") {
        let entry = entry.expect("a member can be read");
        let name = entry.path().expect("a member has a path").to_string_lossy().replace('\\', "/");
        if name != ".gate-outputs.json" {
            sizes.insert(name, entry.header().size().expect("a member has a size"));
        }
    }
    sizes
}

/// Puts the process `pid` in a new job that lets each of its processes commit at most `bytes` of memory and lets a
/// process it starts break away, as the box's command must. The job lasts as long as a process is in it.
fn limit_memory(pid: u32, bytes: usize) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

    // SAFETY: no security attributes and no name are passed; a null handle is a failure.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    assert!(!job.is_null(), "the memory job is created: {}", std::io::Error::last_os_error());
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_PROCESS_MEMORY;
    limits.ProcessMemoryLimit = bytes;
    let size = u32::try_from(size_of_val(&limits)).expect("the limits fit a u32");
    // SAFETY: the job is open and `limits` is a whole extended limit structure of `size` bytes.
    let set = unsafe { SetInformationJobObject(job, JobObjectExtendedLimitInformation, (&raw const limits).cast(), size) };
    assert_ne!(set, 0, "the memory limit is set: {}", std::io::Error::last_os_error());
    // SAFETY: a plain open of a process by its id; a null handle is a failure.
    let process = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
    assert!(!process.is_null(), "the box can be opened: {}", std::io::Error::last_os_error());
    // SAFETY: both handles are open.
    let assigned = unsafe { AssignProcessToJobObject(job, process) };
    let error = std::io::Error::last_os_error();
    // SAFETY: both handles are open and closed once each; the job lives on while the box is in it.
    unsafe {
        CloseHandle(process);
        CloseHandle(job);
    }
    assert_ne!(assigned, 0, "the box enters the memory job: {error}");
}

/// The memory each process of the box may commit in [`a_large_output_is_packed_in_flat_memory`]: a quarter of the file
/// it packs.
const BOX_MEMORY: usize = 128 * 1024 * 1024;
/// The size of the file that case packs.
const LARGE_OUTPUT: u64 = 512 * 1024 * 1024;

#[test]
fn a_large_output_is_packed_in_flat_memory() {
    let case = Case::new("box_outputs_flat_memory");
    let size = LARGE_OUTPUT.to_string();
    let mut payload = outputs_payload(&case, &[FAKE_TOOL, "fill", r"bin\large.bin", &size], &["bin/**"]);
    snapshot(&case, &mut payload, &[IGNORE_BIN]);

    // The box waits for the manifest before it checks anything out, so the limit is on before it runs a step.
    let (running, mut stdin) = start_with_stdin(box_command(&case, &["box", "run", &self::payload(&payload)]));
    limit_memory(running.id(), BOX_MEMORY);
    stdin.write_all(manifest(&[]).as_bytes()).expect("the manifest can be written");
    let outcome = running.finish();
    drop(stdin);

    assert_eq!(outcome.code, Some(0), "the pack stays under the memory limit: {outcome:?}");
    assert!(has_line(&outcome.stderr, &packed_line(&payload, 1)), "{outcome:?}");
    assert_eq!(
        archive_sizes(&archive_path(&payload)),
        BTreeMap::from([("bin/large.bin".to_owned(), LARGE_OUTPUT)])
    );
}

#[test]
fn box_run_beats_before_it_reads_the_outputs_manifest() {
    let case = Case::new("box_outputs_beats_first");
    let mut payload = outputs_payload(&case, &[FAKE_TOOL, "err", "ran", "write", r"bin\a.txt", "a"], &["bin/**"]);
    snapshot(&case, &mut payload, &[IGNORE_BIN]);

    let (running, mut stdin) = start_with_stdin(bare_box_command(
        &case,
        &["box", "--test-heartbeat-ms", "30", "run", &self::payload(&payload)],
    ));
    let beats = |stderr: &str| stderr.lines().filter(|line| line.starts_with("gate-remote-heartbeat: ")).count();
    let before = running.wait_for_stderr_where("two heartbeats before the manifest is written", |stderr| beats(stderr) >= 2);
    assert!(!has_line(&before, "ran"), "the command waits for the manifest: {before}");
    stdin.write_all(manifest(&[]).as_bytes()).expect("the manifest can be written");
    let outcome = running.finish();
    drop(stdin);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(has_line(&outcome.stderr, "ran"), "the command ran once the manifest came: {outcome:?}");
    assert!(has_line(&outcome.stderr, &packed_line(&payload, 1)), "{outcome:?}");
    assert_eq!(archive_files(&archive_path(&payload)), expected(&[("bin/a.txt", "a")]));
}

#[test]
fn box_run_without_output_globs_never_reads_stdin() {
    let case = Case::new("box_outputs_no_globs");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "out", "ran"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let (running, stdin) = start_with_stdin(box_command(&case, &["box", "run", &self::payload(&payload)]));
    let outcome = running.finish();
    drop(stdin);

    assert_eq!(
        outcome.code,
        Some(0),
        "a run with no globs ends with its stdin open and unwritten: {outcome:?}"
    );
    assert_eq!(outcome.stdout, "ran\n");
    assert!(!outcome.stderr.contains("gate-remote-outputs"), "{outcome:?}");
    assert!(!Path::new(field(&payload, "OutputFolder")).exists(), "nothing is packed");
}

#[test]
fn box_run_reads_a_manifest_line_without_waiting_for_its_input_to_end() {
    let case = Case::new("box_outputs_line_no_eof");
    let mut payload = outputs_payload(&case, &[FAKE_TOOL, "write", r"bin\a.txt", "a"], &["bin/**"]);
    snapshot(&case, &mut payload, &[IGNORE_BIN]);

    let outcome = run_with_manifest(&case, &payload, &manifest(&[("bin/a.txt", "a")]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        has_line(&outcome.stderr, "gate-remote-outputs: 0"),
        "the PC already holds the file: {outcome:?}"
    );
    assert!(!archive_path(&payload).exists(), "no archive is written when nothing is packed");
}

#[test]
fn box_run_packs_the_files_the_pc_lacks_or_holds_otherwise() {
    let case = Case::new("box_outputs_selection");
    let first_command = [
        FAKE_TOOL,
        "write",
        r"bin\old.txt",
        "old",
        "write",
        r"bin\same.txt",
        "same",
        "write",
        r"bin\other.txt",
        "other",
    ];
    let mut first = outputs_payload(&case, &first_command, &["bin/**"]);
    snapshot(&case, &mut first, &[IGNORE_BIN]);
    let built = run_with_manifest(&case, &first, &manifest(&[]));
    assert!(has_line(&built.stderr, &packed_line(&first, 3)), "{built:?}");

    let mut second = renamed(&first, 1);
    second["Command"] = json!([FAKE_TOOL, "write", r"bin\new.txt", "new", "write", r"bin\same.txt", "same"]);
    let held = [("bin/same.txt", "same"), ("bin/other.txt", "pc-other")];
    let outcome = run_with_manifest(&case, &second, &manifest(&held));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(has_line(&outcome.stderr, &packed_line(&second, 2)), "{outcome:?}");
    assert_eq!(
        archive_files(&archive_path(&second)),
        expected(&[("bin/new.txt", "new"), ("bin/old.txt", "old")]),
        "a file the run made and one the PC lacks come back; one rewritten as the PC holds it and one the run left \
         alone stay"
    );

    let mut full = renamed(&first, 2);
    full["Command"] = json!([FAKE_TOOL, "out", "idle"]);
    full["OutputsFull"] = json!(true);
    let all_held = [held[0], held[1], ("bin/new.txt", "new"), ("bin/old.txt", "old")];
    let outcome = run_with_manifest(&case, &full, &manifest(&all_held));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        archive_files(&archive_path(&full)),
        expected(&[("bin/other.txt", "other")]),
        "with OutputsFull every file counts as changed, so the one the PC holds otherwise comes back"
    );
}

#[test]
fn box_run_never_packs_an_excluded_path_or_a_path_through_a_junction() {
    let case = Case::new("box_outputs_exclusions");
    let script = [
        r"@mkdir out\.git out\.tmp out\TestResults elsewhere",
        r"@echo x>out\.git\x.txt",
        r"@echo x>out\.tmp\x.txt",
        r"@echo x>out\TestResults\x.txt",
        r"@echo x>out\.env",
        r"@echo x>out\a.env",
        r"@echo x>out\.env.x",
        r"@echo x>out\keep.txt",
        r"@echo x>elsewhere\z.txt",
        r"@mklink /J out\link elsewhere >nul",
        "",
    ]
    .join("\r\n");
    let mut payload = outputs_payload(&case, &[r".\make.cmd"], &["out/**"]);
    snapshot(&case, &mut payload, &[(".gitignore", "out/\nelsewhere/\n"), ("make.cmd", &script)]);

    let outcome = run_with_manifest(&case, &payload, &manifest(&[]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        Path::new(field(&payload, "Work")).join(r"out\link\z.txt").exists(),
        "the junction was made"
    );
    assert_eq!(archive_files(&archive_path(&payload)), expected(&[("out/keep.txt", "x\r\n")]));
}

#[test]
fn box_run_of_a_failed_command_packs_nothing() {
    let case = Case::new("box_outputs_failed_command");
    let mut payload = outputs_payload(&case, &[FAKE_TOOL, "write", r"bin\a.txt", "a", "exit", "3"], &["bin/**"]);
    snapshot(&case, &mut payload, &[IGNORE_BIN]);

    let outcome = run_with_manifest(&case, &payload, &manifest(&[]));

    assert_eq!(outcome.code, Some(3), "the command's own status: {outcome:?}");
    assert!(!outcome.stderr.contains("gate-remote-outputs"), "{outcome:?}");
    assert!(!archive_path(&payload).exists(), "nothing is packed");
}

#[test]
fn box_run_leaves_a_file_of_the_back_commit_out_of_the_archive() {
    let case = Case::new("box_outputs_back_commit");
    let command = [FAKE_TOOL, "write", r"bin\a.txt", "a", "write", "new.txt", "n"];
    let mut payload = outputs_payload(&case, &command, &["**/*.txt"]);
    snapshot(&case, &mut payload, &[IGNORE_BIN]);

    let outcome = run_with_manifest(&case, &payload, &manifest(&[]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let bare = format!("--git-dir={}", field(&payload, "Bare"));
    assert_eq!(git(&[&bare, "show", "refs/gate-back/k:new.txt"]), "n", "the sync-back carries new.txt");
    assert_eq!(
        archive_files(&archive_path(&payload)),
        expected(&[("bin/a.txt", "a")]),
        "the archive leaves out what the back commit carries"
    );
}

#[test]
fn box_run_packs_a_file_with_a_non_ascii_name() {
    let case = Case::new("box_outputs_non_ascii");
    let mut payload = outputs_payload(&case, &[FAKE_TOOL, "write", "bin\\\u{e9}t\u{e9}.txt", "\u{e9}"], &["bin/**"]);
    snapshot(&case, &mut payload, &[IGNORE_BIN]);

    let outcome = run_with_manifest(&case, &payload, &manifest(&[]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(archive_files(&archive_path(&payload)), expected(&[("bin/\u{e9}t\u{e9}.txt", "\u{e9}")]));
}

#[test]
fn box_run_whose_archive_cannot_be_written_brings_nothing_back_and_fails() {
    let case = Case::new("box_outputs_unwritable");
    let mut payload = outputs_payload(&case, &[FAKE_TOOL, "write", r"bin\a.txt", "a"], &["bin/**"]);
    snapshot(&case, &mut payload, &[IGNORE_BIN]);
    fs::create_dir_all(archive_path(&payload)).expect("a folder can be put where the archive goes");

    let outcome = run_with_manifest(&case, &payload, &manifest(&[]));

    assert_eq!(
        outcome.code,
        Some(1),
        "a passing command's run fails when its outputs cannot be packed: {outcome:?}"
    );
    assert!(
        outcome
            .stderr
            .lines()
            .any(|line| line.starts_with("gate: remote outputs not brought back: ")),
        "{outcome:?}"
    );
    assert!(!outcome.stderr.contains("gate-remote-outputs"), "{outcome:?}");
}

/// Whether `outcome` said the line that a passing run's outputs are not brought back.
fn not_brought_back(outcome: &Outcome) -> bool {
    outcome
        .stderr
        .lines()
        .any(|line| line.starts_with("gate: remote outputs not brought back: "))
}

#[test]
fn a_malformed_manifest_on_a_passing_run_is_not_brought_back() {
    let case = Case::new("box_outputs_bad_manifest");
    let command = [FAKE_TOOL, "write", r"bin\a.txt", "a", "write", "new.txt", "n"];
    let mut payload = outputs_payload(&case, &command, &["bin/**"]);
    snapshot(&case, &mut payload, &[IGNORE_BIN]);

    let outcome = run_with_manifest(&case, &payload, "not a manifest\n");

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(not_brought_back(&outcome), "{outcome:?}");
    assert!(has_ref(&payload, "refs/gate-back/k"), "the back commit is pushed: {outcome:?}");
    assert!(!archive_path(&payload).exists(), "nothing is packed");
}

#[test]
fn box_run_leaves_a_file_already_at_the_archives_name_alone() {
    let case = Case::new("box_outputs_planted");
    let mut payload = outputs_payload(&case, &[FAKE_TOOL, "write", r"bin\a.txt", "a"], &["bin/**"]);
    snapshot(&case, &mut payload, &[IGNORE_BIN]);
    let archive = archive_path(&payload);
    fs::create_dir_all(archive.parent().expect("the archive has a folder")).expect("the outputs folder can be made");
    fs::write(&archive, "planted").expect("a file can be put where the archive goes");

    let outcome = run_with_manifest(&case, &payload, &manifest(&[]));

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    let refused = format!("gate: remote outputs not brought back: cannot create {}: ", archive.display());
    assert!(
        outcome.stderr.lines().any(|line| line.starts_with(&refused)),
        "{refused:?} in {outcome:?}"
    );
    assert_eq!(fs::read_to_string(&archive).expect("the planted file is there"), "planted");
}

#[test]
fn a_bad_nonce_or_output_folder_refuses_the_run() {
    let case = Case::new("box_outputs_bad_payload");
    let mut payload = outputs_payload(&case, &[FAKE_TOOL, "out", "ran"], &["bin/**"]);
    payload["Snapshot"] = json!("0".repeat(40));
    let mut bad_nonce = payload.clone();
    bad_nonce["Nonce"] = json!("not-hex");
    let mut bad_folder = payload.clone();
    bad_folder["OutputFolder"] = json!(case.folder().join("elsewhere").to_str().expect("the case folder is Unicode"));
    let refusals = [
        (bad_nonce, "gate: box payload Nonce is not 32 lower-case hex digits: not-hex\n".to_owned()),
        (
            bad_folder,
            format!("gate: box payload OutputFolder must be {}\n", field(&payload, "OutputFolder")),
        ),
    ];

    for (bad, line) in refusals {
        let outcome = run_box(&case, &bad);

        assert_eq!(outcome.code, Some(2), "{outcome:?}");
        assert_eq!(outcome.stdout, "", "{outcome:?}");
        assert_eq!(outcome.stderr, line);
    }
}

/// A folder holding a `git.cmd` that appends each call's words to the case's `git-calls.txt`, a line a call, and fails
/// `diff-tree` with status 9 without running it; every other call runs the real git. A case puts the folder on `PATH`
/// through [`path_for_run`]. The folder and the calls file.
fn git_failing_diff_tree(case: &Case) -> (PathBuf, PathBuf) {
    let stub = case.folder().join("stub-git");
    fs::create_dir_all(&stub).expect("the stub folder can be made");
    let calls = case.folder().join("git-calls.txt");
    let last = case.folder().join("git-last.txt");
    let (last_text, calls_text) = (shown(&last), shown(&calls));
    let script = [
        "@echo off".to_owned(),
        format!(">\"{last_text}\" echo %*"),
        format!(">>\"{calls_text}\" type \"{last_text}\""),
        format!("findstr /c:\" diff-tree \" \"{last_text}\" >nul && exit /b 9"),
        format!("\"{}\" %*", shown(&git_folder().join("git.exe"))),
        String::new(),
    ]
    .join("\r\n");
    fs::write(stub.join("git.cmd"), script).expect("the git stub can be written");
    (stub, calls)
}

#[test]
fn a_back_commit_whose_paths_cannot_be_read_is_pushed_and_its_outputs_not_brought_back() {
    let case = Case::new("box_outputs_diff_tree_fails");
    let command = [FAKE_TOOL, "write", r"bin\a.txt", "a", "write", "new.txt", "n"];
    let mut payload = outputs_payload(&case, &command, &["bin/**"]);
    snapshot(&case, &mut payload, &[IGNORE_BIN]);
    let (stub, _) = git_failing_diff_tree(&case);
    let mut run = box_command(&case, &["box", "run", &self::payload(&payload)]);
    run.env("PATH", path_for_run(&[stub]));

    let outcome = finish_with_manifest(run, &manifest(&[]));

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(
        has_line(&outcome.stderr, "gate: remote outputs not brought back: diff-tree failed (status 9)"),
        "{outcome:?}"
    );
    assert!(!outcome.stderr.contains("sync-back failed"), "{outcome:?}");
    let bare = format!("--git-dir={}", field(&payload, "Bare"));
    assert_eq!(git(&[&bare, "show", "refs/gate-back/k:new.txt"]), "n", "the back commit is pushed");
}

#[test]
fn box_run_without_output_globs_reads_no_back_commit_paths() {
    let case = Case::new("box_outputs_no_globs_no_diff_tree");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "write", "new.txt", "n"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    let (stub, calls) = git_failing_diff_tree(&case);

    let outcome = run_box_with_path(&case, &payload, Some(&path_for_run(&[stub])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let calls = fs::read_to_string(&calls).expect("the stub git ran");
    assert!(calls.lines().any(|call| call.contains(" push ")), "{calls}");
    assert!(!calls.contains(" diff-tree "), "{calls}");
    assert!(has_ref(&payload, "refs/gate-back/k"), "the back commit is pushed");
}

/// Makes an empty file at `path` last written `age` ago.
fn file_aged(path: &Path, age: Duration) {
    let file = File::create(path).expect("the file can be made");
    file.set_modified(SystemTime::now() - age).expect("the file's last write can be set");
}

/// Makes a file at `path` holding `bytes` zero bytes, last written `age` ago.
fn file_sized_aged(path: &Path, bytes: usize, age: Duration) {
    fs::write(path, vec![0_u8; bytes]).expect("the file can be written");
    let file = OpenOptions::new().write(true).open(path).expect("the file can be opened");
    file.set_modified(SystemTime::now() - age).expect("the file's last write can be set");
}

#[test]
fn box_run_removes_the_outputs_archives_older_than_a_day() {
    let case = Case::new("box_outputs_prune");
    let mut payload = outputs_payload(&case, &[FAKE_TOOL, "out", "ran"], &["bin/**"]);
    snapshot(&case, &mut payload, &[IGNORE_BIN]);
    let folder = PathBuf::from(field(&payload, "OutputFolder"));
    fs::create_dir_all(&folder).expect("the outputs folder can be made");
    let day = Duration::from_hours(24);
    let archive = |digit: char| folder.join(format!("k-{}.tar.zst", digit.to_string().repeat(32)));
    let old = archive('0');
    let fresh = archive('1');
    let other = folder.join("old.txt");
    let unnamed = folder.join("k-old.tar.zst");
    let held = archive('2');
    file_aged(&old, day + Duration::from_secs(3600));
    file_aged(&fresh, Duration::ZERO);
    file_aged(&other, day + Duration::from_secs(3600));
    file_aged(&unnamed, day + Duration::from_secs(3600));
    file_aged(&held, day + Duration::from_secs(3600));
    let holding = OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&held)
        .expect("the archive can be held open sharing nothing");

    let outcome = run_with_manifest(&case, &payload, &manifest(&[]));
    drop(holding);

    assert_eq!(outcome.code, Some(0), "an archive that cannot be removed fails nothing: {outcome:?}");
    assert!(!old.exists(), "an archive older than a day is removed");
    assert!(fresh.exists(), "a fresh archive is kept");
    assert!(other.exists(), "a file that is not an archive is kept");
    assert!(unnamed.exists(), "a .tar.zst file not named as an outputs archive is kept");
    assert!(held.exists(), "an archive held open stays");
    let refused = format!("gate: remote could not remove the old outputs archive {}: ", held.display());
    assert!(
        outcome.stderr.lines().any(|line| line.starts_with(&refused)),
        "{refused:?} in {outcome:?}"
    );
    assert!(has_line(&outcome.stderr, "gate-remote-outputs: 0"), "{outcome:?}");
}

#[test]
fn a_pack_past_the_budget_removes_the_oldest_archives() {
    let case = Case::new("box_outputs_budget");
    let mut payload = outputs_payload(&case, &[FAKE_TOOL, "write", r"bin\a.txt", "a"], &["bin/**"]);
    snapshot(&case, &mut payload, &[IGNORE_BIN]);
    let folder = PathBuf::from(field(&payload, "OutputFolder"));
    fs::create_dir_all(&folder).expect("the outputs folder can be made");
    let archive = |digit: char| folder.join(format!("k-{}.tar.zst", digit.to_string().repeat(32)));
    let oldest = archive('a');
    let newer = archive('b');
    // A megabyte and a byte against a megabyte budget: the oldest goes and the newer stays, since the run's own
    // archive is far under the megabyte left once the oldest is gone.
    file_sized_aged(&oldest, 1_000_000, Duration::from_secs(3600));
    file_sized_aged(&newer, 1, Duration::from_secs(1));

    let outcome = finish_with_manifest(
        box_command(&case, &["box", "--test-outputs-budget-bytes", "1000000", "run", &self::payload(&payload)]),
        &manifest(&[]),
    );

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(has_line(&outcome.stderr, &packed_line(&payload, 1)), "{outcome:?}");
    assert!(!oldest.exists(), "the oldest archive is removed to make room");
    assert!(newer.exists(), "the newer archive fits and stays");
    assert!(archive_path(&payload).exists(), "the run's own archive stays");
    assert!(
        !outcome.stderr.lines().any(|line| line.starts_with("gate: remote could not remove")),
        "{outcome:?}"
    );
}

#[test]
fn an_archive_larger_than_the_budget_is_not_brought_back() {
    let case = Case::new("box_outputs_over_budget");
    let mut payload = outputs_payload(&case, &[FAKE_TOOL, "write", r"bin\a.txt", "a"], &["bin/**"]);
    snapshot(&case, &mut payload, &[IGNORE_BIN]);
    let folder = PathBuf::from(field(&payload, "OutputFolder"));
    fs::create_dir_all(&folder).expect("the outputs folder can be made");
    let other = folder.join(format!("k-{}.tar.zst", "a".repeat(32)));
    file_sized_aged(&other, 1, Duration::from_secs(60));

    let outcome = finish_with_manifest(
        box_command(&case, &["box", "--test-outputs-budget-bytes", "1", "run", &self::payload(&payload)]),
        &manifest(&[]),
    );

    assert_eq!(
        outcome.code,
        Some(1),
        "a passing run whose outputs are not brought back exits 1: {outcome:?}"
    );
    assert!(has_line(&outcome.stderr, &packed_line(&payload, 1)), "{outcome:?}");
    assert!(
        has_line(
            &outcome.stderr,
            "gate: remote outputs not brought back: the archive is 0.0 GiB, more than the box keeps (50 GiB)"
        ),
        "{outcome:?}"
    );
    assert!(!archive_path(&payload).exists(), "the over-large archive is removed");
    assert!(other.exists(), "the other archive is left alone");
}

/// The payload of the `outputs` verb for the archive `name` of the work folder `k`.
fn verb_payload(case: &Case, name: &str) -> Value {
    let mut payload = box_payload(case, "k", &[]);
    payload["Name"] = json!(name);
    payload
}

#[test]
fn box_outputs_sends_the_archive_whole_then_removes_it_without_the_work_folders_lock() {
    let case = Case::new("box_outputs_verb");
    let mut payload = verb_payload(&case, "");
    let name = archive_name(&payload);
    payload["Name"] = json!(name);
    let archive = archive_path(&payload);
    fs::create_dir_all(archive.parent().expect("the archive has a folder")).expect("the outputs folder can be made");
    let bytes: Vec<u8> = [0x00, 0x0a, 0x0d, 0x1a, 0xff, 0x0d, 0x0a].iter().copied().cycle().take(70_001).collect();
    fs::write(&archive, &bytes).expect("the archive can be written");
    let lock = PathBuf::from(field(&payload, "Lock"));
    fs::create_dir_all(lock.parent().expect("the lock has a folder")).expect("the lock's folder can be made");
    let held = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(&lock)
        .expect("the work folder's lock can be held");

    let outcome = gate_box(&case, "outputs", &self::payload(&payload), None);
    drop(held);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr, "");
    assert!(
        outcome.raw_stdout == bytes,
        "the archive's bytes, exactly: {} bytes came",
        outcome.raw_stdout.len()
    );
    assert!(!archive.exists(), "a sent archive is removed");
}

#[test]
fn the_outputs_verb_exits_0_when_only_the_removal_fails() {
    let case = Case::new("box_outputs_verb_held");
    let mut payload = verb_payload(&case, "");
    let name = archive_name(&payload);
    payload["Name"] = json!(name);
    let archive = archive_path(&payload);
    fs::create_dir_all(archive.parent().expect("the archive has a folder")).expect("the outputs folder can be made");
    let bytes: Vec<u8> = (0..=255_u8).cycle().take(70_001).collect();
    fs::write(&archive, &bytes).expect("the archive can be written");
    let held = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(&archive)
        .expect("the archive can be held open sharing only reads");

    let outcome = gate_box(&case, "outputs", &self::payload(&payload), None);
    drop(held);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome.raw_stdout == bytes,
        "the archive's bytes, exactly: {} bytes came",
        outcome.raw_stdout.len()
    );
    let refused = format!("gate: remote could not remove the outputs archive {name}: ");
    assert_eq!(outcome.stderr.lines().count(), 1, "{outcome:?}");
    assert!(outcome.stderr.starts_with(&refused), "{refused:?} in {outcome:?}");
    assert!(archive.exists(), "an archive held open stays");
}

#[test]
fn box_outputs_without_a_name_is_a_usage_error() {
    let case = Case::new("box_outputs_verb_no_name");
    let payload = box_payload(&case, "k", &[]);

    let outcome = gate_box(&case, "outputs", &self::payload(&payload), None);

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(outcome.stdout, "");
    assert_eq!(outcome.stderr, "gate: box payload lacks Name\n");
}

#[test]
fn box_outputs_refuses_a_name_that_is_not_an_archive_of_its_work_folder() {
    let case = Case::new("box_outputs_verb_bad_name");
    let payload = verb_payload(&case, r"..\k.tar.zst");

    let outcome = gate_box(&case, "outputs", &self::payload(&payload), None);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "");
    assert_eq!(
        outcome.stderr,
        "gate: remote outputs archive name '..\\k.tar.zst' is not an outputs archive of this work folder\n"
    );
}

#[test]
fn box_outputs_says_an_archive_that_does_not_exist() {
    let case = Case::new("box_outputs_verb_missing");
    let mut payload = verb_payload(&case, "");
    let name = archive_name(&payload);
    payload["Name"] = json!(name);

    let outcome = gate_box(&case, "outputs", &self::payload(&payload), None);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "");
    assert_eq!(outcome.stderr, format!("gate: remote outputs archive {name} does not exist\n"));
}
