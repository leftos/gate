//! `gate warm` through the scripted remote client (`--test-remote-client`): where a warm-up is worded and sent from,
//! what keeps one from starting and what it says then, foreground and `--background`, and the warm-up record each way
//! the client says a warm-up ended leaves.

use std::fs::{self, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use gate_router::warm::warmup_hash;
use serde_json::{Value, json};

use crate::routing::{CONTROL_C_EXIT, HOST, repo, routed_case};
use crate::support::{Case, Outcome, build_version, finish, git, junction, shown, start, wait_until, write_remote_json};

/// The routing git limit and drain no machine reaches, so a starved gate still finds its worktree.
const GIT_HANG_GUARDS: [&str; 4] = ["--test-git-limit-ms", "3600000", "--test-git-drain-ms", "600000"];
/// A warm-up file of one command, fake-tool at the worktree root.
const ONE_COMMAND: &str = "commands = [[\"tool.exe\", \"out\", \"hi\"]]\n";
/// Why a warm-up the client ended without saying how it ended failed.
const NOT_STARTED: &str = "the client did not start the warm-up";

static NEXT_STEPS: AtomicU64 = AtomicU64::new(0);

/// Writes `steps` as a step file of the scripted client in the case folder, one step a line; its path, as a word.
fn steps(case: &Case, steps: &[&str]) -> String {
    let path = case
        .folder()
        .join(format!("warm-steps-{}.txt", NEXT_STEPS.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, steps.join("\n")).expect("the step file can be written");
    shown(&path)
}

/// Writes the case's routing file, its repo listed as [`routed_case`] lists it, with `warmup` as given; no `warmup` key
/// when it is null.
fn routing_with(case: &Case, warmup: &Value) {
    let mut routing = json!({ "host": HOST, "repos": { repo(case): ["*tool*"] } });
    if !warmup.is_null() {
        routing["warmup"] = warmup.clone();
    }
    write_remote_json(case, &routing.to_string());
}

/// A case folder made a checkout of its own holding fake-tool as `tool.exe`, whose repo consents to warm-ups and
/// whose `.gate/warmup.toml` is [`ONE_COMMAND`].
fn warm_case(name: &str) -> Case {
    let case = routed_case(name);
    routing_with(&case, &json!([repo(&case)]));
    warmup_file(&case, ONE_COMMAND.as_bytes());
    case
}

/// Writes `bytes` as the case worktree's `.gate/warmup.toml`.
fn warmup_file(case: &Case, bytes: &[u8]) {
    let folder = case.folder().join(".gate");
    fs::create_dir_all(&folder).expect("the .gate folder can be made");
    fs::write(folder.join("warmup.toml"), bytes).expect("the warm-up file can be written");
}

/// `gate warm` in the case's folder with the case's state root, the scripted client of `steps`, the git guards, and
/// `extra`, ready to run.
fn warm_command(case: &Case, steps: &str, extra: &[&str]) -> Command {
    let root = shown(&case.root());
    let mut args = vec!["warm", "--state-dir", &root, "--test-remote-client", steps];
    args.extend(GIT_HANG_GUARDS);
    args.extend(extra);
    case.gate(&args)
}

fn warm(case: &Case, steps: &str, extra: &[&str]) -> Outcome {
    finish(warm_command(case, steps, extra))
}

/// The case's warm-up folder, `<state>\remote\warm`.
fn warm_folder(case: &Case) -> PathBuf {
    case.root().join("remote").join("warm")
}

/// The path of the case's one warm-up file with `extension`; `None` when there is none.
fn warm_entry(case: &Case, extension: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(warm_folder(case)).ok()?;
    let found: Vec<PathBuf> = entries
        .map(|entry| entry.expect("a warm entry can be read").path())
        .filter(|path| path.extension().is_some_and(|found| found == extension))
        .collect();
    assert!(found.len() <= 1, "one warm-up {extension} at most: {found:?}");
    found.into_iter().next()
}

/// The case's warm-up record; `None` when there is none.
fn record(case: &Case) -> Option<Value> {
    let path = warm_entry(case, "json")?;
    Some(serde_json::from_str(&fs::read_to_string(path).expect("the record can be read")).expect("the record is JSON"))
}

/// The call the scripted client recorded at `path`.
fn recorded(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).expect("the client recorded its call")).expect("the record is JSON")
}

/// Asserts `outcome` refused the warm-up before any call with `line` and status 2, leaving no record.
fn assert_refused(case: &Case, outcome: &Outcome, line: &str) {
    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(outcome.stderr.lines().collect::<Vec<_>>(), [line], "{outcome:?}");
    assert_eq!(record(case), None, "no record");
}

#[test]
fn a_warm_up_from_a_subfolder_is_worded_and_sent_from_the_root() {
    let case = warm_case("warm-words");
    fs::create_dir_all(case.folder().join("sub")).expect("the subfolder can be made");
    fs::write(case.folder().join("sub").join("a.txt"), "a\n").expect("the subfolder holds a file");
    let file = "commands = [[\"tool.exe\", \"out\", \"hi\"], [\"dotnet\", \"build\"]]\n";
    warmup_file(&case, file.as_bytes());
    git(&case, &["commit", "--quiet", "--allow-empty", "-m", "a"]);
    let head = git(&case, &["rev-parse", "HEAD"]);
    let call = case.folder().join("call.json");
    let steps = steps(&case, &[&format!("record {}", shown(&call)), "out warming now", "warmed"]);

    let outcome = warm(&case, &steps, &["--cd", "sub"]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: warm-up of builder passed"), "{outcome:?}");
    let call = recorded(&call);
    assert_eq!(call["subfolder"], "", "sent from the root: {call}");
    assert_eq!(call["words"], json!([]), "{call}");
    let hash = warmup_hash(file.as_bytes());
    let stamp = format!("-p:SourceRevisionId={head}");
    assert_eq!(
        call["warmth"],
        json!({"warmUp": {"commands": [["./tool.exe", "out", "hi"], ["dotnet", "build", stamp]], "hash": hash}}),
        "tool.exe is marked as the root's file, which sub does not hold, and the build is stamped: {call}"
    );
    let log = warm_entry(&case, "log").expect("the warm-up log is written");
    assert!(
        fs::read_to_string(log).expect("the log can be read").contains("warming now"),
        "the client writes the warm-up log"
    );
    assert_eq!(record(&case).expect("a record")["state"], "warm");
}

#[test]
fn a_warm_up_given_a_cd_it_cannot_enter_is_refused() {
    let case = warm_case("warm-cd");
    fs::write(case.folder().join("a-file.txt"), "x").expect("a file can be written");
    let steps = steps(&case, &["warmed"]);
    for (folder, line) in [
        ("missing", "gate: --cd missing is not a folder"),
        ("a-file.txt", "gate: --cd a-file.txt is not a folder"),
        (r"\\server\share", r"gate: --cd \\server\share is not a folder on a drive"),
    ] {
        let outcome = warm(&case, &steps, &["--cd", folder]);
        assert_refused(&case, &outcome, line);
    }
}

#[test]
fn a_warm_up_without_consent_is_refused_and_quiet_in_the_background() {
    let case = warm_case("warm-consent");
    let call = case.folder().join("call.json");
    let steps = steps(&case, &[&format!("record {}", shown(&call)), "warmed"]);
    let no_consent = format!("gate: warm: {} has not consented to warm-ups in remote.json", repo(&case));
    for warmup in [json!(null), json!(["another-repo"])] {
        routing_with(&case, &warmup);
        assert_refused(&case, &warm(&case, &steps, &[]), &no_consent);
        let background = warm(&case, &steps, &["--background"]);
        assert_eq!((background.code, background.stderr.as_str()), (Some(0), ""), "{background:?}");
    }
    routing_with(&case, &json!(5));
    let bad = warm(&case, &steps, &[]);
    assert_eq!(bad.code, Some(2), "{bad:?}");
    assert!(
        bad.stderr.starts_with("gate: warm: remote.json's warmup cannot be read: "),
        "a bad value is no consent: {bad:?}"
    );
    let background = warm(&case, &steps, &["--background"]);
    assert_eq!((background.code, background.stderr.as_str()), (Some(0), ""), "{background:?}");
    assert!(!call.exists(), "no call is made");
    assert_eq!(record(&case), None, "no record");
}

#[test]
fn a_warm_up_consent_names_the_repo_in_any_letter_case() {
    let case = warm_case("warm-consent-case");
    routing_with(&case, &json!([repo(&case).to_uppercase()]));
    let outcome = warm(&case, &steps(&case, &["warmed"]), &[]);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

#[test]
fn a_warm_up_of_a_repo_remote_json_does_not_list_is_refused() {
    let case = warm_case("warm-unlisted");
    let steps = steps(&case, &["warmed"]);
    let line = format!("gate: warm: {} is not listed in remote.json", repo(&case));
    write_remote_json(
        &case,
        &json!({ "host": HOST, "repos": { "another-repo": ["*tool*"] }, "warmup": [repo(&case)] }).to_string(),
    );
    assert_refused(&case, &warm(&case, &steps, &[]), &line);
    let routing = case.local_app_data().join("gate").join("remote.json");
    fs::remove_file(&routing).expect("the routing file can be removed");
    let missing = format!("gate: warm: {} does not exist", shown(&routing));
    assert_refused(&case, &warm(&case, &steps, &[]), &missing);
}

#[test]
fn a_warm_up_without_a_file_is_refused_and_quiet_in_the_background() {
    let case = warm_case("warm-no-file");
    fs::remove_file(case.folder().join(".gate").join("warmup.toml")).expect("the file can be removed");
    let steps = steps(&case, &["warmed"]);
    let outcome = warm(&case, &steps, &[]);
    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    let line = outcome.stderr.trim_end();
    assert!(
        line.starts_with("gate: warm: ") && line.ends_with(r"\.gate\warmup.toml does not exist") && line.lines().count() == 1,
        "{outcome:?}"
    );
    let background = warm(&case, &steps, &["--background"]);
    assert_eq!((background.code, background.stderr.as_str()), (Some(0), ""), "{background:?}");
    assert_eq!(record(&case), None, "no record");
}

#[test]
fn a_warm_up_file_with_a_problem_is_refused_with_no_call() {
    let case = warm_case("warm-file-problem");
    let call = case.folder().join("call.json");
    let steps = steps(&case, &[&format!("record {}", shown(&call)), "warmed"]);
    let outside = shown(&case.folder().parent().expect("the case folder has a parent").join("elsewhere.txt"));
    let nine = format!("commands = [{}]\n", ["[\"tool.exe\"]"; 9].join(", "));
    let large = format!("{ONE_COMMAND}# {}\n", "x".repeat(64 * 1024));
    let outside_file = format!("commands = [[\"tool.exe\", {outside:?}]]\n");
    for (text, problem) in [
        ("commands = []\n", "declares no commands".to_owned()),
        (nine.as_str(), "has more than 8 commands".to_owned()),
        (large.as_str(), "is larger than 64 KiB".to_owned()),
        (outside_file.as_str(), format!("command 1 names {outside}, outside the worktree")),
        ("commands = [[], []]\n", "commands[0] is empty (and 1 more)".to_owned()),
        (
            "commands = [[\"tool.exe\"], [\"git\", \"fetch\"]]\n",
            "command 2 starts with git, which a routed run never sends".to_owned(),
        ),
        (
            "commands = [[\"prek.exe\", \"run\"]]\n",
            "command 1 starts with prek.exe, which a routed run never sends".to_owned(),
        ),
    ] {
        warmup_file(&case, text.as_bytes());
        let outcome = warm(&case, &steps, &[]);
        assert_refused(&case, &outcome, &format!(r"gate: warm: .gate\warmup.toml {problem}"));
        let background = warm(&case, &steps, &["--background"]);
        assert_eq!(background.code, Some(2), "a file problem is said in the background too: {background:?}");
    }
    fs::remove_dir_all(case.folder().join(".gate")).expect("the .gate folder can be removed");
    let elsewhere = case.folder().join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("the junction's target can be made");
    fs::write(elsewhere.join("warmup.toml"), ONE_COMMAND).expect("the file can be written");
    junction(&case.folder().join(".gate"), &elsewhere);
    let outcome = warm(&case, &steps, &[]);
    assert_refused(&case, &outcome, "gate: not routed: .gate/siblings.toml: .gate is a link or a junction");
    assert!(!call.exists(), "no call is made");
}

/// The case `name` warmed through the scripted client of `ending`, the step that says how the warm-up ended: the
/// outcome and the record.
fn warmed_with(name: &str, ending: &[&str]) -> (Case, Outcome, Option<Value>) {
    let case = warm_case(name);
    let steps = steps(&case, ending);
    let outcome = warm(&case, &steps, &[]);
    let record = record(&case);
    (case, outcome, record)
}

/// Asserts `record` failed of `failure` for `reason`, and `outcome` said so with status 1.
fn assert_failed(outcome: &Outcome, record: Option<&Value>, failure: &str, reason: &str) {
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(
        outcome.stderr.contains(&format!("gate: warm-up of builder failed: {reason}")),
        "{outcome:?}"
    );
    let record = record.expect("a record");
    assert_eq!(
        (&record["state"], &record["failure"], &record["reason"]),
        (&json!("failed"), &json!(failure), &json!(reason)),
        "{record}"
    );
}

#[test]
fn a_warm_up_record_is_what_the_client_says_of_its_end() {
    let (case, outcome, warm) = warmed_with("warm-warmed", &["warmed"]);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let warm = warm.expect("a record");
    assert_eq!((&warm["state"], &warm["host"]), (&json!("warm"), &json!(HOST)), "{warm}");
    assert_eq!(warm["hash"], warmup_hash(ONE_COMMAND.as_bytes()), "{warm}");
    assert_eq!(warm["gate"], build_version(&case), "{warm}");
    assert!(warm["pid"].as_u64().is_some_and(|pid| pid > 0), "{warm}");
    assert!(warm.get("failure").is_none(), "{warm}");

    let (_, outcome, failed) = warmed_with("warm-failed-commands", &["warm-failed commands command 1 stopped"]);
    assert_failed(&outcome, failed.as_ref(), "commands", "command 1 stopped");
    let (_, outcome, failed) = warmed_with("warm-failed-refused", &["warm-failed refused the builder said no"]);
    assert_failed(&outcome, failed.as_ref(), "refused", "the builder said no");

    let (_, outcome, deleted) = warmed_with("warm-deleted", &["warm-deleted"]);
    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert_eq!(deleted, None, "Ctrl+C deletes the record");
}

#[test]
fn a_warm_up_whose_client_says_no_end_is_refused_whatever_its_status() {
    let (_, outcome, record) = warmed_with("warm-status-0", &["status 0"]);
    assert_failed(&outcome, record.as_ref(), "refused", NOT_STARTED);
    let (_, outcome, record) = warmed_with("warm-refuse-busy", &["refuse busy"]);
    assert_failed(&outcome, record.as_ref(), "refused", NOT_STARTED);
}

#[test]
fn a_warm_up_file_with_a_byte_order_mark_is_hashed_with_it() {
    let case = warm_case("warm-bom");
    let bytes = [b"\xef\xbb\xbf".as_slice(), ONE_COMMAND.as_bytes()].concat();
    warmup_file(&case, &bytes);
    let outcome = warm(&case, &steps(&case, &["warmed"]), &[]);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(record(&case).expect("a record")["hash"], warmup_hash(&bytes));
}

#[test]
fn a_warm_up_whose_client_cannot_start_is_recorded_refused() {
    let (_, outcome, failed) = warmed_with("warm-fail-start", &["fail-start no way to the builder"]);
    assert_failed(&outcome, failed.as_ref(), "refused", "no way to the builder");
}

#[test]
fn a_warm_up_whose_client_cannot_be_chosen_is_recorded_refused() {
    let case = warm_case("warm-no-ssh");
    let transport = format!("local:{}", shown(&case.folder().join("builder")));
    let root = shown(&case.root());
    let mut args = vec![
        "warm",
        "--state-dir",
        &root,
        "--test-remote-transport",
        &transport,
        "--test-remote-ssh",
        "no-such-ssh.exe",
    ];
    args.extend(GIT_HANG_GUARDS);
    let outcome = finish(case.gate(&args));
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    let record = record(&case).expect("a record");
    assert_eq!((&record["state"], &record["failure"]), (&json!("failed"), &json!("refused")), "{record}");
    assert!(
        record["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("no-such-ssh.exe") && !reason.starts_with("gate: ")),
        "the reason names the missing stand-in, without the line's prefix: {record}"
    );
}

/// The case's warm-up cap, `<state>\remote\warm.lock`, made and held as another warm-up holds it.
fn held_cap(case: &Case) -> fs::File {
    let path = case.root().join("remote").join("warm.lock");
    fs::create_dir_all(case.root().join("remote")).expect("the remote folder can be made");
    fs::write(&path, "").expect("the cap can be made");
    hold(&path)
}

#[test]
fn a_warm_up_kept_from_the_cap_past_the_wait_is_recorded_refused() {
    let case = warm_case("warm-cap");
    let call = case.folder().join("call.json");
    let cap = held_cap(&case);
    let steps = steps(&case, &[&format!("record {}", shown(&call)), "warmed"]);
    let outcome = warm(&case, &steps, &["--test-warm-wait-ms", "50"]);
    drop(cap);
    assert_failed(
        &outcome,
        record(&case).as_ref(),
        "refused",
        "another warm-up held the warm-up cap past the wait",
    );
    assert!(!call.exists(), "no call is made");
}

#[test]
fn a_warm_up_whose_routing_entry_changes_while_it_waits_is_withdrawn() {
    for background in [false, true] {
        let case = warm_case(if background { "warm-entry-bg" } else { "warm-entry" });
        let call = case.folder().join("call.json");
        let cap = held_cap(&case);
        let steps = steps(&case, &[&format!("record {}", shown(&call)), "warmed"]);
        let mut extra = vec!["--test-warm-wait-ms", "3600000"];
        if background {
            extra.push("--background");
        }
        let running = start(warm_command(&case, &steps, &extra));
        wait_until("the warm-up waits on the cap", || record(&case).is_some());
        let routing = json!({
            "host": HOST,
            "repos": { repo(&case): ["*tool*"] },
            "inputs": { repo(&case): ["*.dll"] },
            "warmup": [repo(&case)],
        });
        write_remote_json(&case, &routing.to_string());
        drop(cap);
        let outcome = running.finish();

        let line = format!("gate: warm: remote.json's entry for {} changed\n", repo(&case));
        let expected = if background { (Some(0), String::new()) } else { (Some(2), line) };
        assert_eq!((outcome.code, outcome.stderr.clone()), expected, "{outcome:?}");
        assert_eq!(record(&case), None, "the record is deleted");
        assert!(!call.exists(), "nothing is sent");
    }
}

/// Opens `path` sharing nothing, as another warm-up holds its lock.
fn hold(path: &Path) -> fs::File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(path)
        .expect("the lock can be held")
}

#[test]
fn a_warm_up_whose_lock_another_holds_leaves_the_record_alone() {
    let case = warm_case("warm-held");
    let call = case.folder().join("call.json");
    let first = warm(&case, &steps(&case, &["warm-failed refused first"]), &[]);
    assert_eq!(first.code, Some(1), "{first:?}");
    let record_path = warm_entry(&case, "json").expect("the first warm-up wrote its record");
    let before = fs::read(&record_path).expect("the record can be read");
    let held = hold(&warm_entry(&case, "lock").expect("the first warm-up made its lock"));
    let steps = steps(&case, &[&format!("record {}", shown(&call)), "warmed"]);

    let outcome = warm(&case, &steps, &[]);
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stderr.trim_end(), "gate: a warm-up of builder is already running", "{outcome:?}");
    let background = warm(&case, &steps, &["--background"]);
    assert_eq!((background.code, background.stderr.as_str()), (Some(0), ""), "{background:?}");
    assert!(!call.exists(), "no call is made");
    assert_eq!(fs::read(&record_path).expect("the record can be read"), before, "the record is untouched");
    drop(held);
}

/// The name a sibling of `case` has and the folder it would be in: beside the case folder, named for it so no other
/// case shares it.
fn sibling_plan(case: &Case) -> (String, PathBuf) {
    let leaf = case
        .folder()
        .file_name()
        .expect("the case folder has a name")
        .to_string_lossy()
        .into_owned();
    let name = format!("{leaf}-sibling");
    (name.clone(), case.folder().parent().expect("the case folder has a parent").join(name))
}

/// Makes `beside` a sibling of `case` a routed run can carry: a folder holding a `.git`, as a worktree beside the root
/// does, emptied of anything an earlier run left.
fn sibling_folder(beside: &Path) {
    if beside.exists() {
        fs::remove_dir_all(beside).expect("a sibling left by an earlier run can be removed");
    }
    fs::create_dir_all(beside.join(".git")).expect("the sibling's .git can be made");
}

/// Declares `name` as `case`'s one sibling in its `.gate/siblings.toml`.
fn declare_sibling(case: &Case, name: &str) {
    let folder = case.folder().join(".gate");
    fs::create_dir_all(&folder).expect("the .gate folder can be made");
    let declaration = format!("siblings = [\"{name}\"]\n");
    fs::write(folder.join("siblings.toml"), declaration).expect("the declaration can be written");
}

/// Allows `name` as `case`'s sibling in the case's routing file.
fn allow_sibling(case: &Case, name: &str) {
    let path = case.local_app_data().join("gate").join("remote.json");
    let text = fs::read_to_string(&path).expect("the routing file can be read");
    let mut routing: Value = serde_json::from_str(&text).expect("the routing file is JSON");
    routing["siblings"] = json!({ repo(case): [name] });
    write_remote_json(case, &routing.to_string());
}

/// Starts the warm-up of the case `name` with the cap held and its sibling name allowed in the routing file, waits until
/// it waits on the cap, runs `meanwhile` on the case, lets the cap go and returns the case, its call path and the
/// outcome.
fn waiting_on_the_cap(name: &str, background: bool, meanwhile: impl FnOnce(&Case)) -> (Case, PathBuf, Outcome) {
    let case = warm_case(name);
    let (sibling, beside) = sibling_plan(&case);
    allow_sibling(&case, &sibling);
    let call = case.folder().join("call.json");
    let cap = held_cap(&case);
    let steps = steps(&case, &[&format!("record {}", shown(&call)), "warmed"]);
    let mut extra = vec!["--test-warm-wait-ms", "3600000"];
    if background {
        extra.push("--background");
    }
    let running = start(warm_command(&case, &steps, &extra));
    wait_until("the warm-up waits on the cap", || record(&case).is_some());
    meanwhile(&case);
    drop(cap);
    let outcome = running.finish();
    if beside.exists() {
        fs::remove_dir_all(&beside).expect("the sibling can be removed");
    }
    (case, call, outcome)
}

#[test]
fn a_warm_up_whose_siblings_change_while_it_waits_says_so() {
    for background in [false, true] {
        let name = if background { "warm-siblings-bg" } else { "warm-siblings" };
        let (case, call, outcome) = waiting_on_the_cap(name, background, |case| {
            let (sibling, beside) = sibling_plan(case);
            sibling_folder(&beside);
            declare_sibling(case, &sibling);
        });

        let line = format!("gate: warm: the siblings of {} changed while the warm-up waited\n", repo(&case));
        let expected = if background { (Some(0), String::new()) } else { (Some(2), line) };
        assert_eq!((outcome.code, outcome.stderr.clone()), expected, "{outcome:?}");
        assert_eq!(record(&case), None, "the record is deleted");
        assert!(!call.exists(), "nothing is sent");
    }
}

#[test]
fn a_warm_up_whose_sibling_check_fails_at_the_recheck_is_refused() {
    for background in [false, true] {
        let name = if background { "warm-sibling-check-bg" } else { "warm-sibling-check" };
        let (case, call, outcome) = waiting_on_the_cap(name, background, |case| {
            let (sibling, _) = sibling_plan(case);
            declare_sibling(case, &sibling);
        });

        let (sibling, _) = sibling_plan(&case);
        let line = format!("gate: not routed: sibling {sibling} not found beside the worktree\n");
        assert_eq!(
            (outcome.code, outcome.stderr.clone()),
            (Some(2), line),
            "said in the background too: {outcome:?}"
        );
        assert_eq!(record(&case), None, "the record is deleted");
        assert!(!call.exists(), "nothing is sent");
    }
}

#[test]
fn a_warm_up_without_consent_and_a_broken_siblings_file_stays_quiet_in_the_background() {
    let case = routed_case("warm-consent-siblings");
    routing_with(&case, &json!(null));
    warmup_file(&case, ONE_COMMAND.as_bytes());
    fs::write(case.folder().join(".gate").join("siblings.toml"), "siblings = [\n").expect("the broken file can be written");
    let steps = steps(&case, &["warmed"]);

    let said = warm(&case, &steps, &[]);
    let consent = format!("gate: warm: {} has not consented to warm-ups in remote.json", repo(&case));
    assert_eq!(said.code, Some(2), "{said:?}");
    assert_eq!(
        said.stderr.lines().collect::<Vec<_>>(),
        [consent],
        "consent is checked before the siblings: {said:?}"
    );
    let background = warm(&case, &steps, &["--background"]);
    assert_eq!((background.code, background.stderr.as_str()), (Some(0), ""), "{background:?}");
    assert_eq!(record(&case), None, "no record");
}

#[test]
fn a_final_record_write_that_fails_leaves_no_record() {
    let case = warm_case("warm-record-write");
    let outcome = warm(&case, &steps(&case, &["fail-start "]), &[]);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(
        outcome.stderr.starts_with("gate: warm: cannot write the warm-up record ") && outcome.stderr.contains("state failed with an empty reason"),
        "the write's line is said: {outcome:?}"
    );
    assert_eq!(record(&case), None, "the failed write's record is deleted");
}

#[test]
fn a_stale_warming_record_with_a_free_lock_is_overwritten() {
    let case = warm_case("warm-stale");
    let first = warm(&case, &steps(&case, &["warmed"]), &[]);
    assert_eq!(first.code, Some(0), "{first:?}");
    let record_path = warm_entry(&case, "json").expect("the first warm-up wrote its record");
    let mut stale = record(&case).expect("a record");
    stale["state"] = json!("warming");
    stale["pid"] = json!(1);
    fs::write(&record_path, stale.to_string()).expect("a stale record can be written");

    let outcome = warm(&case, &steps(&case, &["warmed"]), &[]);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let record = record(&case).expect("a record");
    assert_eq!(record["state"], "warm", "{record}");
    assert_ne!(record["pid"], 1, "the record is this warm-up's: {record}");
}
