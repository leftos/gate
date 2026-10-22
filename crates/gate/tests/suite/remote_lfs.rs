//! A routed run's Git LFS content, sent by the client through the stand-in for ssh to the real `gate box lfs` and
//! checked out by the real `gate box run`: the diff and the put before the run call, a store that holds the objects
//! sent no more, and each way the step refuses the route `lfs` and runs the command here.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use gate_router::repo_name;
use regex::Regex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::remote_client::{Tuned, client_case, client_gate, client_gate_tuned, fallbacks, pair_case, sibling_repository, write};
use crate::routing::assert_refused;
use crate::support::{Case, Outcome, calls, events, finish, git, log_texts, shown, ssh_steps};

/// The content of the case's LFS file, `a.bin`, which its snapshot holds as a pointer.
const CONTENT: &str = "the LFS file's content, never pointer text\n";
/// The command that prints the LFS file: on the builder, or here once the route is refused.
const PRINT: [&str; 4] = ["cmd", "/c", "type", "a.bin"];
/// The console line of a run the LFS step kept here.
const FELL_BACK: &str = "gate: remote builder lfs; running here";
/// The log line of a run whose object the builder's store held already.
const PRESENT: &str = "gate: lfs: 1 object(s) present";
/// The variable whose words the stand-in gives the box's `run` and `lfs` before their verb.
const BOX_SEAMS_VARIABLE: &str = "GATE_FAKE_SSH_BOX_SEAMS";
/// The status Windows reports for a process Ctrl+C ended, `STATUS_CONTROL_C_EXIT`, as a signed exit code.
const CONTROL_C_EXIT: i32 = 0xC000_013A_u32.cast_signed();

/// The console line of a put that sent the one object.
fn sent_line() -> Regex {
    Regex::new(r"gate: lfs: 1 object\(s\), \d+\.\d MB in \d+\.\d s").expect("the pattern is valid")
}

/// The case folder's name.
fn leaf(case: &Case) -> String {
    case.folder()
        .file_name()
        .expect("the case folder has a name")
        .to_string_lossy()
        .into_owned()
}

/// Writes the routing file that sends the case's `*tool*` and `cmd *` commands to the builder.
fn routed(case: &Case) {
    let repo = repo_name(&leaf(case));
    let routing = json!({
        "host": "builder",
        "slots": 2,
        "repos": { &repo: ["*tool*", "cmd *"] },
        "outputs": { &repo: [] },
    });
    crate::support::write_remote_json(case, &routing.to_string());
}

/// Has git-lfs keep the case's `*.bin` files, its filter set in the repository alone, and commits `a.bin` holding
/// [`CONTENT`], so the repository's LFS store holds its object.
fn with_lfs_file(case: &Case) {
    git(case, &["lfs", "install", "--local", "--skip-repo"]);
    write(case, ".gitattributes", "*.bin filter=lfs diff=lfs merge=lfs -text\n");
    write(case, "a.bin", CONTENT);
    git(case, &["add", "-A"]);
    git(case, &["commit", "--quiet", "-m", "lfs"]);
}

/// A client case whose repo keeps `a.bin` in Git LFS.
fn lfs_case(name: &str) -> Case {
    let case = client_case(name);
    with_lfs_file(&case);
    routed(&case);
    case
}

/// The lower-case hex SHA-256 of `bytes`.
fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// The path of the case's own LFS store's object of [`CONTENT`].
fn pc_object(case: &Case) -> PathBuf {
    let oid = sha256(CONTENT.as_bytes());
    case.folder()
        .join(".git")
        .join("lfs")
        .join("objects")
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(&oid)
}

/// The text of the case's log `log`.
fn log_of(case: &Case, log: &str) -> String {
    log_texts(&case.folder().join(log))
}

/// Whether `text` holds `line` as a line of its own.
fn has_line(text: &str, line: &str) -> bool {
    text.lines().any(|said| said == line)
}

/// The log line saying why the LFS content was not sent, of the run logging to `log`.
fn not_sent(case: &Case, log: &str) -> String {
    let said = log_of(case, log);
    said.lines()
        .find(|line| line.starts_with("gate: remote lfs not sent: "))
        .map_or_else(|| format!("no line says why: {said}"), str::to_owned)
}

/// Asserts the run logging to `log` fell back `lfs`, its log line holding `why`, and ran its command here.
fn assert_fell_back(case: &Case, outcome: &Outcome, log: &str, why: &str) {
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(FELL_BACK), "{outcome:?}");
    let line = not_sent(case, log);
    assert!(line.contains(why), "{line}");
    assert_eq!(fallbacks(&events(case)), ["lfs"]);
}

/// The request the stand-in of `steps` kept of a `fed-hold` call, decoded.
fn kept_request(steps: &Path) -> Value {
    let input = fs::read_to_string(format!("{}.input", steps.display())).expect("the stand-in kept the call's input");
    let word = input.lines().next().expect("the input has a request line");
    serde_json::from_slice(&STANDARD.decode(word).expect("the request is base64")).expect("the request is JSON")
}

/// The steps of a run of `count` calls whose last, the run call, the stand-in keeps the input of, its go file made
/// first.
fn run_kept(case: &Case, count: usize) -> PathBuf {
    kept_steps(case, count, count - 1)
}

/// The steps of a run of `count` calls whose call at `kept`, counted from 0, the stand-in keeps the input of, its go
/// file made first.
fn kept_steps(case: &Case, count: usize, kept: usize) -> PathBuf {
    let go = case.folder().join("ssh").join("go");
    fs::write(&go, "").expect("the go file can be made");
    let held = format!("fed-hold {}", shown(&go));
    let mut steps = vec!["ok"; count];
    steps[kept] = held.as_str();
    ssh_steps(case, "ssh", &steps)
}

#[test]
fn a_routed_run_brings_its_lfs_content_to_the_command() {
    let case = lfs_case("client-lfs-sent");
    let steps = run_kept(&case, 4);

    let outcome = finish(client_gate(&case, "out.log", &steps, &PRINT));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "lfs", "lfs", "run"], "the diff, then the put");
    assert!(outcome.stdout.contains(CONTENT.trim_end()), "the object's bytes: {outcome:?}");
    assert!(!outcome.stdout.contains("git-lfs.github.com"), "no pointer text: {outcome:?}");
    assert!(sent_line().is_match(&outcome.stderr), "{outcome:?}");
    let digest = sha256(format!("{} {}", sha256(CONTENT.as_bytes()), CONTENT.len()).as_bytes());
    assert_eq!(kept_request(&steps)["LfsDigest"], json!(digest));
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

#[test]
fn a_second_run_sends_no_object_again() {
    let case = lfs_case("client-lfs-present");
    let first = ssh_steps(&case, "first", &["ok"; 4]);
    let sent = finish(client_gate(&case, "first.log", &first, &PRINT));
    assert_eq!(sent.code, Some(0), "{sent:?}");
    let second = ssh_steps(&case, "second", &["ok"; 3]);

    let outcome = finish(client_gate(&case, "second.log", &second, &PRINT));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&second), ["preflight", "lfs", "run"], "the diff alone, no put");
    assert!(has_line(&log_of(&case, "second.log"), PRESENT), "{outcome:?}");
    assert!(!outcome.stderr.contains(PRESENT), "the line is the log's alone: {outcome:?}");
    assert!(!sent_line().is_match(&outcome.stderr), "{outcome:?}");
    assert!(outcome.stdout.contains(CONTENT.trim_end()), "{outcome:?}");
}

#[test]
fn an_object_the_pc_lacks_falls_back_lfs_naming_git_lfs_fetch() {
    let case = lfs_case("client-lfs-pc-lacks");
    fs::remove_file(pc_object(&case)).expect("the object can be removed");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &PRINT));

    assert_eq!(calls(&steps), ["preflight", "lfs"], "the diff alone");
    assert_fell_back(&case, &outcome, "out.log", "is missing; run `git lfs fetch` and route again");
    assert!(not_sent(&case, "out.log").contains("a.bin"), "the line names the path");
}

#[test]
fn an_object_whose_bytes_differ_from_its_pointer_falls_back_lfs() {
    let case = lfs_case("client-lfs-pc-differs");
    let lies = "x".repeat(CONTENT.len());
    fs::write(pc_object(&case), &lies).expect("the object can be rewritten at its size");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &PRINT));

    assert_eq!(calls(&steps), ["preflight", "lfs"], "the diff alone");
    assert_fell_back(&case, &outcome, "out.log", &sha256(lies.as_bytes()));
    assert!(not_sent(&case, "out.log").contains("git lfs fetch"), "the line says to fetch it");
}

#[test]
fn a_set_past_the_lfs_budget_falls_back_lfs() {
    let case = lfs_case("client-lfs-budget");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);
    let tuned = Tuned {
        extra: &["--test-remote-lfs-budget-bytes", "1"],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &PRINT));

    assert_eq!(calls(&steps), ["preflight", "lfs"], "the diff alone");
    let why = format!(
        "the LFS objects the builder needs hold {} bytes, more than one put carries (1 bytes)",
        CONTENT.len()
    );
    assert_fell_back(&case, &outcome, "out.log", &why);
}

#[test]
fn a_put_the_box_refuses_falls_back_lfs() {
    let case = lfs_case("client-lfs-put-refused");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);
    let mut gate = client_gate(&case, "out.log", &steps, &PRINT);
    gate.env(BOX_SEAMS_VARIABLE, "--test-lfs-budget-bytes 4");

    let outcome = finish(gate);

    assert_eq!(calls(&steps), ["preflight", "lfs", "lfs"], "no run call");
    assert_fell_back(&case, &outcome, "out.log", "gate: remote lfs not sent: remote lfs put failed: ");
    let line = not_sent(&case, "out.log");
    assert_eq!(line.matches("gate:").count(), 1, "the box's line is kept without its own prefix: {line}");
}

/// A builder whose run scratch folders another account made refuses the run call `lfs`: the box says it cannot make the
/// run's LFS scratch folder, and the PC logs that as why it runs the command here, not a store that lacks content.
#[test]
fn a_scratch_folder_the_builder_cannot_make_falls_back_lfs_naming_it() {
    let case = lfs_case("client-lfs-no-scratch");
    // Made here, so each has this process's security, not the box's: another account's, as the box sees it.
    for stages in ["lfs-run", "lfs-run-medium"] {
        fs::create_dir_all(case.folder().join("builder").join(stages)).expect("the planted folder can be made");
    }
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &PRINT));

    assert_eq!(calls(&steps), ["preflight", "lfs", "lfs", "run"], "the run call is refused");
    assert_fell_back(&case, &outcome, "out.log", "the builder could not make the run's LFS scratch folder");
    let log = log_of(&case, "out.log");
    assert!(
        log.lines()
            .any(|line| line.starts_with("gate: remote cannot make the run's LFS scratch folder: ")
                && line.contains("its owner, DACL or label is not the box's; remove it")),
        "the box's line: {log}"
    );
}

#[test]
fn a_pair_with_pointers_falls_back_lfs_before_any_call() {
    let (case, main, _, sibling) = pair_case("client-lfs-pair", &[], sibling_repository);
    with_lfs_file(&case);
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran"]));
    fs::remove_dir_all(&sibling).expect("the sibling can be removed");

    assert_eq!(calls(&steps), Vec::<String>::new(), "no call reached the builder");
    assert!(outcome.stdout.contains("ran"), "the command ran here: {outcome:?}");
    let why = format!("{main} holds LFS pointers, and a sibling pair does not send LFS content yet");
    assert_fell_back(&case, &outcome, "out.log", &why);
}

#[test]
fn a_repo_without_pointers_makes_no_lfs_call_and_sends_no_digest() {
    let case = client_case("client-lfs-none");
    write(&case, "a.bin", CONTENT);
    let steps = run_kept(&case, 2);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"]);
    assert!(kept_request(&steps).get("LfsDigest").is_none(), "no LfsDigest is sent");
    assert!(!log_of(&case, "out.log").contains("gate: lfs:"), "no LFS line");
}

/// The pointer text of [`CONTENT`], as git-lfs writes it.
fn content_pointer() -> String {
    format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize {}\n",
        sha256(CONTENT.as_bytes()),
        CONTENT.len()
    )
}

/// Commits the case's `.gitattributes`, which gives no path the LFS filter, and `a.bin` holding `bytes`.
fn with_plain_attributes(case: &Case, bytes: &str) {
    write(case, ".gitattributes", "*.txt text\n");
    write(case, "a.bin", bytes);
    git(case, &["add", "-A"]);
    git(case, &["commit", "--quiet", "-m", "plain"]);
}

#[test]
fn a_core_attributes_file_on_the_pc_changes_nothing_sent() {
    let case = client_case("client-lfs-attributes-file");
    with_plain_attributes(&case, &content_pointer());
    let attributes = case.folder().join(".git").join("pc.attributes");
    fs::write(&attributes, "*.bin filter=lfs diff=lfs merge=lfs -text\n").expect("the attributes file can be written");
    git(&case, &["config", "core.attributesFile", &attributes.display().to_string()]);
    routed(&case);
    let steps = run_kept(&case, 2);

    let outcome = finish(client_gate(&case, "out.log", &steps, &PRINT));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"], "no LFS call");
    assert!(kept_request(&steps).get("LfsDigest").is_none(), "no LfsDigest is sent");
    assert!(
        outcome.stdout.contains("git-lfs.github.com/spec/v1"),
        "the pointer text as committed: {outcome:?}"
    );
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

/// Writes the case's `.git\info\attributes`, holding `text`.
fn info_attributes(case: &Case, text: &str) {
    let info = case.folder().join(".git").join("info");
    fs::create_dir_all(&info).expect("the info folder can be made");
    fs::write(info.join("attributes"), text).expect("info/attributes can be written");
}

#[test]
fn a_non_empty_info_attributes_falls_back_lfs_naming_it() {
    let case = client_case("client-lfs-info-attributes");
    write(&case, ".gitattributes", "*.psd filter=lfs diff=lfs merge=lfs -text\n");
    write(&case, "a.bin", CONTENT);
    git(&case, &["add", "-A"]);
    git(&case, &["commit", "--quiet", "-m", "plain"]);
    info_attributes(&case, "*.bin filter=lfs diff=lfs merge=lfs -text\n");
    routed(&case);
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &PRINT));

    assert_eq!(calls(&steps), Vec::<String>::new(), "no call reached the builder");
    assert!(outcome.stdout.contains(CONTENT.trim_end()), "the command ran here: {outcome:?}");
    assert_fell_back(&case, &outcome, "out.log", r".git\info\attributes sets attributes the builder cannot see");
}

/// A repo whose tracked `.gitattributes` names no LFS filter uses no LFS, so a non-empty `info/attributes` keeps nothing
/// here.
#[test]
fn an_info_attributes_of_a_repo_without_an_lfs_filter_routes() {
    let case = client_case("client-lfs-info-no-filter");
    with_plain_attributes(&case, CONTENT);
    info_attributes(&case, "# nothing is set here\n");
    routed(&case);
    let steps = run_kept(&case, 2);

    let outcome = finish(client_gate(&case, "out.log", &steps, &PRINT));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"], "no LFS call");
    assert!(
        !log_of(&case, "out.log").contains("gate: remote lfs not sent"),
        "no lfs line: {outcome:?}"
    );
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

/// A `PATH` whose first folder holds a stand-in `git-lfs` that says it is 3.0.2, before this process's own folders.
fn old_git_lfs_path(case: &Case) -> std::ffi::OsString {
    let stub = case.folder().join("ssh").join("stub-git-lfs");
    fs::create_dir_all(&stub).expect("the stub's folder can be made");
    fs::write(stub.join("git-lfs.cmd"), "@echo git-lfs/3.0.2 (GitHub; windows amd64; go 1.17.2)\r\n").expect("the stub can be written");
    let own = std::env::var_os("PATH").expect("PATH is set");
    std::env::join_paths(std::iter::once(stub).chain(std::env::split_paths(&own))).expect("the folders join into a PATH")
}

/// Asserts `outcome` fell back `parity` on git-lfs 3.0.2, its line naming the floor and the version found, after the
/// preflight alone of `steps`.
fn assert_old_git_lfs_parity(case: &Case, outcome: &Outcome, steps: &Path) {
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(fallbacks(&events(case)), ["parity"], "{outcome:?}");
    assert!(
        outcome
            .stderr
            .lines()
            .any(|line| line.contains("git-lfs 3.1.3 or later") && line.contains("3.0.2")),
        "the line names the floor and the version found: {outcome:?}"
    );
    assert_eq!(calls(steps), ["preflight"], "the preflight alone reached the builder");
}

/// A manifest declaring a git-lfs older than 3.1.3 is weighed at 3.1.3: a builder whose git-lfs is 3.0.2 lacks it.
#[test]
fn a_declared_git_lfs_below_3_1_3_is_weighed_at_3_1_3() {
    let case = lfs_case("client-lfs-declared-old");
    write(&case, ".gate/toolchain.toml", "[tools]\ngit-lfs = \"3\"\n");
    git(&case, &["add", "-A"]);
    git(&case, &["commit", "--quiet", "-m", "manifest"]);
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);
    let mut gate = client_gate(&case, "out.log", &steps, &PRINT);
    gate.env("PATH", old_git_lfs_path(&case));

    let outcome = finish(gate);

    assert_old_git_lfs_parity(&case, &outcome, &steps);
}

/// A repo whose tracked `.gitattributes` names the LFS filter needs git-lfs 3.1.3 or later of the builder even when its
/// files are committed as their own bytes, so its snapshot holds no pointer: the box's checkout runs git-lfs for them.
/// The preflight asks for it, and a builder whose git-lfs is 3.0.2 falls back `parity` before the run call.
#[test]
fn a_repo_with_an_lfs_filter_and_no_pointers_needs_git_lfs_3_1_3() {
    let case = client_case("client-lfs-filter-no-pointers");
    // No LFS filter driver in the repo, whatever the machine's config says, as in a repo whose files were committed
    // without git-lfs: the snapshot keeps `a.bin` as its own bytes.
    git(&case, &["config", "filter.lfs.process", ""]);
    git(&case, &["config", "filter.lfs.clean", ""]);
    git(&case, &["config", "filter.lfs.required", "false"]);
    write(&case, ".gitattributes", "*.bin filter=lfs diff=lfs merge=lfs -text\n");
    write(&case, "a.bin", CONTENT);
    git(&case, &["add", "--", ".gitattributes"]);
    let blob = git(&case, &["hash-object", "-w", "--no-filters", "--", "a.bin"]);
    git(&case, &["update-index", "--add", "--cacheinfo", &format!("100644,{blob},a.bin")]);
    git(&case, &["commit", "--quiet", "-m", "plain under the lfs filter"]);
    routed(&case);
    let steps = run_kept(&case, 1);
    let mut gate = client_gate(&case, "out.log", &steps, &PRINT);
    gate.env("PATH", old_git_lfs_path(&case));

    let outcome = finish(gate);

    assert_eq!(kept_request(&steps)["Needs"]["git-lfs"], json!("3.1.3"), "the preflight asks for git-lfs");
    assert!(!log_of(&case, "out.log").contains("gate: lfs:"), "no LFS content is sent: {outcome:?}");
    assert!(
        !case.folder().join(".git").join("lfs").exists(),
        "the snapshot cleaned nothing into a pointer: {outcome:?}"
    );
    assert_old_git_lfs_parity(&case, &outcome, &steps);
}

/// A repo whose attributes name no LFS filter asks the builder for no git-lfs, so a builder whose git-lfs is too old to
/// count, as one without it, still runs the command.
#[test]
fn a_repo_without_an_lfs_filter_routes_to_a_builder_without_a_usable_git_lfs() {
    let case = client_case("client-lfs-no-filter-old-git-lfs");
    with_plain_attributes(&case, CONTENT);
    routed(&case);
    let steps = kept_steps(&case, 2, 0);
    let mut gate = client_gate(&case, "out.log", &steps, &PRINT);
    gate.env("PATH", old_git_lfs_path(&case));

    let outcome = finish(gate);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"], "routed: {outcome:?}");
    let preflight = kept_request(&steps);
    assert!(
        preflight["Needs"].get("git-lfs").is_none(),
        "the preflight asks for no git-lfs: {preflight}"
    );
    assert!(outcome.stdout.contains(CONTENT.trim_end()), "the command ran: {outcome:?}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

/// A pair whose sibling alone tracks a `.gitattributes` naming the LFS filter needs git-lfs 3.1.3 or later of the
/// builder, as the box's checkout of the sibling runs git-lfs: a builder whose git-lfs is 3.0.2 falls back `parity`.
#[test]
fn a_pair_whose_sibling_alone_names_the_lfs_filter_needs_git_lfs_3_1_3() {
    let (case, _, _, sibling) = pair_case("client-lfs-pair-sibling-filter", &[], |case, sibling| {
        sibling_repository(case, sibling);
        fs::write(sibling.join(".gitattributes"), "*.psd filter=lfs -text\n").expect("the sibling's attributes can be written");
        git(case, &["-C", &shown(sibling), "add", "--", ".gitattributes"]);
    });
    let steps = kept_steps(&case, 1, 0);
    let mut gate = client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", "ran"]);
    gate.env("PATH", old_git_lfs_path(&case));

    let outcome = finish(gate);
    fs::remove_dir_all(&sibling).expect("the sibling can be removed");

    assert_eq!(kept_request(&steps)["Needs"]["git-lfs"], json!("3.1.3"), "the preflight asks for git-lfs");
    assert_old_git_lfs_parity(&case, &outcome, &steps);
}

#[test]
fn a_ctrl_c_at_the_lfs_phase_ends_the_run_interrupted() {
    let case = lfs_case("client-lfs-interrupted");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);
    let tuned = Tuned {
        extra: &["--test-remote-interrupt-at", "lfs"],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &PRINT));

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert!(!outcome.stderr.contains(FELL_BACK), "{outcome:?}");
    assert!(!outcome.stdout.contains(CONTENT.trim_end()), "the command ran nowhere: {outcome:?}");
    assert_eq!(calls(&steps), ["preflight"], "no LFS call reached the builder");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

#[test]
fn an_lfs_diff_past_its_ceiling_is_watched() {
    let case = lfs_case("client-lfs-diff-ceiling");
    let never = case.folder().join("ssh").join("never");
    let steps = ssh_steps(&case, "ssh", &["ok", &format!("hold {}", shown(&never))]);
    let tuned = Tuned {
        extra: &[
            "--test-remote-watch-call",
            "lfs-diff",
            "--test-stall-ms",
            "600000",
            "--test-remote-timeout-ms",
            "300",
        ],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &PRINT));

    assert!(
        !outcome.stderr.contains("TIMED OUT"),
        "the cut is the reason's, not a watchdog line: {outcome:?}"
    );
    let made = calls(&steps);
    assert!(made == ["preflight", "lfs"] || made == ["preflight"], "{made:?}");
    assert_fell_back(&case, &outcome, "out.log", "the diff passed its transfer ceiling of 1 s");
}

#[test]
fn the_passed_line_splits_out_lfs_time() {
    let case = lfs_case("client-lfs-split");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &PRINT));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    // The case's first run meets a new work folder: its 3600 s ceiling is raised to 5400 s.
    let line = Regex::new(concat!(
        r"(?m)^gate: passed in \d+ s on builder ",
        r"\(load-adjusted \d+ s, ceiling 5400 s; command \d+ s, push \d+ s, lfs \d+ s\)\. ",
        r"Full output: .*out\.log\r?$",
    ))
    .expect("the pattern is valid");
    assert!(line.is_match(&outcome.stdout), "{outcome:?}");
}

#[test]
fn an_lfs_fallback_is_journalled_lfs_and_the_run_released_here() {
    let case = lfs_case("client-lfs-journal");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);
    let tuned = Tuned {
        extra: &["--test-remote-lfs-budget-bytes", "1"],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &PRINT));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_refused(&events(&case), "lfs");
}
