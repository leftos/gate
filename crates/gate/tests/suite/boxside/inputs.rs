//! A routed run's ignored inputs on the box: the `inputs` verb's diff of the PC's manifest against the work folder, its
//! put of an archive into the work folder, the record and the exclude file it keeps, and the run's check of the digest
//! its request carries.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Write;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL};

use super::ReadBack;
use super::{box_call, field, gate_box, git, lines, passed, payload, refused_line, run_box, snapshot};
use crate::support::{Case, FAKE_TOOL, Outcome, box_payload, junction};

/// The lower-case hex SHA-256 of `bytes`.
fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// The payload of an inputs call of `step` for the work folder `k`, which a run of it would run `fake-tool out ran`.
/// The work folder's parent and bare repository are readied by a preflight first, as every route's first call does.
fn inputs_payload(case: &Case, step: &str) -> Value {
    let payload = unreadied_payload(case, step);
    passed(&gate_box(case, "preflight", &self::payload(&payload), None));
    payload
}

/// [`inputs_payload`] with no preflight run.
fn unreadied_payload(case: &Case, step: &str) -> Value {
    let mut payload = box_payload(case, "k", &[FAKE_TOOL, "out", "ran"]);
    payload["Step"] = json!(step);
    payload
}

/// `payload` asking for `step`.
fn stepped(payload: &Value, step: &str) -> Value {
    let mut stepped = payload.clone();
    stepped["Step"] = json!(step);
    stepped
}

/// The manifest line the PC sends for `files`, each a `/`-separated path and its bytes, without its newline.
fn manifest_line(files: &[(&str, &[u8])]) -> String {
    manifest_line_at(files, 1)
}

/// [`manifest_line`] with each file last written at the PC file time `modified`.
fn manifest_line_at(files: &[(&str, &[u8])], modified: u64) -> String {
    let files: Map<String, Value> = files
        .iter()
        .map(|(path, bytes)| {
            let file = json!({"size": bytes.len(), "modified": modified, "sha256": sha256(bytes)});
            ((*path).to_owned(), file)
        })
        .collect();
    json!({ "files": files }).to_string()
}

/// Appends the regular file `path` holding `bytes` to `builder`, its name written into the header as it is, so a
/// case can send a name the tar crate would refuse to write.
fn append(builder: &mut tar::Builder<Vec<u8>>, path: &str, bytes: &[u8], mtime: u64) {
    let mut header = tar::Header::new_gnu();
    header.as_gnu_mut().expect("a GNU header").name[..path.len()].copy_from_slice(path.as_bytes());
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(mtime);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    builder.append(&header, bytes).expect("a member can be appended");
}

/// The archive the PC sends for `members`, each a path and its bytes, in the outputs archive's format: each member,
/// then the list of every path with its size and SHA-256, the whole compressed with zstd.
fn archive(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mtime = SystemTime::now().duration_since(UNIX_EPOCH).expect("the clock is past 1970").as_secs();
    let mut builder = tar::Builder::new(Vec::new());
    let mut list = Vec::new();
    for (path, bytes) in members {
        append(&mut builder, path, bytes, mtime);
        list.push(json!({"path": path, "size": bytes.len(), "sha256": sha256(bytes)}));
    }
    append(&mut builder, ".gate-outputs.json", Value::Array(list).to_string().as_bytes(), mtime);
    let tar = builder.into_inner().expect("the tar stream ends");
    zstd::encode_all(tar.as_slice(), 3).expect("the tar stream compresses")
}

/// `gate box <seams> inputs` of `payload`, its request line, then `manifest` and its newline, then `archive` written to
/// its stdin, which is then closed; to its end.
fn inputs_call(case: &Case, payload: &Value, seams: &[&str], manifest: &str, archive: &[u8]) -> Outcome {
    let mut args = vec!["box"];
    args.extend_from_slice(seams);
    args.push("inputs");
    let (running, mut stdin) = box_call(case, &args, &self::payload(payload)).start_with_stdin();
    // A box that refuses before it reads the archive may close its end first; what it said is the case's to assert.
    drop(stdin.write_all(format!("{manifest}\n").as_bytes()));
    drop(stdin.write_all(archive));
    drop(stdin);
    running.finish_read()
}

/// A diff of `files` in the work folder of `payload`.
fn diff(case: &Case, payload: &Value, files: &[(&str, &[u8])]) -> Outcome {
    inputs_call(case, &stepped(payload, "diff"), &[], &manifest_line(files), &[])
}

/// A put of `members` against the manifest of `files` in the work folder of `payload`, with `seams` before the verb.
fn put_with(case: &Case, payload: &Value, seams: &[&str], files: &[(&str, &[u8])], members: &[(&str, &[u8])]) -> Outcome {
    inputs_call(case, &stepped(payload, "put"), seams, &manifest_line(files), &archive(members))
}

/// A put of `files`, all of them sent, which must install every one.
fn put_all(case: &Case, payload: &Value, files: &[(&str, &[u8])]) {
    let outcome = put_with(case, payload, &[], files, files);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, format!("gate-remote-inputs: installed {}\n", files.len()), "{outcome:?}");
}

/// What a diff that needs `paths` says on stdout.
fn needs(paths: &[&str]) -> String {
    let mut said = format!("gate-remote-inputs: need {}\n", paths.len());
    for path in paths {
        let _ = writeln!(said, "gate-remote-inputs: path {path}");
    }
    said
}

/// Every file below `folder` but under `.git`, by `/`-separated path, with its bytes.
fn tree_files(folder: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    collect(folder, "", &mut files);
    files
}

fn collect(folder: &Path, prefix: &str, files: &mut BTreeMap<String, Vec<u8>>) {
    for entry in fs::read_dir(folder).expect("the folder can be listed") {
        let entry = entry.expect("an entry can be read");
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = format!("{prefix}{name}");
        let kind = entry.file_type().expect("an entry's kind can be read");
        if kind.is_dir() && name != ".git" {
            collect(&entry.path(), &format!("{path}/"), files);
        } else if kind.is_file() {
            files.insert(path, fs::read(entry.path()).expect("a file can be read"));
        }
    }
}

/// The work folder of `payload`.
fn work(payload: &Value) -> &Path {
    Path::new(field(payload, "Work"))
}

/// The stages folder below `root` of a box run by this suite's process: `inputs-stage` when it is elevated,
/// `inputs-stage-medium` when it is not, as the box chooses it.
fn stages_folder(root: &Path) -> PathBuf {
    let elevated = gate_win32::token_facts().expect("this process's token can be read").elevated;
    root.join(if elevated { "inputs-stage" } else { "inputs-stage-medium" })
}

/// The stages folder of `payload`'s box root, which holds nothing once a put has ended.
fn stages_left(payload: &Value) -> Vec<String> {
    let root = work(payload)
        .parent()
        .and_then(Path::parent)
        .expect("the work folder is <root>\\work\\<key>");
    match fs::read_dir(stages_folder(root)) {
        Ok(entries) => entries
            .map(|entry| entry.expect("an entry").file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// One input: its `/`-separated path and its bytes.
type Input = (&'static str, &'static [u8]);

const HELPER: Input = (".tmp/helper/a.dll", b"helper bytes");
const DATA: (&str, &[u8]) = ("data/b.txt", b"data\n");

#[test]
fn a_diff_of_an_empty_tree_needs_every_file_and_after_a_put_none() {
    let case = Case::new("box_inputs_diff_then_put");
    let payload = inputs_payload(&case, "diff");

    let first = diff(&case, &payload, &[HELPER, DATA]);
    assert_eq!(first.code, Some(0), "{first:?}");
    assert_eq!(first.stdout, needs(&[".tmp/helper/a.dll", "data/b.txt"]), "{first:?}");

    put_all(&case, &payload, &[HELPER, DATA]);
    let installed = tree_files(work(&payload));
    assert_eq!(installed.get(HELPER.0).map(Vec::as_slice), Some(HELPER.1));
    assert_eq!(installed.get(DATA.0).map(Vec::as_slice), Some(DATA.1));
    assert!(stages_left(&payload).is_empty(), "the stage is removed");

    let again = diff(&case, &payload, &[HELPER, DATA]);
    assert_eq!(again.code, Some(0), "{again:?}");
    assert_eq!(again.stdout, needs(&[]), "{again:?}");
}

#[test]
fn a_copy_changed_on_the_box_is_needed_again_and_one_only_touched_is_not() {
    let case = Case::new("box_inputs_changed_copy");
    let payload = inputs_payload(&case, "put");
    put_all(&case, &payload, &[HELPER, DATA]);

    fs::write(work(&payload).join(r"data\b.txt"), "changed\n").expect("the copy can be changed");
    let touched = File::options()
        .write(true)
        .open(work(&payload).join(r".tmp\helper\a.dll"))
        .expect("the copy can be opened");
    touched
        .set_modified(SystemTime::now() - Duration::from_hours(48))
        .expect("the copy's last write can be set");
    drop(touched);

    let outcome = diff(&case, &payload, &[HELPER, DATA]);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stdout,
        needs(&["data/b.txt"]),
        "the touched copy hashes to the manifest's; the changed one does not: {outcome:?}"
    );
}

#[test]
fn put_refuses_a_member_it_should_not_take_and_leaves_the_tree_unchanged() {
    let case = Case::new("box_inputs_put_refusals");
    let payload = inputs_payload(&case, "put");
    put_all(&case, &payload, &[DATA]);
    let before = tree_files(work(&payload));
    let other: (&str, &[u8]) = ("data/b.txt", b"other bytes\n");
    let rows: [(&str, Vec<Input>); 5] = [
        ("a member not in the manifest", vec![HELPER, ("extra.txt", b"extra")]),
        ("a .. path", vec![HELPER, ("../up.txt", b"up")]),
        ("a .git path", vec![HELPER, (".git/config", b"[core]")]),
        ("an absolute path", vec![HELPER, ("C:/abs.txt", b"abs")]),
        ("a member whose hash differs", vec![HELPER, other]),
    ];
    for (what, members) in rows {
        let outcome = put_with(&case, &payload, &[], &[HELPER, DATA], &members);

        assert_eq!(outcome.code, Some(1), "{what}: {outcome:?}");
        assert_eq!(outcome.stdout, "", "{what}: {outcome:?}");
        assert!(outcome.stderr.starts_with("gate: remote inputs put failed: "), "{what}: {outcome:?}");
        assert_eq!(tree_files(work(&payload)), before, "{what}: the tree is unchanged");
        assert!(stages_left(&payload).is_empty(), "{what}: the stage is removed");
    }
    assert!(!work(&payload).parent().expect("a parent").join("up.txt").exists());
}

#[test]
fn put_refuses_a_target_that_is_a_folder_or_below_a_junction() {
    let case = Case::new("box_inputs_put_targets");
    let payload = inputs_payload(&case, "put");
    let laid = diff(&case, &payload, &[DATA]);
    assert_eq!(laid.code, Some(0), "the diff lays the work folder out: {laid:?}");
    fs::create_dir_all(work(&payload).join(r"data\b.txt")).expect("a folder can be made at the target");

    let folder = put_with(&case, &payload, &[], &[DATA], &[DATA]);
    assert_eq!(folder.code, Some(1), "{folder:?}");
    assert!(folder.stderr.contains("data/b.txt"), "the line names the path: {folder:?}");
    assert!(work(&payload).join(r"data\b.txt").is_dir(), "the folder is left");

    let outside = case.folder().join("outside");
    fs::create_dir_all(&outside).expect("the outside folder can be made");
    junction(&work(&payload).join("linked"), &outside);
    let through: (&str, &[u8]) = ("linked/c.txt", b"c");
    let linked = put_with(&case, &payload, &[], &[through], &[through]);
    assert_eq!(linked.code, Some(1), "{linked:?}");
    assert!(linked.stderr.contains("linked/c.txt"), "the line names the path: {linked:?}");
    assert!(!outside.join("c.txt").exists(), "nothing is written through the junction");
}

#[test]
fn put_past_the_budget_seam_is_refused() {
    let case = Case::new("box_inputs_put_budget");
    let payload = inputs_payload(&case, "put");

    let outcome = put_with(&case, &payload, &["--test-inputs-budget-bytes", "4"], &[DATA], &[DATA]);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(outcome.stderr.starts_with("gate: remote inputs put failed: "), "{outcome:?}");
    assert!(!work(&payload).join(r"data\b.txt").exists(), "nothing is installed");
}

#[test]
fn a_put_deletes_a_stale_input_left_unchanged_and_keeps_one_changed_on_the_box() {
    let case = Case::new("box_inputs_stale");
    let payload = inputs_payload(&case, "put");
    let changed: (&str, &[u8]) = ("changed.txt", b"as sent");
    put_all(&case, &payload, &[HELPER, DATA, changed]);
    fs::write(work(&payload).join("changed.txt"), "changed on the box").expect("the copy can be changed");

    let outcome = put_with(&case, &payload, &[], &[HELPER], &[]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "gate-remote-inputs: installed 0\n", "{outcome:?}");
    let left = tree_files(work(&payload));
    assert!(left.contains_key(HELPER.0), "an input still in the manifest stays");
    assert!(!left.contains_key(DATA.0), "an unchanged input that left the manifest is deleted");
    assert_eq!(
        left.get("changed.txt").map(Vec::as_slice),
        Some(&b"changed on the box"[..]),
        "a changed one is kept"
    );
}

#[test]
fn the_exclude_file_holds_the_inputs_and_git_and_the_checkout_leave_them() {
    let case = Case::new("box_inputs_exclude");
    let mut payload = inputs_payload(&case, "put");
    snapshot(&case, &mut payload, &[("tracked.txt", "tracked\n")]);
    let first = run_box(&case, &payload);
    assert_eq!(first.code, Some(0), "{first:?}");
    let bracketed: (&str, &[u8]) = ("x y[1].txt", b"bracketed");

    put_all(&case, &payload, &[HELPER, bracketed]);

    let exclude = fs::read_to_string(work(&payload).join(r".git\info\exclude")).expect("the exclude file can be read");
    assert_eq!(exclude, "/.tmp/helper/a.dll\n/x y\\[1].txt\n");
    let tree = field(&payload, "Work");
    assert_eq!(git(&["-C", tree, "status", "--porcelain"]), "", "git sees none of the inputs");

    let second = run_box(&case, &payload);
    assert_eq!(second.code, Some(0), "{second:?}");
    let kept = tree_files(work(&payload));
    assert!(
        kept.contains_key(HELPER.0) && kept.contains_key(bracketed.0),
        "the checkout's clean keeps them"
    );
}

#[test]
fn an_inputs_call_on_a_held_lock_is_refused_busy() {
    let case = Case::new("box_inputs_busy");
    let payload = inputs_payload(&case, "diff");
    fs::create_dir_all(work(&payload).parent().expect("a parent")).expect("the work folder's parent can be made");
    let held = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(field(&payload, "Lock"))
        .expect("the work folder's lock can be held");

    let outcome = diff(&case, &payload, &[DATA]);

    drop(held);
    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    let tag = refused_line(&payload, "busy");
    assert_eq!(
        lines(&outcome.stderr),
        [tag.as_str(), "gate: remote work folder k is busy"],
        "{outcome:?}"
    );
    assert_eq!(outcome.stdout, "", "{outcome:?}");
}

#[test]
fn a_run_with_its_record_s_inputs_digest_runs_and_another_or_none_is_refused() {
    let case = Case::new("box_inputs_run_digest");
    let mut payload = inputs_payload(&case, "put");
    snapshot(&case, &mut payload, &[("tracked.txt", "tracked\n")]);
    let digest = sha256(manifest_line(&[DATA]).as_bytes());
    let mut digested = payload.clone();
    digested["InputsDigest"] = json!(digest);

    let unrecorded = run_box(&case, &digested);
    assert_eq!(unrecorded.code, Some(75), "no record: {unrecorded:?}");
    assert_eq!(lines(&unrecorded.stderr)[0], refused_line(&payload, "inputs"), "{unrecorded:?}");
    assert_eq!(unrecorded.stdout, "", "the command never ran: {unrecorded:?}");

    put_all(&case, &payload, &[DATA]);
    let matched = run_box(&case, &digested);
    assert_eq!(matched.code, Some(0), "{matched:?}");
    assert_eq!(matched.stdout, "ran\n", "{matched:?}");

    let mut other = payload.clone();
    other["InputsDigest"] = json!(sha256(b"another manifest"));
    let refused = run_box(&case, &other);
    assert_eq!(refused.code, Some(75), "{refused:?}");
    assert_eq!(lines(&refused.stderr)[0], refused_line(&payload, "inputs"), "{refused:?}");

    let plain = run_box(&case, &payload);
    assert_eq!(plain.code, Some(0), "without the key nothing is checked: {plain:?}");
    assert_eq!(plain.stdout, "ran\n", "{plain:?}");
}

#[test]
fn an_inputs_request_with_a_pair_is_refused() {
    let case = Case::new("box_inputs_pair");
    let mut payload = unreadied_payload(&case, "diff");
    payload["Pair"] = json!({"Leaf": "app", "Siblings": ["lib"]});

    let outcome = diff(&case, &payload, &[DATA]);

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(outcome.stderr, "gate box: inputs for a sibling pair are not taken yet\n");
    assert!(!work(&payload).exists(), "nothing is laid out");
}

/// `payload` carrying the digest of the manifest line `line` as its `InputsDigest`.
fn digested(payload: &Value, line: &str) -> Value {
    let mut digested = payload.clone();
    digested["InputsDigest"] = json!(sha256(line.as_bytes()));
    digested
}

/// The payload of a put whose work folder has a snapshot of one tracked file, for a run to check out.
fn snapshot_payload(case: &Case) -> Value {
    let mut payload = inputs_payload(case, "put");
    snapshot(case, &mut payload, &[("tracked.txt", "tracked\n")]);
    payload
}

/// Says whether `outcome` is a run refused `inputs` for `payload`.
fn assert_refused_inputs(payload: &Value, outcome: &Outcome, what: &str) {
    assert_eq!(outcome.code, Some(75), "{what}: {outcome:?}");
    assert_eq!(lines(&outcome.stderr)[0], refused_line(payload, "inputs"), "{what}: {outcome:?}");
    assert_eq!(outcome.stdout, "", "{what}: the command never ran: {outcome:?}");
}

#[test]
fn a_diff_that_needs_nothing_finishes_the_install_so_a_run_of_its_digest_runs() {
    let case = Case::new("box_inputs_diff_finishes");
    let payload = snapshot_payload(&case);
    put_all(&case, &payload, &[HELPER, DATA]);

    // The same bytes, rewritten on the PC, and DATA left the input set: a new manifest, nothing to send.
    let line = manifest_line_at(&[HELPER], 2);
    let outcome = inputs_call(&case, &stepped(&payload, "diff"), &[], &line, &[]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, needs(&[]), "{outcome:?}");
    assert!(
        !work(&payload).join(r"data\b.txt").exists(),
        "the input that left the manifest is deleted"
    );
    let ran = run_box(&case, &digested(&payload, &line));
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(ran.stdout, "ran\n", "{ran:?}");
}

#[test]
fn a_put_that_leaves_a_file_of_the_manifest_missing_fails_and_its_digest_is_refused() {
    let case = Case::new("box_inputs_put_missing");
    let payload = snapshot_payload(&case);

    let outcome = put_with(&case, &payload, &[], &[HELPER, DATA], &[HELPER]);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(
        outcome.stderr, "gate: remote inputs put failed: data/b.txt is missing after the install\n",
        "{outcome:?}"
    );
    let run = run_box(&case, &digested(&payload, &manifest_line(&[HELPER, DATA])));
    assert_refused_inputs(&payload, &run, "a put that did not finish");
}

#[test]
fn an_exclude_file_that_cannot_be_written_leaves_no_matching_digest() {
    let case = Case::new("box_inputs_exclude_fails");
    let payload = snapshot_payload(&case);
    put_all(&case, &payload, &[HELPER]);
    // A folder at the exclude file's name that holds a file open without delete sharing cannot be removed.
    let blocker = work(&payload).join(r".git\info\exclude");
    fs::remove_file(&blocker).expect("the exclude file can be removed");
    fs::create_dir(&blocker).expect("a folder can take its name");
    let held = File::options()
        .write(true)
        .create(true)
        .truncate(true)
        .share_mode(0)
        .open(blocker.join("held.txt"))
        .expect("a file in it can be held");

    let outcome = put_with(&case, &payload, &[], &[HELPER, DATA], &[DATA]);

    drop(held);
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    let run = run_box(&case, &digested(&payload, &manifest_line(&[HELPER, DATA])));
    assert_refused_inputs(&payload, &run, "the record is written only after the exclude file");
}

#[test]
fn a_recorded_input_changed_between_the_put_and_the_run_refuses_the_run() {
    let case = Case::new("box_inputs_run_changed");
    let payload = snapshot_payload(&case);
    put_all(&case, &payload, &[DATA]);
    fs::write(work(&payload).join(r"data\b.txt"), "changed on the box\n").expect("the copy can be changed");

    let run = run_box(&case, &digested(&payload, &manifest_line(&[DATA])));

    assert_refused_inputs(&payload, &run, "a copy that is not the one installed");
}

#[test]
fn a_run_rewrites_the_exclude_file_so_a_git_made_again_keeps_the_inputs() {
    let case = Case::new("box_inputs_run_exclude");
    let payload = snapshot_payload(&case);
    let first = run_box(&case, &payload);
    assert_eq!(first.code, Some(0), "{first:?}");
    put_all(&case, &payload, &[HELPER]);
    let exclude = work(&payload).join(r".git\info\exclude");
    fs::remove_file(&exclude).expect("the exclude file can be removed, as a .git made again lacks it");

    let run = run_box(&case, &digested(&payload, &manifest_line(&[HELPER])));

    assert_eq!(run.code, Some(0), "{run:?}");
    assert!(work(&payload).join(r".tmp\helper\a.dll").exists(), "the checkout's clean keeps the input");
    assert_eq!(
        fs::read_to_string(&exclude).expect("the exclude file is written again"),
        "/.tmp/helper/a.dll\n"
    );
}

/// Replaces the exclude file of `payload`'s work folder with a folder holding `inner.txt`: the exclude file's path and
/// the line a step that moves the folder aside says.
fn folder_at_exclude(payload: &Value) -> (PathBuf, String) {
    let exclude = work(payload).join(r".git\info\exclude");
    fs::remove_file(&exclude).expect("the exclude file can be removed");
    fs::create_dir(&exclude).expect("a folder can take the exclude file's name");
    fs::write(exclude.join("inner.txt"), "inner").expect("a file in it can be written");
    let line = format!(
        "gate: remote moved aside the folder {} to {}\n",
        exclude.display(),
        exclude.with_file_name("exclude.gate-moved-1").display()
    );
    (exclude, line)
}

#[test]
fn a_run_says_the_folder_it_moved_aside_from_the_exclude_file_s_name() {
    let case = Case::new("box_inputs_run_moved_aside");
    let payload = snapshot_payload(&case);
    put_all(&case, &payload, &[HELPER]);
    let (exclude, line) = folder_at_exclude(&payload);

    let run = run_box(&case, &digested(&payload, &manifest_line(&[HELPER])));

    assert_eq!(run.code, Some(0), "{run:?}");
    assert!(run.stderr.contains(&line), "{run:?}");
    assert_eq!(fs::read_to_string(&exclude).expect("the exclude file is written"), "/.tmp/helper/a.dll\n");
    assert!(
        exclude.with_file_name("exclude.gate-moved-1").join("inner.txt").exists(),
        "the folder is moved aside whole"
    );
}

#[test]
fn a_diff_that_needs_nothing_says_the_folder_it_moved_aside_from_the_exclude_file_s_name() {
    let case = Case::new("box_inputs_diff_moved_aside");
    let payload = snapshot_payload(&case);
    put_all(&case, &payload, &[HELPER]);
    let (exclude, line) = folder_at_exclude(&payload);

    let outcome = inputs_call(&case, &stepped(&payload, "diff"), &[], &manifest_line(&[HELPER]), &[]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, needs(&[]), "{outcome:?}");
    assert_eq!(outcome.stderr, line, "{outcome:?}");
    assert_eq!(fs::read_to_string(&exclude).expect("the exclude file is written"), "/.tmp/helper/a.dll\n");
    assert!(
        exclude.with_file_name("exclude.gate-moved-1").join("inner.txt").exists(),
        "the folder is moved aside whole"
    );
}

#[test]
fn a_junction_at_the_last_folder_refuses_the_put_and_nothing_is_written_through_it() {
    let case = Case::new("box_inputs_last_folder_junction");
    let payload = inputs_payload(&case, "put");
    let laid = diff(&case, &payload, &[DATA]);
    assert_eq!(laid.code, Some(0), "the diff lays the work folder out: {laid:?}");
    let outside = case.folder().join("outside");
    fs::create_dir_all(&outside).expect("the outside folder can be made");
    junction(&work(&payload).join("data"), &outside);

    let outcome = put_with(&case, &payload, &[], &[DATA], &[DATA]);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(outcome.stderr.contains("data/b.txt"), "the line names the path: {outcome:?}");
    assert_eq!(
        fs::read_dir(&outside).expect("the outside folder can be listed").count(),
        0,
        "nothing is written outside"
    );
}

#[test]
#[expect(clippy::print_stderr, reason = "a skipped case says why it skipped")]
fn a_file_symlink_at_the_exclude_file_is_replaced_not_followed() {
    let case = Case::new("box_inputs_exclude_symlink");
    let payload = inputs_payload(&case, "put");
    put_all(&case, &payload, &[HELPER]);
    let outside = case.folder().join("outside.txt");
    fs::write(&outside, "keep\n").expect("the outside file can be written");
    let exclude = work(&payload).join(r".git\info\exclude");
    fs::remove_file(&exclude).expect("the exclude file can be removed");
    if let Err(error) = std::os::windows::fs::symlink_file(&outside, &exclude) {
        eprintln!("skipped: a file symlink cannot be made here (no privilege, or developer mode off): {error}");
        return;
    }

    put_all(&case, &payload, &[HELPER, DATA]);

    assert_eq!(
        fs::read_to_string(&outside).expect("the outside file is there"),
        "keep\n",
        "the link is not followed"
    );
    let metadata = fs::symlink_metadata(&exclude).expect("the exclude file is there");
    assert!(!metadata.file_type().is_symlink(), "the link is replaced by a file");
    assert_eq!(
        fs::read_to_string(&exclude).expect("the exclude file can be read"),
        "/.tmp/helper/a.dll\n/data/b.txt\n"
    );
}

#[test]
fn a_stale_input_below_a_junction_is_kept_and_nothing_outside_is_deleted() {
    let case = Case::new("box_inputs_stale_junction");
    let payload = inputs_payload(&case, "put");
    let below: (&str, &[u8]) = ("sub/x.txt", b"x");
    put_all(&case, &payload, &[below]);
    let outside = case.folder().join("outside");
    fs::create_dir_all(&outside).expect("the outside folder can be made");
    fs::write(outside.join("x.txt"), "x").expect("the outside file can be written");
    fs::remove_dir_all(work(&payload).join("sub")).expect("the folder can be removed");
    junction(&work(&payload).join("sub"), &outside);

    let outcome = put_with(&case, &payload, &[], &[HELPER], &[HELPER]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outside.join("x.txt").exists(), "the file the junction leads to is kept: {outcome:?}");
    assert!(outcome.stderr.contains("gate: remote kept the stale input sub/x.txt: "), "{outcome:?}");
}

/// `path`, a file or a folder, opened to read its security.
fn security_of(path: &Path) -> String {
    let opened = File::options()
        .access_mode(READ_CONTROL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .expect("the entry can be opened");
    gate_win32::security_text(&opened).expect("its security can be read")
}

#[test]
fn the_stages_folder_keeps_lower_processes_out_and_a_moved_input_takes_its_folder_s_security() {
    let case = Case::new("box_inputs_security");
    let payload = inputs_payload(&case, "put");
    put_all(&case, &payload, &[DATA]);
    let facts = gate_win32::token_facts().expect("this process's token can be read");

    let stages = security_of(&stages_folder(work(&payload).parent().and_then(Path::parent).expect("a root")));
    let stripped = gate_win32::without_auto_inherit(&stages);
    let (dacl, label) = stripped.split_once("S:").expect("the stages folder has a label");
    assert!(dacl.starts_with("D:P"), "the DACL is protected: {stages}");
    assert_eq!(dacl.matches("(A;").count(), 3, "three grants and no more: {stages}");
    for grant in ["(A;OICI;FA;;;SY)", "(A;OICI;FA;;;BA)", &format!("(A;OICI;FA;;;{})", facts.user)] {
        assert!(dacl.contains(grant), "{grant} in {stages}");
    }
    let level = match facts.integrity {
        0x2000 => "ME".to_owned(),
        0x3000 => "HI".to_owned(),
        other => format!("S-1-16-{other}"),
    };
    assert_eq!(label, format!("(ML;OICI;NW;;;{level})"), "{stages}");

    let fresh = work(&payload).join(r"data\fresh.txt");
    fs::write(&fresh, "fresh").expect("a fresh file can be written");
    assert_eq!(
        gate_win32::without_auto_inherit(&security_of(&work(&payload).join(r"data\b.txt"))),
        gate_win32::without_auto_inherit(&security_of(&fresh)),
        "the moved input's DACL and label"
    );
}

/// Waits, up to a bound no busy machine reaches, until the stages folder of `payload` holds a stage.
fn await_stage(payload: &Value) {
    let deadline = Instant::now() + Duration::from_mins(2);
    while stages_left(payload).is_empty() {
        assert!(Instant::now() < deadline, "the put made no stage");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_put_whose_archive_stalls_holds_no_lock() {
    let case = Case::new("box_inputs_put_stall");
    let payload = inputs_payload(&case, "put");
    let (running, mut stdin) = box_call(&case, &["box", "inputs"], &self::payload(&stepped(&payload, "put"))).start_with_stdin();
    stdin
        .write_all(format!("{}\n", manifest_line(&[DATA])).as_bytes())
        .expect("the manifest can be sent");
    stdin.flush().expect("the manifest is sent");
    // The stage is made before the archive is read: from then on the put waits on its stdin.
    await_stage(&payload);

    let concurrent = diff(&case, &payload, &[DATA]);

    stdin.write_all(&archive(&[DATA])).expect("the archive can be sent");
    drop(stdin);
    let put = running.finish_read();
    assert_eq!(concurrent.code, Some(0), "the diff is not refused busy: {concurrent:?}");
    assert_eq!(put.code, Some(0), "{put:?}");
}

#[test]
fn a_record_that_is_not_one_reads_as_empty_and_one_that_is_a_junction_is_removed() {
    let case = Case::new("box_inputs_bad_records");
    let payload = inputs_payload(&case, "put");
    put_all(&case, &payload, &[DATA]);
    let record = format!("{}.inputs", field(&payload, "Work"));
    let digest = sha256(manifest_line(&[DATA]).as_bytes());
    let entry = json!({"size": 5, "sha256": sha256(DATA.1), "modified": 1, "id": "0-0"});
    let rows = [
        ("a corrupt record", "{not json".to_owned()),
        ("a record naming ..", json!({"digest": digest, "files": {"../b.txt": entry}}).to_string()),
    ];
    for (what, text) in rows {
        fs::write(&record, text).expect("the record can be written");

        let outcome = diff(&case, &payload, &[DATA]);

        assert_eq!(outcome.code, Some(0), "{what}: {outcome:?}");
        assert!(
            outcome.stderr.contains("is not an inputs record; every copy is hashed"),
            "{what}: {outcome:?}"
        );
    }

    let outside = case.folder().join("outside");
    fs::create_dir_all(&outside).expect("the outside folder can be made");
    fs::remove_file(&record).expect("the record can be removed");
    junction(Path::new(&record), &outside);
    put_all(&case, &payload, &[DATA]);
    assert!(
        fs::symlink_metadata(&record).expect("the record is there").is_file(),
        "the junction is replaced by a record"
    );
    assert_eq!(
        fs::read_dir(&outside).expect("the outside folder can be listed").count(),
        0,
        "nothing is written through it"
    );
}

#[test]
fn a_manifest_naming_a_file_inputs_never_carry_is_refused() {
    let case = Case::new("box_inputs_never_carried");
    let payload = unreadied_payload(&case, "diff");
    for name in [".lfsconfig", ".env", "config/prod.env", ".gitattributes", ".gitignore", "sub/.gitmodules"] {
        let outcome = diff(&case, &payload, &[(name, b"x")]);

        assert_eq!(outcome.code, Some(1), "{name}: {outcome:?}");
        assert_eq!(
            outcome.stderr,
            format!("gate: remote inputs diff failed: the inputs manifest names '{name}', which inputs never carry\n"),
            "{name}"
        );
    }
}

#[test]
fn a_manifest_naming_a_path_that_is_not_plain_is_refused_before_any_work() {
    let case = Case::new("box_inputs_manifest_not_plain");
    let payload = unreadied_payload(&case, "diff");

    let outcome = diff(&case, &payload, &[("../x.txt", b"x")]);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        "gate: remote inputs diff failed: the inputs manifest names '../x.txt', which is not a plain relative path\n"
    );
    assert!(!work(&payload).exists(), "nothing is laid out");
}

#[test]
fn a_manifest_that_is_not_json_is_said_to_be_the_inputs_manifest() {
    let case = Case::new("box_inputs_manifest_not_json");
    let payload = unreadied_payload(&case, "diff");

    let outcome = inputs_call(&case, &payload, &[], "{not json", &[]);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(
        outcome
            .stderr
            .starts_with("gate: remote inputs diff failed: the inputs manifest is not JSON: "),
        "{outcome:?}"
    );
}

#[test]
fn a_manifest_line_ending_in_cr_lf_has_the_digest_of_one_ending_in_lf() {
    let case = Case::new("box_inputs_crlf");
    let payload = snapshot_payload(&case);
    let line = manifest_line(&[DATA]);

    let outcome = inputs_call(&case, &stepped(&payload, "put"), &[], &format!("{line}\r"), &archive(&[DATA]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let run = run_box(&case, &digested(&payload, &line));
    assert_eq!(run.code, Some(0), "the record holds the digest of the line without its CR: {run:?}");
}

#[test]
fn an_input_that_becomes_a_folder_replaces_its_stale_file() {
    let case = Case::new("box_inputs_file_to_folder");
    let payload = inputs_payload(&case, "put");
    put_all(&case, &payload, &[("a", b"a")]);

    put_all(&case, &payload, &[("a/b", b"b")]);

    assert_eq!(fs::read_to_string(work(&payload).join(r"a\b")).expect("the new input is there"), "b");
}

#[test]
fn a_layout_clear_removes_the_inputs_record() {
    let case = Case::new("box_inputs_layout_clear");
    let payload = snapshot_payload(&case);
    put_all(&case, &payload, &[DATA]);
    let record = PathBuf::from(format!("{}.inputs", field(&payload, "Work")));
    assert!(record.exists(), "the put writes the record");
    fs::write(format!("{}.layout", field(&payload, "Work")), "pair app lib").expect("the layout record can be changed");

    let run = run_box(&case, &payload);

    assert_eq!(run.code, Some(0), "{run:?}");
    assert!(!record.exists(), "the layout clear removes the inputs record");
}

#[test]
fn a_copy_whose_stamp_the_record_holds_is_trusted_without_hashing_by_design_and_a_replaced_one_is_not() {
    let case = Case::new("box_inputs_trusted_stamp");
    let payload = inputs_payload(&case, "put");
    put_all(&case, &payload, &[DATA]);
    let copy = work(&payload).join(r"data\b.txt");
    let written = fs::metadata(&copy)
        .and_then(|metadata| metadata.modified())
        .expect("the last write can be read");
    let mut file = File::options().write(true).open(&copy).expect("the copy can be opened");
    file.write_all(b"DATA\n").expect("the copy can be rewritten in place, at its size");
    file.set_modified(written).expect("the last write can be restored");
    drop(file);

    let trusted = diff(&case, &payload, &[DATA]);
    assert_eq!(
        trusted.stdout,
        needs(&[]),
        "same size, last write and file id: trusted, never hashed: {trusted:?}"
    );

    fs::remove_file(&copy).expect("the copy can be removed");
    fs::write(&copy, "DATA\n").expect("another file can take its place");
    File::options()
        .write(true)
        .open(&copy)
        .and_then(|file| file.set_modified(written))
        .expect("the last write can be set");
    let replaced = diff(&case, &payload, &[DATA]);
    assert_eq!(replaced.stdout, needs(&["data/b.txt"]), "another file id: hashed: {replaced:?}");
}

#[test]
fn a_target_with_a_second_name_is_refused() {
    let case = Case::new("box_inputs_hard_link");
    let payload = inputs_payload(&case, "put");
    put_all(&case, &payload, &[DATA]);
    let twin = case.folder().join("twin.txt");
    fs::hard_link(work(&payload).join(r"data\b.txt"), &twin).expect("a second name can be made");
    let other: (&str, &[u8]) = ("data/b.txt", b"other bytes\n");

    let outcome = put_with(&case, &payload, &[], &[other], &[other]);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(outcome.stderr.contains("data/b.txt"), "the line names the path: {outcome:?}");
    assert_eq!(fs::read(&twin).expect("the second name is there"), DATA.1, "its bytes are untouched");
}

#[test]
fn a_put_removes_a_stage_older_than_a_day_and_keeps_a_fresh_one() {
    let case = Case::new("box_inputs_old_stages");
    let payload = inputs_payload(&case, "put");
    put_all(&case, &payload, &[DATA]);
    let stages = stages_folder(work(&payload).parent().and_then(Path::parent).expect("a root"));
    let (old, fresh) = (
        stages.join(format!("k-{}", "0".repeat(32))),
        stages.join(format!("other-{}", "1".repeat(32))),
    );
    for stage in [&old, &fresh] {
        fs::create_dir_all(stage.join("files")).expect("a stage can be made");
    }
    File::options()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(&old)
        .and_then(|folder| folder.set_modified(SystemTime::now() - Duration::from_hours(48)))
        .expect("the old stage's last write can be set");

    put_all(&case, &payload, &[DATA, HELPER]);

    assert!(!old.exists(), "the stage older than a day is removed");
    assert!(fresh.exists(), "a fresh stage, maybe another put's, is kept");
}

#[test]
fn an_inputs_request_missing_bare_or_with_another_step_and_a_run_with_a_bad_digest_are_usage_errors() {
    let case = Case::new("box_inputs_usage");
    let payload = unreadied_payload(&case, "diff");
    let mut bare_less = payload.clone();
    bare_less.as_object_mut().expect("an object").remove("Bare");
    for (what, asked) in [("no Bare", bare_less), ("another step", stepped(&payload, "other"))] {
        let outcome = inputs_call(&case, &asked, &[], &manifest_line(&[DATA]), &[]);
        assert_eq!(outcome.code, Some(2), "{what}: {outcome:?}");
    }

    let mut bad = snapshot_payload(&case);
    bad["InputsDigest"] = json!("not a digest");
    let run = run_box(&case, &bad);
    assert_eq!(run.code, Some(2), "{run:?}");
    assert!(run.stderr.contains("InputsDigest"), "{run:?}");
}

/// Makes the file `path` read-only.
fn read_only(path: &Path) {
    let mut permissions = fs::metadata(path).expect("the file is there").permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions).expect("the file can be made read-only");
}

#[test]
fn a_read_only_input_is_replaced_and_a_read_only_stale_copy_is_deleted() {
    let case = Case::new("box_inputs_read_only");
    let payload = inputs_payload(&case, "put");
    put_all(&case, &payload, &[HELPER, DATA]);
    read_only(&work(&payload).join(r"data\b.txt"));
    let other: (&str, &[u8]) = ("data/b.txt", b"other bytes\n");

    put_all(&case, &payload, &[HELPER, other]);
    assert_eq!(fs::read(work(&payload).join(r"data\b.txt")).expect("the input is there"), other.1);

    read_only(&work(&payload).join(r"data\b.txt"));
    let outcome = put_with(&case, &payload, &[], &[HELPER], &[]);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        !work(&payload).join(r"data\b.txt").exists(),
        "the read-only stale copy is deleted: {outcome:?}"
    );
}

#[test]
fn a_record_naming_a_git_file_reads_as_not_a_record() {
    let case = Case::new("box_inputs_record_git_name");
    let payload = inputs_payload(&case, "put");
    put_all(&case, &payload, &[DATA]);
    let entry = json!({"size": 5, "sha256": sha256(DATA.1), "modified": 1, "id": "0-0"});
    let text = json!({"digest": sha256(manifest_line(&[DATA]).as_bytes()), "files": {".gitignore": entry}}).to_string();
    fs::write(format!("{}.inputs", field(&payload, "Work")), text).expect("the record can be written");

    let outcome = diff(&case, &payload, &[DATA]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains("is not an inputs record; every copy is hashed"), "{outcome:?}");
}

/// The tree's exclude file of `payload`'s work folder.
fn exclude_of(payload: &Value) -> String {
    fs::read_to_string(work(payload).join(r".git\info\exclude")).expect("the exclude file can be read")
}

#[test]
fn a_stale_copy_kept_because_it_changed_stays_excluded_through_later_puts() {
    let case = Case::new("box_inputs_kept_excluded");
    let payload = inputs_payload(&case, "put");
    let changed: (&str, &[u8]) = ("changed.txt", b"as sent");
    put_all(&case, &payload, &[HELPER, changed]);
    fs::write(work(&payload).join("changed.txt"), "changed on the box").expect("the copy can be changed");

    put_all(&case, &payload, &[HELPER]);
    assert_eq!(exclude_of(&payload), "/.tmp/helper/a.dll\n/changed.txt\n", "the kept copy is excluded");

    put_all(&case, &payload, &[HELPER]);
    assert_eq!(
        fs::read(work(&payload).join("changed.txt")).expect("the kept copy is there"),
        b"changed on the box",
        "a later put leaves the kept copy in place"
    );
    assert_eq!(exclude_of(&payload), "/.tmp/helper/a.dll\n/changed.txt\n", "and still excluded");
    let tree = field(&payload, "Work");
    assert_eq!(git(&["-C", tree, "status", "--porcelain"]), "", "git sees no input, kept or installed");

    put_all(&case, &payload, &[HELPER, changed]);
    assert_eq!(
        fs::read(work(&payload).join("changed.txt")).expect("the input is there"),
        changed.1,
        "a manifest naming it again installs it"
    );
    assert_eq!(exclude_of(&payload), "/.tmp/helper/a.dll\n/changed.txt\n", "as an input again");
}

#[test]
fn a_kept_copy_deleted_by_hand_leaves_the_record_and_the_exclude_file() {
    let case = Case::new("box_inputs_kept_gone");
    let payload = inputs_payload(&case, "put");
    let changed: (&str, &[u8]) = ("changed.txt", b"as sent");
    put_all(&case, &payload, &[HELPER, changed]);
    fs::write(work(&payload).join("changed.txt"), "changed on the box").expect("the copy can be changed");
    put_all(&case, &payload, &[HELPER]);
    assert_eq!(exclude_of(&payload), "/.tmp/helper/a.dll\n/changed.txt\n", "the kept copy is excluded");
    fs::remove_file(work(&payload).join("changed.txt")).expect("the kept copy can be deleted");

    put_all(&case, &payload, &[HELPER]);

    assert_eq!(
        exclude_of(&payload),
        "/.tmp/helper/a.dll\n",
        "the gone copy's line leaves the exclude file"
    );
    let record = fs::read_to_string(format!("{}.inputs", field(&payload, "Work"))).expect("the record can be read");
    assert!(!record.contains("changed.txt"), "the gone copy's entry leaves the record: {record}");
}

#[test]
fn a_junction_at_the_git_info_folder_is_replaced_by_a_folder_and_nothing_is_written_outside() {
    let case = Case::new("box_inputs_info_junction");
    let payload = inputs_payload(&case, "put");
    put_all(&case, &payload, &[HELPER]);
    let info = work(&payload).join(r".git\info");
    fs::remove_dir_all(&info).expect("the info folder can be removed");
    let outside = case.folder().join("outside");
    fs::create_dir_all(&outside).expect("the outside folder can be made");
    junction(&info, &outside);

    put_all(&case, &payload, &[HELPER, DATA]);

    assert!(
        fs::symlink_metadata(&info).expect("the info folder is there").is_dir(),
        "the junction is replaced by a folder of its own"
    );
    assert_eq!(exclude_of(&payload), "/.tmp/helper/a.dll\n/data/b.txt\n");
    assert_eq!(
        fs::read_dir(&outside).expect("the outside folder can be listed").count(),
        0,
        "nothing is written outside"
    );
}

#[test]
fn a_put_that_fails_after_moving_a_file_in_leaves_the_record_without_the_last_digest() {
    let case = Case::new("box_inputs_interim_record");
    let payload = snapshot_payload(&case);
    put_all(&case, &payload, &[DATA]);
    let first = manifest_line(&[DATA]);
    let other: (&str, &[u8]) = ("data/b.txt", b"other bytes\n");
    // Read and write shared but not deletion: the check reads the target, and the rename onto it fails.
    let held = File::options()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(work(&payload).join(r"data\b.txt"))
        .expect("the last target can be held");

    let outcome = put_with(&case, &payload, &[], &[HELPER, other], &[HELPER, other]);

    drop(held);
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(outcome.stderr.contains("cannot install 'data/b.txt'"), "{outcome:?}");
    assert!(work(&payload).join(r".tmp\helper\a.dll").exists(), "the first file was moved in");
    let run = run_box(&case, &digested(&payload, &first));
    assert_refused_inputs(&payload, &run, "the interim record holds no digest");
}

/// The bin folder a sent build of the project `app` in `Release` installs below.
const SENT_BIN: &str = "app/bin/Release";
const BUILT_DLL: Input = ("app/bin/Release/net10.0/app.dll", b"built dll");
const BUILT_PDB: Input = ("app/bin/Release/net10.0/app.pdb", b"built pdb");
const RESTORE_PROPS: Input = ("app/obj/app.csproj.nuget.g.props", b"<Project>props</Project>");
const RESTORE_TARGETS: Input = ("app/obj/app.csproj.nuget.g.targets", b"<Project>targets</Project>");

/// `payload` as a request sending the PC's build of the project `app` in `Release` ([`SENT_BIN`]).
fn sent(payload: &Value) -> Value {
    let mut sent = payload.clone();
    sent["SentBin"] = json!(SENT_BIN);
    sent
}

/// A put of a sent build of `files`, all of them sent, which must install every one.
fn sent_all(case: &Case, payload: &Value, files: &[(&str, &[u8])]) {
    put_all(case, &sent(payload), files);
}

/// Writes `bytes` at the `/`-separated `path` of `payload`'s work folder, its folders made.
fn written(payload: &Value, path: &str, bytes: &[u8]) {
    let at = work(payload).join(path.replace('/', "\\"));
    fs::create_dir_all(at.parent().expect("a parent")).expect("the folders can be made");
    fs::write(at, bytes).expect("the file can be written");
}

/// `payload` carrying the digest of the manifest line `line` as its `SentDigest`.
fn sent_digested(payload: &Value, line: &str) -> Value {
    let mut digested = payload.clone();
    digested["SentDigest"] = json!(sha256(line.as_bytes()));
    digested
}

#[test]
fn sent_build_restore_files_on_the_box_count_as_matched_and_are_never_replaced() {
    let case = Case::new("box_sent_restore_matched");
    let payload = inputs_payload(&case, "diff");
    let laid = diff(&case, &sent(&payload), &[BUILT_DLL]);
    assert_eq!(laid.code, Some(0), "the diff lays the work folder out: {laid:?}");
    written(&payload, RESTORE_PROPS.0, b"the builder's own props");
    written(&payload, RESTORE_TARGETS.0, b"the builder's own targets");
    let files = [BUILT_DLL, RESTORE_PROPS, RESTORE_TARGETS];

    let outcome = diff(&case, &sent(&payload), &files);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, needs(&[BUILT_DLL.0]), "only the bin file is needed: {outcome:?}");

    let put = put_with(&case, &sent(&payload), &[], &files, &[BUILT_DLL, RESTORE_PROPS]);
    assert_eq!(put.code, Some(1), "a restore file on the box refuses the put: {put:?}");
    assert!(put.stderr.contains(RESTORE_PROPS.0), "the line names the path: {put:?}");
    let left = tree_files(work(&payload));
    assert_eq!(left.get(RESTORE_PROPS.0).map(Vec::as_slice), Some(&b"the builder's own props"[..]));

    let installed = put_with(&case, &sent(&payload), &[], &files, &[BUILT_DLL]);
    assert_eq!(installed.code, Some(0), "{installed:?}");
    let left = tree_files(work(&payload));
    assert_eq!(left.get(BUILT_DLL.0).map(Vec::as_slice), Some(BUILT_DLL.1));
    assert_eq!(
        left.get(RESTORE_TARGETS.0).map(Vec::as_slice),
        Some(&b"the builder's own targets"[..]),
        "the builder's own restore file stays"
    );
}

#[test]
fn sent_build_with_one_restore_file_alone_on_the_box_fails_the_diff() {
    let case = Case::new("box_sent_restore_pair");
    let payload = inputs_payload(&case, "diff");
    let laid = diff(&case, &sent(&payload), &[BUILT_DLL]);
    assert_eq!(laid.code, Some(0), "{laid:?}");
    written(&payload, RESTORE_PROPS.0, RESTORE_PROPS.1);

    let outcome = diff(&case, &sent(&payload), &[BUILT_DLL, RESTORE_PROPS, RESTORE_TARGETS]);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert!(outcome.stderr.starts_with("gate: remote inputs diff failed: "), "{outcome:?}");
    assert!(outcome.stderr.contains(RESTORE_TARGETS.0), "the line names the missing one: {outcome:?}");
}

#[test]
fn sent_build_restore_file_leaving_the_manifest_is_never_deleted() {
    let case = Case::new("box_sent_restore_kept");
    let payload = inputs_payload(&case, "put");
    sent_all(&case, &payload, &[BUILT_DLL, RESTORE_PROPS, RESTORE_TARGETS]);

    sent_all(&case, &payload, &[BUILT_DLL]);

    let left = tree_files(work(&payload));
    assert!(left.contains_key(RESTORE_PROPS.0), "the props stay: {:?}", left.keys());
    assert!(left.contains_key(RESTORE_TARGETS.0), "the targets stay: {:?}", left.keys());
}

#[test]
fn sent_build_run_with_its_digest_runs_and_a_changed_bin_file_is_refused_sent() {
    let case = Case::new("box_sent_run_digest");
    let payload = snapshot_payload(&case);
    let files = [BUILT_DLL, RESTORE_PROPS, RESTORE_TARGETS];
    let digested = sent_digested(&payload, &manifest_line(&files));

    let unrecorded = run_box(&case, &digested);
    assert_eq!(unrecorded.code, Some(75), "no record: {unrecorded:?}");
    assert_eq!(lines(&unrecorded.stderr)[0], refused_line(&payload, "sent"), "{unrecorded:?}");
    assert_eq!(unrecorded.stdout, "", "the command never ran: {unrecorded:?}");

    sent_all(&case, &payload, &files);
    let matched = run_box(&case, &digested);
    assert_eq!(matched.code, Some(0), "{matched:?}");
    assert_eq!(matched.stdout, "ran\n", "{matched:?}");

    let other = run_box(&case, &sent_digested(&payload, "another manifest"));
    assert_eq!(other.code, Some(75), "{other:?}");
    assert_eq!(lines(&other.stderr)[0], refused_line(&payload, "sent"), "{other:?}");

    written(&payload, BUILT_DLL.0, b"rebuilt on the box");
    let changed = run_box(&case, &digested);
    assert_eq!(changed.code, Some(75), "{changed:?}");
    assert_eq!(lines(&changed.stderr)[0], refused_line(&payload, "sent"), "{changed:?}");
    assert!(changed.stderr.contains(BUILT_DLL.0), "the line names the file: {changed:?}");
}

#[test]
fn sent_build_record_is_removed_by_a_layout_clear() {
    let case = Case::new("box_sent_layout_clear");
    let payload = snapshot_payload(&case);
    sent_all(&case, &payload, &[BUILT_DLL]);
    let record = PathBuf::from(format!("{}.sent-build", field(&payload, "Work")));
    assert!(record.exists(), "the put writes the sent-build record");
    assert!(
        !Path::new(&format!("{}.inputs", field(&payload, "Work"))).exists(),
        "and not the inputs record"
    );
    fs::write(format!("{}.layout", field(&payload, "Work")), "pair app lib").expect("the layout record can be changed");

    let run = run_box(&case, &payload);

    assert_eq!(run.code, Some(0), "{run:?}");
    assert!(!record.exists(), "the layout clear removes the sent-build record");
}

#[test]
fn sent_build_put_deletes_bin_files_neither_record_holds_and_no_folder() {
    let case = Case::new("box_sent_leftovers");
    let payload = inputs_payload(&case, "put");
    let input: Input = ("app/bin/Release/settings.local.json", b"an input");
    put_all(&case, &payload, &[input]);
    written(&payload, "app/bin/Release/net10.0/old.dll", b"an older build");
    written(&payload, "app/bin/Release/net10.0/sub/old.xml", b"an older build");
    written(&payload, "app/bin/Debug/other.dll", b"another configuration");
    fs::create_dir_all(work(&payload).join(r"app\bin\Release\empty")).expect("a folder can be made");

    sent_all(&case, &payload, &[BUILT_DLL, BUILT_PDB]);

    let left: Vec<String> = tree_files(work(&payload)).into_keys().filter(|path| path.starts_with("app/")).collect();
    assert_eq!(
        left,
        ["app/bin/Debug/other.dll", BUILT_DLL.0, BUILT_PDB.0, "app/bin/Release/settings.local.json"],
        "only the files of neither record under the bin folder go"
    );
    assert!(work(&payload).join(r"app\bin\Release\net10.0\sub").is_dir(), "a folder stays");
    assert!(work(&payload).join(r"app\bin\Release\empty").is_dir(), "an empty folder stays");
}

#[test]
fn sent_build_and_inputs_share_one_exclude_file() {
    let case = Case::new("box_sent_exclude");
    let payload = inputs_payload(&case, "put");
    put_all(&case, &payload, &[DATA]);

    sent_all(&case, &payload, &[BUILT_DLL]);
    assert_eq!(
        exclude_of(&payload),
        format!("/{}\n/data/b.txt\n", BUILT_DLL.0),
        "the send keeps the inputs' line"
    );

    put_all(&case, &payload, &[HELPER]);
    assert_eq!(
        exclude_of(&payload),
        format!("/.tmp/helper/a.dll\n/{}\n", BUILT_DLL.0),
        "an inputs put keeps the send's line"
    );
}

/// A path the repo's inputs installed is never also the sent build's: the send's diff fails on it, naming it, before
/// any upload, and a path the inputs record does not hold still diffs.
#[test]
fn sent_build_diff_fails_on_a_path_the_inputs_record_holds() {
    let case = Case::new("box_sent_held_by_inputs");
    let payload = inputs_payload(&case, "put");
    put_all(&case, &payload, &[BUILT_DLL]);

    let outcome = diff(&case, &sent(&payload), &[BUILT_DLL, BUILT_PDB]);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "no path is asked for: {outcome:?}");
    assert!(outcome.stderr.starts_with("gate: remote inputs diff failed: "), "{outcome:?}");
    assert!(outcome.stderr.contains(BUILT_DLL.0), "the line names the path: {outcome:?}");
    let other = diff(&case, &sent(&payload), &[BUILT_PDB]);
    assert_eq!(other.code, Some(0), "{other:?}");
}

#[test]
fn sent_build_manifest_path_outside_its_bin_folder_and_restore_names_is_refused() {
    let case = Case::new("box_sent_scope");
    let payload = inputs_payload(&case, "diff");
    let rows: [(&str, Input); 4] = [
        ("a tracked-tree file", DATA),
        ("another configuration's bin", ("app/bin/Debug/app.dll", b"debug")),
        ("another obj file", ("app/obj/project.assets.json", b"{}")),
        ("another project's restore file", ("lib/obj/lib.csproj.nuget.g.props", b"<Project/>")),
    ];
    for (what, file) in rows {
        let outcome = diff(&case, &sent(&payload), &[BUILT_DLL, file]);

        assert_eq!(outcome.code, Some(1), "{what}: {outcome:?}");
        assert_eq!(outcome.stdout, "", "{what}: {outcome:?}");
        assert!(outcome.stderr.contains(file.0), "{what}: the line names the path: {outcome:?}");
    }
    let taken = diff(&case, &sent(&payload), &[BUILT_DLL, RESTORE_PROPS, RESTORE_TARGETS]);
    assert_eq!(taken.code, Some(0), "{taken:?}");
}

#[test]
fn sent_build_diff_fails_on_a_needed_bin_file_with_a_second_name_before_any_upload() {
    let case = Case::new("box_sent_hard_link");
    let payload = inputs_payload(&case, "put");
    sent_all(&case, &payload, &[BUILT_DLL]);
    let twin = case.folder().join("twin.dll");
    fs::hard_link(work(&payload).join(r"app\bin\Release\net10.0\app.dll"), &twin).expect("a second name can be made");
    let rebuilt: Input = (BUILT_DLL.0, b"rebuilt on the PC");

    let outcome = diff(&case, &sent(&payload), &[rebuilt]);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "no path is asked for: {outcome:?}");
    assert!(outcome.stderr.contains(BUILT_DLL.0), "the line names the path: {outcome:?}");
}

#[test]
#[expect(clippy::print_stderr, reason = "a skipped part says why it skipped")]
fn sent_build_leftover_delete_follows_no_link_and_leaves_a_file_of_two_names_whole() {
    let case = Case::new("box_sent_leftover_links");
    let payload = inputs_payload(&case, "put");
    sent_all(&case, &payload, &[BUILT_DLL]);
    let outside = case.folder().join("outside");
    fs::create_dir_all(&outside).expect("the outside folder can be made");
    fs::write(outside.join("kept.dll"), "outside").expect("the outside file can be written");
    fs::write(outside.join("twin.dll"), "a second name").expect("the outside file can be written");
    let bin = work(&payload).join(r"app\bin\Release");
    junction(&bin.join("linked"), &outside);
    fs::hard_link(outside.join("twin.dll"), bin.join("twin.dll")).expect("a second name can be made");
    let symlinked = match std::os::windows::fs::symlink_file(outside.join("kept.dll"), bin.join("link.dll")) {
        Ok(()) => true,
        Err(error) => {
            eprintln!("no file symlink is planted, as this user may not make one: {error}");
            false
        }
    };

    sent_all(&case, &payload, &[BUILT_DLL, BUILT_PDB]);

    assert_eq!(fs::read(outside.join("kept.dll")).expect("the outside file stays"), b"outside");
    assert_eq!(
        fs::read(outside.join("twin.dll")).expect("the other name stays"),
        b"a second name",
        "the hard link's other name keeps its content"
    );
    assert_eq!(fs::read_dir(&outside).expect("it can be listed").count(), 2, "nothing outside is removed");
    assert!(fs::symlink_metadata(bin.join("linked")).is_ok(), "the junction stays as itself");
    if symlinked {
        assert!(fs::symlink_metadata(bin.join("link.dll")).is_ok(), "the symlink stays as itself");
    }
}

#[test]
fn an_inputs_put_of_a_restore_named_file_installs_and_replaces_it_as_any_input() {
    let case = Case::new("box_inputs_restore_name");
    let payload = inputs_payload(&case, "put");
    let restore: Input = ("obj/x.nuget.g.props", b"<Project>pc</Project>");
    put_all(&case, &payload, &[restore]);
    written(&payload, restore.0, b"changed on the box");

    let outcome = diff(&case, &payload, &[restore]);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, needs(&[restore.0]), "a changed copy is needed again: {outcome:?}");

    put_all(&case, &payload, &[restore]);
    let left = tree_files(work(&payload));
    assert_eq!(left.get(restore.0).map(Vec::as_slice), Some(restore.1), "the put replaces it");

    put_all(&case, &payload, &[DATA]);
    let left = tree_files(work(&payload));
    assert!(!left.contains_key(restore.0), "a copy leaving the manifest is deleted: {:?}", left.keys());
}

#[test]
fn sent_build_whose_manifest_names_no_bin_file_is_refused_and_the_bin_keeps_its_build() {
    let case = Case::new("box_sent_no_bin_file");
    let payload = inputs_payload(&case, "put");
    sent_all(&case, &payload, &[BUILT_DLL]);
    let rows: [(&str, &[Input]); 2] = [("an empty manifest", &[]), ("restore files alone", &[RESTORE_PROPS, RESTORE_TARGETS])];
    for (what, files) in rows {
        for outcome in [diff(&case, &sent(&payload), files), put_with(&case, &sent(&payload), &[], files, files)] {
            assert_eq!(outcome.code, Some(1), "{what}: {outcome:?}");
            assert_eq!(outcome.stdout, "", "{what}: {outcome:?}");
            assert!(
                outcome.stderr.contains("the sent build's manifest names no file below app/bin/Release/"),
                "{what}: {outcome:?}"
            );
        }
        let left = tree_files(work(&payload));
        assert_eq!(
            left.get(BUILT_DLL.0).map(Vec::as_slice),
            Some(BUILT_DLL.1),
            "{what}: the bin keeps its build"
        );
    }
}

/// The start of the line a box refuses a malformed `SentBin` with.
const MALFORMED_SENT_BIN: &str = "gate: box payload SentBin is not a plain <project>/bin/<configuration> path: ";

/// A diff of the built dll in the work folder of `payload` sending the bin folder `bin`.
fn sent_bin_diff(case: &Case, payload: &Value, bin: &str) -> Outcome {
    let mut asked = payload.clone();
    asked["SentBin"] = json!(bin);
    diff(case, &asked, &[BUILT_DLL])
}

/// Asserts that `outcome`, of a request sending `what`, is the usage error of a malformed `SentBin`.
fn assert_malformed_sent_bin(what: &str, outcome: &Outcome) {
    assert_eq!(outcome.code, Some(2), "{what}: {outcome:?}");
    assert_eq!(outcome.stdout, "", "{what}: {outcome:?}");
    assert!(outcome.stderr.starts_with(MALFORMED_SENT_BIN), "{what}: {outcome:?}");
}

#[test]
fn sent_bin_that_is_not_a_plain_project_bin_configuration_path_is_a_usage_error() {
    let case = Case::new("box_sent_bin_malformed");
    let payload = inputs_payload(&case, "diff");
    let rows = [
        ("a parent part", "../bin/Release"),
        ("a leading slash", "/app/bin/Release"),
        ("a drive-relative part", "C:x/bin/Release"),
        ("a UNC path", r"\\server\share\bin\Release"),
        ("a stream", "app/bin/Release:stream"),
        ("a trailing dot", "app./bin/Release"),
        ("a trailing space", "app /bin/Release"),
        ("an 8.3 alias", "APP~1/bin/Release"),
        ("a device name", "CON/bin/Release"),
        ("another folder than bin", "app/obj/Release"),
        ("an empty configuration", "app/bin/"),
    ];
    for (what, bin) in rows {
        assert_malformed_sent_bin(what, &sent_bin_diff(&case, &payload, bin));
    }
}

#[test]
fn sent_bin_with_a_git_name_is_a_usage_error() {
    let case = Case::new("box_sent_bin_git");
    let payload = inputs_payload(&case, "diff");
    for (what, bin) in [("a .git* project", ".github/bin/Release"), ("a .git* configuration", "app/bin/.gitkeep")] {
        assert_malformed_sent_bin(what, &sent_bin_diff(&case, &payload, bin));
    }
}
