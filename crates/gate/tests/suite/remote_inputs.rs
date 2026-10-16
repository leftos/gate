//! A routed run's ignored inputs, sent by the client through the stand-in for ssh to the real `gate box inputs`: the
//! diff and the put before the run call, an unchanged set sent no more, and each way the step refuses the route
//! `inputs` and runs the command here.

use std::fs;
use std::path::Path;

use gate_router::repo_name;
use regex::Regex;
use serde_json::json;

use crate::remote_client::{
    CLIENT_IGNORED, Tuned, client_case, client_gate, client_gate_tuned, fallbacks, pair_case, sibling_repository, work_folder, write,
};
use crate::support::{Case, Go, Outcome, calls, events, finish, shown, ssh_steps, start, write_remote_json};

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
fn inputs_case(name: &str) -> Case {
    let case = client_case(name);
    with_input(&case);
    routed(&case, None);
    case
}

/// The text of the case's log `log`.
fn log_of(case: &Case, log: &str) -> String {
    fs::read_to_string(case.folder().join(log)).expect("the log was written")
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

#[test]
fn a_pair_with_inputs_falls_back_before_any_inputs_call() {
    let (case, _, name, sibling) = pair_case("client-inputs-pair", sibling_repository);
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

#[test]
fn a_pass_that_sent_inputs_names_them_in_its_split() {
    let case = inputs_case("client-inputs-split");
    let steps = ssh_steps(&case, "ssh", &["ok"; 4]);

    let outcome = finish(client_gate(&case, "out.log", &steps, &RAN));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let line = Regex::new(concat!(
        r"(?m)^gate: passed in \d+ s on builder ",
        r"\(load-adjusted \d+ s, ceiling 3600 s; command \d+ s, push \d+ s, inputs \d+ s\)\. ",
        r"Full output: .*out\.log\r?$",
    ))
    .expect("the pattern is valid");
    assert!(line.is_match(&outcome.stdout), "{outcome:?}");
}
