//! A routed run's ignored inputs on the box: the `inputs` verb's diff of the PC's manifest against the work folder, its
//! put of an archive into the work folder, the record and the exclude file it keeps, and the run's check of the digest
//! its request carries.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Write;
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

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
    let files: Map<String, Value> = files
        .iter()
        .map(|(path, bytes)| {
            let file = json!({"size": bytes.len(), "modified": 1, "sha256": sha256(bytes)});
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
    running.finish()
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

/// The stages folder of `payload`'s box root, which holds nothing once a put has ended.
fn stages_left(payload: &Value) -> Vec<String> {
    let root = work(payload)
        .parent()
        .and_then(Path::parent)
        .expect("the work folder is <root>\\work\\<key>");
    match fs::read_dir(root.join("inputs-stage")) {
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
