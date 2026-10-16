//! `gate warm` through the scripted remote client (`--test-remote-client`): where a warm-up is worded and sent from,
//! what keeps one from starting and what it says then, foreground and `--background`, and the warm-up record each way
//! the client says a warm-up ended leaves; and through the real client over the suite's `local:` transport and its
//! stand-in for ssh, where the warm-up leaves its record, its log and the builder's `{Work}.warm`, how the box's end
//! is classed, and what it brings back, which is nothing.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::hash::{DefaultHasher, Hasher};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use gate_router::warm::warmup_hash;
use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};

use crate::remote_client::{client_case, client_gate};
use crate::routing::{CONTROL_C_EXIT, HOST, names, repo, routed_case};
use crate::support::{
    Case, FAKE_SSH, GATE, GATE_VARIABLE, Go, Outcome, Running, STEPS_VARIABLE, build_version, calls, events, finish, git, heavy_words, junction,
    nothing_back, shown, ssh_steps, start, wait_until, write_remote_json,
};

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

// `gate warm` through the real client over the suite's `local:` transport and its `fake-ssh` stand-in, whose builder
// is `gate box` run here in the case folder: where the warm-up leaves its record, its log and the builder's
// `{Work}.warm`, what keeps one from starting, how the box's end is classed, and what a warm-up brings back, which
// is nothing.

/// A real-client case: a checkout of its own holding fake-tool as `tool.exe`, a `builder` folder for the `local:`
/// transport, an `ssh` folder for the stand-in, a `.gitignore` that leaves the gate's own folders out of the
/// snapshot, its repo consented in the routing file and [`ONE_COMMAND`] as its `.gate/warmup.toml`.
fn warm_client_case(name: &str) -> Case {
    let case = client_case(name);
    routing_with(&case, &json!([repo(&case)]));
    warmup_file(&case, ONE_COMMAND.as_bytes());
    case
}

/// `gate warm --state-dir <state>` in `worktree`'s folder through the real client: the builder root `builder` in
/// `worktree`'s folder, each ssh call made through the stand-in of the steps file `steps`, the retry pause 1 ms, the
/// git guards, and `extra` after them.
fn warm_client_at(worktree: &Case, state: &Path, steps: &Path, extra: &[&str]) -> Command {
    let root = shown(state);
    let transport = format!("local:{}", shown(&worktree.folder().join("builder")));
    let mut args = vec![
        "warm",
        "--state-dir",
        root.as_str(),
        "--test-remote-transport",
        transport.as_str(),
        "--test-remote-ssh",
        FAKE_SSH,
        "--test-remote-retry-pause-ms",
        "1",
    ];
    args.extend(GIT_HANG_GUARDS);
    args.extend(extra);
    let mut command = worktree.gate(&args);
    command.env(STEPS_VARIABLE, steps).env(GATE_VARIABLE, GATE);
    command
}

/// [`warm_client_at`] with the case's own state root.
fn warm_client(case: &Case, steps: &Path, extra: &[&str]) -> Command {
    warm_client_at(case, &case.root(), steps, extra)
}

/// Whether the lock file at `path` can be opened sharing nothing, as a warm-up's own open of it does: no warm-up
/// holds it.
fn free(path: &Path) -> bool {
    OpenOptions::new().read(true).write(true).share_mode(0).open(path).is_ok()
}

/// A path no write of the case makes, for a step that must never be released.
fn never(case: &Case) -> String {
    shown(&case.folder().join("ssh").join("never"))
}

/// The hash the builder's `{Work}.warm` of the case's one work folder holds; `None` when the builder holds none.
fn box_warm(case: &Case) -> Option<String> {
    let entries = fs::read_dir(case.folder().join("builder").join("work")).ok()?;
    let warm: Vec<PathBuf> = entries
        .map(|entry| entry.expect("a builder work entry can be read").path())
        .filter(|path| path.extension().is_some_and(|kind| kind == "warm"))
        .collect();
    assert!(warm.len() <= 1, "one work folder at most: {warm:?}");
    warm.first()
        .map(|path| fs::read_to_string(path).expect("the builder's warm record can be read"))
}

/// Every warm-up record of the state root `case`, in entry order: a second worktree sharing the root writes its own.
fn records(case: &Case) -> Vec<Value> {
    let folder = warm_folder(case);
    let entries = match fs::read_dir(&folder) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        listed => listed.expect("the warm-up folder can be listed"),
    };
    entries
        .map(|entry| entry.expect("a warm entry can be read").path())
        .filter(|path| path.extension().is_some_and(|kind| kind == "json"))
        .map(|path| serde_json::from_str(&fs::read_to_string(path).expect("the record can be read")).expect("the record is JSON"))
        .collect()
}

/// The entries of the state root's `remote` folder named like `suffix`.
fn remote_entries(case: &Case, suffix: &str) -> Vec<String> {
    let entries = match fs::read_dir(case.root().join("remote")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        listed => listed.expect("the remote folder can be listed"),
    };
    entries
        .map(|entry| entry.expect("a remote entry can be read").file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(suffix))
        .collect()
}

/// The case's worktree files, each with its length and a hash of its bytes: everything under the case folder but the
/// gate's own folders — the builder, the stand-in's `ssh` folder, the state root and LOCALAPPDATA — and the logs.
fn worktree(case: &Case) -> BTreeMap<PathBuf, (u64, u64)> {
    let mut files = BTreeMap::new();
    walk(&case.folder(), true, &mut files);
    files
}

/// [`worktree`]'s walk of `folder`, leaving the gate's own top-level folders out when `top`.
fn walk(folder: &Path, top: bool, files: &mut BTreeMap<PathBuf, (u64, u64)>) {
    for entry in fs::read_dir(folder).expect("a worktree folder can be listed") {
        let path = entry.expect("a worktree entry can be read").path();
        let name = path.file_name().expect("an entry has a name").to_string_lossy().into_owned();
        let the_gates = path.extension().is_some_and(|kind| kind == "log");
        if the_gates || (top && ["builder", "appdata", "state", "ssh", ".git"].contains(&name.as_str())) {
            continue;
        }
        if path.is_dir() {
            walk(&path, false, files);
        } else {
            let bytes = fs::read(&path).expect("a listed file can be read");
            let mut hasher = DefaultHasher::new();
            hasher.write(&bytes);
            files.insert(path, (bytes.len() as u64, hasher.finish()));
        }
    }
}

/// The case's routed run, held at its run call: its steps file `name`, its log `log`, and `command` on the builder.
/// The run call is held until the returned go file is written, and the work folder's key with it. The run is started
/// with the repo's consent taken out of the routing file, so a work folder that is not warm does not keep it here, and
/// the consent is given back once the run is at its run call.
fn held_run(case: &Case, name: &str, log: &str, command: &[&str]) -> (Running, Go) {
    let go = Go(case.folder().join("ssh").join(format!("{name}-go")));
    let steps = ssh_steps(case, name, &["ok", &format!("hold {}", shown(go.path()))]);
    routing_with(case, &json!(null));
    let run = start(client_gate(case, log, &steps, command));
    run.wait_until("the routed run's run call", || calls(&steps).len() == 2);
    routing_with(case, &json!([repo(case)]));
    (run, go)
}

/// Starts the warm-up of `case` while `run` holds the work folder's key, runs `meanwhile` once the warm-up has taken
/// its own lock and written its record (so it waits on the key), lets the run go, and returns the warm-up's steps
/// file and outcome.
fn while_the_key_is_held(case: &Case, run: (Running, Go), background: bool, meanwhile: impl FnOnce(&Case)) -> (PathBuf, Outcome) {
    let (held, go) = run;
    let steps = ssh_steps(case, "warm-key-wait", &["ok", "ok"]);
    let mut extra = vec!["--test-warm-wait-ms", "3600000"];
    if background {
        extra.push("--background");
    }
    let warm = start(warm_client(case, &steps, &extra));
    warm.wait_until("the warm-up's lock and record", || {
        warm_entry(case, "lock").is_some_and(|lock| record(case).is_some() && !free(&lock))
    });
    meanwhile(case);
    fs::write(go.path(), "").expect("the go file can be made");
    let outcome = warm.finish();
    let _ = held.finish();
    (steps, outcome)
}

#[test]
fn a_real_client_warm_up_writes_the_record_the_log_and_the_builder_s_record() {
    let case = warm_client_case("warm-real");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = finish(warm_client(&case, &steps, &[]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"], "no outputs and no fetch call: {outcome:?}");
    let hash = warmup_hash(ONE_COMMAND.as_bytes());
    let record = record(&case).expect("a record");
    assert_eq!((&record["state"], &record["host"]), (&json!("warm"), &json!(HOST)), "{record}");
    assert_eq!(record["hash"], hash, "{record}");
    assert_eq!(record["gate"], build_version(&case), "{record}");
    assert!(record["pid"].as_u64().is_some_and(|pid| pid > 0), "{record}");
    assert_eq!(box_warm(&case).as_deref(), Some(hash.as_str()), "the builder holds the hash");
    let log = fs::read_to_string(warm_entry(&case, "log").expect("the warm-up wrote its log")).expect("the log can be read");
    assert!(
        log.lines().any(|line| line == format!("gate: remote warm {hash}")),
        "the warm log holds the box's line: {log}"
    );
    let lock = warm_entry(&case, "lock").expect("the warm-up made its lock");
    assert!(free(&lock), "the per-key lock is free afterwards");
}

#[test]
fn a_real_client_warm_up_withdrawn_when_consent_goes_is_deleted() {
    for background in [false, true] {
        let case = warm_client_case(if background { "warm-real-consent-bg" } else { "warm-real-consent" });
        let held = held_run(&case, "run", "run.log", &[r".\tool.exe", "out", "ran there"]);

        let (steps, outcome) = while_the_key_is_held(&case, held, background, |case| routing_with(case, &json!(null)));

        let line = format!("gate: warm: {} has not consented to warm-ups in remote.json\n", repo(&case));
        let expected = if background { (Some(0), String::new()) } else { (Some(2), line) };
        assert_eq!((outcome.code, outcome.stderr.clone()), expected, "{outcome:?}");
        assert_eq!(record(&case), None, "the record is deleted");
        assert!(calls(&steps).is_empty(), "nothing is sent: {outcome:?}");
    }
}

#[test]
fn a_real_client_warm_up_withdrawn_when_the_host_changes_is_deleted() {
    for background in [false, true] {
        let case = warm_client_case(if background { "warm-real-host-bg" } else { "warm-real-host" });
        let held = held_run(&case, "run", "run.log", &[r".\tool.exe", "out", "ran there"]);

        let (steps, outcome) = while_the_key_is_held(&case, held, background, |case| {
            let routing = json!({ "host": "other-builder", "repos": { repo(case): ["*tool*"] }, "warmup": [repo(case)] });
            write_remote_json(case, &routing.to_string());
        });

        let line = "gate: warm: remote.json's builder changed from builder to other-builder\n";
        let expected = if background {
            (Some(0), String::new())
        } else {
            (Some(2), line.to_owned())
        };
        assert_eq!((outcome.code, outcome.stderr.clone()), expected, "{outcome:?}");
        assert_eq!(record(&case), None, "the record is deleted");
        assert!(calls(&steps).is_empty(), "nothing is sent: {outcome:?}");
    }
}

#[test]
fn a_real_client_warm_up_sends_the_file_read_late() {
    let case = warm_client_case("warm-real-late-read");
    let early = warmup_hash(ONE_COMMAND.as_bytes());
    let changed = "commands = [[\"tool.exe\", \"out\", \"late\"]]\n";
    let late = warmup_hash(changed.as_bytes());
    assert_ne!(early, late, "the file really changed");
    let held = held_run(&case, "run", "run.log", &[r".\tool.exe", "out", "ran there"]);

    let (steps, outcome) = while_the_key_is_held(&case, held, false, |case| warmup_file(case, changed.as_bytes()));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight", "run"], "{outcome:?}");
    let record = record(&case).expect("a record");
    assert_eq!(
        (&record["state"], &record["hash"]),
        (&json!("warm"), &json!(late)),
        "the late hash: {record}"
    );
    assert_eq!(box_warm(&case).as_deref(), Some(late.as_str()), "the builder confirmed the hash sent");
}

#[test]
fn a_real_client_warm_up_whose_command_fails_is_recorded_commands() {
    let case = warm_client_case("warm-real-command-fails");
    warmup_file(&case, b"commands = [[\"tool.exe\", \"exit\", \"3\"]]\n");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = finish(warm_client(&case, &steps, &[]));

    assert_failed(&outcome, record(&case).as_ref(), "commands", "command stopped with status 3");
    assert!(
        outcome
            .stderr
            .lines()
            .any(|line| line == "gate: remote warm-up stopped at command 1 of 1"),
        "the box's line: {outcome:?}"
    );
    assert_eq!(box_warm(&case), None, "a failed warm-up leaves no record on the builder");
}

#[test]
fn a_real_client_warm_up_refused_before_the_marker_is_recorded_refused() {
    let case = warm_client_case("warm-real-version");
    let steps = ssh_steps(&case, "ssh", &["version 0.0.0-old"]);

    let outcome = finish(warm_client(&case, &steps, &[]));

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    let record = record(&case).expect("a record");
    assert_eq!((&record["state"], &record["failure"]), (&json!("failed"), &json!("refused")), "{record}");
    assert_eq!(calls(&steps), ["preflight"], "the run call is never made: {outcome:?}");
}

#[test]
fn a_real_client_warm_up_whose_connection_drops_after_the_marker_is_recorded_commands() {
    let case = warm_client_case("warm-real-drop");
    // The drop leaves the client to stop the command and to ask the builder's boot time: a step for each call.
    let steps = ssh_steps(&case, "ssh", &["ok", "truncate 0", "ok", "ok"]);

    let outcome = finish(warm_client(&case, &steps, &[]));

    assert_failed(&outcome, record(&case).as_ref(), "commands", "the connection to the builder was lost");
    assert_eq!(calls(&steps), ["preflight", "run", "stop", "boot"], "{outcome:?}");
}

#[test]
fn a_real_client_warm_up_that_drops_before_the_marker_is_recorded_refused() {
    let case = warm_client_case("warm-real-drop-early");
    let steps = ssh_steps(&case, "ssh", &["ok", "out-then-drop", "ok", "ok"]);

    let outcome = finish(warm_client(&case, &steps, &[]));

    assert_failed(&outcome, record(&case).as_ref(), "refused", "the connection to the builder was lost");
}

#[test]
fn a_real_client_warm_up_whose_key_is_held_past_the_wait_is_refused() {
    let case = warm_client_case("warm-real-key-past");
    let (held, go) = held_run(&case, "run", "run.log", &[r".\tool.exe", "out", "ran there"]);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = finish(warm_client(&case, &steps, &["--test-warm-wait-ms", "300"]));
    fs::write(go.path(), "").expect("the go file can be made");
    let _ = held.finish();

    assert_failed(
        &outcome,
        record(&case).as_ref(),
        "refused",
        "another routed run held the work folder past the wait",
    );
    assert!(calls(&steps).is_empty(), "the warm-up sends nothing: {outcome:?}");
    let lock = warm_entry(&case, "lock").expect("the warm-up made its lock");
    assert!(free(&lock), "the per-key lock is released");
}

#[test]
fn a_real_client_warm_up_whose_lock_another_warm_up_holds_leaves_the_record_alone() {
    let case = warm_client_case("warm-real-second-holder");
    let (held, go) = held_run(&case, "run", "run.log", &[r".\tool.exe", "out", "ran there"]);
    let first_steps = ssh_steps(&case, "first", &["ok", "ok"]);
    let first = start(warm_client(&case, &first_steps, &["--test-warm-wait-ms", "3600000"]));
    first.wait_until("the first warm-up's lock and record", || {
        warm_entry(&case, "lock").is_some_and(|lock| record(&case).is_some() && !free(&lock))
    });
    let record_path = warm_entry(&case, "json").expect("the first warm-up wrote its record");
    let before = fs::read(&record_path).expect("the record can be read");
    let second_steps = ssh_steps(&case, "second", &["ok", "ok"]);

    let second = finish(warm_client(&case, &second_steps, &[]));

    assert_eq!(second.code, Some(1), "{second:?}");
    assert_eq!(second.stderr.trim_end(), "gate: a warm-up of builder is already running", "{second:?}");
    assert_eq!(fs::read(&record_path).expect("the record can be read"), before, "the record is untouched");
    assert!(calls(&second_steps).is_empty(), "the second warm-up sends nothing");
    fs::write(go.path(), "").expect("the go file can be made");
    let first = first.finish();
    assert_eq!(first.code, Some(0), "the first warm-up passes: {first:?}");
    let _ = held.finish();
}

#[test]
fn a_real_client_warm_up_waits_for_the_cap_of_another_worktree() {
    let first_case = warm_client_case("warm-real-cap-a");
    let second_case = warm_client_case("warm-real-cap-b");
    let go = Go(first_case.folder().join("ssh").join("go"));
    let first_steps = ssh_steps(&first_case, "ssh", &["ok", &format!("hold {}", shown(go.path())), "ok"]);
    let first = start(warm_client(&first_case, &first_steps, &["--test-warm-wait-ms", "3600000"]));
    first.wait_until("the first warm-up's run call", || calls(&first_steps).len() == 2);
    let second_steps = ssh_steps(&second_case, "ssh", &["ok", "ok"]);
    let second = start(warm_client_at(
        &second_case,
        &first_case.root(),
        &second_steps,
        &["--test-warm-wait-ms", "3600000"],
    ));
    second.wait_until("the second warm-up's record", || records(&first_case).len() == 2);

    fs::write(go.path(), "").expect("the go file can be made");
    let first = first.finish();
    let second = second.finish();

    assert_eq!(first.code, Some(0), "{first:?}");
    assert_eq!(second.code, Some(0), "the second waited for the cap: {second:?}");
    assert_eq!(calls(&second_steps), ["preflight", "run"], "{second:?}");
    let hash = warmup_hash(ONE_COMMAND.as_bytes());
    let ended = records(&first_case);
    assert_eq!(ended.len(), 2, "one record each: {ended:?}");
    assert!(
        ended.iter().all(|record| record["state"] == "warm" && record["hash"] == hash),
        "{ended:?}"
    );
    assert_eq!(box_warm(&first_case).as_deref(), Some(hash.as_str()));
    assert_eq!(box_warm(&second_case).as_deref(), Some(hash.as_str()));
    assert!(free(&first_case.root().join("remote").join("warm.lock")), "the cap is released");
}

#[test]
fn a_real_client_warm_up_interrupted_at_its_run_call_is_deleted() {
    let case = warm_client_case("warm-real-ctrl-c");
    let steps = ssh_steps(&case, "ssh", &["ok", &format!("say-hold started {}", never(&case)), "ok"]);

    let outcome = finish(warm_client(&case, &steps, &["--test-remote-interrupt-at", "run"]));

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert_eq!(record(&case), None, "Ctrl+C deletes the record");
    let next = ssh_steps(&case, "next", &["ok", "ok"]);
    let again = finish(warm_client(&case, &next, &[]));
    assert_eq!(again.code, Some(0), "a following warm-up takes the locks: {again:?}");
}

#[test]
fn a_real_client_warm_up_brings_nothing_back() {
    let case = warm_client_case("warm-real-nothing-back");
    let before = worktree(&case);
    let status = git(&case, &["--no-optional-locks", "status", "--porcelain=v2", "--untracked-files=all"]);
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let outcome = finish(warm_client(&case, &steps, &[]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(worktree(&case), before, "the PC worktree is unchanged");
    assert_eq!(
        git(&case, &["--no-optional-locks", "status", "--porcelain=v2", "--untracked-files=all"]),
        status,
        "git's status is unchanged"
    );
    assert!(nothing_back(&case).is_none(), "no nothing-back record");
    assert_eq!(
        remote_entries(&case, ".outputs-pending"),
        Vec::<String>::new(),
        "no outputs-pending marker"
    );
    let bare = shown(&case.folder().join("builder").join(format!("{}.git", repo(&case))));
    let back = git(&case, &["--git-dir", &bare, "for-each-ref", "--format=%(refname)", "refs/gate-back/"]);
    assert_eq!(back, "", "the box wrote no back commit");
}

#[test]
fn a_real_client_warm_up_of_a_warm_folder_runs_again() {
    let case = warm_client_case("warm-real-twice");
    let first = finish(warm_client(&case, &ssh_steps(&case, "first", &["ok", "ok"]), &[]));
    assert_eq!(first.code, Some(0), "{first:?}");
    let second_steps = ssh_steps(&case, "second", &["ok", "ok"]);

    let second = finish(warm_client(&case, &second_steps, &[]));

    assert_eq!(second.code, Some(0), "{second:?}");
    assert_eq!(calls(&second_steps), ["preflight", "run"], "a warm folder warms again: {second:?}");
    assert_eq!(record(&case).expect("a record")["state"], "warm");
}

#[test]
fn a_real_client_warm_up_a_box_refuses_is_recorded_refused() {
    let case = warm_client_case("warm-real-payload-refused");
    let steps = ssh_steps(&case, "ssh", &["ok", "say-exit 2 gate: box payload lacks Command"]);

    let outcome = finish(warm_client(&case, &steps, &[]));

    assert_failed(
        &outcome,
        record(&case).as_ref(),
        "refused",
        "the builder ended the warm-up before its commands with status 2",
    );
}

#[test]
fn a_real_client_warm_up_on_a_held_lock_says_it_or_stays_quiet() {
    let case = warm_client_case("warm-real-held-lock");
    let first = finish(warm_client(&case, &ssh_steps(&case, "first", &["ok", "ok"]), &[]));
    assert_eq!(first.code, Some(0), "{first:?}");
    let record_path = warm_entry(&case, "json").expect("the first warm-up wrote its record");
    let before = fs::read(&record_path).expect("the record can be read");
    let held = hold(&warm_entry(&case, "lock").expect("the first warm-up made its lock"));
    let steps = ssh_steps(&case, "second", &["ok", "ok"]);

    let said = finish(warm_client(&case, &steps, &[]));
    let quiet = finish(warm_client(&case, &steps, &["--background"]));

    assert_eq!(said.code, Some(1), "{said:?}");
    assert_eq!(said.stderr.trim_end(), "gate: a warm-up of builder is already running", "{said:?}");
    assert_eq!((quiet.code, quiet.stderr.as_str()), (Some(0), ""), "{quiet:?}");
    assert!(calls(&steps).is_empty(), "no call is made");
    assert_eq!(fs::read(&record_path).expect("the record can be read"), before, "the record is untouched");
    drop(held);
}

// A routed run of a consenting repo with a warm-up file, through the scripted client: the route judges the work
// folder's warm-up from the record a scripted `gate warm` left, the warm-up lock and the file, and either routes asking
// the builder for the file's hash or runs here saying why; a `cold` or `layout` refusal drops the record.

/// The command of the routed cases: fake-tool at the worktree root, which prints `ran here`.
const RAN_HERE: [&str; 3] = [r".\tool.exe", "out", "ran here"];
/// The line of a run kept here because the work folder is not warm.
const UNWARMED: &str = "gate: not routed: the work folder for this worktree on builder is not warm; gate warm warms it";
/// The line a routed run says below its `cold` fallback.
const WARM_HINT: &str = "gate: gate warm warms the work folder for this worktree on builder";

/// A heavy run of `command` in the case's folder logging to `out.log`, through the scripted client of `steps`, with
/// the git guards and `extra` before `--`.
fn routed_run(case: &Case, steps: &str, extra: &[&str], command: &[&str]) -> Outcome {
    let mut before = vec!["--test-remote-client", steps];
    before.extend(GIT_HANG_GUARDS);
    before.extend(extra);
    finish(case.gate(&heavy_words("out.log", &before, command)))
}

/// The steps of a routed run's scripted client that records its call at `call` and passes.
fn passing(case: &Case, call: &Path) -> String {
    steps(case, &[&format!("record {}", shown(call)), "status 0"])
}

/// A [`warm_case`] whose scripted `gate warm` ended as `ending` says, leaving its record.
fn recorded_case(name: &str, ending: &str) -> Case {
    let case = warm_case(name);
    let outcome = warm(&case, &steps(&case, &[ending]), &[]);
    assert!(record(&case).is_some(), "the warm-up left a record: {outcome:?}");
    case
}

/// Sets the case's record's `field` to `value`.
fn rewrite(case: &Case, field: &str, value: &Value) {
    let path = warm_entry(case, "json").expect("the case has a record");
    let mut written = record(case).expect("the case has a record");
    written[field] = value.clone();
    fs::write(path, written.to_string()).expect("the record can be rewritten");
}

/// The case's record's `at` moved back to the year 2000, past every backoff.
fn long_ago(case: &Case) {
    let at = record(case).expect("the case has a record")["at"]
        .as_str()
        .expect("the record's time is text")
        .to_owned();
    rewrite(case, "at", &json!(format!("2000{}", &at[4..])));
}

/// Asserts `outcome` ran the command here after saying `line`, with no call to the client of `call` and the journal's
/// last three lines those of a run kept here.
fn assert_here(case: &Case, outcome: &Outcome, line: &str, call: &Path) {
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.lines().any(|said| said == line), "{line}: {outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    assert!(!call.exists(), "no call is made: {outcome:?}");
    let events = events(case);
    let last = &events[events.len() - 3..];
    assert_eq!(names(last), ["queue", "take", "release"], "{events:#?}");
    assert!(last.iter().all(|event| event.get("remote").is_none()), "{events:#?}");
}

#[test]
fn a_consented_run_with_no_record_runs_here_and_says_the_work_folder_is_not_warm() {
    let case = warm_case("warm-route-none");
    let call = case.folder().join("call.json");

    let outcome = routed_run(&case, &passing(&case, &call), &[], &RAN_HERE);

    assert_here(&case, &outcome, UNWARMED, &call);
    assert!(!warm_folder(&case).exists(), "the route writes no record and makes no lock");
}

#[test]
fn a_run_whose_lock_is_held_runs_here_and_says_the_builder_is_warming() {
    let case = recorded_case("warm-route-held", "warmed");
    let call = case.folder().join("call.json");
    let lock = warm_entry(&case, "lock").expect("the warm-up made its lock");
    let warming = "gate: not routed: builder is warming the work folder for this worktree";
    let held = hold(&lock);

    let outcome = routed_run(&case, &passing(&case, &call), &[], &RAN_HERE);
    assert_here(&case, &outcome, warming, &call);
    fs::remove_file(warm_entry(&case, "json").expect("the record")).expect("the record can be removed");
    let outcome = routed_run(&case, &passing(&case, &call), &[], &RAN_HERE);
    assert_here(&case, &outcome, warming, &call);

    drop(held);
}

#[test]
fn a_warming_record_whose_lock_is_free_runs_here_and_says_the_work_folder_is_not_warm() {
    let case = recorded_case("warm-route-stale", "warmed");
    rewrite(&case, "state", &json!("warming"));
    let call = case.folder().join("call.json");

    let outcome = routed_run(&case, &passing(&case, &call), &[], &RAN_HERE);

    assert_here(&case, &outcome, UNWARMED, &call);
}

#[test]
fn a_warm_record_of_this_host_and_file_routes_and_asks_the_box_for_its_hash() {
    let case = recorded_case("warm-route-ready", "warmed");
    let hash = warmup_hash(ONE_COMMAND.as_bytes());
    for gate in [None, Some("0.0.0-another-build")] {
        if let Some(gate) = gate {
            rewrite(&case, "gate", &json!(gate));
        }
        let call = case.folder().join(format!("call-{}.json", gate.unwrap_or("this")));

        let outcome = routed_run(&case, &passing(&case, &call), &[], &RAN_HERE);

        assert_eq!(outcome.code, Some(0), "{outcome:?}");
        assert!(!outcome.stderr.contains("not routed"), "{outcome:?}");
        let call = recorded(&call);
        assert_eq!(call["warmth"], json!({"needs": hash}), "a warm record of any gate build routes: {call}");
    }
}

#[test]
fn a_warm_record_of_another_host_or_file_runs_here_and_says_the_work_folder_is_not_warm() {
    let case = recorded_case("warm-route-other-host", "warmed");
    rewrite(&case, "host", &json!("other-builder"));
    let call = case.folder().join("call.json");
    let outcome = routed_run(&case, &passing(&case, &call), &[], &RAN_HERE);
    assert_here(&case, &outcome, UNWARMED, &call);

    let case = recorded_case("warm-route-other-file", "warmed");
    warmup_file(&case, b"commands = [[\"tool.exe\", \"out\", \"changed\"]]\n");
    let call = case.folder().join("call.json");
    let outcome = routed_run(&case, &passing(&case, &call), &[], &RAN_HERE);
    assert_here(&case, &outcome, UNWARMED, &call);
}

#[test]
fn a_failure_within_its_backoff_runs_here_and_says_when_it_failed() {
    // Each record's `at` is written just before its routed run, so only the route's own start-up stands between it and
    // the judgement: a machine that starves the route past the refused class's 60 s backoff still sees a start.
    let case = recorded_case("warm-route-failed-commands", "warm-failed commands command 1 stopped");
    rewrite(&case, "at", &json!((Timestamp::now() - SignedDuration::from_mins(5)).to_string()));
    let call = case.folder().join("call.json");
    let outcome = routed_run(&case, &passing(&case, &call), &[], &RAN_HERE);
    let (before, after) = ("gate: not routed: the warm-up failed ", " min ago: command 1 stopped");
    let line = outcome
        .stderr
        .lines()
        .find(|line| line.starts_with(before) && line.ends_with(after))
        .expect("the failure's line is said");
    let minutes = line.trim_start_matches(before).trim_end_matches(after);
    assert!(minutes.parse::<u64>().is_ok_and(|minutes| (5..30).contains(&minutes)), "{line}");
    assert_here(&case, &outcome, line, &call);

    let case = recorded_case("warm-route-failed-refused", "warm-failed refused the builder said no");
    rewrite(&case, "at", &json!(Timestamp::now().to_string()));
    let call = case.folder().join("call.json");
    let outcome = routed_run(&case, &passing(&case, &call), &[], &RAN_HERE);
    let line = "gate: not routed: the warm-up failed less than a minute ago: the builder said no";
    assert_here(&case, &outcome, line, &call);
}

#[test]
fn a_failure_past_its_backoff_or_of_another_gate_build_runs_here_and_says_the_work_folder_is_not_warm() {
    let case = recorded_case("warm-route-failed-past", "warm-failed commands command 1 stopped");
    long_ago(&case);
    let call = case.folder().join("call.json");
    let outcome = routed_run(&case, &passing(&case, &call), &[], &RAN_HERE);
    assert_here(&case, &outcome, UNWARMED, &call);

    let case = recorded_case("warm-route-failed-build", "warm-failed commands command 1 stopped");
    rewrite(&case, "gate", &json!("0.0.0-another-build"));
    let call = case.folder().join("call.json");
    let outcome = routed_run(&case, &passing(&case, &call), &[], &RAN_HERE);
    assert_here(&case, &outcome, UNWARMED, &call);
}

#[test]
fn a_run_with_no_consent_or_no_warmup_file_routes_and_asks_for_no_warmth() {
    let case = warm_case("warm-route-unasked");
    let mut next = 0;
    let mut routes_unasked = |case: &Case| {
        next += 1;
        let call = case.folder().join(format!("call-{next}.json"));
        let outcome = routed_run(case, &passing(case, &call), &[], &RAN_HERE);
        assert_eq!(outcome.code, Some(0), "{outcome:?}");
        assert!(!outcome.stderr.contains("warm"), "{outcome:?}");
        let call = recorded(&call);
        assert_eq!(call.get("warmth"), Some(&Value::Null), "{call}");
        assert!(!warm_folder(case).exists(), "nothing is made under remote\\warm");
    };
    for warmup in [json!(null), json!(["another-repo"]), json!(5)] {
        routing_with(&case, &warmup);
        routes_unasked(&case);
    }
    routing_with(&case, &json!([repo(&case)]));
    fs::remove_file(case.folder().join(".gate").join("warmup.toml")).expect("the file can be removed");
    routes_unasked(&case);
}

#[test]
fn a_warmup_file_problem_runs_here_and_says_it() {
    let case = warm_case("warm-route-file-problem");
    warmup_file(&case, b"commands = []\n");
    let call = case.folder().join("call.json");

    let outcome = routed_run(&case, &passing(&case, &call), &[], &RAN_HERE);

    assert_here(&case, &outcome, "gate: not routed: .gate/warmup.toml: declares no commands", &call);
}

#[test]
fn an_unreadable_warm_record_runs_here_and_says_it() {
    let case = recorded_case("warm-route-unreadable", "warmed");
    let path = warm_entry(&case, "json").expect("the record");
    fs::write(&path, "{").expect("the record can be broken");
    let call = case.folder().join("call.json");

    let outcome = routed_run(&case, &passing(&case, &call), &[], &RAN_HERE);

    let line = outcome
        .stderr
        .lines()
        .find(|line| line.starts_with("gate: not routed: the warm-up record "))
        .expect("the record's line is said");
    let file = path.file_name().expect("the record has a name").to_string_lossy().into_owned();
    assert!(line.contains(&format!(r"\{file} cannot be read: not JSON")), "{line}");
    assert_here(&case, &outcome, line, &call);
    assert_eq!(
        fs::read_to_string(&path).expect("the record can be read"),
        "{",
        "the record is left alone"
    );
}

#[test]
fn a_run_kept_here_for_another_reason_never_reads_the_warmup() {
    let case = recorded_case("warm-route-other-reason", "warmed");
    fs::write(warm_entry(&case, "json").expect("the record"), "{").expect("the record can be broken");
    let call = case.folder().join("call.json");
    for (extra, command) in [
        (vec!["--local"], RAN_HERE.to_vec()),
        (Vec::new(), vec!["cmd.exe", "/d", "/c", "echo ran here"]),
    ] {
        let outcome = routed_run(&case, &passing(&case, &call), &extra, &command);

        assert_eq!(outcome.code, Some(0), "{outcome:?}");
        assert!(!outcome.stderr.contains("warm"), "no warm line: {outcome:?}");
        assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
        assert!(!call.exists(), "no call is made");
    }

    let steps = passing(&case, &call);
    let mut words = vec!["--test-remote-client", steps.as_str()];
    words.extend(GIT_HANG_GUARDS);
    let mut command = case.gate(&heavy_words("out.log", &words, &[r".\tool.exe", "out"]));
    command.arg(OsString::from_wide(&[u16::from(b'x'), 0xD800]));
    let outcome = finish(command);
    assert!(
        outcome.stderr.contains("gate: not routed: a word of the command is not valid Unicode"),
        "{outcome:?}"
    );
    assert!(!outcome.stderr.contains("warm"), "no warm line: {outcome:?}");
    assert!(!call.exists(), "no call is made");
}

/// A [`recorded_case`] left warm whose routed run the scripted client refuses for `reason`: the outcome.
fn refused(name: &str, reason: &str) -> (Case, Outcome) {
    let case = recorded_case(name, "warmed");
    let outcome = routed_run(&case, &steps(&case, &[&format!("refuse {reason}")]), &[], &RAN_HERE);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    crate::routing::assert_refused(&events(&case), reason);
    (case, outcome)
}

#[test]
fn a_cold_refusal_runs_here_removes_the_record_and_says_gate_warm_warms_it() {
    let (case, outcome) = refused("warm-route-cold", "cold");

    let lines: Vec<&str> = outcome.stderr.lines().collect();
    let cold = lines
        .iter()
        .position(|line| *line == "gate: remote builder cold; running here")
        .expect("the cold line is said");
    assert_eq!(lines.get(cold + 1), Some(&WARM_HINT), "the hint follows the cold line: {outcome:?}");
    assert_eq!(record(&case), None, "the record is removed");
}

#[test]
fn a_layout_refusal_removes_the_record_and_says_no_warm_hint() {
    let (case, outcome) = refused("warm-route-layout", "layout");

    assert!(outcome.stderr.contains("gate: remote builder layout; running here"), "{outcome:?}");
    assert!(!outcome.stderr.contains(WARM_HINT), "{outcome:?}");
    assert_eq!(record(&case), None, "the record is removed");
}

#[test]
fn another_fallback_keeps_the_warm_record() {
    for reason in ["version", "tools", "inputs", "parity", "unreachable", "rebooted"] {
        let case = recorded_case(&format!("warm-route-keeps-{reason}"), "warmed");
        let before = fs::read(warm_entry(&case, "json").expect("the record")).expect("the record can be read");
        let outcome = routed_run(&case, &steps(&case, &[&format!("refuse {reason}")]), &[], &RAN_HERE);

        assert_eq!(outcome.code, Some(0), "{outcome:?}");
        assert!(
            outcome.stderr.contains(&format!("gate: remote builder {reason}; running here")),
            "{outcome:?}"
        );
        assert!(!outcome.stderr.contains(WARM_HINT), "{outcome:?}");
        let after = fs::read(warm_entry(&case, "json").expect("the record is kept")).expect("the record can be read");
        assert_eq!(after, before, "{reason} keeps the record as it was");
    }
}

#[test]
fn a_real_client_run_on_a_folder_the_box_holds_no_warmth_for_falls_back_cold_and_removes_the_record() {
    let case = warm_client_case("warm-real-route-cold");
    let warmed = warm(&case, &steps(&case, &["warmed"]), &[]);
    assert_eq!(warmed.code, Some(0), "the scripted warm-up leaves a warm record: {warmed:?}");
    let steps = ssh_steps(&case, "ssh", &["ok"]);

    let outcome = finish(client_gate(&case, "run.log", &steps, &RAN_HERE));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: remote builder cold; running here"), "{outcome:?}");
    assert!(outcome.stderr.lines().any(|line| line == WARM_HINT), "{outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight"], "nothing is pushed or run: {outcome:?}");
    assert_eq!(record(&case), None, "the record is removed");
    assert_eq!(box_warm(&case), None, "the builder holds no warmth");
}

#[test]
fn a_cold_refusal_whose_record_cannot_be_removed_says_so_before_the_hint() {
    let case = recorded_case("warm-route-cold-held", "warmed");
    let path = warm_entry(&case, "json").expect("the record");
    // Held open sharing reading alone: the route can read the record, but nothing can delete it.
    let held = OpenOptions::new().read(true).share_mode(1).open(&path).expect("the record can be held");

    let outcome = routed_run(&case, &steps(&case, &["refuse cold"]), &[], &RAN_HERE);
    drop(held);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let lines: Vec<&str> = outcome.stderr.lines().collect();
    let cold = lines
        .iter()
        .position(|line| *line == "gate: remote builder cold; running here")
        .expect("the cold line is said");
    let failed = lines.get(cold + 1).copied().unwrap_or_default();
    let file = path.file_name().expect("the record has a name").to_string_lossy().into_owned();
    assert!(
        failed.starts_with("gate: cannot remove the warm-up record ") && failed.contains(&format!(r"\{file}: ")),
        "the failure follows the cold line: {outcome:?}"
    );
    assert_eq!(lines.get(cold + 2), Some(&WARM_HINT), "the hint follows the failure: {outcome:?}");
    assert!(record(&case).is_some(), "the record is still there");
}

#[test]
fn a_real_client_folder_gate_warm_warmed_routes_and_stays_warm() {
    let case = warm_client_case("warm-real-route-warm");
    let warmed = finish(warm_client(&case, &ssh_steps(&case, "warm", &["ok", "ok"]), &[]));
    assert_eq!(warmed.code, Some(0), "{warmed:?}");
    let hash = warmup_hash(ONE_COMMAND.as_bytes());
    for run in ["first", "second"] {
        let steps = ssh_steps(&case, run, &["ok", "ok", "ok", "ok"]);

        let outcome = finish(client_gate(&case, &format!("{run}.log"), &steps, &RAN_HERE));

        assert_eq!(outcome.code, Some(0), "{outcome:?}");
        assert!(
            !outcome.stderr.contains("running here") && !outcome.stderr.contains("not routed") && !outcome.stderr.contains("cold"),
            "the {run} run ran on the builder: {outcome:?}"
        );
        assert!(outcome.stdout.contains("ran here"), "{outcome:?}");
        let made = calls(&steps);
        assert!(made.starts_with(&["preflight".to_owned(), "run".to_owned()]), "{made:?}: {outcome:?}");
        let record = record(&case).expect("a record");
        assert_eq!((&record["state"], &record["hash"]), (&json!("warm"), &json!(hash)), "{record}");
        assert_eq!(
            box_warm(&case).as_deref(),
            Some(hash.as_str()),
            "the builder still holds the hash after the {run} run"
        );
    }
}
