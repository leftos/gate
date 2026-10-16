//! The sync-back's quarantine, through the whole client (`--test-remote-transport local:<folder>`, whose builder is
//! `gate box` run here on a builder root in the case folder, each ssh call made through the `fake-ssh` stand-in): the
//! objects a fetch brings reach the worktree's object store when the back commit is applied or kept, and no quarantine
//! folder is left behind, a leftover of a gate that ended included.

use std::fs;
use std::process::{Command, Stdio};

use gate_router::repo_name;
use serde_json::{Value, json};

use crate::support::{Case, GATE, GIT_REDIRECTS, allow_breakaway, copy_tool, events, finish, git_init, heavy_words, shown, write_remote_json};

const HOST: &str = "builder";
/// The suite's stand-in for ssh, which `--test-remote-ssh` names.
const FAKE_SSH: &str = env!("CARGO_BIN_EXE_fake-ssh");
/// The variable naming the stand-in's steps file; the stand-in logs each call's verb to `<steps>.log`.
const STEPS_VARIABLE: &str = "GATE_FAKE_SSH_STEPS";
/// The variable naming the gate the stand-in runs `box <verb> --stdin` with, feeding it the request on standard input.
const GATE_VARIABLE: &str = "GATE_FAKE_SSH_GATE";
/// What a sync-back case's snapshot leaves out: the gate's state root, LOCALAPPDATA and logs, the builder root and the
/// stand-in's files, and `.tmp/`, which the client writes in. `tool.exe` is sent, so the builder runs it.
const CLIENT_IGNORED: &str = "state/\nappdata/\n*.log\nbuilder/\nssh/\n.tmp/\n";
/// What the name of a quarantine folder a sync-back makes in the object store starts with.
const INCOMING: &str = "incoming-gate-";
/// A pid no process has: Windows pids are multiples of 4.
const DEAD_PID: u32 = u32::MAX - 1;

/// A case folder made a checkout of its own holding fake-tool as `tool.exe`, which its snapshot sends, with a routing
/// file that sends its `*tool*` commands to the builder with no output globs. The suite enters a job that allows
/// breakaway first, as the box's command needs.
fn sync_back_case(name: &str) -> Case {
    allow_breakaway();
    let case = Case::new(name);
    git_init(&case);
    copy_tool(&case, "tool.exe");
    fs::write(case.folder().join(".gitignore"), CLIENT_IGNORED).expect("the .gitignore can be written");
    fs::create_dir_all(case.folder().join("ssh")).expect("the stand-in's folder can be made");
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let routing = json!({ "host": HOST, "slots": 2, "repos": { &repo: ["*tool*"] }, "outputs": { &repo: [] } });
    write_remote_json(&case, &routing.to_string());
    case
}

/// The gate of a routed run of `command` logging to `out.log` through the client: the builder root `builder` in the
/// case folder, each ssh call made through the stand-in, whose steps answer the preflight and the run, the retry
/// pause 1 ms and the ceiling an hour.
fn sync_back_gate(case: &Case, command: &[&str]) -> Command {
    let steps = case.folder().join("ssh").join("ssh.steps");
    fs::write(&steps, "ok\nok").expect("the steps file can be written");
    let transport = format!("local:{}", shown(&case.folder().join("builder")));
    let seams = [
        "--test-remote-transport",
        transport.as_str(),
        "--test-remote-ssh",
        FAKE_SSH,
        "--test-remote-retry-pause-ms",
        "1",
        "--test-git-limit-ms",
        "3600000",
    ];
    let mut words = heavy_words("out.log", &seams, command);
    assert_eq!(words[3], "--timeout", "the ceiling's value follows it");
    words[4] = "3600";
    let mut gate = case.gate(&words);
    gate.env(STEPS_VARIABLE, &steps).env(GATE_VARIABLE, GATE);
    gate
}

/// What git with `args` printed on stdout in the case folder, its trailing line end cut, with none of the variables
/// that point git elsewhere; fails the case when git fails.
fn git_out(case: &Case, args: &[&str]) -> String {
    let mut command = Command::new("git");
    for name in GIT_REDIRECTS {
        command.env_remove(name);
    }
    crate::support::git_home_for(&mut command);
    let output = command
        .args(args)
        .current_dir(case.folder())
        .stdin(Stdio::null())
        .output()
        .expect("git starts");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).expect("git's output is UTF-8").trim_end().to_owned()
}

/// The work-folder key the run's queue line names.
fn key_of(events: &[Value]) -> String {
    events[0]["key"].as_str().expect("the queue line has a key").to_owned()
}

/// The back commit the builder holds for the case's run, from its bare repository's `refs/gate-back/<key>`.
fn builder_back(case: &Case) -> String {
    let repo = repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy());
    let bare = shown(&case.folder().join("builder").join(format!("{repo}.git")));
    let reference = format!("refs/gate-back/{}", key_of(&events(case)));
    let back = git_out(case, &["--git-dir", &bare, "for-each-ref", "--format=%(objectname)", &reference]);
    assert_eq!(back.len(), 40, "the box wrote the back commit: {back:?}");
    back
}

/// The quarantine folders a sync-back left in the case's own object store.
fn quarantines(case: &Case) -> Vec<String> {
    fs::read_dir(case.folder().join(".git").join("objects"))
        .expect("the objects folder can be listed")
        .map(|entry| entry.expect("an entry can be read").file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(INCOMING))
        .collect()
}

/// A synced-back run moved the back commit's objects into the worktree's own store, read with no variable pointing
/// git elsewhere, left it whole as `git fsck` reads it, and left no quarantine.
#[test]
fn a_synced_back_run_leaves_its_back_commit_in_the_store_and_no_quarantine() {
    let case = sync_back_case("syncback-quarantine-synced");

    let outcome = finish(sync_back_gate(&case, &[r".\tool.exe", "mark", "made-on-box.txt"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: synced back 1 file(s): made-on-box.txt"), "{outcome:?}");
    let back = builder_back(&case);
    assert_eq!(git_out(&case, &["cat-file", "-e", &back]), "", "the back commit is in the store");
    git_out(&case, &["fsck", "--no-progress"]);
    assert_eq!(quarantines(&case), Vec::<String>::new(), "no quarantine is left: {outcome:?}");
}

/// A back commit refused for touching `.gitignore` is kept at `refs/gate-remote/<key>-back`, which resolves to a commit
/// in the worktree's own store, the store whole, and no quarantine left.
#[test]
fn a_kept_back_commit_resolves_in_the_store_and_leaves_no_quarantine() {
    let case = sync_back_case("syncback-quarantine-kept");

    let outcome = finish(sync_back_gate(&case, &[r".\tool.exe", "mark", ".gitignore"]));

    assert_eq!(outcome.code, Some(1), "a passing run becomes 1: {outcome:?}");
    assert!(outcome.stderr.contains("they touch .gitignore"), "{outcome:?}");
    let back = builder_back(&case);
    let kept = format!("refs/gate-remote/{}-back", key_of(&events(&case)));
    assert_eq!(git_out(&case, &["rev-parse", &kept]), back, "the changes are kept at the key's -back ref");
    assert_eq!(git_out(&case, &["cat-file", "-t", &kept]), "commit", "the kept ref resolves in the store");
    git_out(&case, &["fsck", "--no-progress"]);
    assert_eq!(quarantines(&case), Vec::<String>::new(), "no quarantine is left: {outcome:?}");
}

/// A quarantine a gate that ended left, planted before the run, is swept by the run's sync-back, which says nothing of
/// it.
#[test]
fn a_leftover_quarantine_of_a_gate_that_ended_is_swept_by_the_next_sync_back() {
    let case = sync_back_case("syncback-quarantine-swept");
    let leftover = case
        .folder()
        .join(".git")
        .join("objects")
        .join(format!("{INCOMING}{DEAD_PID}-1-0123456789ab"));
    fs::create_dir_all(leftover.join("pack")).expect("the leftover can be made");
    fs::write(leftover.join("pack").join("pack-0123.pack"), "left").expect("the leftover's pack can be written");

    let outcome = finish(sync_back_gate(&case, &[r".\tool.exe", "mark", "made-on-box.txt"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!leftover.exists(), "the leftover is swept: {outcome:?}");
    assert!(!outcome.stderr.contains("could not remove"), "a removal says nothing: {outcome:?}");
    assert_eq!(quarantines(&case), Vec::<String>::new(), "no quarantine is left: {outcome:?}");
}
