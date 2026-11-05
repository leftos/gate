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
use crate::support::{Case, Go, Outcome, assert_in_order, beside, calls, events, finish, git, log_texts, shown, ssh_steps, start, write_remote_json};

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

/// A client case whose repo declares no input glob and carries the folders `.tmp/mocks/*-src`, holding the gitignored
/// mock folders `a-src` and `b-src`, each a page and a model below it.
fn mocks_case(name: &str) -> Case {
    let case = client_case(name);
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let routing = json!({
        "host": "builder",
        "slots": 2,
        "repos": { &repo: ["*tool*"] },
        "outputs": { &repo: [] },
        "carriedFolders": { &repo: [".tmp/mocks/*-src"] },
    });
    write_remote_json(&case, &routing.to_string());
    for folder in ["a-src", "b-src"] {
        write(&case, &format!(".tmp/mocks/{folder}/index.html"), folder);
        write(&case, &format!(".tmp/mocks/{folder}/models/m.glb"), "model");
    }
    case
}

#[test]
fn a_word_naming_a_file_in_a_carried_folder_sends_the_whole_folder_and_a_later_folder_replaces_it() {
    let case = mocks_case("client-inputs-carried-folder");
    let first = ssh_steps(&case, "first", &["ok"; 4]);

    let sent = finish(client_gate(
        &case,
        "first.log",
        &first,
        &[r".\tool.exe", "out", ".tmp/mocks/a-src/index.html"],
    ));

    assert_eq!(sent.code, Some(0), "{sent:?}");
    assert_eq!(
        calls(&first),
        ["preflight", "inputs", "inputs", "run"],
        "the diff, then the put: {sent:?}"
    );
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
    let mocks = work_folder(&case).join(".tmp").join("mocks");
    assert_eq!(recorded(&case), [".tmp/mocks/a-src/index.html", ".tmp/mocks/a-src/models/m.glb"]);
    assert!(
        mocks.join("a-src").join("models").join("m.glb").exists(),
        "the folder's other file is sent: {sent:?}"
    );
    assert!(!mocks.join("b-src").exists(), "a folder no word names is not sent");
    let second = ssh_steps(&case, "second", &["ok"; 4]);

    let replaced = finish(client_gate(
        &case,
        "second.log",
        &second,
        &[r".\tool.exe", "out", ".tmp/mocks/b-src/models/m.glb"],
    ));

    assert_eq!(replaced.code, Some(0), "{replaced:?}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
    assert_eq!(recorded(&case), [".tmp/mocks/b-src/index.html", ".tmp/mocks/b-src/models/m.glb"]);
    assert!(mocks.join("b-src").join("index.html").exists(), "{replaced:?}");
    assert!(!mocks.join("a-src").join("index.html").exists(), "the first folder's copies are removed");
    assert!(
        !mocks.join("a-src").join("models").join("m.glb").exists(),
        "the first folder's copies are removed"
    );
}

/// The file of the submodule case's initialized submodule, `/`-separated below the worktree, and as the builder's work
/// folder holds it.
const SHARED: &str = "tools/shared/shared.txt";
const SHARED_ON_DISK: &str = r"tools\shared\shared.txt";

/// A client case, its repo given no input globs, whose `tools/shared` is an initialized submodule: a repository of its
/// own holding a committed [`SHARED`], declared in the case's `.gitmodules` and added to it as a gitlink.
fn submodule_case(name: &str) -> Case {
    let case = client_case(name);
    write(&case, SHARED, "shared");
    git(&case, &["-C", "tools/shared", "init", "--quiet"]);
    git(&case, &["-C", "tools/shared", "add", "shared.txt"]);
    git(&case, &["-C", "tools/shared", "commit", "--quiet", "-m", "shared"]);
    write(
        &case,
        ".gitmodules",
        "[submodule \"tools/shared\"]\n\tpath = tools/shared\n\turl = ./shared\n",
    );
    git(&case, &["add", "tools/shared", ".gitmodules"]);
    case
}

#[test]
fn a_routed_run_carries_an_initialized_submodule_s_files() {
    let case = submodule_case("client-inputs-submodule");
    let first = ssh_steps(&case, "first", &["ok"; 4]);

    let carried = finish(client_gate(&case, "first.log", &first, &RAN));

    assert_eq!(carried.code, Some(0), "{carried:?}");
    assert_eq!(
        calls(&first),
        ["preflight", "inputs", "inputs", "run"],
        "the diff, then the put: {carried:?}"
    );
    assert!(carried.stdout.contains("ran"), "{carried:?}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
    let on_box = work_folder(&case).join(SHARED_ON_DISK);
    assert_eq!(fs::read_to_string(&on_box).ok().as_deref(), Some("shared"), "{carried:?}");
    let said = log_of(&case, "first.log");
    assert!(has_line(&said, "gate: inputs: submodule tools/shared: 1 file sent as inputs"), "{said}");

    fs::remove_file(case.folder().join(SHARED_ON_DISK)).expect("the submodule's file can be deleted");
    let second = run_kept(&case, "second", &["ok", "ok"]);

    let emptied = finish(client_gate(&case, "second.log", &second, &RAN));

    assert_eq!(emptied.code, Some(0), "{emptied:?}");
    assert_eq!(
        calls(&second),
        ["preflight", "inputs", "run"],
        "the diff of an empty manifest alone: {emptied:?}"
    );
    assert!(!on_box.exists(), "the deleted file is gone on the box: {emptied:?}");
    assert!(has_line(&log_of(&case, "second.log"), REMOVED), "{emptied:?}");
}

#[test]
fn a_routed_run_whose_submodule_files_pass_the_inputs_budget_falls_back_after_the_diff() {
    let case = submodule_case("client-inputs-submodule-budget");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);
    let tuned = Tuned {
        extra: &["--test-remote-inputs-budget-bytes", "1"],
        ..Tuned::default()
    };

    let outcome = finish(client_gate_tuned(&case, "out.log", &steps, &tuned, &RAN));

    assert_eq!(calls(&steps), ["preflight", "inputs"], "the diff alone");
    let why = "the inputs the builder needs hold 6 bytes, more than one put carries (1 bytes)";
    assert_fell_back(&case, &outcome, "out.log", why);
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
    let running = start(client_gate(&case, "out.log", &steps, &RAN));
    running.wait_until("the run call", || calls(&steps).len() == 4);
    fs::remove_file(work_folder(&case).join(INPUT_ON_DISK)).expect("the installed input can be removed");
    drop(go);

    let outcome = running.finish();

    assert_eq!(calls(&steps), ["preflight", "inputs", "inputs", "run"]);
    assert_fell_back(&case, &outcome, "out.log", "the builder's work folder does not hold the inputs sent");
}

/// The sibling's one input, `/`-separated below its own tree, and the bytes it holds.
const SIBLING_INPUT: (&str, &str) = ("secrets/lib.json", "lib input");

/// Writes the routing file of a pair case: its repo's `*tool*` commands to the builder with the sibling `sibling_name`,
/// the main repo's input globs `main` and the sibling repo's `sibling`, each list left out when empty.
fn pair_inputs_routed(case: &Case, leaf: &str, sibling_name: &str, main: &[&str], sibling: &[&str]) {
    let (repo, sibling_repo) = (repo_name(leaf), repo_name(sibling_name));
    let mut inputs = serde_json::Map::new();
    for (name, globs) in [(&repo, main), (&sibling_repo, sibling)] {
        if !globs.is_empty() {
            inputs.insert(name.clone(), json!(globs));
        }
    }
    let routing = json!({
        "host": HOST,
        "slots": 2,
        "repos": { &repo: ["*tool*"] },
        "outputs": { &repo: [] },
        "siblings": { &repo: [sibling_name] },
        "inputs": inputs,
    });
    write_remote_json(case, &routing.to_string());
}

/// A pair case whose main repo holds the ignored input [`INPUT`] and whose sibling holds the ignored input
/// [`SIBLING_INPUT`], its routing file giving the main repo the globs `main` and the sibling's `sibling`: the case, its
/// folder name, the sibling's name and its folder.
fn pair_inputs_case(name: &str, main: &[&str], sibling: &[&str]) -> (Case, String, String, PathBuf) {
    let (case, leaf, sibling_name, sibling_path) = pair_case(name, &[], sibling_repository);
    with_input(&case);
    fs::write(sibling_path.join(".gitignore"), "secrets/\n.tmp/\n").expect("the sibling's .gitignore can be written");
    fs::create_dir_all(sibling_path.join("secrets")).expect("the sibling's secrets folder can be made");
    fs::write(sibling_path.join(SIBLING_INPUT.0.replace('/', "\\")), SIBLING_INPUT.1).expect("the sibling's input can be written");
    pair_inputs_routed(&case, &leaf, &sibling_name, main, sibling);
    (case, leaf, sibling_name, sibling_path)
}

/// The file at the `/`-separated `path` of the tree `tree` in the builder's work folder of a pair run, `None` when it
/// holds none.
fn in_tree(case: &Case, tree: &str, path: &str) -> Option<String> {
    fs::read_to_string(work_folder(case).join(tree).join(path.replace('/', "\\"))).ok()
}

#[test]
fn a_pair_sends_each_repo_s_inputs_into_its_own_tree() {
    let (case, leaf, name, sibling) = pair_inputs_case("client-inputs-pair", &["secrets/**"], &["secrets/**"]);
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &RAN));
    remove_sibling(&sibling);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        calls(&steps),
        ["preflight", "inputs", "inputs", "run"],
        "one diff and one put for both trees"
    );
    assert_eq!(
        in_tree(&case, &leaf, INPUT).as_deref(),
        Some("one"),
        "the main repo's input is in its tree"
    );
    assert_eq!(
        in_tree(&case, &name, SIBLING_INPUT.0).as_deref(),
        Some(SIBLING_INPUT.1),
        "the sibling's in its own"
    );
    assert!(
        !work_folder(&case).join(&leaf).join(&name).exists(),
        "nothing of the sibling lands below the leaf"
    );
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

#[test]
fn a_pair_whose_sibling_alone_declares_inputs_sends_them() {
    let (case, leaf, name, sibling) = pair_inputs_case("client-inputs-pair-sibling", &[], &["secrets/**"]);
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &RAN));
    remove_sibling(&sibling);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "inputs", "inputs", "run"]);
    assert_eq!(in_tree(&case, &name, SIBLING_INPUT.0).as_deref(), Some(SIBLING_INPUT.1));
    assert_eq!(in_tree(&case, &leaf, INPUT), None, "the main repo declares none");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

#[test]
fn an_unchanged_pair_input_is_not_sent_again() {
    let (case, _, _, sibling) = pair_inputs_case("client-inputs-pair-unchanged", &["secrets/**"], &["secrets/**"]);
    let first = ssh_steps(&case, "first", &["ok"; 4]);
    let sent = finish(client_gate(&case, "first.log", &first, &RAN));
    assert_eq!(sent.code, Some(0), "{sent:?}");

    let second = ssh_steps(&case, "second", &["ok"; 4]);
    let outcome = finish(client_gate(&case, "second.log", &second, &RAN));
    remove_sibling(&sibling);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&second), ["preflight", "inputs", "run"], "the diff alone, no put");
    let said = log_of(&case, "second.log");
    assert!(has_line(&said, "gate: inputs: 2 file(s) unchanged"), "{said}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

#[test]
fn a_sibling_s_own_gate_temp_files_are_not_sent() {
    let (case, _, name, sibling) = pair_inputs_case("client-inputs-pair-own-tmp", &[], &[".tmp/**"]);
    let sibling_key = gate_router::work_folder_key(&shown(&sibling).replace('\\', "/"));
    let own = format!(".tmp/{sibling_key}-0a1b2c3d.outputs.tar.zst");
    fs::create_dir_all(sibling.join(".tmp")).expect("the sibling's .tmp can be made");
    fs::write(sibling.join(own.replace('/', "\\")), "the sibling's own archive").expect("the archive can be written");
    fs::write(sibling.join(r".tmp\data.txt"), "data").expect("the data can be written");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &RAN));
    remove_sibling(&sibling);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        in_tree(&case, &name, ".tmp/data.txt").as_deref(),
        Some("data"),
        "the sibling's input is sent"
    );
    assert_eq!(in_tree(&case, &name, &own), None, "its own gate archive is not: {outcome:?}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

#[test]
fn a_pair_whose_inputs_list_is_emptied_sweeps_its_old_copies() {
    let (case, leaf, name, sibling) = pair_inputs_case("client-inputs-pair-emptied", &["secrets/**"], &["secrets/**"]);
    let first = ssh_steps(&case, "first", &["ok"; 4]);
    let sent = finish(client_gate(&case, "first.log", &first, &RAN));
    assert_eq!(sent.code, Some(0), "{sent:?}");
    assert_eq!(in_tree(&case, &name, SIBLING_INPUT.0).as_deref(), Some(SIBLING_INPUT.1));
    pair_inputs_routed(&case, &leaf, &name, &[], &[]);

    let second = ssh_steps(&case, "second", &["ok"; 4]);
    let swept = finish(client_gate(&case, "second.log", &second, &RAN));
    let third = ssh_steps(&case, "third", &["ok"; 4]);
    let after = finish(client_gate(&case, "third.log", &third, &RAN));
    remove_sibling(&sibling);

    assert_eq!(swept.code, Some(0), "{swept:?}");
    assert_eq!(calls(&second), ["preflight", "inputs", "run"], "the empty manifest's diff, no put");
    assert_eq!(in_tree(&case, &leaf, INPUT), None, "the main repo's old copy is swept");
    assert_eq!(in_tree(&case, &name, SIBLING_INPUT.0), None, "and the sibling's");
    let said = log_of(&case, "second.log");
    assert!(has_line(&said, "gate: inputs: removed the old copies of every tree"), "{said}");
    assert!(!said.contains("0 file(s) unchanged"), "the sweep is not said unchanged: {said}");
    assert_eq!(after.code, Some(0), "{after:?}");
    assert_eq!(calls(&third), ["preflight", "run"], "once swept, no inputs call");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

/// Sets the file at `path` last written an hour from now, so no build's hash cache keeps its hash: it never settles.
fn unsettled(path: &Path) {
    let file = fs::File::options().write(true).open(path).expect("the file can be opened");
    file.set_modified(std::time::SystemTime::now() + Duration::from_hours(1))
        .expect("its last write can be set");
}

#[test]
fn a_pair_whose_inputs_never_settled_still_sweeps_them_once_its_list_is_emptied() {
    let (case, leaf, name, sibling) = pair_inputs_case("client-inputs-pair-unsettled", &["secrets/**"], &["secrets/**"]);
    unsettled(&case.folder().join(INPUT_ON_DISK));
    unsettled(&sibling.join(SIBLING_INPUT.0.replace('/', "\\")));
    let first = ssh_steps(&case, "first", &["ok"; 4]);
    let sent = finish(client_gate(&case, "first.log", &first, &RAN));
    assert_eq!(sent.code, Some(0), "{sent:?}");
    assert_eq!(in_tree(&case, &name, SIBLING_INPUT.0).as_deref(), Some(SIBLING_INPUT.1));
    pair_inputs_routed(&case, &leaf, &name, &[], &[]);

    let second = ssh_steps(&case, "second", &["ok"; 4]);
    let swept = finish(client_gate(&case, "second.log", &second, &RAN));
    remove_sibling(&sibling);

    assert_eq!(swept.code, Some(0), "{swept:?}");
    assert_eq!(calls(&second), ["preflight", "inputs", "run"], "the empty manifest's diff, no put");
    let said = log_of(&case, "second.log");
    assert!(has_line(&said, "gate: inputs: removed the old copies of every tree"), "{said}");
    assert_eq!(in_tree(&case, &leaf, INPUT), None, "the main repo's copy is swept");
    assert_eq!(in_tree(&case, &name, SIBLING_INPUT.0), None, "and the sibling's");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

/// The diff of the sweep is refused by the stand-in, as a box refusing the request would: the run goes on without it,
/// and the next run sends the sweep again.
#[test]
fn a_pair_sweep_the_box_refuses_still_runs_the_command() {
    let (case, leaf, name, sibling) = pair_inputs_case("client-inputs-pair-sweep-refused", &["secrets/**"], &["secrets/**"]);
    let first = ssh_steps(&case, "first", &["ok"; 4]);
    let sent = finish(client_gate(&case, "first.log", &first, &RAN));
    assert_eq!(sent.code, Some(0), "{sent:?}");
    pair_inputs_routed(&case, &leaf, &name, &[], &[]);

    let second = run_kept(&case, "second", &["ok", "say-exit 2 gate: the inputs request is refused"]);
    let refused = finish(client_gate(&case, "second.log", &second, &RAN));
    let third = ssh_steps(&case, "third", &["ok"; 4]);
    let again = finish(client_gate(&case, "third.log", &third, &RAN));
    remove_sibling(&sibling);

    assert_eq!(refused.code, Some(0), "{refused:?}");
    assert_eq!(calls(&second), ["preflight", "inputs", "run"], "the refused sweep, then the run");
    let said = log_of(&case, "second.log");
    assert!(
        has_line(&said, "gate: inputs: the old copies were not removed: the inputs request is refused"),
        "{said}"
    );
    assert_eq!(kept_inputs_digest(&second), None, "the run call carries no inputs digest");
    assert_eq!(again.code, Some(0), "{again:?}");
    assert_eq!(calls(&third), ["preflight", "inputs", "run"], "the next run sends the sweep again");
    assert!(
        has_line(&log_of(&case, "third.log"), "gate: inputs: removed the old copies of every tree"),
        "{again:?}"
    );
    assert_eq!(in_tree(&case, &leaf, INPUT), None, "the main repo's old copy is swept");
    assert_eq!(in_tree(&case, &name, SIBLING_INPUT.0), None, "and the sibling's");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

/// Sets the file at `path` last written an hour ago, so a build's hash cache keeps its hash.
fn settled(path: &Path) {
    let file = fs::File::options().write(true).open(path).expect("the file can be opened");
    let before = std::time::SystemTime::now()
        .checked_sub(Duration::from_hours(1))
        .expect("the clock is past an hour");
    file.set_modified(before).expect("its last write can be set");
}

#[test]
fn a_single_repo_cache_of_the_same_key_starts_no_pair_sweep() {
    let (case, leaf, name, sibling) = pair_inputs_case("client-inputs-pair-single-cache", &["secrets/**"], &[]);
    settled(&case.folder().join(INPUT_ON_DISK));
    let declared = case.folder().join(r".gate\siblings.toml");
    let siblings_file = fs::read_to_string(&declared).expect("the siblings file can be read");
    fs::remove_file(&declared).expect("the siblings file can be removed");
    let single = ssh_steps(&case, "single", &["ok"; 4]);
    let alone = finish(client_gate(&case, "single.log", &single, &RAN));
    assert_eq!(alone.code, Some(0), "{alone:?}");
    assert_eq!(on_builder(&case).as_deref(), Some("one"), "one repo's input is in the work folder itself");

    fs::write(&declared, &siblings_file).expect("the siblings file can be written");
    pair_inputs_routed(&case, &leaf, &name, &[], &[]);
    let paired = ssh_steps(&case, "paired", &["ok"; 4]);
    let pair = finish(client_gate(&case, "paired.log", &paired, &RAN));
    remove_sibling(&sibling);

    assert_eq!(pair.code, Some(0), "{pair:?}");
    assert_eq!(calls(&paired), ["preflight", "run"], "no inputs call: {pair:?}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

/// The put installs on the box, but the stand-in passes none of its output on and ends as a dropped connection does, so
/// the PC never sees it land: the copies are on the box, and the next run with the pair's lists emptied still sweeps
/// them.
#[test]
fn a_pair_put_whose_landing_is_lost_still_sweeps_once_its_list_is_emptied() {
    let (case, leaf, name, sibling) = pair_inputs_case("client-inputs-pair-landing-lost", &["secrets/**"], &["secrets/**"]);
    let first = ssh_steps(&case, "first", &["ok", "ok", "truncate 0", "exit 1", "exit 1", "exit 1"]);
    let lost = finish(client_gate(&case, "first.log", &first, &RAN));
    assert_eq!(fallbacks(&events(&case)), ["inputs"], "{lost:?}");
    assert_eq!(
        in_tree(&case, &name, SIBLING_INPUT.0).as_deref(),
        Some(SIBLING_INPUT.1),
        "the box installed the put the PC lost: {:?}",
        calls(&first)
    );
    pair_inputs_routed(&case, &leaf, &name, &[], &[]);

    let second = ssh_steps(&case, "second", &["ok"; 4]);
    let swept = finish(client_gate(&case, "second.log", &second, &RAN));
    remove_sibling(&sibling);

    assert_eq!(swept.code, Some(0), "{swept:?}");
    assert_eq!(calls(&second), ["preflight", "inputs", "run"], "the sweep's diff, then the run");
    assert!(
        has_line(&log_of(&case, "second.log"), "gate: inputs: removed the old copies of every tree"),
        "{swept:?}"
    );
    assert_eq!(in_tree(&case, &leaf, INPUT), None, "the main repo's copy is swept");
    assert_eq!(in_tree(&case, &name, SIBLING_INPUT.0), None, "and the sibling's");
    assert_eq!(fallbacks(&events(&case)), ["inputs"], "the second run does not fall back");
}

#[test]
fn a_pair_whose_globs_match_nothing_clears_its_marker() {
    let (case, leaf, name, sibling) = pair_inputs_case("client-inputs-pair-matches-nothing", &["secrets/**"], &["secrets/**"]);
    let first = ssh_steps(&case, "first", &["ok"; 4]);
    let sent = finish(client_gate(&case, "first.log", &first, &RAN));
    assert_eq!(sent.code, Some(0), "{sent:?}");
    pair_inputs_routed(&case, &leaf, &name, &["nothing/**"], &["nothing/**"]);

    let second = ssh_steps(&case, "second", &["ok"; 4]);
    let empty = finish(client_gate(&case, "second.log", &second, &RAN));
    pair_inputs_routed(&case, &leaf, &name, &[], &[]);
    let third = ssh_steps(&case, "third", &["ok"; 4]);
    let after = finish(client_gate(&case, "third.log", &third, &RAN));
    remove_sibling(&sibling);

    assert_eq!(empty.code, Some(0), "{empty:?}");
    assert_eq!(calls(&second), ["preflight", "inputs", "run"], "the empty manifest's diff, no put");
    assert_eq!(in_tree(&case, &leaf, INPUT), None, "the main repo's old copy is gone");
    assert_eq!(in_tree(&case, &name, SIBLING_INPUT.0), None, "and the sibling's");
    assert_eq!(after.code, Some(0), "{after:?}");
    assert_eq!(calls(&third), ["preflight", "run"], "nothing is left to sweep: {after:?}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

/// The files below `folder`, at any depth, whose names end `.inputs-hashes`: the inputs hash caches.
fn inputs_hash_caches(folder: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries {
        let path = entry.expect("an entry can be read").path();
        if path.is_dir() {
            found.extend(inputs_hash_caches(&path));
        } else if path.extension().is_some_and(|extension| extension == "inputs-hashes") {
            found.push(path);
        }
    }
    found
}

#[test]
fn a_landed_sweep_removes_the_cache_of_a_sibling_no_longer_in_the_pair() {
    let (case, leaf, name, sibling) = pair_inputs_case("client-inputs-pair-gone-sibling", &["secrets/**"], &["secrets/**"]);
    // Settled, so each tree's build keeps a hash in its cache and leaves the cache file.
    settled(&case.folder().join(INPUT_ON_DISK));
    settled(&sibling.join(SIBLING_INPUT.0.replace('/', "\\")));
    let first = ssh_steps(&case, "first", &["ok"; 4]);
    let sent = finish(client_gate(&case, "first.log", &first, &RAN));
    assert_eq!(sent.code, Some(0), "{sent:?}");
    let own = format!(".{name}.inputs-hashes");
    let sibling_cache = inputs_hash_caches(&case.root()).into_iter().find_map(|path| {
        let file = path.file_name()?.to_string_lossy().into_owned();
        file.strip_suffix(&own)
            .map(|key| path.with_file_name(format!("{key}.gone.inputs-hashes")))
    });
    let gone = sibling_cache.expect("the pair run left the sibling's cache");
    fs::write(&gone, "").expect("a cache of a sibling no longer in the pair can be written");
    pair_inputs_routed(&case, &leaf, &name, &[], &[]);

    let second = ssh_steps(&case, "second", &["ok"; 4]);
    let swept = finish(client_gate(&case, "second.log", &second, &RAN));
    remove_sibling(&sibling);

    assert_eq!(swept.code, Some(0), "{swept:?}");
    assert!(
        has_line(&log_of(&case, "second.log"), "gate: inputs: removed the old copies of every tree"),
        "{swept:?}"
    );
    assert!(!gone.exists(), "the cache of the sibling no longer in the pair is removed");
    assert_eq!(
        inputs_hash_caches(&case.root()),
        Vec::<PathBuf>::new(),
        "no inputs hash cache of the key is left"
    );
}

#[test]
fn a_run_switching_between_one_repo_and_a_pair_ends_with_each_input_in_its_tree() {
    let (case, leaf, name, sibling) = pair_inputs_case("client-inputs-pair-switch", &["secrets/**"], &["secrets/**"]);
    let declared = case.folder().join(r".gate\siblings.toml");
    let siblings_file = fs::read_to_string(&declared).expect("the siblings file can be read");
    fs::remove_file(&declared).expect("the siblings file can be removed");
    let single = ssh_steps(&case, "single", &["ok"; 4]);
    let alone = finish(client_gate(&case, "single.log", &single, &RAN));
    assert_eq!(alone.code, Some(0), "{alone:?}");
    assert_eq!(on_builder(&case).as_deref(), Some("one"), "one repo's input is in the work folder itself");

    fs::write(&declared, &siblings_file).expect("the siblings file can be written");
    let paired = ssh_steps(&case, "paired", &["ok"; 6]);
    let pair = finish(client_gate(&case, "paired.log", &paired, &RAN));
    assert_eq!(pair.code, Some(0), "{pair:?}");
    assert_eq!(
        in_tree(&case, &leaf, INPUT).as_deref(),
        Some("one"),
        "the main repo's input is in its tree"
    );
    assert_eq!(in_tree(&case, &name, SIBLING_INPUT.0).as_deref(), Some(SIBLING_INPUT.1));

    fs::remove_file(&declared).expect("the siblings file can be removed");
    let back = ssh_steps(&case, "back", &["ok"; 6]);
    let again = finish(client_gate(&case, "back.log", &back, &RAN));
    remove_sibling(&sibling);

    assert_eq!(again.code, Some(0), "{again:?}");
    assert_eq!(
        on_builder(&case).as_deref(),
        Some("one"),
        "back to one repo, its input is in the work folder itself"
    );
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

/// The gitignored file a pair case's sibling holds for its command to name, `/`-separated below the sibling's tree,
/// and the bytes it holds.
const SIBLING_CARRIED: (&str, &str) = (".tmp/probe.targets", "<Project><!-- the sibling's --></Project>");

/// A pair case declaring no input glob for either repo, holding the gitignored probe project `.tmp/probe` in its main
/// tree and [`SIBLING_CARRIED`] in its sibling: the case, its folder name, the sibling's name and its folder.
fn carried_pair_case(name: &str) -> (Case, String, String, PathBuf) {
    let (case, leaf, sibling_name, sibling) = pair_inputs_case(name, &[], &[]);
    write(&case, ".tmp/probe/Probe.csproj", "<Project />");
    write(&case, ".tmp/probe/Program.cs", "class P {}");
    fs::create_dir_all(sibling.join(".tmp")).expect("the sibling's .tmp can be made");
    fs::write(sibling.join(SIBLING_CARRIED.0.replace('/', "\\")), SIBLING_CARRIED.1).expect("the sibling's file can be written");
    (case, leaf, sibling_name, sibling)
}

/// The word naming [`SIBLING_CARRIED`] from the main worktree's root, as the rewrite lands a path in the sibling `name`.
fn sibling_word(name: &str) -> String {
    format!(r"..\{name}\{}", SIBLING_CARRIED.0.replace('/', "\\"))
}

#[test]
fn a_pair_carries_a_gitignored_project_of_its_main_tree_into_the_main_tree() {
    let (case, leaf, name, sibling) = carried_pair_case("client-inputs-pair-carried-main");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &PROBE_RAN));
    remove_sibling(&sibling);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "inputs", "inputs", "run"], "the diff, then the put");
    assert_eq!(in_tree(&case, &leaf, ".tmp/probe/Probe.csproj").as_deref(), Some("<Project />"));
    assert_eq!(in_tree(&case, &leaf, ".tmp/probe/Program.cs").as_deref(), Some("class P {}"));
    assert!(
        !work_folder(&case).join(&name).join(".tmp").join("probe").exists(),
        "nothing lands in the sibling"
    );
    assert_eq!(carried_markers(&case.root()), Vec::<PathBuf>::new(), "a pair writes no carried marker");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

#[test]
fn a_pair_carries_a_gitignored_file_of_its_sibling_into_the_sibling_s_tree() {
    let (case, leaf, name, sibling) = carried_pair_case("client-inputs-pair-carried-sibling");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);
    let word = sibling_word(&name);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", &word]));
    remove_sibling(&sibling);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "inputs", "inputs", "run"], "the diff, then the put");
    assert_eq!(in_tree(&case, &name, SIBLING_CARRIED.0).as_deref(), Some(SIBLING_CARRIED.1));
    assert_eq!(
        in_tree(&case, &leaf, SIBLING_CARRIED.0),
        None,
        "nothing of the sibling lands in the main tree"
    );
    assert_eq!(carried_markers(&case.root()), Vec::<PathBuf>::new(), "a pair writes no carried marker");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

#[test]
fn a_tracked_sibling_file_a_pair_s_command_names_is_not_carried() {
    let (case, _, name, sibling) = carried_pair_case("client-inputs-pair-carried-tracked");
    let added = std::process::Command::new("git")
        .args(["-C", &shown(&sibling), "add", "--force", SIBLING_CARRIED.0])
        .output()
        .expect("git can be started");
    assert!(added.status.success(), "the sibling's file is tracked: {added:?}");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);
    let word = sibling_word(&name);

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", &word]));
    remove_sibling(&sibling);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"], "no carried entry, so no inputs call: {outcome:?}");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

#[test]
fn a_plain_pair_run_after_a_carried_pair_run_sweeps_the_carried_copies() {
    let (case, leaf, name, sibling) = carried_pair_case("client-inputs-pair-carried-swept");
    let first = ssh_steps(&case, "first", &["ok"; 4]);
    let word = sibling_word(&name);
    let ran = [r".\tool.exe", "out", ".tmp/probe/Probe.csproj", "out", &word];
    let carried = finish(client_gate(&case, "first.log", &first, &ran));
    assert_eq!(carried.code, Some(0), "{carried:?}");
    assert_eq!(in_tree(&case, &leaf, ".tmp/probe/Probe.csproj").as_deref(), Some("<Project />"));
    assert_eq!(in_tree(&case, &name, SIBLING_CARRIED.0).as_deref(), Some(SIBLING_CARRIED.1));

    let second = ssh_steps(&case, "second", &["ok"; 4]);
    let swept = finish(client_gate(&case, "second.log", &second, &RAN));
    remove_sibling(&sibling);

    assert_eq!(swept.code, Some(0), "{swept:?}");
    assert_eq!(calls(&second), ["preflight", "inputs", "run"], "the empty manifest's diff, no put");
    assert_eq!(
        in_tree(&case, &leaf, ".tmp/probe/Probe.csproj"),
        None,
        "the main tree's carried copy is swept"
    );
    assert_eq!(in_tree(&case, &name, SIBLING_CARRIED.0), None, "and the sibling's");
    let said = log_of(&case, "second.log");
    assert!(has_line(&said, "gate: inputs: removed the old copies of every tree"), "{said}");
    assert_eq!(carried_markers(&case.root()), Vec::<PathBuf>::new(), "no carried marker at any point");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

#[test]
fn a_pair_s_carried_files_past_the_caps_across_both_trees_keep_the_command_here() {
    let (case, _, name, sibling) = carried_pair_case("client-inputs-pair-carried-capped");
    for index in 0..998 {
        write(&case, &format!(".tmp/probe/{index}.cs"), "x");
    }
    let folder = sibling.join(".tmp").join("sprobe");
    fs::create_dir_all(&folder).expect("the sibling's probe folder can be made");
    fs::write(folder.join("S.csproj"), "<Project />").expect("the sibling's project can be written");
    for index in 0..1000 {
        fs::write(folder.join(format!("{index}.cs")), "x").expect("the sibling's source can be written");
    }
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);
    let word = format!(r"..\{name}\.tmp\sprobe\S.csproj");

    let ran = [r".\tool.exe", "out", ".tmp/probe/Probe.csproj", "out", &word];
    let outcome = finish(client_gate(&case, "out.log", &steps, &ran));
    remove_sibling(&sibling);

    assert_eq!(outcome.code, Some(0), "the command ran here: {outcome:?}");
    let line = "gate: not routed: .tmp/probe/Probe.csproj is gitignored and a command's inputs carry at most 64 MiB in 2000 files; \
                its files hold 1 MiB in 2001 files (the pair's trees together)\n";
    assert!(
        outcome.stderr.contains(line),
        "each tree is under the caps, the two together are not: {outcome:?}"
    );
    assert!(!calls(&steps).iter().any(|call| call == "run"), "{:?}", calls(&steps));
}

#[test]
fn a_worktree_whose_siblings_are_all_absent_carries_as_a_single_repo() {
    let (case, _, _, sibling) = carried_pair_case("client-inputs-pair-carried-absent");
    remove_sibling(&sibling);
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &PROBE_RAN));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "inputs", "inputs", "run"], "the diff, then the put");
    let probe = work_folder(&case).join(".tmp").join("probe");
    assert_eq!(
        fs::read_to_string(probe.join("Probe.csproj")).ok().as_deref(),
        Some("<Project />"),
        "in the work folder itself"
    );
    assert_eq!(carried_markers(&case.root()).len(), 1, "a single repo's carried run leaves its marker");
    assert_eq!(fallbacks(&events(&case)), Vec::<&str>::new());
}

/// The project file `.tmp/probe/Probe.csproj` of a pair case's main tree, importing the file at `path` written as from
/// its own folder.
fn probe_importing(path: &str) -> String {
    format!(r#"<Project><Import Project="{path}" /></Project>"#)
}

#[test]
fn a_pair_candidate_whose_build_reads_a_gitignored_sibling_file_not_sent_stays_here() {
    let (case, _, name, sibling) = carried_pair_case("client-inputs-pair-cross-unsent");
    write(
        &case,
        ".tmp/probe/Probe.csproj",
        &probe_importing(&format!(r"..\..\..\{name}\.tmp\gen.props")),
    );
    fs::write(sibling.join(".tmp").join("gen.props"), "<Project />").expect("the sibling's props can be written");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &PROBE_RAN));
    remove_sibling(&sibling);

    assert_eq!(outcome.code, Some(0), "the command ran here: {outcome:?}");
    let line = format!(
        "gate: not routed: .tmp/probe/Probe.csproj is gitignored and its files cannot be sent: the build reads \
         ../{name}/.tmp/gen.props outside its folder, which is not sent\n"
    );
    assert!(outcome.stderr.contains(&line), "{outcome:?}");
    assert!(!calls(&steps).iter().any(|call| call == "run"), "{:?}", calls(&steps));
}

#[test]
fn a_sibling_candidate_s_refusal_names_its_build_file_from_the_main_root() {
    let (case, _, name, sibling) = carried_pair_case("client-inputs-pair-sibling-above");
    let app = sibling.join(".tmp").join("app");
    fs::create_dir_all(&app).expect("the sibling's app folder can be made");
    fs::write(app.join("App.csproj"), "<Project />").expect("the sibling's project can be written");
    fs::write(sibling.join(".tmp").join("Directory.Packages.props"), "<Project />").expect("the sibling's props can be written");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);
    let word = format!(r"..\{name}\.tmp\app\App.csproj");

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "out", &word]));
    remove_sibling(&sibling);

    assert_eq!(outcome.code, Some(0), "the command ran here: {outcome:?}");
    let said = format!("the build reads ../{name}/.tmp/Directory.Packages.props above its folder, which is not sent\n");
    assert!(outcome.stderr.contains(&said), "{outcome:?}");
    assert!(!calls(&steps).iter().any(|call| call == "run"), "{:?}", calls(&steps));
}

#[test]
fn a_sibling_candidate_whose_build_reads_a_main_tree_file_the_run_carries_routes() {
    let (case, leaf, name, sibling) = carried_pair_case("client-inputs-pair-cross-carried");
    write(&case, ".tmp/probe/x.props", "<Project />");
    let folder = sibling.join(".tmp").join("sprobe");
    fs::create_dir_all(&folder).expect("the sibling's probe folder can be made");
    let import = format!(r#"<Project><Import Project="..\..\..\{leaf}\.tmp\probe\x.props" /></Project>"#);
    fs::write(folder.join("S.csproj"), import).expect("the sibling's project can be written");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);
    let word = format!(r"..\{name}\.tmp\sprobe\S.csproj");

    let ran = [r".\tool.exe", "out", ".tmp/probe/Probe.csproj", "out", &word];
    let outcome = finish(client_gate(&case, "out.log", &steps, &ran));
    remove_sibling(&sibling);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "inputs", "inputs", "run"], "{outcome:?}");
    assert_eq!(in_tree(&case, &leaf, ".tmp/probe/x.props").as_deref(), Some("<Project />"));
    assert!(
        in_tree(&case, &name, ".tmp/sprobe/S.csproj").is_some(),
        "the sibling's candidate is carried"
    );
}

#[test]
fn a_main_tree_candidate_whose_build_reads_a_sibling_file_its_inputs_send_routes() {
    let (case, _, name, sibling) = pair_inputs_case("client-inputs-pair-cross-declared", &[], &["secrets/**"]);
    let import = format!(r"..\..\..\{name}\{}", SIBLING_INPUT.0.replace('/', "\\"));
    write(&case, ".tmp/probe/Probe.csproj", &probe_importing(&import));
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &PROBE_RAN));
    remove_sibling(&sibling);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "inputs", "inputs", "run"], "{outcome:?}");
    assert_eq!(in_tree(&case, &name, SIBLING_INPUT.0).as_deref(), Some(SIBLING_INPUT.1));
}

#[test]
fn a_failing_pair_run_names_a_gitignored_sibling_file_and_the_sibling_s_inputs() {
    let (case, _, name, sibling) = pair_inputs_case("client-inputs-pair-hint", &[], &[]);
    fs::create_dir_all(sibling.join(".tmp")).expect("the sibling's .tmp can be made");
    fs::write(sibling.join(".tmp").join("a.jsonl"), "{}").expect("the sibling's file can be written");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);
    let word = format!(r"..\{name}\.tmp\a.jsonl");

    let outcome = finish(client_gate(&case, "out.log", &steps, &[r".\tool.exe", "exit", "1", &word]));
    remove_sibling(&sibling);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    let hint = format!(
        "gate: hint: ../{name}/.tmp/a.jsonl is gitignored, so the builder had no copy of it; if the command reads it, add \
         it to {}'s inputs in remote.json, or rerun with --local\n",
        repo_name(&name)
    );
    assert!(outcome.stderr.contains(&hint), "{outcome:?}");
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
