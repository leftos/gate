//! A routed run's outputs on the box: the manifest a run with output globs reads on stdin once its heartbeat beats, the
//! files it packs after a passing command, the old archives it removes, and the `outputs` verb that sends an archive
//! once and removes it.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use gate_win32::socket::Stream;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

use super::ReadBack;
use super::channel_pc::{
    BOX_PROOF, PC_PROOF, assert_no_proof, channel_line, free_port, has_line, hex_bytes, listening, not_completed, pc_connected, proved, with_windows,
};
use super::{
    BoxCall, bare_box_call, box_call, ended_within, field, gate_box, git, git_folder, has_ref, path_for_run, payload, renamed, run_box,
    run_box_with_path, snapshot,
};
use crate::support::{Case, FAKE_TOOL, HANG_GUARD, Outcome, Running, box_payload, shown};

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

/// `gate box run` of `payload` with its request line and then `line` written to its stdin, which stays open until the
/// run has ended.
fn run_with_manifest(case: &Case, payload: &Value, line: &str) -> Outcome {
    finish_with_manifest(box_call(case, &["box", "run"], &self::payload(payload)), line)
}

/// `call` started with its request line and then `line` written to its stdin, which stays open until it has ended.
fn finish_with_manifest(call: BoxCall, line: &str) -> Outcome {
    let (running, mut stdin) = call.start_with_stdin();
    stdin.write_all(line.as_bytes()).expect("the manifest can be written");
    let outcome = running.finish_read();
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

/// The line a run of `payload` that packed an archive says just before its outputs line: the archive's size.
fn size_line(payload: &Value) -> String {
    let size = fs::metadata(archive_path(payload)).expect("the archive the run packed is there").len();
    format!("gate-remote-outputs-size: {size}")
}

/// One item of an outputs archive's list: a path it brings back, its size and SHA-256, and whether it is a copy, whose
/// bytes the archive does not hold.
#[derive(Debug, PartialEq, Eq)]
struct Listed {
    path: String,
    size: u64,
    sha256: String,
    copy: bool,
}

/// The item a list holds for `path` of `text`, a copy or not.
fn listed(path: &str, text: &str, copy: bool) -> Listed {
    Listed {
        path: path.to_owned(),
        size: text.len() as u64,
        sha256: sha256(text.as_bytes()),
        copy,
    }
}

/// Reads the outputs archive at `path`, handing each member but the list to `member` with its path and its header's
/// size; the archive's list.
fn read_archive(path: &Path, mut member: impl FnMut(String, u64, &mut dyn Read)) -> Vec<Listed> {
    let file = File::open(path).expect("the archive can be opened");
    let decoder = zstd::stream::read::Decoder::new(file).expect("the archive is zstd");
    let mut archive = tar::Archive::new(decoder);
    let mut list = Vec::new();
    for entry in archive.entries().expect("the archive is a tar stream") {
        let mut entry = entry.expect("a member can be read");
        let name = entry.path().expect("a member has a path").to_string_lossy().replace('\\', "/");
        let size = entry.header().size().expect("a member has a size");
        if name == ".gate-outputs.json" {
            entry.read_to_end(&mut list).expect("the list can be read");
        } else {
            member(name, size, &mut entry);
        }
    }
    let items: Vec<Value> = serde_json::from_slice(&list).expect("the list is a JSON array");
    items
        .iter()
        .map(|item| Listed {
            path: item["path"].as_str().expect("an item has a path").to_owned(),
            size: item["size"].as_u64().expect("an item has a size"),
            sha256: item["sha256"].as_str().expect("an item has a SHA-256").to_owned(),
            copy: item.get("copy") == Some(&Value::Bool(true)),
        })
        .collect()
}

/// The paths of the members of the outputs archive at `path`, in order, the archive's own list left out.
fn archive_members(path: &Path) -> Vec<String> {
    let mut members = Vec::new();
    read_archive(path, |name, _, _| members.push(name));
    members
}

/// The files the outputs archive at `path` brings back from its own bytes, by path, with their bytes: each member, and
/// each copy of a member; the archive's own list left out, and so is a copy of a file the PC holds.
fn archive_files(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let listed = read_archive(path, |name, _, data| {
        let mut bytes = Vec::new();
        data.read_to_end(&mut bytes).expect("a member's bytes can be read");
        files.insert(name, bytes);
    });
    for item in listed.into_iter().filter(|item| item.copy) {
        let member = files.values().find(|bytes| sha256(bytes) == item.sha256).cloned();
        if let Some(bytes) = member {
            files.insert(item.path, bytes);
        }
    }
    files
}

/// `files`, each a path and its text, as [`archive_files`] gives them.
fn expected(files: &[(&str, &str)]) -> BTreeMap<String, Vec<u8>> {
    files.iter().map(|(path, text)| ((*path).to_owned(), text.as_bytes().to_vec())).collect()
}

/// The size of each path the outputs archive at `path` brings back, by path: a member's from its header, a copy's from
/// the list; the archive's own list left out, and the members' bytes passed over, never held.
fn archive_sizes(path: &Path) -> BTreeMap<String, u64> {
    let mut sizes = BTreeMap::new();
    let listed = read_archive(path, |name, size, _| {
        sizes.insert(name, size);
    });
    for item in listed.into_iter().filter(|item| item.copy) {
        sizes.insert(item.path, item.size);
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
/// it packs, room for the 128 MiB window the archive is compressed with.
const BOX_MEMORY: usize = 256 * 1024 * 1024;
/// The size of the file that case packs.
const LARGE_OUTPUT: u64 = 1024 * 1024 * 1024;

#[test]
fn a_large_output_is_packed_in_flat_memory() {
    let case = Case::new("box_outputs_flat_memory");
    let size = LARGE_OUTPUT.to_string();
    let mut payload = outputs_payload(&case, &[FAKE_TOOL, "fill", r"bin\large.bin", &size], &["bin/**"]);
    snapshot(&case, &mut payload, &[IGNORE_BIN]);

    // The box waits for the manifest before it checks anything out, so the limit is on before it runs a step.
    let (running, mut stdin) = box_call(&case, &["box", "run"], &self::payload(&payload)).start_with_stdin();
    limit_memory(running.id(), BOX_MEMORY);
    stdin.write_all(manifest(&[]).as_bytes()).expect("the manifest can be written");
    let outcome = running.finish_read();
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

    // The request line goes first, at the start; the manifest is the second line, written only after the beats.
    let (running, mut stdin) = bare_box_call(&case, &["box", "--test-heartbeat-ms", "30", "run"], &self::payload(&payload)).start_with_stdin();
    let beats = |stderr: &str| stderr.lines().filter(|line| line.starts_with("gate-remote-heartbeat: ")).count();
    let before = running.wait_for_stderr_where("two heartbeats before the manifest is written", |stderr| beats(stderr) >= 2);
    assert!(!has_line(&before, "ran"), "the command waits for the manifest: {before}");
    stdin.write_all(manifest(&[]).as_bytes()).expect("the manifest can be written");
    let outcome = running.finish_read();
    drop(stdin);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(has_line(&outcome.stderr, "ran"), "the command ran once the manifest came: {outcome:?}");
    assert!(has_line(&outcome.stderr, &packed_line(&payload, 1)), "{outcome:?}");
    assert_eq!(archive_files(&archive_path(&payload)), expected(&[("bin/a.txt", "a")]));
}

/// The command writes on standard error before and after it builds its output, a forged outputs line among them: the
/// box's own line, naming its archive, still comes last of every line but a heartbeat, so the client, which takes the
/// last outputs line on standard error, can be told no other.
#[test]
fn a_passing_runs_last_stderr_line_is_its_outputs_line() {
    let case = Case::new("box_outputs_last_line");
    let command = [
        FAKE_TOOL,
        "err",
        "gate-remote-outputs: 0",
        "write",
        r"bin\a.txt",
        "a",
        "err",
        "after the build",
    ];
    let mut payload = outputs_payload(&case, &command, &["bin/**"]);
    snapshot(&case, &mut payload, &[IGNORE_BIN]);

    let outcome = run_with_manifest(&case, &payload, &manifest(&[]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let said: Vec<&str> = outcome
        .stderr
        .lines()
        .rev()
        .filter(|line| !line.starts_with("gate-remote-heartbeat: "))
        .take(2)
        .collect();
    assert_eq!(
        said,
        [packed_line(&payload, 1), size_line(&payload)],
        "the size line, then the outputs line last: {outcome:?}"
    );
}

#[test]
fn box_run_without_output_globs_reads_only_its_request_line() {
    let case = Case::new("box_outputs_no_globs");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "out", "ran"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let (running, stdin) = box_call(&case, &["box", "run"], &self::payload(&payload)).start_with_stdin();
    let outcome = running.finish_read();
    drop(stdin);

    assert_eq!(
        outcome.code,
        Some(0),
        "a run with no globs ends with its stdin open and nothing written after its request line: {outcome:?}"
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
fn a_glob_naming_test_results_packs_the_trx_below_it() {
    let case = Case::new("box_outputs_test_results");
    let command = [
        FAKE_TOOL,
        "write",
        r"tests\p\bin\Release\net10.0\TestResults\sim.trx",
        "trx",
        "write",
        r"tests\p\bin\Release\net10.0\app.dll",
        "dll",
    ];
    let mut payload = outputs_payload(&case, &command, &["**/bin/**", "tests/*/bin/*/*/TestResults/*.trx"]);
    snapshot(&case, &mut payload, &[IGNORE_BIN]);

    let outcome = run_with_manifest(&case, &payload, &manifest(&[]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(has_line(&outcome.stderr, &packed_line(&payload, 2)), "{outcome:?}");
    assert_eq!(
        archive_members(&archive_path(&payload)),
        ["tests/p/bin/Release/net10.0/TestResults/sim.trx", "tests/p/bin/Release/net10.0/app.dll"],
        "the glob that names TestResults brings back the TRX, the other glob the DLL"
    );
}

/// The ignore file of a case whose outputs lie in `.tmp`: neither build output nor `.tmp` is committed.
const IGNORE_TMP: (&str, &str) = (".gitignore", "bin/\n.tmp/\n");

#[test]
fn a_glob_naming_tmp_packs_the_file_below_it() {
    let case = Case::new("box_outputs_named_tmp");
    let command = [
        FAKE_TOOL,
        "write",
        r".tmp\taxi-coverage-1.json",
        "coverage",
        "write",
        r".tmp\sub\taxi-coverage-2.json",
        "deeper",
        "write",
        r".tmp\other.json",
        "other",
        "write",
        r"bin\app.dll",
        "dll",
    ];
    let mut payload = outputs_payload(&case, &command, &["**/bin/**", ".tmp/taxi-*.json"]);
    snapshot(&case, &mut payload, &[IGNORE_TMP]);

    let outcome = run_with_manifest(&case, &payload, &manifest(&[]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(has_line(&outcome.stderr, &packed_line(&payload, 2)), "{outcome:?}");
    assert_eq!(
        archive_files(&archive_path(&payload)),
        expected(&[(".tmp/taxi-coverage-1.json", "coverage"), ("bin/app.dll", "dll")]),
        "the glob that names .tmp brings back its own file there, and nothing deeper or unmatched"
    );
}

#[test]
fn box_run_never_packs_the_gate_s_own_tmp_names() {
    let case = Case::new("box_outputs_gate_tmp");
    let nonce = "0123456789abcdef0123456789abcdef";
    let archive = format!(r".tmp\yaat-1a2b3c4d-{nonce}.tar.zst");
    let staged = format!(r".tmp\yaat-1a2b3c4d-{nonce}\bin\a.dll");
    let inputs = format!(r".tmp\yaat-1a2b3c4d-{nonce}.inputs-diff");
    let command = [
        FAKE_TOOL,
        "write",
        archive.as_str(),
        "archive",
        "write",
        staged.as_str(),
        "staged",
        "write",
        inputs.as_str(),
        "diff",
        "write",
        r".tmp\keep.txt",
        "keep",
    ];
    let mut payload = outputs_payload(&case, &command, &[".tmp/**"]);
    snapshot(&case, &mut payload, &[IGNORE_TMP]);

    let outcome = run_with_manifest(&case, &payload, &manifest(&[]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(has_line(&outcome.stderr, &packed_line(&payload, 1)), "{outcome:?}");
    assert_eq!(
        archive_files(&archive_path(&payload)),
        expected(&[(".tmp/keep.txt", "keep")]),
        "the gate's own archive, stage and diff names are never packed"
    );
}

#[test]
fn box_run_does_not_pack_a_tmp_file_an_earlier_run_left() {
    let case = Case::new("box_outputs_old_tmp");
    let mut first = outputs_payload(&case, &[FAKE_TOOL, "write", r".tmp\old.json", "old"], &[".tmp/*.json"]);
    snapshot(&case, &mut first, &[IGNORE_TMP]);
    let built = run_with_manifest(&case, &first, &manifest(&[]));
    assert!(has_line(&built.stderr, &packed_line(&first, 1)), "{built:?}");

    let mut second = renamed(&first, 1);
    second["Command"] = json!([FAKE_TOOL, "write", r".tmp\new.json", "new"]);
    let outcome = run_with_manifest(&case, &second, &manifest(&[]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(has_line(&outcome.stderr, &packed_line(&second, 1)), "{outcome:?}");
    assert_eq!(
        archive_files(&archive_path(&second)),
        expected(&[(".tmp/new.json", "new")]),
        "the file the run wrote comes back; the one an earlier run left stays, though the PC lacks it"
    );
}

#[test]
fn box_run_under_outputs_full_still_leaves_an_earlier_tmp_file_behind() {
    let case = Case::new("box_outputs_full_old_tmp");
    let first_command = [FAKE_TOOL, "write", r".tmp\old.json", "old", "write", r"bin\a.txt", "a"];
    let mut first = outputs_payload(&case, &first_command, &["bin/**", ".tmp/*.json"]);
    snapshot(&case, &mut first, &[IGNORE_TMP]);
    let built = run_with_manifest(&case, &first, &manifest(&[]));
    assert!(has_line(&built.stderr, &packed_line(&first, 2)), "{built:?}");

    let mut full = renamed(&first, 1);
    full["Command"] = json!([FAKE_TOOL, "write", r".tmp\new.json", "new"]);
    full["OutputsFull"] = json!(true);
    let outcome = run_with_manifest(&case, &full, &manifest(&[("bin/a.txt", "pc-a")]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        archive_files(&archive_path(&full)),
        expected(&[(".tmp/new.json", "new"), ("bin/a.txt", "a")]),
        "OutputsFull still sends the untouched file outside .tmp the PC holds otherwise, and the .tmp file the run \
         wrote, but not the one an earlier run left"
    );
}

#[test]
fn box_run_packs_identical_files_once_and_lists_every_path() {
    let case = Case::new("box_outputs_identical");
    let command = [
        FAKE_TOOL,
        "write",
        r"bin\a.txt",
        "same",
        "write",
        r"bin\b.txt",
        "same",
        "write",
        r"bin\c.txt",
        "other",
    ];
    let mut payload = outputs_payload(&case, &command, &["bin/**"]);
    snapshot(&case, &mut payload, &[IGNORE_BIN]);

    let outcome = run_with_manifest(&case, &payload, &manifest(&[]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(has_line(&outcome.stderr, &packed_line(&payload, 3)), "every path counts: {outcome:?}");
    let archive = archive_path(&payload);
    assert_eq!(archive_members(&archive), ["bin/a.txt", "bin/c.txt"], "the content is packed once");
    assert_eq!(
        read_archive(&archive, |_, _, _| {}),
        [
            listed("bin/a.txt", "same", false),
            listed("bin/b.txt", "same", true),
            listed("bin/c.txt", "other", false)
        ],
        "the list holds every path, in order"
    );
    assert_eq!(
        archive_files(&archive),
        expected(&[("bin/a.txt", "same"), ("bin/b.txt", "same"), ("bin/c.txt", "other")])
    );
}

#[test]
fn box_run_does_not_send_a_content_the_pc_holds_under_another_path() {
    let case = Case::new("box_outputs_pc_holds");
    let command = [FAKE_TOOL, "write", r"bin\new.txt", "kept", "write", r"bin\other.txt", "fresh"];
    let mut payload = outputs_payload(&case, &command, &["bin/**"]);
    snapshot(&case, &mut payload, &[IGNORE_BIN]);

    let outcome = run_with_manifest(&case, &payload, &manifest(&[("bin/old.txt", "kept")]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(has_line(&outcome.stderr, &packed_line(&payload, 2)), "{outcome:?}");
    let archive = archive_path(&payload);
    assert_eq!(archive_members(&archive), ["bin/other.txt"], "the PC's content is not sent");
    assert_eq!(
        read_archive(&archive, |_, _, _| {}),
        [listed("bin/new.txt", "kept", true), listed("bin/other.txt", "fresh", false)]
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
fn box_run_of_a_failed_command_with_no_test_results_glob_packs_nothing() {
    let case = Case::new("box_outputs_failed_command");
    let mut payload = outputs_payload(&case, &[FAKE_TOOL, "write", r"bin\a.txt", "a", "exit", "3"], &["bin/**"]);
    snapshot(&case, &mut payload, &[IGNORE_BIN]);

    let outcome = run_with_manifest(&case, &payload, &manifest(&[]));

    assert_eq!(outcome.code, Some(3), "the command's own status: {outcome:?}");
    assert!(!outcome.stderr.contains("gate-remote-outputs"), "{outcome:?}");
    assert!(!archive_path(&payload).exists(), "nothing is packed");
}

#[test]
fn box_run_of_a_failed_command_packs_its_test_reports_alone() {
    let case = Case::new("box_outputs_failed_reports");
    let command = [FAKE_TOOL, "write", r"bin\a.txt", "a", "write", r"TestResults\r.trx", "trx", "exit", "3"];
    let mut payload = outputs_payload(&case, &command, &["bin/**", "**/TestResults/**"]);
    snapshot(&case, &mut payload, &[(".gitignore", "bin/\nTestResults/\n")]);

    let outcome = run_with_manifest(&case, &payload, &manifest(&[]));

    assert_eq!(outcome.code, Some(3), "the command's own status: {outcome:?}");
    assert!(has_line(&outcome.stderr, &packed_line(&payload, 1)), "{outcome:?}");
    assert_eq!(
        archive_members(&archive_path(&payload)),
        ["TestResults/r.trx"],
        "only the glob that names TestResults brings anything back"
    );
}

#[test]
fn box_run_of_a_failed_command_whose_test_results_glob_matches_nothing_packs_nothing_and_says_so() {
    let case = Case::new("box_outputs_failed_no_reports");
    let command = [FAKE_TOOL, "write", r"bin\a.txt", "a", "exit", "3"];
    let mut payload = outputs_payload(&case, &command, &["bin/**", "**/TestResults/**"]);
    snapshot(&case, &mut payload, &[(".gitignore", "bin/\nTestResults/\n")]);

    let outcome = run_with_manifest(&case, &payload, &manifest(&[]));

    assert_eq!(outcome.code, Some(3), "the command's own status: {outcome:?}");
    let said: Vec<&str> = outcome.stderr.lines().filter(|line| line.starts_with("gate-remote-outputs")).collect();
    assert_eq!(said, ["gate-remote-outputs: 0"], "{outcome:?}");
    assert!(!archive_path(&payload).exists(), "nothing is packed");
}

#[test]
fn box_run_of_a_failed_command_whose_reports_are_not_brought_back_keeps_its_status() {
    let case = Case::new("box_outputs_failed_bad_manifest");
    let command = [FAKE_TOOL, "write", r"TestResults\r.trx", "trx", "exit", "3"];
    let mut payload = outputs_payload(&case, &command, &["**/TestResults/**"]);
    snapshot(&case, &mut payload, &[(".gitignore", "TestResults/\n")]);

    let outcome = run_with_manifest(&case, &payload, "not a manifest\n");

    assert_eq!(outcome.code, Some(3), "the command's own status, not 1: {outcome:?}");
    assert!(not_brought_back(&outcome), "{outcome:?}");
    assert!(!archive_path(&payload).exists(), "nothing is packed");
}

/// The ignore file of a failure outputs case's snapshot: neither build output, test reports nor dumps are committed.
const IGNORE_ALL: (&str, &str) = (".gitignore", "bin/\nTestResults/\ndumps/\n");

/// The payload of a run of `command` asking for the outputs under `globs` and the failure outputs under `failure`.
fn failure_payload(case: &Case, command: &[&str], globs: &[&str], failure: &[&str]) -> Value {
    let mut payload = outputs_payload(case, command, globs);
    payload["FailureOutputs"] = json!(failure);
    payload
}

#[test]
fn box_run_of_a_failed_command_with_a_failure_output_glob_packs_its_file() {
    let case = Case::new("box_outputs_failed_dump");
    let command = [FAKE_TOOL, "write", r"bin\a.txt", "a", "write", r"dumps\a.dmp", "dmp", "exit", "3"];
    let mut payload = failure_payload(&case, &command, &["bin/**"], &["dumps/*.dmp"]);
    snapshot(&case, &mut payload, &[IGNORE_ALL]);

    let outcome = run_with_manifest(&case, &payload, &manifest(&[]));

    assert_eq!(outcome.code, Some(3), "the command's own status: {outcome:?}");
    assert!(has_line(&outcome.stderr, &packed_line(&payload, 1)), "{outcome:?}");
    assert_eq!(
        archive_members(&archive_path(&payload)),
        ["dumps/a.dmp"],
        "the dump comes back, the build does not"
    );
}

#[test]
fn box_run_of_a_failed_command_packs_a_tmp_failure_output() {
    let case = Case::new("box_outputs_failed_tmp_dump");
    let command = [
        FAKE_TOOL,
        "write",
        r"bin\a.txt",
        "a",
        "write",
        r".tmp\dumps\r1\a.dmp",
        "dmp",
        "write",
        r".tmp\other.dmp",
        "other",
        "exit",
        "3",
    ];
    let mut payload = failure_payload(&case, &command, &["bin/**"], &[".tmp/dumps/**"]);
    snapshot(&case, &mut payload, &[(".gitignore", "bin/\n.tmp/\n")]);

    let outcome = run_with_manifest(&case, &payload, &manifest(&[]));

    assert_eq!(outcome.code, Some(3), "the command's own status: {outcome:?}");
    assert!(has_line(&outcome.stderr, &packed_line(&payload, 1)), "{outcome:?}");
    assert_eq!(
        archive_members(&archive_path(&payload)),
        [".tmp/dumps/r1/a.dmp"],
        "the dump the failure glob names in .tmp comes back, nothing else does"
    );
}

#[test]
fn box_run_of_a_failed_command_packs_only_the_tmp_dumps_it_wrote() {
    let case = Case::new("box_outputs_failed_old_tmp_dump");
    let first_command = [FAKE_TOOL, "write", r".tmp\dumps\old.dmp", "old", "exit", "3"];
    let mut first = failure_payload(&case, &first_command, &["bin/**"], &[".tmp/dumps/**"]);
    snapshot(&case, &mut first, &[(".gitignore", "bin/\n.tmp/\n")]);
    let failed = run_with_manifest(&case, &first, &manifest(&[]));
    assert!(has_line(&failed.stderr, &packed_line(&first, 1)), "{failed:?}");

    let mut second = renamed(&first, 1);
    second["Command"] = json!([FAKE_TOOL, "write", r".tmp\dumps\new.dmp", "new", "exit", "3"]);
    let outcome = run_with_manifest(&case, &second, &manifest(&[]));

    assert_eq!(outcome.code, Some(3), "the command's own status: {outcome:?}");
    assert!(has_line(&outcome.stderr, &packed_line(&second, 1)), "{outcome:?}");
    assert_eq!(
        archive_members(&archive_path(&second)),
        [".tmp/dumps/new.dmp"],
        "the dump this run wrote comes back; the one an earlier run left stays on the builder"
    );
}

#[test]
fn box_run_of_a_failed_command_with_both_kinds_of_glob_packs_both() {
    let case = Case::new("box_outputs_failed_both");
    let command = [
        FAKE_TOOL,
        "write",
        r"bin\a.txt",
        "a",
        "write",
        r"TestResults\r.trx",
        "trx",
        "write",
        r"dumps\a.dmp",
        "dmp",
        "exit",
        "3",
    ];
    let mut payload = failure_payload(&case, &command, &["bin/**", "**/TestResults/**"], &["dumps/*.dmp"]);
    snapshot(&case, &mut payload, &[IGNORE_ALL]);

    let outcome = run_with_manifest(&case, &payload, &manifest(&[]));

    assert_eq!(outcome.code, Some(3), "the command's own status: {outcome:?}");
    assert!(has_line(&outcome.stderr, &packed_line(&payload, 2)), "{outcome:?}");
    assert_eq!(archive_members(&archive_path(&payload)), ["TestResults/r.trx", "dumps/a.dmp"]);
}

#[test]
fn box_run_of_a_passing_command_does_not_pack_a_file_only_a_failure_output_glob_matches() {
    let case = Case::new("box_outputs_passed_dump");
    let command = [FAKE_TOOL, "write", r"dumps\a.dmp", "dmp"];
    let mut payload = failure_payload(&case, &command, &[], &["dumps/*.dmp"]);
    snapshot(&case, &mut payload, &[IGNORE_ALL]);

    let outcome = run_with_manifest(&case, &payload, &manifest(&[]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let said: Vec<&str> = outcome.stderr.lines().filter(|line| line.starts_with("gate-remote-outputs")).collect();
    assert_eq!(said, ["gate-remote-outputs: 0"], "{outcome:?}");
    assert!(!archive_path(&payload).exists(), "nothing is packed");
}

#[test]
fn box_run_of_a_failed_command_leaves_a_failure_output_over_the_cap_and_names_it() {
    let case = Case::new("box_outputs_failed_capped");
    let command = [
        FAKE_TOOL,
        "write",
        r"dumps\a.dmp",
        "ab",
        "write",
        r"dumps\big.dmp",
        "0123456789",
        "exit",
        "3",
    ];
    let mut payload = failure_payload(&case, &command, &[], &["dumps/*.dmp"]);
    snapshot(&case, &mut payload, &[IGNORE_ALL]);

    let outcome = finish_with_manifest(
        box_call(&case, &["box", "--test-box-failure-cap-bytes", "8", "run"], &self::payload(&payload)),
        &manifest(&[]),
    );

    assert_eq!(outcome.code, Some(3), "the command's own status: {outcome:?}");
    let left = "gate: remote failure output left on the builder: dumps/big.dmp (1 MiB, past the 8 bytes cap)";
    let lines: Vec<&str> = outcome.stderr.lines().collect();
    let said = lines.iter().position(|line| *line == left);
    let packed = lines.iter().position(|line| *line == packed_line(&payload, 1));
    assert!(
        said.is_some() && said < packed,
        "the file left is named before the packed line: {outcome:?}"
    );
    assert_eq!(
        archive_members(&archive_path(&payload)),
        ["dumps/a.dmp"],
        "the file under the cap comes back"
    );
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
fn a_back_commit_path_is_never_a_copy_source() {
    let case = Case::new("box_outputs_back_commit_source");
    let command = [FAKE_TOOL, "write", r"bin\a.txt", "n", "write", "new.txt", "n"];
    let mut payload = outputs_payload(&case, &command, &["**/*.txt"]);
    snapshot(&case, &mut payload, &[IGNORE_BIN]);

    let outcome = run_with_manifest(&case, &payload, &manifest(&[("new.txt", "n")]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let archive = archive_path(&payload);
    assert_eq!(
        archive_members(&archive),
        ["bin/a.txt"],
        "the sync-back replaces the PC's new.txt, so its content is sent"
    );
    assert_eq!(read_archive(&archive, |_, _, _| {}), [listed("bin/a.txt", "n", false)]);
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
fn box_run_never_packs_a_name_the_pcs_install_refuses_and_says_so() {
    let case = Case::new("box_outputs_unplain_name");
    let command = [FAKE_TOOL, "write", r"bin\m~10.txt", "x", "write", r"bin\ok.txt", "ok"];
    let mut payload = outputs_payload(&case, &command, &["bin/**"]);
    snapshot(&case, &mut payload, &[IGNORE_BIN]);

    let outcome = run_with_manifest(&case, &payload, &manifest(&[]));

    assert_eq!(outcome.code, Some(0), "the run keeps its status: {outcome:?}");
    assert_eq!(archive_files(&archive_path(&payload)), expected(&[("bin/ok.txt", "ok")]));
    let skipped = "gate: remote outputs: skipped 1 file(s) with names this PC cannot take: bin/m~10.txt";
    let lines: Vec<&str> = outcome.stderr.lines().collect();
    let said = lines.iter().position(|line| *line == skipped);
    let packed = lines.iter().position(|line| *line == packed_line(&payload, 1));
    assert!(
        said.is_some() && said < packed,
        "the skip is said once, before the packed line: {outcome:?}"
    );
    assert_eq!(lines.iter().filter(|line| line.contains("skipped")).count(), 1, "{outcome:?}");
}

#[test]
fn box_run_that_skips_several_names_names_the_first_and_marks_the_rest() {
    let case = Case::new("box_outputs_unplain_names");
    let command = [
        FAKE_TOOL,
        "write",
        r"bin\m~2.txt",
        "m",
        "write",
        r"bin\m~10.txt",
        "x",
        "write",
        r"bin\l~10.txt",
        "l",
        "write",
        r"bin\x~1\a.txt",
        "a",
        "write",
        r"bin\ok.txt",
        "ok",
    ];
    let mut payload = outputs_payload(&case, &command, &["bin/**"]);
    snapshot(&case, &mut payload, &[IGNORE_BIN]);

    let outcome = run_with_manifest(&case, &payload, &manifest(&[]));

    assert_eq!(outcome.code, Some(0), "the run keeps its status: {outcome:?}");
    assert_eq!(archive_files(&archive_path(&payload)), expected(&[("bin/ok.txt", "ok")]));
    let skipped = "gate: remote outputs: skipped 4 file(s) with names this PC cannot take: bin/l~10.txt, \u{2026}";
    assert!(has_line(&outcome.stderr, skipped), "{outcome:?}");
}

#[test]
fn box_run_whose_only_output_is_skipped_says_so_and_packs_nothing() {
    let case = Case::new("box_outputs_only_unplain");
    let mut payload = outputs_payload(&case, &[FAKE_TOOL, "write", r"bin\m~10.txt", "x"], &["bin/**"]);
    snapshot(&case, &mut payload, &[IGNORE_BIN]);

    let outcome = run_with_manifest(&case, &payload, &manifest(&[]));

    assert_eq!(outcome.code, Some(0), "the run keeps its status: {outcome:?}");
    let skipped = "gate: remote outputs: skipped 1 file(s) with names this PC cannot take: bin/m~10.txt";
    let lines: Vec<&str> = outcome.stderr.lines().collect();
    let said = lines.iter().position(|line| *line == skipped);
    let nothing = lines.iter().position(|line| *line == "gate-remote-outputs: 0");
    assert!(said.is_some() && said < nothing, "the skip, then nothing packed: {outcome:?}");
    assert!(!archive_path(&payload).exists(), "no archive is written");
}

#[test]
fn box_run_skips_an_output_named_as_the_archive_s_own_list() {
    let case = Case::new("box_outputs_list_name");
    let command = [FAKE_TOOL, "write", ".gate-outputs.json", "{}", "write", r"bin\ok.txt", "ok"];
    let mut payload = outputs_payload(&case, &command, &["bin/**", "*.json"]);
    snapshot(&case, &mut payload, &[(".gitignore", "bin/\n/.gate-outputs.json\n")]);

    let outcome = run_with_manifest(&case, &payload, &manifest(&[]));

    assert_eq!(outcome.code, Some(0), "the run keeps its status: {outcome:?}");
    assert_eq!(archive_files(&archive_path(&payload)), expected(&[("bin/ok.txt", "ok")]));
    let skipped = "gate: remote outputs: skipped 1 file(s) with names this PC cannot take: .gate-outputs.json";
    assert!(has_line(&outcome.stderr, skipped), "{outcome:?}");
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
    let mut run = box_call(&case, &["box", "run"], &self::payload(&payload));
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
        box_call(&case, &["box", "--test-outputs-budget-bytes", "1000000", "run"], &self::payload(&payload)),
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
        box_call(&case, &["box", "--test-outputs-budget-bytes", "1", "run"], &self::payload(&payload)),
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

/// The `outputs` payload of the work folder `k`'s archive, asking for a data channel on loopback from loopback with the
/// case's proofs, and the archive's path, written with `bytes` unless `bytes` is `None`.
fn channel_payload(case: &Case, bytes: Option<&[u8]>) -> (Value, PathBuf) {
    let mut payload = verb_payload(case, "");
    payload["Name"] = json!(archive_name(&payload));
    payload["Channel"] = json!({ "Address": "127.0.0.1", "Peer": "127.0.0.1", "BoxProof": BOX_PROOF, "PcProof": PC_PROOF });
    let archive = archive_path(&payload);
    if let Some(bytes) = bytes {
        fs::create_dir_all(archive.parent().expect("the archive has a folder")).expect("the outputs folder can be made");
        fs::write(&archive, bytes).expect("the archive can be written");
    }
    (payload, archive)
}

/// `gate box outputs` of `payload` started with its data channel's ports narrowed to `port` alone and the seams `seams`
/// before the verb, its request written and its stdin closed.
fn channel_box(case: &Case, payload: &Value, port: u16, seams: &[&str]) -> Running {
    channel_call(case, payload, port, seams).start()
}

/// The `gate box outputs` call of `payload` with its data channel's ports narrowed to `port` alone and the seams
/// `seams` before the verb, not yet started.
fn channel_call(case: &Case, payload: &Value, port: u16, seams: &[&str]) -> BoxCall {
    let ports = format!("{port}-{port}");
    let mut args = vec!["box", "--test-box-channel-ports", &ports];
    args.extend(with_windows(seams));
    args.push("outputs");
    box_call(case, &args, &self::payload(payload))
}

/// Reads `size` bytes of archive on `stream`.
fn pulled(stream: &mut TcpStream, size: usize) -> Vec<u8> {
    let mut bytes = vec![0_u8; size];
    stream.read_exact(&mut bytes).expect("the archive's bytes come");
    bytes
}

/// A whole pull over the box's data channel on `port`: proofs, the archive's `size` bytes and the ack of their SHA-256;
/// then the PC closes first. The bytes pulled.
fn pc_pull(port: u16, size: usize) -> Vec<u8> {
    let mut stream = pc_connected(port);
    proved(&mut stream, PC_PROOF);
    let bytes = pulled(&mut stream, size);
    stream.write_all(&Sha256::digest(&bytes)).expect("the ack is sent");
    bytes
}

/// The bytes of a case's archive: every byte value, cycled past a socket buffer's worth.
fn channel_archive() -> Vec<u8> {
    (0..=255_u8).cycle().take(300_001).collect()
}

#[test]
fn box_outputs_sends_the_archive_over_the_data_channel_and_removes_it_once_acked() {
    let case = Case::new("box_outputs_channel_whole");
    let bytes = channel_archive();
    let (payload, archive) = channel_payload(&case, Some(&bytes));
    let port = free_port();
    let running = channel_box(&case, &payload, port, &[]);
    listening(&running, &payload, port);

    let pulled = pc_pull(port, bytes.len());
    let outcome = running.finish_read();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(pulled == bytes, "the archive's bytes, exactly");
    assert_eq!(
        outcome.stderr,
        format!(
            "{}\ngate: data channel sent {} bytes sha256 {}\n",
            channel_line(&payload, port),
            bytes.len(),
            sha256(&bytes)
        )
    );
    assert!(outcome.raw_stdout.is_empty(), "nothing goes on stdout: {outcome:?}");
    assert!(!archive.exists(), "an acked archive is removed");
    assert_no_proof(&outcome);
}

#[test]
fn box_outputs_keeps_the_archive_when_the_pcs_proof_is_wrong() {
    let case = Case::new("box_outputs_channel_wrong_proof");
    let (payload, archive) = channel_payload(&case, Some(&channel_archive()));
    let port = free_port();
    let running = channel_box(&case, &payload, port, &[]);
    listening(&running, &payload, port);

    let mut stream = pc_connected(port);
    proved(&mut stream, BOX_PROOF);
    let mut rest = Vec::new();
    let read = stream.read_to_end(&mut rest);
    let outcome = running.finish_read();

    assert!(
        read.is_err() || rest.is_empty(),
        "no archive byte follows a wrong proof: {} came",
        rest.len()
    );
    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        format!(
            "{}\n{}",
            channel_line(&payload, port),
            not_completed(&payload, "the PC's proof does not match")
        )
    );
    assert!(archive.exists(), "the archive is kept");
    assert_no_proof(&outcome);
}

#[test]
fn box_outputs_keeps_the_archive_when_the_pc_closes_before_its_ack() {
    let case = Case::new("box_outputs_channel_no_ack");
    let bytes = channel_archive();
    let (payload, archive) = channel_payload(&case, Some(&bytes));
    let port = free_port();
    let running = channel_box(&case, &payload, port, &[]);
    listening(&running, &payload, port);

    let mut stream = pc_connected(port);
    proved(&mut stream, PC_PROOF);
    let pulled = pulled(&mut stream, bytes.len());
    drop(stream);
    let outcome = running.finish_read();

    assert!(pulled == bytes, "the archive's bytes, exactly");
    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        format!(
            "{}\n{}",
            channel_line(&payload, port),
            not_completed(&payload, "the PC closed the channel before its acknowledgement")
        )
    );
    assert!(archive.exists(), "the archive is kept");
    assert_no_proof(&outcome);
}

#[test]
fn box_outputs_with_a_channel_says_a_missing_archive_and_opens_no_channel() {
    let case = Case::new("box_outputs_channel_missing");
    let (payload, _) = channel_payload(&case, None);
    let name = archive_name(&payload);

    let outcome = channel_box(&case, &payload, free_port(), &[]).finish_read();

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "");
    assert_eq!(outcome.stderr, format!("gate: remote outputs archive {name} does not exist\n"));
}

#[test]
fn box_outputs_with_a_channel_refuses_another_keys_archive_before_any_channel_line() {
    let case = Case::new("box_outputs_channel_other_key");
    let (mut payload, _) = channel_payload(&case, Some(b"archive"));
    let other = format!("other-{}.tar.zst", field(&payload, "Nonce"));
    payload["Name"] = json!(other);

    let outcome = channel_box(&case, &payload, free_port(), &[]).finish_read();

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "");
    assert_eq!(
        outcome.stderr,
        format!("gate: remote outputs archive name '{other}' is not an outputs archive of this work folder\n")
    );
}

#[test]
fn box_outputs_of_a_drained_box_sends_over_the_channel_and_keeps_the_archive() {
    const T: u64 = 1_800_000_000;
    let case = Case::new("box_outputs_channel_drained");
    let bytes = channel_archive();
    let (mut payload, archive) = channel_payload(&case, Some(&bytes));
    payload["Session"] = json!("s9");
    let root = shown(&case.folder().join("remote"));
    let raise = json!({ "Root": root, "Action": "raise" });
    let now = T.to_string();
    let raised = box_call(
        &case,
        &["box", "--test-box-now", &now, "--test-box-drain-readable-only", "true", "drain"],
        &self::payload(&raise),
    )
    .finish();
    assert_eq!(raised.code, Some(0), "{raised:?}");
    let port = free_port();
    let later = (T + 1).to_string();
    let running = channel_box(
        &case,
        &payload,
        port,
        &["--test-box-now", &later, "--test-box-drain-readable-only", "true"],
    );
    listening(&running, &payload, port);

    let pulled = pc_pull(port, bytes.len());
    let outcome = running.finish_read();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(pulled == bytes, "the archive's bytes, exactly");
    assert_eq!(
        outcome.stderr,
        format!(
            "{}\ngate: data channel sent {} bytes sha256 {}\ngate: remote outputs kept: the builder is drained\n",
            channel_line(&payload, port),
            bytes.len(),
            sha256(&bytes)
        )
    );
    assert!(archive.exists(), "a drained box keeps the archive");
    assert_no_proof(&outcome);
}

#[test]
fn box_outputs_closes_a_connection_from_another_address_unread_and_serves_the_pc() {
    let case = Case::new("box_outputs_channel_other_peer");
    let bytes = channel_archive();
    let (payload, archive) = channel_payload(&case, Some(&bytes));
    let port = free_port();
    let running = channel_box(&case, &payload, port, &[]);
    listening(&running, &payload, port);

    let impostor = Stream::connect(
        Ipv4Addr::new(127, 0, 0, 2),
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
        Instant::now() + HANG_GUARD,
    )
    .expect("the connect does not fail")
    .expect("the other address connects");
    let mut buffer = [0_u8; 64];
    let read = impostor.read(&mut buffer, Instant::now() + HANG_GUARD);
    assert!(
        matches!(read, Ok(Some(0)) | Err(_)),
        "the box closes the other address's connection with no byte: {read:?}"
    );
    drop(impostor);
    let pulled = pc_pull(port, bytes.len());
    let outcome = running.finish_read();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(pulled == bytes, "the archive's bytes, exactly");
    assert!(
        has_line(
            &outcome.stderr,
            &format!("gate: data channel sent {} bytes sha256 {}", bytes.len(), sha256(&bytes))
        ),
        "{outcome:?}"
    );
    assert!(!archive.exists(), "an acked archive is removed");
    assert_no_proof(&outcome);
}

/// The proof's wait has one end, which no byte moves: a PC that sends part of its proof and then nothing is refused
/// once that end passes, though the idle limit has not.
#[test]
fn box_outputs_refuses_a_proof_not_whole_by_its_fixed_end() {
    let case = Case::new("box_outputs_channel_slow_proof");
    let (payload, archive) = channel_payload(&case, Some(&channel_archive()));
    let port = free_port();
    let running = channel_box(&case, &payload, port, &["--test-box-channel-proof-ms", "300"]);
    listening(&running, &payload, port);

    let mut stream = pc_connected(port);
    let mut box_proof = [0_u8; 16];
    stream.read_exact(&mut box_proof).expect("the box's proof comes");
    // One byte of the sixteen, then nothing: the idle limit is the built-in 10 s, so only the proof's end can end it.
    stream.write_all(&hex_bytes(PC_PROOF)[..1]).expect("a byte of the proof is sent");
    let outcome = running.finish_read();
    drop(stream);

    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        format!(
            "{}\n{}",
            channel_line(&payload, port),
            not_completed(&payload, "the PC sent too little within 300 ms before its proof")
        )
    );
    assert!(archive.exists(), "the archive is kept");
    assert_no_proof(&outcome);
}

#[test]
fn box_outputs_refuses_the_channel_when_no_pc_connects_within_the_accept_window() {
    let case = Case::new("box_outputs_channel_no_pc");
    let (payload, archive) = channel_payload(&case, Some(&channel_archive()));
    let port = free_port();

    let outcome = channel_box(&case, &payload, port, &["--test-box-channel-accept-ms", "200"]).finish_read();

    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        format!(
            "{}\n{}",
            channel_line(&payload, port),
            not_completed(&payload, "no connection from the PC within 200 ms")
        )
    );
    assert!(archive.exists(), "the archive is kept");
}

#[test]
fn box_outputs_refuses_the_channel_when_the_pc_goes_silent_after_its_proof() {
    let case = Case::new("box_outputs_channel_silent_pc");
    let (payload, archive) = channel_payload(&case, Some(&channel_archive()));
    let port = free_port();
    let running = channel_box(&case, &payload, port, &["--test-box-channel-idle-ms", "300"]);
    listening(&running, &payload, port);

    let mut stream = pc_connected(port);
    proved(&mut stream, PC_PROOF);
    let outcome = running.finish_read();
    drop(stream);

    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        format!(
            "{}\n{}",
            channel_line(&payload, port),
            not_completed(&payload, "the PC sent nothing for 300 ms before its acknowledgement")
        )
    );
    assert!(archive.exists(), "the archive is kept");
    assert_no_proof(&outcome);
}

/// A PC that proves itself and then reads nothing leaves the box's writes no room: with the box's send buffer narrowed
/// and an archive larger than the buffers hold, a write of the archive waits out the idle limit, and the box says so
/// and keeps the archive.
#[test]
fn box_outputs_refuses_the_channel_when_the_pc_takes_nothing_of_the_archive() {
    let case = Case::new("box_outputs_channel_pc_takes_nothing");
    let bytes: Vec<u8> = (0..=255_u8).cycle().take(4 * 1024 * 1024 + 1).collect();
    let (payload, archive) = channel_payload(&case, Some(&bytes));
    let port = free_port();
    let seams = ["--test-box-channel-idle-ms", "300", "--test-box-channel-send-buffer-bytes", "4096"];
    let running = channel_box(&case, &payload, port, &seams);
    listening(&running, &payload, port);

    let mut stream = pc_connected(port);
    proved(&mut stream, PC_PROOF);
    let outcome = running.finish_read();
    drop(stream);

    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        format!(
            "{}\n{}",
            channel_line(&payload, port),
            not_completed(&payload, "the PC took nothing for 300 ms")
        )
    );
    assert!(archive.exists(), "the archive is kept");
    assert_no_proof(&outcome);
}

/// The box learns its client went only when a write to stderr fails: the channel's accept wait probes stderr, so a
/// box whose control call went while it waited for the PC ends without waiting out its window.
#[test]
fn box_outputs_ends_when_its_client_goes_during_the_accept_wait() {
    let case = Case::new("box_outputs_channel_client_gone");
    let (payload, archive) = channel_payload(&case, Some(&channel_archive()));
    let port = free_port();
    let call = channel_call(&case, &payload, port, &["--test-box-channel-accept-ms", "600000"]);
    let (running, stderr) = call.start_with_closable_stderr();
    listening(&running, &payload, port);

    stderr.close();
    let outcome = ended_within(running, HANG_GUARD);

    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    assert!(archive.exists(), "the archive is kept");
}

#[test]
fn box_outputs_refuses_the_channel_when_no_port_is_free_and_keeps_the_archive() {
    let case = Case::new("box_outputs_channel_no_port");
    let (payload, archive) = channel_payload(&case, Some(b"archive"));
    let held = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a loopback listener binds");
    let port = held.local_addr().expect("the listener's address is read").port();

    let outcome = channel_box(&case, &payload, port, &[]).finish_read();
    drop(held);

    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    assert_eq!(outcome.stderr, not_completed(&payload, "no data channel port is free"));
    assert!(outcome.raw_stdout.is_empty(), "nothing goes on stdout: {outcome:?}");
    assert!(archive.exists(), "the archive is kept");
    assert_no_proof(&outcome);
}
