//! A routed run's Git LFS objects on the box: the `lfs` verb's diff of the PC's manifest against the repo's store,
//! its put of an archive of objects into the store, and the store's security.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Write;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ, READ_CONTROL};

use super::ReadBack;
use super::{
    Does, box_call, field, gate_box, git, has_ref, lines, pair_payload, pair_snapshot, passed, path_for_run, payload, refused_line, renamed, run_box,
    run_box_with_env, run_box_with_path, snapshot, started_line, stub_git_doing,
};
use crate::support::{Case, FAKE_TOOL, Outcome, box_payload};

/// The lower-case hex SHA-256 of `bytes`.
fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// The payload of an LFS call for the work folder `k`, its work folder's parent and bare repository readied by a
/// preflight first, as every route's first call does.
fn lfs_payload(case: &Case) -> Value {
    let payload = box_payload(case, "k", &[FAKE_TOOL, "out", "ran"]);
    passed(&gate_box(case, "preflight", &self::payload(&payload), None));
    payload
}

/// `payload` asking for `step`.
fn stepped(payload: &Value, step: &str) -> Value {
    let mut stepped = payload.clone();
    stepped["Step"] = json!(step);
    stepped
}

/// The manifest line the PC sends for `objects`, each its bytes, without its newline.
fn manifest_line(objects: &[&[u8]]) -> String {
    let entries: Vec<String> = objects.iter().map(|bytes| format!("{} {}", sha256(bytes), bytes.len())).collect();
    entries.join(" ")
}

/// Appends the regular file `path` holding `bytes` to `builder`.
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

/// The archive the PC sends for `members`, each a member's name and its bytes, in the outputs archive's format.
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

/// The archive of `objects`, each member named by its oid.
fn objects_archive(objects: &[&[u8]]) -> Vec<u8> {
    let oids: Vec<String> = objects.iter().map(|bytes| sha256(bytes)).collect();
    let members: Vec<(&str, &[u8])> = oids.iter().map(String::as_str).zip(objects.iter().copied()).collect();
    archive(&members)
}

/// `gate box <seams> lfs` of `payload` with `env` set, its request line, then `manifest` and its newline, then
/// `archive` written to its stdin, which is then closed; to its end.
fn lfs_call_with(case: &Case, payload: &Value, seams: &[&str], env: &[(&str, &str)], input: (&str, &[u8])) -> Outcome {
    let (manifest, archive) = input;
    let mut args = vec!["box"];
    args.extend_from_slice(seams);
    args.push("lfs");
    let mut call = box_call(case, &args, &self::payload(payload));
    for (name, value) in env {
        call.env(name, value);
    }
    let (running, mut stdin) = call.start_with_stdin();
    // A box that refuses before it reads the archive may close its end first; what it said is the case's to assert.
    drop(stdin.write_all(format!("{manifest}\n").as_bytes()));
    drop(stdin.write_all(archive));
    drop(stdin);
    running.finish_read()
}

/// A diff of `objects` in the store of `payload`.
fn diff(case: &Case, payload: &Value, objects: &[&[u8]]) -> Outcome {
    lfs_call_with(case, &stepped(payload, "diff"), &[], &[], (&manifest_line(objects), &[]))
}

/// A put of the archive `archive` against the manifest of `objects`, with `seams` before the verb.
fn put_with(case: &Case, payload: &Value, seams: &[&str], objects: &[&[u8]], archive: &[u8]) -> Outcome {
    lfs_call_with(case, &stepped(payload, "put"), seams, &[], (&manifest_line(objects), archive))
}

/// A put of `objects`, all of them sent, which must install every one.
fn put_all(case: &Case, payload: &Value, objects: &[&[u8]]) {
    let outcome = put_with(case, payload, &[], objects, &objects_archive(objects));
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, format!("gate-remote-lfs: installed {}\n", objects.len()), "{outcome:?}");
}

/// What a diff that needs `objects` says on stdout.
fn needs(objects: &[&[u8]]) -> String {
    let mut said = format!("gate-remote-lfs: need {}\n", objects.len());
    for bytes in objects {
        let _ = writeln!(said, "gate-remote-lfs: oid {}", sha256(bytes));
    }
    said
}

/// The repo's LFS store of `payload`: `{Bare}\lfs`.
fn store(payload: &Value) -> PathBuf {
    Path::new(field(payload, "Bare")).join("lfs")
}

/// The path the store of `payload` keeps the object of `bytes` at.
fn object_path(payload: &Value, bytes: &[u8]) -> PathBuf {
    let oid = sha256(bytes);
    store(payload).join("objects").join(&oid[..2]).join(&oid[2..4]).join(&oid)
}

/// The stages of `payload`'s box root, which hold nothing once a put has ended.
fn stages_left(payload: &Value) -> Vec<String> {
    let root = Path::new(field(payload, "Work"))
        .parent()
        .and_then(Path::parent)
        .expect("the work folder is <root>\\work\\<key>");
    match fs::read_dir(root.join("lfs-stage")) {
        Ok(entries) => entries
            .map(|entry| entry.expect("an entry").file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Asserts `outcome` is the `lfs` refusal of `payload`'s `step`: status 1, nothing on stdout, the tag line first and
/// then the step's failure line; that failure line.
fn assert_refused<'a>(payload: &Value, outcome: &'a Outcome, step: &str, what: &str) -> &'a str {
    assert_eq!(outcome.code, Some(1), "{what}: {outcome:?}");
    assert_eq!(outcome.stdout, "", "{what}: {outcome:?}");
    let said = lines(&outcome.stderr);
    assert_eq!(said.first().copied(), Some(refused_line(payload, "lfs").as_str()), "{what}: {outcome:?}");
    let failed = said.get(1).copied().unwrap_or_default();
    assert!(failed.starts_with(&format!("gate: remote lfs {step} failed: ")), "{what}: {outcome:?}");
    failed
}

const A: &[u8] = b"first object";
const B: &[u8] = b"second object";

#[test]
fn a_diff_names_each_object_the_store_lacks() {
    let case = Case::new("box_lfs_diff_lacks");
    let payload = lfs_payload(&case);

    let outcome = diff(&case, &payload, &[A, B]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, needs(&[A, B]), "{outcome:?}");
}

#[test]
fn a_diff_of_objects_all_present_needs_none() {
    let case = Case::new("box_lfs_diff_present");
    let payload = lfs_payload(&case);
    put_all(&case, &payload, &[A, B]);

    let outcome = diff(&case, &payload, &[A, B]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, needs(&[]), "{outcome:?}");
}

#[test]
fn a_put_installs_each_needed_object_under_its_oid_path() {
    let case = Case::new("box_lfs_put_installs");
    let payload = lfs_payload(&case);

    put_all(&case, &payload, &[A, B]);

    assert_eq!(fs::read(object_path(&payload, A)).expect("the object is installed"), A);
    assert_eq!(fs::read(object_path(&payload, B)).expect("the object is installed"), B);
    assert!(stages_left(&payload).is_empty(), "the stage is removed");
}

#[test]
fn a_put_member_whose_hash_differs_from_its_name_installs_nothing() {
    let case = Case::new("box_lfs_put_hash");
    let payload = lfs_payload(&case);
    let liar: &[u8] = b"first objecT";
    let oid = sha256(A);

    let outcome = put_with(&case, &payload, &[], &[A], &archive(&[(&oid, liar)]));

    assert_refused(&payload, &outcome, "put", "a member whose bytes are not its name's");
    assert!(!object_path(&payload, A).exists(), "nothing is installed");
    assert!(stages_left(&payload).is_empty(), "the stage is removed");
}

#[test]
fn a_put_member_whose_size_differs_from_the_manifest_installs_nothing() {
    let case = Case::new("box_lfs_put_size");
    let payload = lfs_payload(&case);
    let manifest = format!("{} {}", sha256(A), A.len() + 1);

    let outcome = lfs_call_with(&case, &stepped(&payload, "put"), &[], &[], (&manifest, &objects_archive(&[A])));

    assert_refused(&payload, &outcome, "put", "a member of another size than the manifest's");
    assert!(!object_path(&payload, A).exists(), "nothing is installed");
}

#[test]
fn a_put_member_the_manifest_does_not_name_is_refused() {
    let case = Case::new("box_lfs_put_unneeded");
    let payload = lfs_payload(&case);

    let outcome = put_with(&case, &payload, &[], &[A], &objects_archive(&[A, B]));

    let failed = assert_refused(&payload, &outcome, "put", "a member the manifest does not name");
    assert!(failed.contains(&sha256(B)), "the line names the member: {outcome:?}");
    assert!(!object_path(&payload, A).exists(), "nothing of the put is installed");
    assert!(!object_path(&payload, B).exists(), "nothing of the put is installed");
}

#[test]
fn two_puts_of_overlapping_sets_both_pass_the_second_keeping_the_shared_object() {
    let case = Case::new("box_lfs_put_overlap");
    let payload = lfs_payload(&case);
    put_all(&case, &payload, &[A]);
    let installed = gate_win32::file_id(&File::open(object_path(&payload, A)).expect("the object is there")).expect("its id can be read");

    put_all(&case, &renamed(&payload, 1), &[A, B]);

    let kept = gate_win32::file_id(&File::open(object_path(&payload, A)).expect("the object is there")).expect("its id can be read");
    assert_eq!(kept, installed, "the shared object is kept, not replaced");
    assert_eq!(fs::read(object_path(&payload, B)).expect("the other object is installed"), B);
}

#[test]
fn an_object_a_run_s_scratch_links_is_held_not_resent() {
    let case = Case::new("box_lfs_linked");
    let payload = lfs_payload(&case);
    put_all(&case, &payload, &[A]);
    // A live run's scratch folder, or one a box killed mid-run left, holds a second name of the object.
    let root = Path::new(field(&payload, "Work"))
        .parent()
        .and_then(Path::parent)
        .expect("the work folder is <root>\\work\\<key>");
    let oid = sha256(A);
    let linked = root.join(r"lfs-run\k-00000000dead\objects").join(&oid[..2]).join(&oid[2..4]);
    fs::create_dir_all(&linked).expect("the scratch folder can be made");
    fs::hard_link(object_path(&payload, A), linked.join(&oid)).expect("the object can be linked");

    let diffed = diff(&case, &renamed(&payload, 1), &[A]);
    assert_eq!(diffed.code, Some(0), "{diffed:?}");
    assert_eq!(diffed.stdout, needs(&[]), "the linked object is not sent again: {diffed:?}");
    put_all(&case, &renamed(&payload, 2), &[A]);

    assert_eq!(fs::read(object_path(&payload, A)).expect("the object is there"), A);
}

#[test]
fn an_unprotected_store_lock_refuses_the_lfs_route() {
    let case = Case::new("box_lfs_lock_foreign");
    let payload = lfs_payload(&case);
    let lock = Path::new(field(&payload, "Bare")).join("lfs.lock");
    fs::write(&lock, "").expect("a lock of the folder's security can be planted");

    let outcome = diff(&case, &payload, &[A]);

    let failed = assert_refused(&payload, &outcome, "diff", "a lock made with another security");
    assert!(failed.contains("its owner, DACL or label is not the box's; remove it"), "{outcome:?}");
    assert!(!failed.contains("Administrators"), "owning it is never enough: {outcome:?}");
    assert!(lock.exists(), "the planted lock is left alone");
}

#[test]
fn a_work_folder_lock_that_cannot_be_opened_is_tagged_lfs() {
    let case = Case::new("box_lfs_lock_unopened");
    let payload = lfs_payload(&case);
    fs::remove_file(field(&payload, "Lock")).expect("the lock the preflight made can be removed");
    fs::create_dir_all(field(&payload, "Lock")).expect("a folder can stand at the lock's path");

    let outcome = diff(&case, &payload, &[A]);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    let said = lines(&outcome.stderr);
    let tag = refused_line(&payload, "lfs");
    let at = said.iter().position(|line| *line == tag).expect("the lfs tag is said");
    assert!(
        said[..at].iter().any(|line| line.contains("could not open the work folder lock")),
        "the lock's own line comes first: {outcome:?}"
    );
    assert!(
        said.get(at + 1).is_some_and(|line| line.starts_with("gate: remote lfs diff failed: ")),
        "the step's failure line follows the tag: {outcome:?}"
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
fn an_object_already_in_the_store_is_kept_not_replaced() {
    let case = Case::new("box_lfs_kept");
    let payload = lfs_payload(&case);
    let (running, mut stdin) = box_call(&case, &["box", "lfs"], &self::payload(&stepped(&payload, "put"))).start_with_stdin();
    stdin
        .write_all(format!("{}\n", manifest_line(&[A])).as_bytes())
        .expect("the manifest can be sent");
    stdin.flush().expect("the manifest is sent");
    // The stage is made once the put has worked out what the store lacks: from then on it waits on its stdin.
    await_stage(&payload);
    put_all(&case, &renamed(&payload, 1), &[A]);
    let installed = gate_win32::file_id(&File::open(object_path(&payload, A)).expect("the object is there")).expect("its id can be read");

    stdin.write_all(&objects_archive(&[A])).expect("the archive can be sent");
    drop(stdin);
    let outcome = running.finish_read();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "gate-remote-lfs: installed 1\n", "{outcome:?}");
    let kept = gate_win32::file_id(&File::open(object_path(&payload, A)).expect("the object is there")).expect("its id can be read");
    assert_eq!(kept, installed, "the object first installed is kept, not replaced");
    assert!(stages_left(&payload).is_empty(), "the staged copy goes with the stage");
}

#[test]
fn a_put_past_the_lfs_budget_is_refused() {
    let case = Case::new("box_lfs_put_budget");
    let payload = lfs_payload(&case);

    let outcome = put_with(&case, &payload, &["--test-lfs-budget-bytes", "4"], &[A], &objects_archive(&[A]));

    assert_refused(&payload, &outcome, "put", "a put past the budget");
    assert!(!object_path(&payload, A).exists(), "nothing is installed");
}

#[test]
fn a_malformed_manifest_entry_is_refused() {
    let case = Case::new("box_lfs_manifest");
    let payload = lfs_payload(&case);
    let oid = sha256(A);
    let rows = [
        ("an oid in capitals", format!("{} 12", oid.to_uppercase()), "which is not an oid"),
        ("a short oid", format!("{} 12", &oid[..63]), "which is not an oid"),
        ("a size that is no number", format!("{oid} twelve"), "which is not a number"),
        ("a signed size", format!("{oid} +12"), "which is not a number"),
        ("an oid named twice", format!("{oid} 12 {oid} 12"), "twice"),
        ("an entry with no size", oid.clone(), "is not a list of '<oid> <size>' entries"),
    ];
    for (what, manifest, said) in rows {
        let outcome = lfs_call_with(&case, &stepped(&payload, "diff"), &[], &[], (&manifest, &[]));

        let failed = assert_refused(&payload, &outcome, "diff", what);
        assert!(failed.contains(said), "{what}: {outcome:?}");
    }
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
fn the_store_folders_are_protected() {
    let case = Case::new("box_lfs_security");
    let payload = lfs_payload(&case);
    put_all(&case, &payload, &[A]);
    let facts = gate_win32::token_facts().expect("this process's token can be read");
    let level = match facts.integrity {
        0x2000 => "ME".to_owned(),
        0x3000 => "HI".to_owned(),
        other => format!("S-1-16-{other}"),
    };
    let object = object_path(&payload, A);
    let bb = object.parent().expect("the object's folder");
    let aa = bb.parent().expect("its folder");
    let objects = aa.parent().expect("the objects folder");

    for folder in [store(&payload).as_path(), objects, aa, bb] {
        let security = security_of(folder);
        let stripped = gate_win32::without_auto_inherit(&security);
        let (dacl, label) = stripped.split_once("S:").expect("the folder has a label");
        assert!(dacl.starts_with("D:P"), "the DACL is protected: {}: {security}", folder.display());
        assert_eq!(
            dacl.matches("(A;").count(),
            3,
            "three grants and no more: {}: {security}",
            folder.display()
        );
        for grant in ["(A;OICI;FA;;;SY)", "(A;OICI;FA;;;BA)", &format!("(A;OICI;FA;;;{})", facts.user)] {
            assert!(dacl.contains(grant), "{grant} in {}: {security}", folder.display());
        }
        assert_eq!(label, format!("(ML;OICI;NW;;;{level})"), "{}: {security}", folder.display());
    }
}

/// The store's `lfs` folder made as the box makes it, with its protected security, beneath the bare repository of
/// `payload`.
fn protected_store(payload: &Value) {
    let facts = gate_win32::token_facts().expect("this process's token can be read");
    let security = gate_win32::protected_descriptor(&facts).expect("the box's security can be made");
    let bare = File::options()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(field(payload, "Bare"))
        .expect("the bare repository can be opened");
    drop(gate_win32::make_folder_beneath(&bare, "lfs", FILE_GENERIC_READ, gate_win32::Share::All, &security).expect("the store can be made"));
}

/// A store folder below `lfs` whose security is not the box's is refused, never moved aside: only `lfs` itself is.
#[test]
fn a_store_folder_whose_security_is_not_the_box_s_is_refused() {
    let case = Case::new("box_lfs_foreign");
    let payload = lfs_payload(&case);
    protected_store(&payload);
    let planted = store(&payload).join(r"objects\aa\planted.txt");
    fs::create_dir_all(planted.parent().expect("a folder")).expect("the planted folder can be made");
    fs::write(&planted, "planted").expect("a file in it can be written");

    let outcome = diff(&case, &payload, &[A]);

    let failed = assert_refused(&payload, &outcome, "diff", "a store folder made with another security");
    let objects = store(&payload).join("objects");
    let refusal = format!("cannot use {}: its owner, DACL or label is not the box's; remove it", objects.display());
    assert!(failed.contains(&refusal), "{outcome:?}");
    assert!(planted.exists(), "nothing in the planted folder is deleted");
    assert!(
        !objects.with_file_name("objects.gate-moved-1").exists(),
        "nothing is moved aside: {outcome:?}"
    );
}

/// A checkout before the box kept the store left git-lfs's own `lfs` folder in the bare repository, made with plain
/// `CreateDirectory` and so with the security its folder hands down: the box moves it aside, never walking it, makes the
/// store new and protected, and says so.
#[test]
fn a_store_folder_git_lfs_made_is_moved_aside_and_made_new() {
    let case = Case::new("box_lfs_leftover");
    let payload = lfs_payload(&case);
    let leftover = store(&payload);
    let left = leftover.join(r"tmp\left.txt");
    fs::create_dir_all(leftover.join("objects")).expect("git-lfs's objects folder can be made");
    fs::create_dir_all(left.parent().expect("a folder")).expect("git-lfs's tmp folder can be made");
    fs::write(&left, "left").expect("a file in it can be written");

    let outcome = diff(&case, &payload, &[A]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, needs(&[A]), "{outcome:?}");
    let moved = leftover.with_file_name("lfs.gate-moved-1");
    let line = format!(
        "gate: remote moved aside the folder {} to {}: its owner, DACL or label was not the box's",
        leftover.display(),
        moved.display()
    );
    assert!(lines(&outcome.stderr).contains(&line.as_str()), "{outcome:?}");
    assert_eq!(
        fs::read_to_string(moved.join(r"tmp\left.txt")).expect("the moved folder keeps its file"),
        "left"
    );
    assert!(!leftover.join("tmp").exists(), "the store is new: {outcome:?}");
    assert!(
        security_of(&leftover).starts_with("D:P"),
        "the new store is protected: {}",
        security_of(&leftover)
    );
    put_all(&case, &payload, &[A]);
    assert_eq!(fs::read(object_path(&payload, A)).expect("the object is installed"), A);
}

#[test]
fn an_unelevated_box_refuses_every_lfs_call() {
    let case = Case::new("box_lfs_unelevated");
    let payload = lfs_payload(&case);
    let check = [("GATE_TEST_BOX_ELEVATION", "check")];

    let diffed = lfs_call_with(&case, &stepped(&payload, "diff"), &[], &check, (&manifest_line(&[A]), &[]));
    let put = lfs_call_with(
        &case,
        &stepped(&payload, "put"),
        &[],
        &check,
        (&manifest_line(&[A]), &objects_archive(&[A])),
    );

    for (step, outcome) in [("diff", &diffed), ("put", &put)] {
        let failed = assert_refused(&payload, outcome, step, step);
        assert!(failed.contains("not elevated"), "{step}: {outcome:?}");
    }
    assert!(!store(&payload).exists(), "no store is made");
}

/// The attributes file that has git-lfs clean every `*.bin` file of a snapshot into a pointer.
const ATTRIBUTES: (&str, &str) = (".gitattributes", "*.bin filter=lfs diff=lfs merge=lfs -text\n");
/// The content of the snapshot's LFS file.
const CONTENT: &str = "the LFS file's content, never pointer text\n";

/// The payload of a run of `out ran` whose snapshot holds `a.bin`, committed through git-lfs's clean as the pointer of
/// [`CONTENT`], beside `files`; no `LfsDigest` yet.
/// The snapshot is pushed with `--no-verify`, as the client pushes one: git-lfs's pre-push hook would send the object
/// to the bare repository's LFS server, which it has none of.
fn lfs_snapshot(case: &Case, files: &[(&str, &str)]) -> Value {
    let mut all = vec![ATTRIBUTES, ("a.bin", CONTENT)];
    all.extend_from_slice(files);
    pushed_snapshot(case, &all, &[])
}

/// The payload of a run of `out ran` whose snapshot holds `filtered`, each added through git's filters as
/// [`lfs_snapshot`] adds its files, and `raw`, each committed as its own bytes whatever its attributes say (`git
/// hash-object --no-filters`), as a file committed without git-lfs is; no `LfsDigest` yet. Pushed as [`lfs_snapshot`]
/// pushes it.
fn pushed_snapshot(case: &Case, filtered: &[(&str, &str)], raw: &[(&str, &str)]) -> Value {
    let mut payload = box_payload(case, "k", &[FAKE_TOOL, "out", "ran"]);
    crate::support::allow_breakaway();
    passed(&gate_box(case, "preflight", &self::payload(&payload), None));
    let source = case.folder().join("source");
    let source_text = source.to_str().expect("the case folder is Unicode").to_owned();
    git(&["init", "-q", &source_text]);
    git(&["-C", &source_text, "config", "core.autocrlf", "false"]);
    for (name, content) in filtered.iter().chain(raw) {
        let file = source.join(name);
        fs::create_dir_all(file.parent().expect("a file has a folder")).expect("the file's folder can be made");
        fs::write(file, content).expect("a snapshot file can be written");
    }
    for (name, _) in filtered {
        git(&["-C", &source_text, "add", "--", name]);
    }
    for (name, _) in raw {
        let blob = git(&["-C", &source_text, "hash-object", "-w", "--no-filters", "--", name]);
        git(&[
            "-C",
            &source_text,
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob},{name}"),
        ]);
    }
    let identity = ["-c", "user.name=gate suite", "-c", "user.email=suite@example.invalid"];
    git(&[&["-C", source_text.as_str()][..], &identity, &["commit", "-q", "-m", "snapshot"]].concat());
    git(&[
        "-C",
        &source_text,
        "push",
        "-q",
        "--force",
        "--no-verify",
        field(&payload, "Bare"),
        "HEAD:refs/gate/k",
    ]);
    payload["Snapshot"] = json!(git(&["-C", &source_text, "rev-parse", "HEAD"]));
    payload
}

/// `payload` carrying the `LfsDigest` of the manifest of `objects`.
fn digested(payload: &Value, objects: &[&[u8]]) -> Value {
    let mut digested = payload.clone();
    digested["LfsDigest"] = json!(sha256(manifest_line(objects).as_bytes()));
    digested
}

/// The path of the `/`-separated `path` in the work folder of `payload`.
fn work_file(payload: &Value, path: &str) -> PathBuf {
    Path::new(field(payload, "Work")).join(path)
}

/// The run scratch folders of `payload`'s box root, which hold nothing once a run has ended.
fn scratch_left(payload: &Value) -> Vec<String> {
    let root = Path::new(field(payload, "Work"))
        .parent()
        .and_then(Path::parent)
        .expect("the work folder is <root>\\work\\<key>");
    let mut left = Vec::new();
    // An elevated box keeps its scratch folders in `lfs-run`, an unelevated one in `lfs-run-medium`.
    for stages in ["lfs-run", "lfs-run-medium"] {
        if let Ok(entries) = fs::read_dir(root.join(stages)) {
            left.extend(entries.map(|entry| entry.expect("an entry").file_name().to_string_lossy().into_owned()));
        }
    }
    left
}

/// Asserts `outcome` is a run of `payload` refused `lfs` before its command: status 75, nothing on stdout, the tag line
/// first; the line after it.
fn assert_refused_run<'a>(payload: &Value, outcome: &'a Outcome, what: &str) -> &'a str {
    assert_eq!(outcome.code, Some(75), "{what}: {outcome:?}");
    assert_eq!(outcome.stdout, "", "{what}: the command never ran: {outcome:?}");
    let said = lines(&outcome.stderr);
    assert_eq!(said.first().copied(), Some(refused_line(payload, "lfs").as_str()), "{what}: {outcome:?}");
    said.get(1).copied().unwrap_or_default()
}

/// Asserts `outcome` is a passed run of `payload` whose work folder holds [`CONTENT`] at `a.bin`, its scratch folder
/// removed.
fn assert_content(payload: &Value, outcome: &Outcome) {
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "ran\n", "{outcome:?}");
    assert_eq!(
        fs::read_to_string(work_file(payload, "a.bin")).expect("the file is checked out"),
        CONTENT,
        "the object's bytes, not pointer text: {outcome:?}"
    );
    assert!(scratch_left(payload).is_empty(), "the scratch folder is removed: {outcome:?}");
}

#[test]
fn a_run_with_pointers_checks_out_their_content_from_the_store() {
    let case = Case::new("box_lfs_run_content");
    let payload = lfs_snapshot(&case, &[]);
    put_all(&case, &payload, &[CONTENT.as_bytes()]);

    let outcome = run_box(&case, &digested(&payload, &[CONTENT.as_bytes()]));

    assert_content(&payload, &outcome);
}

#[test]
fn a_run_whose_store_lacks_an_object_is_refused_lfs_and_runs_nothing() {
    let case = Case::new("box_lfs_run_lacks");
    let payload = lfs_snapshot(&case, &[]);

    let outcome = run_box(&case, &digested(&payload, &[CONTENT.as_bytes()]));

    let line = assert_refused_run(&payload, &outcome, "no object in the store");
    assert!(line.contains("holds no object") && line.contains("a.bin"), "{outcome:?}");
}

#[test]
fn a_run_whose_lfs_digest_differs_from_its_tree_is_refused_lfs() {
    let case = Case::new("box_lfs_run_digest");
    let payload = lfs_snapshot(&case, &[]);
    put_all(&case, &payload, &[CONTENT.as_bytes()]);

    let outcome = run_box(&case, &digested(&payload, &[b"another object"]));

    let line = assert_refused_run(&payload, &outcome, "another digest");
    assert!(line.contains("not the run's"), "{outcome:?}");
}

#[test]
fn a_run_with_pointers_and_no_lfs_digest_fails_its_checkout_without_network() {
    let case = Case::new("box_lfs_run_no_digest");
    let payload = lfs_snapshot(&case, &[(".lfsconfig", TRACKED_SERVER)]);

    let outcome = run_box(&case, &payload);

    assert_checkout_failed(&payload, &outcome);
    assert!(!outcome.stderr.contains(PLANTED_SERVER), "the tracked server is never asked: {outcome:?}");
}

/// The address of the LFS server a snapshot's tracked `.lfsconfig` names in [`TRACKED_SERVER`].
const PLANTED_SERVER: &str = "127.0.0.2:9";
/// A tracked `.lfsconfig` naming the LFS server [`PLANTED_SERVER`].
const TRACKED_SERVER: &str = "[lfs]\n\turl = http://127.0.0.2:9/\n";

/// Asserts `outcome` is a run of `payload` whose checkout failed before its command, `a.bin` left without pointer text.
fn assert_checkout_failed(payload: &Value, outcome: &Outcome) {
    assert_ne!(outcome.code, Some(0), "the run fails: {outcome:?}");
    assert_eq!(outcome.stdout, "", "the command never ran: {outcome:?}");
    assert!(outcome.stderr.contains("checkout failed"), "the checkout fails: {outcome:?}");
    let left = fs::read_to_string(work_file(payload, "a.bin")).unwrap_or_default();
    assert!(
        !left.starts_with("version https://git-lfs.github.com/spec/v1"),
        "no pointer text is left as content: {outcome:?}"
    );
}

/// A tracked `.lfsconfig` that leaves every path out of the smudge does not have a run with no `LfsDigest` write
/// pointer text: its pin (`lfs.fetchexclude=`) wins, and the missing object fails the checkout.
#[test]
fn a_tracked_lfsconfig_fetch_exclude_does_not_leave_pointers_without_a_digest() {
    let case = Case::new("box_lfs_run_fetch_exclude");
    let payload = lfs_snapshot(&case, &[(".lfsconfig", "[lfs]\n\tfetchexclude = *\n")]);

    let outcome = run_box(&case, &payload);

    assert_checkout_failed(&payload, &outcome);
}

/// A tracked `.lfsconfig` that skips download errors does not have a run with no `LfsDigest` write pointer text: its
/// pin (`lfs.skipdownloaderrors=false`) wins, and the missing object fails the checkout.
#[test]
fn a_tracked_lfsconfig_skip_download_errors_does_not_leave_pointers_without_a_digest() {
    let case = Case::new("box_lfs_run_skip_errors");
    let payload = lfs_snapshot(&case, &[(".lfsconfig", "[lfs]\n\tskipdownloaderrors = true\n")]);

    let outcome = run_box(&case, &payload);

    assert_checkout_failed(&payload, &outcome);
}

#[test]
fn a_run_with_an_lfs_digest_and_no_pointers_is_refused_lfs() {
    let case = Case::new("box_lfs_run_no_pointers");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "out", "ran"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let outcome = run_box(&case, &digested(&payload, &[CONTENT.as_bytes()]));

    let line = assert_refused_run(&payload, &outcome, "a digest and no pointer");
    assert!(line.contains("holds no LFS pointer"), "{outcome:?}");
}

#[test]
fn a_tracked_lfsconfig_url_does_not_reach_any_server() {
    let case = Case::new("box_lfs_run_lfsconfig");
    let payload = lfs_snapshot(&case, &[(".lfsconfig", TRACKED_SERVER)]);

    let outcome = run_box(&case, &digested(&payload, &[CONTENT.as_bytes()]));

    let line = assert_refused_run(&payload, &outcome, "a missing object and a tracked server");
    assert!(line.contains("holds no object"), "the cover check's refusal: {outcome:?}");
    assert!(
        !work_file(&payload, "a.bin").exists(),
        "refused before the checkout, so no smudge ran: {outcome:?}"
    );
    assert!(!outcome.stderr.contains(PLANTED_SERVER), "no server is named: {outcome:?}");
}

#[test]
fn skip_smudge_in_the_box_environment_does_not_leave_pointers() {
    let case = Case::new("box_lfs_run_skip_smudge");
    let payload = lfs_snapshot(&case, &[]);
    put_all(&case, &payload, &[CONTENT.as_bytes()]);

    let outcome = run_box_with_env(&case, &digested(&payload, &[CONTENT.as_bytes()]), "GIT_LFS_SKIP_SMUDGE", "1");

    assert_content(&payload, &outcome);
}

#[test]
fn a_pointer_file_an_older_run_left_is_repaired_by_the_post_check() {
    let case = Case::new("box_lfs_run_repair");
    let payload = lfs_snapshot(&case, &[]);
    put_all(&case, &payload, &[CONTENT.as_bytes()]);
    let digest = digested(&payload, &[CONTENT.as_bytes()]);
    assert_content(&payload, &run_box(&case, &digest));
    let pointer = format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize {}\n",
        sha256(CONTENT.as_bytes()),
        CONTENT.len()
    );
    fs::write(work_file(&payload, "a.bin"), pointer).expect("the pointer text can be planted");

    let outcome = run_box(&case, &renamed(&digest, 1));

    assert_content(&payload, &outcome);
}

#[test]
fn a_128_byte_object_s_pointer_left_in_place_is_repaired_by_the_post_check() {
    let case = Case::new("box_lfs_run_repair_128");
    let content = "x".repeat(128);
    let payload = lfs_snapshot(&case, &[("a.bin", &content)]);
    put_all(&case, &payload, &[content.as_bytes()]);
    let digest = digested(&payload, &[content.as_bytes()]);
    let first = run_box(&case, &digest);
    assert_eq!(first.code, Some(0), "{first:?}");
    let pointer = format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 128\n",
        sha256(content.as_bytes())
    );
    assert_eq!(pointer.len(), 128, "the pointer is of its object's size");
    fs::write(work_file(&payload, "a.bin"), pointer).expect("the pointer text can be planted");

    let outcome = run_box(&case, &renamed(&digest, 1));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        fs::read_to_string(work_file(&payload, "a.bin")).expect("the file is checked out"),
        content,
        "the object's bytes, not pointer text: {outcome:?}"
    );
}

#[test]
fn a_work_folder_config_that_skips_the_smudge_still_checks_out_content() {
    let case = Case::new("box_lfs_run_config_skip");
    let payload = lfs_snapshot(&case, &[]);
    put_all(&case, &payload, &[CONTENT.as_bytes()]);
    let config = work_file(&payload, r".git\config");
    let config_text = config.to_str().expect("the case folder is Unicode");
    git(&["config", "-f", config_text, "filter.lfs.smudge", "git-lfs smudge --skip -- %f"]);
    git(&["config", "-f", config_text, "filter.lfs.process", "git-lfs filter-process --skip"]);

    let outcome = run_box(&case, &digested(&payload, &[CONTENT.as_bytes()]));

    assert_content(&payload, &outcome);
}

#[test]
fn a_run_with_no_lfs_digest_syncs_back_a_new_lfs_file_leaving_no_git_lfs_folder() {
    let case = Case::new("box_lfs_run_no_digest_back");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "write", "new.bin", "new content", "out", "ran"]);
    snapshot(&case, &mut payload, &[ATTRIBUTES, ("a.txt", "a\n")]);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "ran\n", "{outcome:?}");
    assert!(has_ref(&payload, "refs/gate-back/k"), "the new file is committed back: {outcome:?}");
    assert!(
        !work_file(&payload, r".git\lfs").exists(),
        "no clean wrote the work folder's .git\\lfs: {outcome:?}"
    );
    assert!(
        scratch_left(&payload).is_empty(),
        "the back commit's scratch folder is removed: {outcome:?}"
    );
}

/// A file committed as its own bytes under a `filter=lfs` rule, as a repo whose template names the filter and whose
/// files were committed without git-lfs holds it, has the PC send no `LfsDigest`: its checkout passes, the file its
/// bytes, and nothing reaches the work folder's `.git\lfs`.
#[test]
fn a_plain_file_under_an_lfs_filter_checks_out_without_a_digest() {
    let case = Case::new("box_lfs_run_plain_under_filter");
    let payload = pushed_snapshot(&case, &[ATTRIBUTES], &[("a.bin", CONTENT)]);

    let outcome = run_box(&case, &payload);

    assert_content(&payload, &outcome);
    assert!(
        !work_file(&payload, r".git\lfs").exists(),
        "no smudge wrote the work folder's .git\\lfs: {outcome:?}"
    );
}

/// An LFS object whose own content is pointer text naming another object is that object's content, not a pointer left
/// in place: its checkout passes.
#[test]
fn an_object_whose_content_is_another_object_s_pointer_text_is_checked_out() {
    let case = Case::new("box_lfs_run_pointer_content");
    let content = format!("version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 12\n", "ab".repeat(32));
    let pointer = format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize {}\n",
        sha256(content.as_bytes()),
        content.len()
    );
    let payload = pushed_snapshot(&case, &[ATTRIBUTES], &[("a.bin", &pointer)]);
    put_all(&case, &payload, &[content.as_bytes()]);

    let outcome = run_box(&case, &digested(&payload, &[content.as_bytes()]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "ran\n", "{outcome:?}");
    assert_eq!(
        fs::read_to_string(work_file(&payload, "a.bin")).expect("the file is checked out"),
        content,
        "the object's bytes: {outcome:?}"
    );
}

/// The back commit's `add -A` takes git-lfs's own filter driver, required, whatever the work folder's config says of
/// it: a driver there that cannot start, not required, still has a new LFS file committed back as its pointer.
#[test]
fn a_work_folder_config_that_breaks_the_clean_still_commits_back_a_pointer() {
    let case = Case::new("box_lfs_run_config_clean");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "write", "new.bin", "new content", "out", "ran"]);
    snapshot(&case, &mut payload, &[ATTRIBUTES, ("a.txt", "a\n")]);
    let config = work_file(&payload, r".git\config");
    let config_text = config.to_str().expect("the case folder is Unicode");
    git(&["config", "-f", config_text, "filter.lfs.process", "gate-no-such-filter"]);
    git(&["config", "-f", config_text, "filter.lfs.clean", "gate-no-such-filter"]);
    git(&["config", "-f", config_text, "filter.lfs.required", "false"]);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let bare = format!("--git-dir={}", field(&payload, "Bare"));
    let committed = git(&[&bare, "cat-file", "-p", "refs/gate-back/k:new.bin"]);
    assert!(
        committed.starts_with("version https://git-lfs.github.com/spec/v1"),
        "the new file is committed back as its pointer: {committed}: {outcome:?}"
    );
}

#[test]
fn a_run_with_pointers_whose_restricted_start_is_refused_fails_the_start() {
    let case = Case::new("box_lfs_run_token");
    let payload = lfs_snapshot(&case, &[]);
    put_all(&case, &payload, &[CONTENT.as_bytes()]);
    let digest = digested(&payload, &[CONTENT.as_bytes()]);

    let outcome = box_call(&case, &["box", "--test-box-token", "refused", "run"], &self::payload(&digest)).finish_read();

    assert_eq!(outcome.code, Some(127), "the start fails: {outcome:?}");
    assert_eq!(outcome.stdout, "", "the command never ran: {outcome:?}");
    let said = lines(&outcome.stderr);
    assert_eq!(said.first().copied(), Some(started_line(&digest).as_str()), "{outcome:?}");
    assert_eq!(
        said.get(1).copied(),
        Some(
            "gate: remote could not start the command with its restricted token, and a run with LFS content never runs with the builder \
             session's own token"
        ),
        "{outcome:?}"
    );
    assert!(
        !outcome.stderr.contains("the command runs with the builder session's own token"),
        "{outcome:?}"
    );
    assert!(scratch_left(&payload).is_empty(), "the scratch folder is removed: {outcome:?}");
}

/// The files below `folder`, at any depth; none when it is not there.
fn files_below(folder: &Path) -> usize {
    fs::read_dir(folder).map_or(0, |entries| {
        entries
            .map(|entry| entry.expect("an entry").path())
            .map(|path| if path.is_dir() { files_below(&path) } else { 1 })
            .sum()
    })
}

/// The files below the store of `payload`'s objects folder.
fn store_objects(payload: &Value) -> usize {
    files_below(&store(payload).join("objects"))
}

#[test]
fn the_back_commit_of_an_untouched_lfs_file_is_empty() {
    let case = Case::new("box_lfs_run_back");
    let payload = lfs_snapshot(&case, &[]);
    put_all(&case, &payload, &[CONTENT.as_bytes()]);
    assert_eq!(store_objects(&payload), 1, "the one object is installed");

    let outcome = run_box(&case, &digested(&payload, &[CONTENT.as_bytes()]));

    assert_content(&payload, &outcome);
    assert!(!has_ref(&payload, "refs/gate-back/k"), "no back commit: {outcome:?}");
    assert_eq!(store_objects(&payload), 1, "the store holds no new object: {outcome:?}");
}

/// The box root's run scratch folders of `payload`, `lfs-run` and `lfs-run-medium`, made here first, so each has this
/// process's security and not the box's: another account's, as the box sees it; their paths.
fn foreign_run_stages(payload: &Value) -> Vec<PathBuf> {
    let root = Path::new(field(payload, "Work"))
        .parent()
        .and_then(Path::parent)
        .expect("the work folder is <root>\\work\\<key>");
    let planted: Vec<PathBuf> = ["lfs-run", "lfs-run-medium"].iter().map(|stages| root.join(stages)).collect();
    for folder in &planted {
        fs::create_dir_all(folder).expect("the planted folder can be made");
    }
    planted
}

/// Asserts each of `planted` is still there and holds nothing: no scratch folder was made in it.
fn assert_untouched(planted: &[PathBuf], outcome: &Outcome) {
    for folder in planted {
        let entries = fs::read_dir(folder).expect("the planted folder is kept").count();
        assert_eq!(entries, 0, "nothing is made in {}: {outcome:?}", folder.display());
    }
}

/// A run whose snapshot names the LFS filter makes its scratch folder even with no `LfsDigest`; a run scratch folder
/// another account made refuses it `lfs`, so the PC runs it, rather than failing it.
#[test]
fn a_foreign_run_scratch_folder_refuses_an_lfs_repo_s_run_lfs() {
    let case = Case::new("box_lfs_run_foreign_scratch");
    let payload = pushed_snapshot(&case, &[ATTRIBUTES], &[("a.bin", CONTENT)]);
    let planted = foreign_run_stages(&payload);

    let outcome = run_box(&case, &payload);

    let line = assert_refused_run(&payload, &outcome, "a foreign run scratch folder");
    assert!(
        line.starts_with("gate: remote cannot make the run's LFS scratch folder: ")
            && line.contains("its owner, DACL or label is not the box's; remove it"),
        "the scratch folder's own line: {outcome:?}"
    );
    assert_untouched(&planted, &outcome);
}

/// A run whose snapshot names no LFS filter makes no scratch folder, so a run scratch folder another account made
/// changes nothing of it.
#[test]
fn a_foreign_run_scratch_folder_leaves_a_repo_without_lfs_routed() {
    let case = Case::new("box_lfs_run_foreign_scratch_plain");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "write", "new.txt", "new", "out", "ran"]);
    snapshot(&case, &mut payload, &[(".gitattributes", "*.txt text\n"), ("a.txt", "a\n")]);
    let planted = foreign_run_stages(&payload);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "ran\n", "{outcome:?}");
    assert!(!outcome.stderr.contains("gate-remote-refused"), "no refusal: {outcome:?}");
    assert!(has_ref(&payload, "refs/gate-back/k"), "the new file is committed back: {outcome:?}");
    assert_untouched(&planted, &outcome);
}

/// The box reads every `.gitattributes` file of the snapshot, at any depth, whether or not the PC's repo tracks it: one
/// in a subfolder naming the LFS filter has the run make its scratch folder, which a foreign one refuses `lfs`.
#[test]
fn a_nested_attributes_file_naming_lfs_makes_the_scratch_folder() {
    let case = Case::new("box_lfs_run_nested_attributes");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "out", "ran"]);
    snapshot(
        &case,
        &mut payload,
        &[("sub/.gitattributes", "*.psd filter=lfs -text\n"), ("a.txt", "a\n")],
    );
    let planted = foreign_run_stages(&payload);

    let outcome = run_box(&case, &payload);

    let line = assert_refused_run(&payload, &outcome, "a nested attributes file and a foreign run scratch folder");
    assert!(line.contains("its owner, DACL or label is not the box's; remove it"), "{outcome:?}");
    assert_untouched(&planted, &outcome);
}

/// A pair run whose sibling alone names the LFS filter makes its scratch folder before the sibling's checkout.
#[test]
fn a_pair_whose_sibling_alone_names_lfs_makes_the_scratch_folder() {
    let case = Case::new("box_lfs_run_pair_sibling");
    let mut payload = pair_payload(&case, &["lib"], &[FAKE_TOOL, "out", "ran"]);
    let sibling: &[(&str, &str)] = &[(".gitattributes", "*.psd filter=lfs -text\n"), ("b.txt", "b\n")];
    pair_snapshot(&case, &mut payload, &[("a.txt", "a\n")], &[("lib", sibling)]);
    let planted = foreign_run_stages(&payload);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "the command never ran: {outcome:?}");
    assert!(
        outcome.stderr.contains(&refused_line(&payload, "lfs")) && outcome.stderr.contains("its owner, DACL or label is not the box's; remove it"),
        "{outcome:?}"
    );
    assert_untouched(&planted, &outcome);
}

/// What a run with no scratch folder says when its back commit's `add -A` fails and the tree's attributes name the LFS
/// filter.
const NO_STORAGE_ADD: &str = "gate: remote sync-back failed: the command added a file under the LFS filter, which a run without LFS content cannot bring back; run it \
     with --local";

/// A run with no scratch folder commits back with an `lfs.storage` that cannot be made: a command that adds a file
/// under the LFS filter fails the sync-back, saying so, and nothing reaches the work folder's `.git\lfs`.
#[test]
fn a_command_adding_an_lfs_file_to_a_repo_without_lfs_fails_its_sync_back() {
    let case = Case::new("box_lfs_run_added_filter");
    let command = [
        FAKE_TOOL,
        "write",
        ".gitattributes",
        "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        "write",
        "new.bin",
        "new content",
        "out",
        "ran",
    ];
    let mut payload = box_payload(&case, "k", &command);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let outcome = run_box(&case, &payload);

    assert_ne!(outcome.code, Some(0), "the sync-back fails: {outcome:?}");
    assert_eq!(
        lines(&outcome.stderr).iter().filter(|said| **said == NO_STORAGE_ADD).count(),
        1,
        "{outcome:?}"
    );
    assert!(!has_ref(&payload, "refs/gate-back/k"), "nothing is committed back: {outcome:?}");
    assert!(
        !work_file(&payload, r".git\lfs").exists(),
        "no clean wrote the work folder's .git\\lfs: {outcome:?}"
    );
    assert!(scratch_left(&payload).is_empty(), "no scratch folder is made: {outcome:?}");
}

/// The back commit's failure names the work tree's attributes wherever the box was started, as sshd starts it in the ssh
/// user's profile: a box whose working directory is a folder outside the work folder still finds the `.gitattributes` the
/// command wrote and says the filter's line, not git's own.
#[test]
fn a_command_s_attributes_file_is_found_by_a_box_started_outside_the_work_folder() {
    let case = Case::new("box_lfs_run_added_filter_elsewhere");
    let command = [
        FAKE_TOOL,
        "write",
        ".gitattributes",
        "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        "write",
        "new.bin",
        "new content",
        "out",
        "ran",
    ];
    let mut payload = box_payload(&case, "k", &command);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    let elsewhere = case.folder().join("profile");
    fs::create_dir_all(&elsewhere).expect("the box's working directory can be made");

    let mut call = box_call(&case, &["box", "run"], &self::payload(&payload));
    call.current_dir(&elsewhere);
    let outcome = call.finish_read();

    assert_ne!(outcome.code, Some(0), "the sync-back fails: {outcome:?}");
    assert_eq!(
        lines(&outcome.stderr).iter().filter(|said| **said == NO_STORAGE_ADD).count(),
        1,
        "the tree's attributes are found, not the box's working directory's: {outcome:?}"
    );
    assert!(!has_ref(&payload, "refs/gate-back/k"), "nothing is committed back: {outcome:?}");
}

/// The fake tool's steps that leave `sub` a nested repository with no commit, which fails the back commit's `add -A`.
const NESTED_REPO: [&str; 9] = [
    "write",
    "sub/.git/HEAD",
    "ref: refs/heads/main\n",
    "write",
    "sub/.git/objects/keep",
    "x",
    "write",
    "sub/.git/refs/keep",
    "x",
];

/// Asserts the run of `payload` that ended as `outcome` failed its sync-back with git's own `add` line, once, and no
/// LFS line, committing nothing back.
fn assert_add_failed_as_git_says(payload: &Value, outcome: &Outcome) {
    assert_ne!(outcome.code, Some(0), "the sync-back fails: {outcome:?}");
    let line = "gate: remote sync-back failed: add failed (status 128)";
    assert_eq!(lines(&outcome.stderr).iter().filter(|said| **said == line).count(), 1, "{outcome:?}");
    assert!(!outcome.stderr.contains("under the LFS filter"), "no LFS line: {outcome:?}");
    assert!(!has_ref(payload, "refs/gate-back/k"), "nothing is committed back: {outcome:?}");
}

/// A run with no scratch folder whose back commit's `add -A` fails for a reason of git's own, a nested repository with
/// no commit the command left, in a tree whose attributes name no LFS filter, says git's own failure, not an LFS one.
#[test]
fn an_add_failure_in_a_tree_without_an_lfs_filter_keeps_git_s_own_line() {
    let case = Case::new("box_lfs_run_add_fails_plain");
    let command = [&[FAKE_TOOL][..], &NESTED_REPO, &["out", "ran"]].concat();
    let mut payload = box_payload(&case, "k", &command);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let outcome = run_box(&case, &payload);

    assert_add_failed_as_git_says(&payload, &outcome);
}

/// The recheck after a failed add passes over ignored folders, whose attributes `add -A` never reads: a third-party
/// `.gitattributes` naming the LFS filter in an ignored folder leaves a non-LFS add failure git's own line.
#[test]
fn an_lfs_attributes_file_in_an_ignored_folder_leaves_an_add_failure_git_s_own_line() {
    let case = Case::new("box_lfs_run_add_fails_ignored_filter");
    let ignored = ["write", "ignored/.gitattributes", "*.bin filter=lfs -text\n"];
    let command = [&[FAKE_TOOL][..], &ignored, &NESTED_REPO, &["out", "ran"]].concat();
    let mut payload = box_payload(&case, "k", &command);
    snapshot(&case, &mut payload, &[(".gitignore", "ignored/\n"), ("a.txt", "a\n")]);

    let outcome = run_box(&case, &payload);

    assert_add_failed_as_git_says(&payload, &outcome);
}

/// A run with no scratch folder whose checkout fails for a reason of git's own, status 128 from a stub, in a tree whose
/// attributes name no LFS filter, says git's own failure, not an LFS one. A stale `index.lock` cannot stand in: the run
/// removes it before its first git step.
#[test]
fn a_checkout_failure_in_a_tree_without_an_lfs_filter_keeps_git_s_own_line() {
    let case = Case::new("box_lfs_run_checkout_fails_plain");
    let mut first = box_payload(&case, "k", &[FAKE_TOOL, "out", "ran"]);
    snapshot(&case, &mut first, &[("a.txt", "a\n")]);
    let ran = run_box(&case, &first);
    assert_eq!(ran.code, Some(0), "{ran:?}");
    let mut second = renamed(&first, 1);
    snapshot(&case, &mut second, &[("b.txt", "b\n")]);
    let stub = case.folder().join("stub");
    stub_git_doing(&stub, &[("checkout", Does::Exit(128))]);

    let outcome = run_box_with_path(&case, &second, Some(&path_for_run(&[stub])));

    assert_ne!(outcome.code, Some(0), "the checkout fails: {outcome:?}");
    assert_eq!(outcome.stdout, "", "the command never ran: {outcome:?}");
    let line = "gate: remote checkout failed (status 128)";
    assert_eq!(lines(&outcome.stderr).iter().filter(|said| **said == line).count(), 1, "{outcome:?}");
    assert!(!outcome.stderr.contains(UNDECLARED), "no LFS line: {outcome:?}");
}

/// A `git grep` of the snapshot's attributes files that fails ends the listing's failure line with git's reason: here
/// the bare repository's copy of the snapshot's `.gitattributes` is corrupt.
#[test]
#[expect(
    clippy::permissions_set_readonly_false,
    reason = "git writes a loose object read-only, and the case rewrites it"
)]
fn a_failed_attributes_listing_says_git_s_reason() {
    let case = Case::new("box_lfs_run_listing_fails");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "out", "ran"]);
    snapshot(&case, &mut payload, &[(".gitattributes", "*.txt text\n"), ("a.txt", "a\n")]);
    let source = case.folder().join("source");
    let blob = git(&[
        "-C",
        source.to_str().expect("the case folder is Unicode"),
        "rev-parse",
        "HEAD:.gitattributes",
    ]);
    let loose = Path::new(field(&payload, "Bare")).join("objects").join(&blob[..2]).join(&blob[2..]);
    let mut permissions = fs::metadata(&loose).expect("the pushed object is loose").permissions();
    permissions.set_readonly(false);
    fs::set_permissions(&loose, permissions).expect("the object can be made writable");
    fs::write(&loose, "not a zlib stream").expect("the object can be corrupted");

    let outcome = run_box(&case, &payload);

    assert_ne!(outcome.code, Some(0), "the listing fails: {outcome:?}");
    assert_eq!(outcome.stdout, "", "the command never ran: {outcome:?}");
    let said = lines(&outcome.stderr);
    let line = said
        .iter()
        .find(|line| line.starts_with("gate: remote lfs listing failed (status "))
        .copied()
        .unwrap_or_default();
    assert!(
        line.split_once("): ").is_some_and(|(_, why)| !why.is_empty()),
        "the listing's failure line ends with git's reason: {outcome:?}"
    );
}

/// The bytes of an object planted in a work folder's `.git\lfs`, as a process that can write the work folder could.
const PLANTED: &[u8] = b"planted bytes, never the run's content";

/// The path of the object [`PLANTED`] in the work folder of `payload`'s `.git\lfs`, at its oid's path.
fn planted_path(payload: &Value) -> PathBuf {
    let oid = sha256(PLANTED);
    work_file(payload, r".git\lfs\objects").join(&oid[..2]).join(&oid[2..4]).join(&oid)
}

/// Plants the object [`PLANTED`] in the work folder of `payload`'s `.git\lfs` ([`planted_path`]); the pointer text
/// naming it.
fn planted_pointer(payload: &Value) -> String {
    let path = planted_path(payload);
    fs::create_dir_all(path.parent().expect("an object has a folder")).expect("the planted object's folder can be made");
    fs::write(&path, PLANTED).expect("the object can be planted");
    format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize {}\n",
        sha256(PLANTED),
        PLANTED.len()
    )
}

/// Asserts the run of `payload` that ended as `outcome` failed before its command, checked out nothing of the planted
/// object at `path`, and left the work folder's `.git\lfs` holding that object alone, unchanged, and no scratch folder.
fn assert_planted_untouched(payload: &Value, path: &str, outcome: &Outcome) {
    assert_ne!(outcome.code, Some(0), "the run fails: {outcome:?}");
    assert_eq!(outcome.stdout, "", "the command never ran: {outcome:?}");
    assert_ne!(
        fs::read(work_file(payload, path)).unwrap_or_default(),
        PLANTED,
        "the planted object is never checked out: {outcome:?}"
    );
    assert_eq!(
        files_below(&work_file(payload, r".git\lfs")),
        1,
        "nothing is added to the work folder's .git\\lfs: {outcome:?}"
    );
    assert_eq!(
        fs::read(planted_path(payload)).expect("the planted object is kept"),
        PLANTED,
        "{outcome:?}"
    );
    assert!(scratch_left(payload).is_empty(), "no scratch folder is left: {outcome:?}");
}

/// What a run with no scratch folder says when its checkout runs the LFS filter for a file its snapshot does not
/// declare.
const UNDECLARED: &str = "gate: remote checkout failed: a file under the LFS filter was found that the run's snapshot does not declare in a \
                          tracked .gitattributes; run it with --local";

/// The box's listing reads an attributes file whatever the case of its name, as NTFS opens `sub\.GitAttributes` as
/// `sub\.gitattributes` once a run has left it on disk: a tracked `sub/.GitAttributes` naming the LFS filter makes the
/// run's scratch folder, so a pointer whose object the store lacks fails the checkout with git's own line, and the object
/// planted in the work folder's `.git\lfs` at its pointer's path is never read.
#[test]
fn a_case_variant_attributes_file_makes_the_scratch_folder_and_never_reads_the_work_folder_s_lfs() {
    let case = Case::new("box_lfs_run_case_variant");
    let attributes = ("sub/.GitAttributes", "*.bin filter=lfs diff=lfs merge=lfs -text\n");
    let first = pushed_snapshot(&case, &[attributes, ("a.txt", "a\n")], &[]);
    let ran = run_box(&case, &first);
    assert_eq!(ran.code, Some(0), "the first run leaves sub/.GitAttributes on disk: {ran:?}");
    let pointer = planted_pointer(&first);
    let second = renamed(&pushed_snapshot(&case, &[], &[("sub/a.bin", &pointer)]), 1);

    let outcome = run_box(&case, &second);

    assert_planted_untouched(&second, "sub/a.bin", &outcome);
    assert!(
        outcome.stderr.contains("gate: remote checkout failed (status "),
        "git's own line, as a run with its scratch folder fails: {outcome:?}"
    );
    assert!(!outcome.stderr.contains(UNDECLARED), "the run had its scratch folder: {outcome:?}");
}

/// The second run of `case`: its first checkout leaves the work folder with a `.gitattributes` naming the LFS filter,
/// which the next snapshot does not track; the second checks out a snapshot holding a pointer whose object the store
/// lacks, the object planted in the work folder's `.git\lfs` at its pointer's path, started in `started` when given as
/// the box's working directory. Its second payload, and what that run did, the planted object asserted untouched.
fn second_run_over_a_leftover(case: &Case, started: Option<&Path>) -> (Value, Outcome) {
    let first = pushed_snapshot(case, &[("a.txt", "a\n")], &[]);
    let ran = run_box(case, &first);
    assert_eq!(ran.code, Some(0), "{ran:?}");
    fs::write(work_file(&first, ".gitattributes"), "*.bin filter=lfs -text\n").expect("a leftover attributes file can be planted");
    let pointer = planted_pointer(&first);
    let second = renamed(&pushed_snapshot(case, &[], &[("a.bin", &pointer)]), 1);

    let mut call = box_call(case, &["box", "run"], &self::payload(&second));
    if let Some(started) = started {
        call.current_dir(started);
    }
    let outcome = call.finish_read();

    assert_planted_untouched(&second, "a.bin", &outcome);
    (second, outcome)
}

/// A run with no scratch folder checks out with an `lfs.storage` that cannot be made: a `.gitattributes` naming the LFS
/// filter that its snapshot does not track, left in the work folder, has git-lfs smudge a pointer, and the checkout fails
/// saying so, the object planted in the work folder's `.git\lfs` at its pointer's path never read. The failed checkout
/// removes the untracked leftover, so the next run checks out and passes.
#[test]
fn a_leftover_attributes_file_naming_lfs_fails_the_checkout_of_a_run_without_a_scratch_folder() {
    let case = Case::new("box_lfs_run_leftover_attributes");
    let (second, outcome) = second_run_over_a_leftover(&case, None);

    assert_eq!(
        lines(&outcome.stderr).iter().filter(|said| **said == UNDECLARED).count(),
        1,
        "{outcome:?}"
    );
    assert!(!work_file(&second, ".gitattributes").exists(), "the leftover is removed: {outcome:?}");
    let next = run_box(&case, &renamed(&second, 2));
    assert_eq!(next.code, Some(0), "the next run checks out and passes: {next:?}");
    assert_eq!(next.stdout, "ran\n", "{next:?}");
}

/// The box's search for a leftover attributes file reads the work folder's tree wherever the box was started, as sshd
/// starts it in the ssh user's profile: a box whose working directory is a folder outside the work folder still finds
/// the tree's leftover `.gitattributes` naming the LFS filter, says so, and removes it.
#[test]
fn a_leftover_attributes_file_is_found_by_a_box_started_outside_the_work_folder() {
    let case = Case::new("box_lfs_run_leftover_attributes_elsewhere");
    let elsewhere = case.folder().join("profile");
    fs::create_dir_all(&elsewhere).expect("the box's working directory can be made");
    let (second, outcome) = second_run_over_a_leftover(&case, Some(&elsewhere));

    assert_eq!(
        lines(&outcome.stderr).iter().filter(|said| **said == UNDECLARED).count(),
        1,
        "the tree's leftover is found, not the box's working directory's: {outcome:?}"
    );
    assert!(!work_file(&second, ".gitattributes").exists(), "the leftover is removed: {outcome:?}");
}

/// A `.gitattributes` naming the LFS filter in the folder the box was started in is not the work tree's: a box started
/// there, whose checkout fails for a reason of git's own in a tree holding no leftover, keeps git's own failure and
/// never says the undeclared-file line.
#[test]
fn a_decoy_attributes_file_where_the_box_started_is_not_taken_for_the_tree_s() {
    let case = Case::new("box_lfs_run_leftover_decoy");
    let mut first = box_payload(&case, "k", &[FAKE_TOOL, "out", "ran"]);
    snapshot(&case, &mut first, &[("a.txt", "a\n")]);
    let ran = run_box(&case, &first);
    assert_eq!(ran.code, Some(0), "{ran:?}");
    let mut second = renamed(&first, 1);
    snapshot(&case, &mut second, &[("b.txt", "b\n")]);
    let stub = case.folder().join("stub");
    stub_git_doing(&stub, &[("checkout", Does::Exit(128))]);
    let decoy = case.folder().join("profile");
    fs::create_dir_all(&decoy).expect("the box's working directory can be made");
    fs::write(decoy.join(".gitattributes"), "*.bin filter=lfs -text\n").expect("a decoy attributes file can be planted");

    let mut call = box_call(&case, &["box", "run"], &self::payload(&second));
    call.current_dir(&decoy);
    call.env("PATH", path_for_run(&[stub]));
    let outcome = call.finish_read();

    assert_ne!(outcome.code, Some(0), "the checkout fails: {outcome:?}");
    assert_eq!(outcome.stdout, "", "the command never ran: {outcome:?}");
    let line = "gate: remote checkout failed (status 128)";
    assert_eq!(
        lines(&outcome.stderr).iter().filter(|said| **said == line).count(),
        1,
        "git's own line: {outcome:?}"
    );
    assert!(!outcome.stderr.contains(UNDECLARED), "the decoy is not the tree's: {outcome:?}");
}

/// A snapshot whose only LFS rule is in a case variant of the attributes file's name, `sub/.GitAttributes`, which NTFS
/// opens as `sub\.gitattributes`, makes the run's scratch folder: a plain file committed under the rule checks out as its
/// bytes, which a run without one, whose `lfs.storage` cannot be made, would fail.
#[test]
fn a_plain_file_under_a_case_variant_attributes_file_checks_out_as_its_bytes() {
    let case = Case::new("box_lfs_run_case_variant_plain");
    let first = pushed_snapshot(&case, &[("sub/.GitAttributes", "*.bin filter=lfs -text\n"), ("a.txt", "a\n")], &[]);
    let ran = run_box(&case, &first);
    assert_eq!(ran.code, Some(0), "the first run leaves sub/.GitAttributes on disk: {ran:?}");
    let second = renamed(&pushed_snapshot(&case, &[], &[("sub/b.bin", CONTENT)]), 1);

    let outcome = run_box(&case, &second);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "ran\n", "{outcome:?}");
    assert_eq!(
        fs::read_to_string(work_file(&second, "sub/b.bin")).expect("the file is checked out"),
        CONTENT,
        "its own bytes: {outcome:?}"
    );
    assert!(scratch_left(&second).is_empty(), "the scratch folder is removed: {outcome:?}");
}

#[test]
fn a_run_with_no_pointers_runs_as_before() {
    let case = Case::new("box_lfs_run_plain");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "out", "ran"]);
    snapshot(&case, &mut payload, &[ATTRIBUTES, ("a.txt", "a\n")]);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "ran\n", "{outcome:?}");
    assert_eq!(outcome.stderr, format!("{}\n", started_line(&payload)), "only the marker: {outcome:?}");
    assert!(!store(&payload).exists(), "no store is touched: {outcome:?}");
    assert!(scratch_left(&payload).is_empty(), "the scratch folder is removed: {outcome:?}");
}
