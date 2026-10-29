//! A routed run's ignored inputs, sent by the client through the stand-in for ssh to the real `gate box inputs`: the
//! diff and the put before the run call, an unchanged set sent no more, and each way the step refuses the route
//! `inputs` and runs the command here.

use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use gate_router::repo_name;
use regex::Regex;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::remote_client::{
    CLIENT_IGNORED, HOST, SENT_TEST, Tuned, client_case, client_gate, client_gate_tuned, dotnet_gate, fallbacks, pair_case, sent_build_case,
    sibling_repository, work_folder, write,
};
use crate::support::{Case, Go, Outcome, assert_in_order, beside, calls, events, finish, log_texts, shown, ssh_steps, start, write_remote_json};

/// The case's one input, `/`-separated as its glob matches it, and as the builder's work folder holds it.
const INPUT: &str = "secrets/keys.json";
const INPUT_ON_DISK: &str = r"secrets\keys.json";
/// The command every case runs: on the builder, or here once the route is refused.
const RAN: [&str; 3] = [r".\tool.exe", "out", "ran"];
/// The console line of a run the inputs step kept here.
const FELL_BACK: &str = "gate: remote builder inputs; running here";
/// The log line of a run whose input was the same on the builder.
const UNCHANGED: &str = "gate: inputs: 1 file(s) unchanged";

/// The console line of a put that sent the one input.
fn sent_line() -> Regex {
    Regex::new(r"gate: inputs: 1 file\(s\), \d+\.\d MB \(\d+\.\d MB compressed\) in \d+\.\d s").expect("the pattern is valid")
}

/// Writes the routing file that sends the case's `*tool*` commands to the builder with the input glob `secrets/**`,
/// and its repo's `sibling` when one is given.
fn routed(case: &Case, sibling: Option<&str>) {
    let leaf = case
        .folder()
        .file_name()
        .expect("the case folder has a name")
        .to_string_lossy()
        .into_owned();
    let repo = repo_name(&leaf);
    let mut routing = json!({
        "host": "builder",
        "slots": 2,
        "repos": { &repo: ["*tool*"] },
        "outputs": { &repo: [] },
        "inputs": { &repo: ["secrets/**"] },
    });
    if let Some(sibling) = sibling {
        routing["siblings"] = json!({ &repo: [sibling] });
    }
    write_remote_json(case, &routing.to_string());
}

/// Makes `case` a worktree whose `.gitignore` leaves `secrets/` out and whose ignored input holds `one`.
fn with_input(case: &Case) {
    write(case, ".gitignore", &format!("{CLIENT_IGNORED}secrets/\n"));
    write(case, INPUT, "one");
}

/// A client case whose repo declares the one ignored input [`INPUT`].
pub(super) fn inputs_case(name: &str) -> Case {
    let case = client_case(name);
    with_input(&case);
    routed(&case, None);
    case
}

/// The text of the case's log `log`.
fn log_of(case: &Case, log: &str) -> String {
    log_texts(&case.folder().join(log))
}

/// Whether `text` holds `line` as a line of its own.
fn has_line(text: &str, line: &str) -> bool {
    text.lines().any(|said| said == line)
}

/// The input as the builder's work folder holds it, `None` when it holds none.
fn on_builder(case: &Case) -> Option<String> {
    fs::read_to_string(work_folder(case).join(INPUT_ON_DISK)).ok()
}

/// Asserts the run logging to `log` fell back `inputs` for `why` and ran its command here.
fn assert_fell_back(case: &Case, outcome: &Outcome, log: &str, why: &str) {
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(FELL_BACK), "{outcome:?}");
    assert!(outcome.stdout.contains("ran"), "the command ran here: {outcome:?}");
    let said = log_of(case, log);
    assert!(has_line(&said, &format!("gate: remote inputs not sent: {why}")), "{said}");
    let events = events(case);
    assert_eq!(fallbacks(&events), ["inputs"], "{events:#?}");
}

#[test]
fn an_ignored_input_is_in_the_builder_s_work_folder_when_the_command_runs() {
    let case = inputs_case("client-inputs-sent");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &RAN));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "inputs", "inputs", "run"], "the diff, then the put");
    assert!(sent_line().is_match(&outcome.stderr), "{outcome:?}");
    assert_eq!(on_builder(&case).as_deref(), Some("one"));
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

#[test]
fn an_unchanged_input_is_not_sent_again() {
    let case = inputs_case("client-inputs-unchanged");
    let first = ssh_steps(&case, "first", &["ok"; 4]);
    let sent = finish(client_gate(&case, "first.log", &first, &RAN));
    assert_eq!(sent.code, Some(0), "{sent:?}");
    let second = ssh_steps(&case, "second", &["ok"; 4]);

    let outcome = finish(client_gate(&case, "second.log", &second, &RAN));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&second), ["preflight", "inputs", "run"], "the diff alone, no put");
    assert!(has_line(&log_of(&case, "second.log"), UNCHANGED), "{outcome:?}");
    assert!(!outcome.stderr.contains(UNCHANGED), "the line is the log's alone: {outcome:?}");
    assert!(!sent_line().is_match(&outcome.stderr), "{outcome:?}");
}

/// A put of the inputs says it as it starts, naming the builder, before the end line that counts the same files.
#[test]
fn a_routed_run_that_sends_inputs_says_the_put_as_it_starts_with_its_file_count() {
    let case = inputs_case("client-inputs-says-the-put");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &RAN));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let said = &outcome.stderr;
    let start = said
        .lines()
        .find(|line| line.starts_with(&format!("gate: sending inputs to {HOST}: ")))
        .expect("the put said its start");
    assert_eq!(start, format!("gate: sending inputs to {HOST}: 1 file(s)"));
    let end = said
        .lines()
        .find(|line| line.starts_with("gate: inputs: 1 file(s), "))
        .expect("the put said its end");
    assert_in_order(said, &[start, end]);
    assert_eq!(on_builder(&case).as_deref(), Some("one"));
}

/// A put of the PC's build says it as it starts, in the build's own form and with the count its end line reports.
#[test]
fn a_routed_run_that_sends_the_pc_s_build_says_the_put_as_it_starts() {
    let case = sent_build_case("client-sent-build-says-the-put", None);
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(dotnet_gate(&case, "out.log", &steps, &SENT_TEST));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let said = &outcome.stderr;
    let started = Regex::new(&format!(r"gate: sending the PC's build to {HOST}: (\d+) file\(s\)")).expect("the pattern is valid");
    let start = said.lines().find(|line| started.is_match(line)).expect("the put said its start");
    let count = started.captures(start).expect("the line matched").get(1).expect("the count").as_str();
    let end = said
        .lines()
        .find(|line| line.starts_with(&format!("gate: sent build: {count} file(s), ")))
        .expect("the end line counts the same files");
    assert_in_order(said, &[start, end]);
}

/// The command naming the gitignored probe project, which its repo declares no input glob for.
const PROBE_RAN: [&str; 3] = [r".\tool.exe", "out", ".tmp/probe/Probe.csproj"];

/// A client case whose repo declares no input glob, holding the gitignored probe project `.tmp/probe`: its project
/// and its source, and below it a restore's `obj/x.json` and a build's `bin/y.dll`.
fn probe_case(name: &str) -> Case {
    let case = client_case(name);
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let routing = json!({ "host": "builder", "slots": 2, "repos": { &repo: ["*tool*"] }, "outputs": { &repo: [] } });
    write_remote_json(&case, &routing.to_string());
    for (path, text) in [
        (".tmp/probe/Probe.csproj", "<Project />"),
        (".tmp/probe/Program.cs", "class P {}"),
        (".tmp/probe/obj/x.json", "{}"),
        (".tmp/probe/bin/y.dll", "dll"),
    ] {
        write(&case, path, text);
    }
    case
}

#[test]
fn a_gitignored_project_a_command_names_is_in_the_builder_s_work_folder_when_the_command_runs() {
    let case = probe_case("client-inputs-probe");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &PROBE_RAN));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "inputs", "inputs", "run"], "the diff, then the put");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
    let probe = work_folder(&case).join(".tmp").join("probe");
    assert_eq!(fs::read_to_string(probe.join("Probe.csproj")).ok().as_deref(), Some("<Project />"));
    assert_eq!(fs::read_to_string(probe.join("Program.cs")).ok().as_deref(), Some("class P {}"));
    assert!(!probe.join("obj").join("x.json").exists(), "a restore's output is not sent");
    assert!(!probe.join("bin").join("y.dll").exists(), "a build's output is not sent");
}

#[test]
fn a_command_naming_the_same_gitignored_project_again_sends_nothing() {
    let case = probe_case("client-inputs-probe-again");
    let first = ssh_steps(&case, "first", &["ok"; 4]);
    let sent = finish(client_gate(&case, "first.log", &first, &PROBE_RAN));
    assert_eq!(sent.code, Some(0), "{sent:?}");
    let second = ssh_steps(&case, "second", &["ok"; 4]);

    let outcome = finish(client_gate(&case, "second.log", &second, &PROBE_RAN));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&second), ["preflight", "inputs", "run"], "the diff alone, no put");
    assert!(has_line(&log_of(&case, "second.log"), "gate: inputs: 2 file(s) unchanged"), "{outcome:?}");
}

#[test]
fn a_carried_file_the_pipeline_cannot_send_falls_back_inputs() {
    let case = probe_case("client-inputs-probe-unsent");
    // The inputs' walk enters no folder whose name is shaped like a short alias, so it never finds the file.
    write(&case, ".tmp/pro~1/P.csproj", "<Project />");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", ".tmp/pro~1/P.csproj"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(FELL_BACK), "{outcome:?}");
    assert!(outcome.stdout.contains(".tmp/pro~1/P.csproj"), "the command ran here: {outcome:?}");
    assert_eq!(fallbacks(&events(&case)), ["inputs"]);
    let said = log_of(&case, "out.log");
    assert!(
        has_line(
            &said,
            "gate: remote inputs not sent: .tmp/pro~1/P.csproj was named by the command but could not be sent"
        ),
        "{said}"
    );
}

/// The paths the builder's inputs record of the case's work folder names, sorted.
fn recorded(case: &Case) -> Vec<String> {
    let mut record = work_folder(case).into_os_string();
    record.push(".inputs");
    let text = fs::read_to_string(&record).expect("the builder keeps an inputs record");
    let value: serde_json::Value = serde_json::from_str(&text).expect("the inputs record is JSON");
    let mut paths: Vec<String> = value["files"]
        .as_object()
        .expect("the inputs record names its files")
        .keys()
        .cloned()
        .collect();
    paths.sort();
    paths
}

/// The run's own log, inside the probe folder its command names.
const PROBE_LOG: &str = ".tmp/probe/run.log";

#[test]
fn a_carried_folder_holding_the_run_s_log_routes_and_leaves_the_log_out() {
    let case = probe_case("client-inputs-probe-log");
    write(&case, PROBE_LOG, "an earlier run's log\n");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);
    // A run pinned to its builder readies its log only once routed, so the earlier log is there when the folder is swept.
    let pinned = Tuned {
        extra: &["--builder", "builder"],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, PROBE_LOG, &steps, &pinned, &PROBE_RAN));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        calls(&steps),
        ["preflight", "inputs", "inputs", "run"],
        "the diff, then the put: {outcome:?}"
    );
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
    let said = log_of(&case, PROBE_LOG);
    assert!(!said.contains("gate: remote inputs not sent"), "{said}");
    assert_eq!(
        recorded(&case),
        [".tmp/probe/Probe.csproj", ".tmp/probe/Program.cs"],
        "the log is not sent"
    );
}

/// The SHA-256 of `bytes`, as lowercase hex.
fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// The steps file `name` of the case's stand-in: `before` each answered as its step says, then the run call, whose
/// request the stand-in keeps ([`kept_inputs_digest`]), its go file made first.
fn run_kept(case: &Case, name: &str, before: &[&str]) -> PathBuf {
    let go = case.folder().join("ssh").join("go");
    fs::write(&go, "").expect("the go file can be made");
    let held = format!("fed-hold {}", shown(&go));
    let mut steps = before.to_vec();
    steps.push(&held);
    ssh_steps(case, name, &steps)
}

/// The `InputsDigest` of the request the stand-in of the steps file `steps` kept, `None` when it carries none.
fn kept_inputs_digest(steps: &Path) -> Option<String> {
    let input = fs::read_to_string(beside(steps, ".input")).expect("the stand-in kept the call's input");
    let word = input.lines().next().expect("the input has a request line");
    let request: serde_json::Value = serde_json::from_slice(&STANDARD.decode(word).expect("the request is base64")).expect("the request is JSON");
    request["InputsDigest"].as_str().map(str::to_owned)
}

/// The marker files below `folder`, at any depth, that say a run carried a command's inputs to a work folder.
fn carried_markers(folder: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries {
        let path = entry.expect("an entry can be read").path();
        if path.is_dir() {
            found.extend(carried_markers(&path));
        } else if path.extension().is_some_and(|extension| extension == "inputs-carried") {
            found.push(path);
        }
    }
    found
}

/// Runs the probe case's command carrying its gitignored project, which lands in the builder's work folder.
fn carried_run(case: &Case) {
    let first = ssh_steps(case, "first", &["ok"; 4]);
    let carried = finish(client_gate(case, "first.log", &first, &PROBE_RAN));
    assert_eq!(carried.code, Some(0), "{carried:?}");
    let probe = work_folder(case).join(".tmp").join("probe");
    assert!(probe.join("Probe.csproj").exists(), "the carried run installed the probe: {carried:?}");
}

/// The log line of a run that removed the copies an earlier run carried.
const REMOVED: &str = "gate: inputs: removed the carried copies of an earlier run";

#[test]
fn a_run_with_no_inputs_after_a_carried_run_deletes_the_carried_copies() {
    // The probe's files are written just now, so the inputs hash cache keeps none of them.
    let case = probe_case("client-inputs-probe-stale");
    carried_run(&case);
    let probe = work_folder(&case).join(".tmp").join("probe");
    let second = run_kept(&case, "second", &["ok", "ok"]);

    let plain = finish(client_gate(&case, "second.log", &second, &RAN));

    assert_eq!(plain.code, Some(0), "{plain:?}");
    assert_eq!(
        calls(&second),
        ["preflight", "inputs", "run"],
        "the diff of an empty manifest alone: {plain:?}"
    );
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
    assert!(!probe.join("Probe.csproj").exists(), "the carried project is deleted");
    assert!(!probe.join("Program.cs").exists(), "the carried source is deleted");
    assert_eq!(recorded(&case), Vec::<String>::new());
    assert_eq!(
        kept_inputs_digest(&second),
        Some(sha256_hex(br#"{"files":{}}"#)),
        "the run call carries the empty manifest's digest"
    );
    assert!(has_line(&log_of(&case, "second.log"), REMOVED), "{plain:?}");
    assert_eq!(carried_markers(&case.root()), Vec::<PathBuf>::new(), "the marker is removed");
}

#[test]
fn a_removal_of_carried_copies_the_box_fails_still_routes_and_keeps_the_marker() {
    let case = probe_case("client-inputs-probe-removal-fails");
    carried_run(&case);
    assert_eq!(carried_markers(&case.root()).len(), 1, "the carried run leaves its marker");
    let steps = run_kept(&case, "second", &["ok", "say-exit 2 gate: the inputs request is refused"]);

    let outcome = finish(client_gate(&case, "second.log", &steps, &RAN));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        calls(&steps),
        ["preflight", "inputs", "run"],
        "the failed diff, then the run: {outcome:?}"
    );
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
    let said = log_of(&case, "second.log");
    assert!(
        has_line(&said, "gate: inputs: the carried copies were not removed: the inputs request is refused"),
        "{said}"
    );
    assert_eq!(kept_inputs_digest(&steps), None, "the run call carries no inputs digest");
    assert_eq!(carried_markers(&case.root()).len(), 1, "the marker is kept for a later run");
}

#[test]
fn a_changed_input_is_sent_again() {
    let case = inputs_case("client-inputs-changed");
    let first = ssh_steps(&case, "first", &["ok"; 4]);
    let sent = finish(client_gate(&case, "first.log", &first, &RAN));
    assert_eq!(sent.code, Some(0), "{sent:?}");
    write(&case, INPUT, "two!");
    let second = ssh_steps(&case, "second", &["ok"; 4]);

    let outcome = finish(client_gate(&case, "second.log", &second, &RAN));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&second), ["preflight", "inputs", "inputs", "run"], "the diff, then the put");
    assert!(sent_line().is_match(&outcome.stderr), "{outcome:?}");
    assert_eq!(on_builder(&case).as_deref(), Some("two!"));
}

/// The put is held until the case makes a folder where the input goes, which the box refuses to replace.
#[test]
fn a_put_the_box_refuses_falls_back_inputs() {
    let case = inputs_case("client-inputs-put-refused");
    let go = Go(case.folder().join("ssh").join("go"));
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", &format!("hold {}", shown(go.path())), "ok"]);
    let running = start(client_gate(&case, "out.log", &steps, &RAN));
    running.wait_until("the put call", || calls(&steps).len() == 3);
    fs::create_dir_all(work_folder(&case).join(INPUT_ON_DISK)).expect("a folder can be made where the input goes");
    drop(go);

    let outcome = running.finish();

    assert_eq!(calls(&steps), ["preflight", "inputs", "inputs"], "no run call");
    let said = log_of(&case, "out.log");
    let why = said.lines().find(|line| line.starts_with("gate: remote inputs not sent: "));
    assert!(
        why.is_some_and(|line| line.contains(INPUT)),
        "the log says why the inputs were not sent, the box's line naming the input: {said}"
    );
    assert!(
        why.is_some_and(|line| line.starts_with("gate: remote inputs not sent: remote inputs put failed: ") && line.matches("gate:").count() == 1),
        "the box's line is kept without its own prefix: {said}"
    );
    assert!(outcome.stderr.contains(FELL_BACK), "{outcome:?}");
    assert!(outcome.stdout.contains("ran"), "the command ran here: {outcome:?}");
    assert_eq!(fallbacks(&events(&case)), ["inputs"]);
}

#[test]
fn a_set_past_the_inputs_budget_falls_back_after_the_diff_without_a_put() {
    let case = inputs_case("client-inputs-budget");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);
    let tuned = Tuned {
        extra: &["--test-remote-inputs-budget-bytes", "1"],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &RAN));

    assert_eq!(calls(&steps), ["preflight", "inputs"], "the diff alone");
    let why = "the inputs the builder needs hold 3 bytes, more than one put carries (1 bytes)";
    assert_fell_back(&case, &outcome, "out.log", why);
}

/// The run call is held until the case removes the input the put installed, so the box's record no longer holds the
/// run's digest.
#[test]
fn a_run_the_box_refuses_for_its_inputs_falls_back_inputs() {
    let case = inputs_case("client-inputs-run-refused");
    let go = Go(case.folder().join("ssh").join("go"));
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", "ok", &format!("hold {}", shown(go.path())), "ok"]);
    // The attempt's lines, the cold note of the work folder made now among them, are said again after the command's
    // own: a tail of 40 lines keeps the command's line in sight.
    let tuned = Tuned {
        extra: &["--tail", "40"],
        ..Tuned::default()
    };
    let running = start(client_gate_tuned(&case, "out.log", &steps, &tuned, &RAN));
    running.wait_until("the run call", || calls(&steps).len() == 4);
    fs::remove_file(work_folder(&case).join(INPUT_ON_DISK)).expect("the installed input can be removed");
    drop(go);

    let outcome = running.finish();

    assert_eq!(calls(&steps), ["preflight", "inputs", "inputs", "run"]);
    assert_fell_back(&case, &outcome, "out.log", "the builder's work folder does not hold the inputs sent");
}

#[test]
fn a_pair_with_inputs_falls_back_before_any_inputs_call() {
    let (case, _, name, sibling) = pair_case("client-inputs-pair", &[], sibling_repository);
    with_input(&case);
    routed(&case, Some(&name));
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &RAN));
    remove_sibling(&sibling);

    let made = calls(&steps);
    assert!(!made.iter().any(|verb| verb == "inputs" || verb == "run"), "{made:?}");
    assert_fell_back(&case, &outcome, "out.log", "a sibling pair does not send inputs yet");
}

/// Removes the pair case's sibling folder.
fn remove_sibling(sibling: &Path) {
    fs::remove_dir_all(sibling).expect("the sibling can be removed");
}

/// The status Windows reports for a process Ctrl+C ended, `STATUS_CONTROL_C_EXIT`, as a signed exit code.
const CONTROL_C_EXIT: i32 = 0xC000_013A_u32.cast_signed();

/// The stall window a watched inputs call gets: every other call keeps the run's own.
const WATCHED_MS: &str = "300";

/// The calls the stand-in logged, `expected` or `expected` without its last call, which a call killed early may have
/// been before the stand-in logged it.
fn made_or_cut_early(steps: &Path, expected: &[&str]) -> bool {
    let made = calls(steps);
    made == expected || expected.split_last().is_some_and(|(_, before)| made == before)
}

/// Overwrites the case's input with `bytes` bytes no compressor shrinks, so its archive fills every pipe on the way.
fn incompressible_input(case: &Case, bytes: usize) {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let noise: Vec<u8> = (0..bytes)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect();
    fs::write(case.folder().join(INPUT_ON_DISK), noise).expect("the input can be written");
}

/// The put's stand-in reads its whole input, so the archive has all been sent, and then holds past the stall window
/// before the box installs it without a word.
#[test]
#[ignore = "needs the machine alone: run with -Slot critical and --ignored"]
fn a_put_whose_box_works_silently_after_the_archive_is_sent_still_passes() {
    let case = inputs_case("client-inputs-silent-install");
    let go = Go(case.folder().join("ssh").join("go"));
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", &format!("fed-hold {}", shown(go.path())), "ok"]);
    let tuned = Tuned {
        extra: &["--test-remote-watch-call", "inputs", "--test-stall-ms", "1000"],
        ..Tuned::default()
    };
    let running = start(client_gate_tuned(&case, "out.log", &steps, &tuned, &RAN));
    let fed = PathBuf::from(format!("{}.input", steps.display()));
    running.wait_until("the put's whole input", || calls(&steps).len() == 3 && fed.exists());
    // Three stall windows of a box that says nothing: a put still watched for silence would be cut by now.
    std::thread::sleep(Duration::from_secs(3));
    drop(go);

    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stderr.contains("STALLED"), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "inputs", "inputs", "run"]);
    assert!(sent_line().is_match(&outcome.stderr), "{outcome:?}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

/// The put's stand-in never reads its input, so the feeder stops once the pipes are full.
#[test]
fn a_put_silent_both_ways_while_its_archive_is_fed_falls_back_inputs() {
    let case = inputs_case("client-inputs-put-stalled");
    incompressible_input(&case, 4 * 1024 * 1024);
    let steps = ssh_steps(
        &case,
        "ssh",
        &["ok", "ok", &format!("hold {}", shown(&case.folder().join("ssh").join("never")))],
    );
    let tuned = Tuned {
        extra: &["--test-remote-watch-call", "inputs", "--test-stall-ms", WATCHED_MS],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &RAN));

    assert!(
        !outcome.stderr.contains("STALLED"),
        "the cut is the reason's, not a watchdog line: {outcome:?}"
    );
    assert!(made_or_cut_early(&steps, &["preflight", "inputs", "inputs"]), "{:?}", calls(&steps));
    let why = "the put stalled: no bytes sent or received for 1 s";
    assert_fell_back(&case, &outcome, "out.log", why);
}

#[test]
fn a_diff_past_its_own_ceiling_falls_back_inputs() {
    let case = inputs_case("client-inputs-diff-ceiling");
    let steps = ssh_steps(
        &case,
        "ssh",
        &["ok", &format!("hold {}", shown(&case.folder().join("ssh").join("never")))],
    );
    let tuned = Tuned {
        extra: &[
            "--test-remote-watch-call",
            "inputs-diff",
            "--test-stall-ms",
            "600000",
            "--test-remote-timeout-ms",
            WATCHED_MS,
        ],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &RAN));

    assert!(
        !outcome.stderr.contains("TIMED OUT"),
        "the cut is the reason's, not a watchdog line: {outcome:?}"
    );
    assert!(made_or_cut_early(&steps, &["preflight", "inputs"]), "{:?}", calls(&steps));
    assert_fell_back(&case, &outcome, "out.log", "the diff passed its transfer ceiling of 1 s");
}

#[test]
fn a_diff_the_box_fails_falls_back_inputs_without_a_put() {
    let case = inputs_case("client-inputs-diff-refused");
    let steps = ssh_steps(&case, "ssh", &["ok", "say-exit 2 gate: the inputs request is refused"]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &RAN));

    assert_eq!(calls(&steps), ["preflight", "inputs"], "the diff alone");
    assert_fell_back(&case, &outcome, "out.log", "the inputs request is refused");
}

#[test]
fn a_ctrl_c_during_the_inputs_ends_the_run_with_no_fallback() {
    let case = inputs_case("client-inputs-interrupted");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);
    let tuned = Tuned {
        extra: &["--test-remote-interrupt-at", "inputs"],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &RAN));

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert!(!outcome.stderr.contains(FELL_BACK), "{outcome:?}");
    assert!(!outcome.stdout.contains("ran"), "the command ran nowhere: {outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "inputs"], "nothing after the diff reached the builder");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

#[test]
fn a_pass_that_sent_inputs_names_them_in_its_split() {
    let case = inputs_case("client-inputs-split");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &RAN));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    // The case's first run meets a new work folder: its 3600 s ceiling is raised to 5400 s.
    let line = Regex::new(concat!(
        r"(?m)^gate: passed in \d+ s on builder ",
        r"\(load-adjusted \d+ s, ceiling 5400 s; command \d+ s, push \d+ s, inputs \d+ s\)\. ",
        r"Full output: .*out\.log\r?$",
    ))
    .expect("the pattern is valid");
    assert!(line.is_match(&outcome.stdout), "{outcome:?}");
}

/// The variable whose words the stand-in gives every box call as box seams.
const BOX_SEAMS_VARIABLE: &str = "GATE_FAKE_SSH_BOX_SEAMS";

/// The line a routed run's log gets once a put moved its archive over the data channel.
const CHANNEL_PUT: &str = r"(?m)^gate: data channel: put (\d+) in \d+\.\d s\r?$";

/// The box's line saying why its data channel did not complete, as the run's log holds it.
const NOT_COMPLETED: &str = "gate: data channel not completed: ";

/// An [`inputs_case`] whose builder has the loopback `data` address when `data`.
fn channel_inputs_case(name: &str, data: bool) -> Case {
    let case = client_case(name);
    with_input(&case);
    let leaf = case
        .folder()
        .file_name()
        .expect("the case folder has a name")
        .to_string_lossy()
        .into_owned();
    let repo = repo_name(&leaf);
    let mut routing = json!({
        "host": "builder",
        "slots": 2,
        "repos": { &repo: ["*tool*"] },
        "outputs": { &repo: [] },
        "inputs": { &repo: ["secrets/**"] },
    });
    if data {
        routing["data"] = json!("127.0.0.1");
    }
    write_remote_json(&case, &routing.to_string());
    case
}

/// The routed run of `case` logging to `log` through the steps file `steps`, given the options `extra`: every box call
/// is given a data channel on one free loopback port, with accept and idle windows an hour long, so no case here ends
/// on a box-side window.
fn channel_gate(case: &Case, log: &str, steps: &Path, extra: &[&str]) -> std::process::Command {
    let tuned = Tuned { extra, ..Tuned::default() };
    let mut gate = client_gate_tuned(case, log, steps, &tuned, &RAN);
    let port = crate::remote_client::free_channel_port();
    gate.env(
        BOX_SEAMS_VARIABLE,
        format!("--test-box-channel-ports {port}-{port} --test-box-channel-accept-ms 3600000 --test-box-channel-idle-ms 3600000"),
    );
    gate
}

/// The request the stand-in of the steps file `steps` kept, `None` while it is not there whole. The input it kept is
/// read as bytes: a put over ssh carries its archive after the request and the manifest.
fn kept_request(steps: &Path) -> Option<serde_json::Value> {
    let input = fs::read(beside(steps, ".input")).ok()?;
    let word = input.split(|byte| *byte == b'\n').next()?;
    serde_json::from_slice(&STANDARD.decode(word.trim_ascii()).ok()?).ok()
}

/// The steps file `name` of a put case: the preflight and the diff, then the put, which the stand-in keeps the request
/// of, its go file made first, then the run call.
fn put_kept(case: &Case, name: &str) -> PathBuf {
    let go = case.folder().join("ssh").join("go");
    fs::write(&go, "").expect("the go file can be made");
    ssh_steps(case, name, &["ok", "ok", &format!("fed-hold {}", shown(&go)), "ok"])
}

/// With `data`, the put goes over the data channel: its request declares the archive's length and SHA-256 beside the
/// channel, the log says the bytes put, which are that length, and the input lands on the builder.
#[test]
fn a_put_with_data_goes_over_the_data_channel_and_installs_what_was_sent() {
    let case = channel_inputs_case("client-inputs-channel-put", true);
    let steps = put_kept(&case, "ssh");

    let outcome = finish(channel_gate(&case, "out.log", &steps, &[]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "inputs", "inputs", "run"]);
    assert_eq!(on_builder(&case).as_deref(), Some("one"));
    let log = log_of(&case, "out.log");
    let put = Regex::new(CHANNEL_PUT).expect("the pattern compiles").captures(&log);
    let bytes = put.map(|found| found[1].to_owned());
    let sent = kept_request(&steps).expect("the stand-in kept the put's request");
    assert_eq!(sent["Step"], json!("put"), "{sent}");
    assert_eq!(sent["Channel"]["Address"], json!("127.0.0.1"), "{sent}");
    assert_eq!(
        bytes.as_deref().map(str::parse::<u64>),
        Some(Ok(sent["Channel"]["Length"].as_u64().expect("the put declares its length"))),
        "the log says the length declared: {log}"
    );
    let sha256 = sent["Channel"]["Sha256"].as_str().expect("the put declares its SHA-256");
    assert!(
        sha256.len() == 64 && sha256.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "{sha256}"
    );
    assert!(!log.contains("gate: data channel not used"), "{log}");
    assert!(sent_line().is_match(&outcome.stderr), "{outcome:?}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

/// With no `data`, the put is today's: its request asks for no channel and the log says nothing of one.
#[test]
fn a_put_without_data_asks_for_no_channel_and_logs_none() {
    let case = channel_inputs_case("client-inputs-channel-none", false);
    let steps = put_kept(&case, "ssh");

    let outcome = finish(channel_gate(&case, "out.log", &steps, &[]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "inputs", "inputs", "run"]);
    let sent = kept_request(&steps).expect("the stand-in kept the put's request");
    assert!(sent.get("Channel").is_none(), "{sent}");
    assert!(!log_of(&case, "out.log").contains("gate: data channel"), "{}", log_of(&case, "out.log"));
    assert_eq!(on_builder(&case).as_deref(), Some("one"));
}

/// A put whose PC side takes the fault `fault`: the box refuses its channel, saying why as `box_why` starts; the PC
/// logs its not-used line, saying why as `why` starts, and puts again over ssh, which installs the input once, and the
/// run goes on with no fallback.
fn assert_put_again_over_ssh(name: &str, fault: &str, (why, box_why): (&str, &str)) {
    let case = channel_inputs_case(name, true);
    let steps = ssh_steps(&case, "ssh", &["ok"; 5]);

    let outcome = finish(channel_gate(&case, "out.log", &steps, &["--test-remote-channel-fault", fault]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        calls(&steps),
        ["preflight", "inputs", "inputs", "inputs", "run"],
        "the diff, the channel put, the ssh put"
    );
    let log = log_of(&case, "out.log");
    let not_used = format!("gate: data channel not used: {why}");
    assert!(
        log.lines().any(|line| line.starts_with(&not_used) && line.ends_with("; sent over ssh")),
        "{log}"
    );
    assert!(log.contains(&format!("{NOT_COMPLETED}{box_why}")), "the box refused the channel: {log}");
    assert!(!Regex::new(CHANNEL_PUT).expect("the pattern compiles").is_match(&log), "{log}");
    assert_eq!(on_builder(&case).as_deref(), Some("one"));
    assert!(sent_line().is_match(&outcome.stderr), "{outcome:?}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

#[test]
fn a_box_proof_the_pc_refuses_gets_no_byte_and_the_put_goes_again_over_ssh() {
    assert_put_again_over_ssh(
        "client-inputs-channel-box-proof",
        "box-proof",
        ("the box's proof does not match", "the PC closed the channel before its proof"),
    );
}

#[test]
fn a_truncated_channel_put_installs_nothing_and_the_ssh_put_installs_the_input_once() {
    assert_put_again_over_ssh(
        "client-inputs-channel-truncate",
        "truncate",
        ("the test seam stopped the put after ", "the PC closed the channel after "),
    );
}

#[test]
fn an_altered_channel_put_is_refused_by_its_digest_and_the_ssh_put_installs_the_input_once() {
    assert_put_again_over_ssh(
        "client-inputs-channel-alter",
        "alter",
        ("", "the bytes received do not have the SHA-256 declared"),
    );
}

/// A PC that sent the whole archive and drops the channel before it reads the box's ack still has its put judged by
/// the box's verdict over ssh: the box verified and installed it, so there is no second put.
#[test]
fn a_put_whose_socket_fails_after_the_box_acked_installs_once_and_makes_no_ssh_put() {
    let case = channel_inputs_case("client-inputs-channel-no-ack", true);
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(channel_gate(&case, "out.log", &steps, &["--test-remote-channel-fault", "no-ack"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "inputs", "inputs", "run"], "one put alone");
    let log = log_of(&case, "out.log");
    assert!(!log.contains("gate: data channel not used"), "{log}");
    assert!(!log.contains(NOT_COMPLETED), "{log}");
    assert_eq!(on_builder(&case).as_deref(), Some("one"));
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

/// A channel put whose box never starts sends nothing and hears nothing: its head's feeder closing standard input does
/// not end the stall watch's look, so the stall watch cuts it, long before the transfer ceiling would.
#[test]
fn a_channel_put_that_stalls_is_cut_by_the_stall_watch_not_the_ceiling() {
    let case = channel_inputs_case("client-inputs-channel-stalled", true);
    let steps = ssh_steps(
        &case,
        "ssh",
        &["ok", "ok", &format!("hold {}", shown(&case.folder().join("ssh").join("never")))],
    );

    let outcome = finish(channel_gate(
        &case,
        "out.log",
        &steps,
        &["--test-remote-watch-call", "inputs", "--test-stall-ms", WATCHED_MS],
    ));

    assert!(
        !outcome.stderr.contains("STALLED"),
        "the cut is the reason's, not a watchdog line: {outcome:?}"
    );
    assert!(made_or_cut_early(&steps, &["preflight", "inputs", "inputs"]), "{:?}", calls(&steps));
    assert_fell_back(&case, &outcome, "out.log", "the put stalled: no bytes sent or received for 1 s");
}

/// The nonce and the two proofs of a put's request kept by the steps file `steps`.
fn put_secrets(steps: &Path) -> [String; 3] {
    let sent = kept_request(steps).expect("the stand-in kept the put's request");
    let text = |value: &serde_json::Value| value.as_str().expect("the request carries it").to_owned();
    [
        text(&sent["Nonce"]),
        text(&sent["Channel"]["BoxProof"]),
        text(&sent["Channel"]["PcProof"]),
    ]
}

/// What of the put's request kept by the steps file `steps` carries no secret's hex but only the secrets: the
/// request's base64 word, and for each of its nonce and proofs the 16 characters of the word that encode only its hex
/// digits.
fn put_secret_words(steps: &Path) -> Vec<String> {
    let input = fs::read(beside(steps, ".input")).expect("the stand-in kept the call's input");
    let word = String::from_utf8_lossy(input.split(|byte| *byte == b'\n').next().expect("the input has a request line"))
        .trim()
        .to_owned();
    let json = STANDARD.decode(&word).expect("the request is base64");
    let mut words = vec![word.clone()];
    for secret in put_secrets(steps) {
        let at = json
            .windows(secret.len())
            .position(|window| window == secret.as_bytes())
            .expect("the request carries the secret's hex");
        // The first whole group of three bytes inside the secret's 32 digits: its four characters, and the next 12,
        // encode 12 of those digits alone.
        let start = at.div_ceil(3) * 4;
        words.push(word[start..start + 16].to_owned());
    }
    words
}

/// No proof or nonce a channel put's request carried reaches the run's log, its journal or its console, as hex or
/// inside the request's base64 word.
#[test]
fn a_channel_put_s_proofs_and_nonce_appear_in_no_log_journal_or_console() {
    let case = channel_inputs_case("client-inputs-channel-secret", true);
    let steps = put_kept(&case, "ssh");

    let outcome = finish(channel_gate(&case, "out.log", &steps, &[]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let log = log_of(&case, "out.log");
    assert!(
        Regex::new(CHANNEL_PUT).expect("the pattern compiles").is_match(&log),
        "the put went over the channel: {log}"
    );
    let secrets = put_secrets(&steps);
    let words = put_secret_words(&steps);
    let journal = fs::read_to_string(case.root().join("events.jsonl")).expect("the journal was written");
    for (what, text) in [
        ("log", &log),
        ("journal", &journal),
        ("stdout", &outcome.stdout),
        ("stderr", &outcome.stderr),
    ] {
        for secret in &secrets {
            assert!(!text.to_ascii_lowercase().contains(secret.as_str()), "the {what} holds no proof or nonce");
        }
        for word in &words {
            assert!(!text.contains(word.as_str()), "the {what} holds no part of the request word");
        }
    }
}

/// Each put draws its own nonce and two proofs: two runs' puts ask with six different ones.
#[test]
fn two_puts_carry_different_proofs_and_nonces() {
    let case = channel_inputs_case("client-inputs-channel-proofs", true);
    let first_steps = put_kept(&case, "first");
    let first = finish(channel_gate(&case, "first.log", &first_steps, &[]));
    write(&case, INPUT, "two!");
    let second_steps = put_kept(&case, "second");

    let second = finish(channel_gate(&case, "second.log", &second_steps, &[]));

    assert_eq!(first.code, Some(0), "{first:?}");
    assert_eq!(second.code, Some(0), "{second:?}");
    let all: Vec<String> = put_secrets(&first_steps).into_iter().chain(put_secrets(&second_steps)).collect();
    for (at, secret) in all.iter().enumerate() {
        assert!(
            all[at + 1..].iter().all(|other| other != secret),
            "every nonce and proof differs: {all:?}"
        );
    }
    assert_eq!(on_builder(&case).as_deref(), Some("two!"));
}

/// The ssh put after a refused channel carries a nonce of its own, so its stage is new to the box, and no channel.
#[test]
fn a_fallback_put_uses_a_nonce_new_to_the_stage() {
    let case = channel_inputs_case("client-inputs-channel-fallback-nonce", true);
    let channel_go = Go(case.folder().join("ssh").join("go-channel"));
    let ssh_go = Go(case.folder().join("ssh").join("go-ssh"));
    let channel_held = format!("fed-hold {}", shown(channel_go.path()));
    let ssh_held = format!("fed-hold {}", shown(ssh_go.path()));
    let steps = ssh_steps(&case, "ssh", &["ok", "ok", &channel_held, &ssh_held, "ok"]);
    let running = start(channel_gate(&case, "out.log", &steps, &["--test-remote-channel-fault", "box-proof"]));
    let mut first = None;
    running.wait_until("the channel put's request", || {
        first = kept_request(&steps).filter(|_| calls(&steps).len() == 3);
        first.is_some()
    });
    let first = first.expect("the channel put's request was kept");
    drop(channel_go);
    let mut second = None;
    running.wait_until("the ssh put's request", || {
        second = kept_request(&steps).filter(|sent| calls(&steps).len() == 4 && sent["Nonce"] != first["Nonce"]);
        second.is_some()
    });
    let second = second.expect("the ssh put's request was kept");
    drop(ssh_go);

    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(first.get("Channel").is_some(), "the first put asked for the channel: {first}");
    assert!(second.get("Channel").is_none(), "the ssh put asks for none: {second}");
    assert_ne!(first["Nonce"], second["Nonce"]);
    assert_eq!(calls(&steps), ["preflight", "inputs", "inputs", "inputs", "run"]);
    assert_eq!(on_builder(&case).as_deref(), Some("one"));
}
