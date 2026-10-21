//! What a sync-back takes of the worktree's checkout conversions, through the whole client
//! (`--test-remote-transport local:<folder>`, whose builder is `gate box` run here on a builder root in the case folder,
//! each ssh call made through the `fake-ssh` stand-in): a back commit that writes or deletes a path through a checkout
//! filter or `ident`, or writes a `.lfsconfig`, is kept and fails a passing run, and the apply cap counts a path
//! converted to CRLF twice and one with a `working-tree-encoding` four times.

use std::fs;
use std::process::{Command, Stdio};

use gate_router::repo_name;
use serde_json::json;

use crate::remote_client::GIT_HANG_GUARDS;
use crate::support::{
    Case, GATE, GIT_REDIRECTS, Outcome, allow_breakaway, copy_tool, events, finish, git, git_init, heavy_words, shown, write_remote_json,
};

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
/// The apply cap every size case runs with: 1 MiB.
const CAP: &str = "1048576";
/// The line a back commit refused for writing 1,200,000 bytes under [`CAP`] ends with, before its `see git diff`.
const OVER_CAP: &str = "they would write 1.1 MiB into the worktree, more than 1.0 MiB";

/// A case folder made a checkout of its own holding fake-tool as `tool.exe`, which its snapshot sends, with a routing
/// file that sends its `*tool*` commands to the builder with no output globs, and the attributes `attributes` in its
/// `.gitattributes`, which the snapshot sends too, when there are any. The suite enters a job that allows breakaway
/// first, as the box's command needs.
fn conversion_case(name: &str, attributes: &str) -> Case {
    allow_breakaway();
    let case = Case::new(name);
    git_init(&case);
    copy_tool(&case, "tool.exe");
    fs::write(case.folder().join(".gitignore"), CLIENT_IGNORED).expect("the .gitignore can be written");
    if !attributes.is_empty() {
        fs::write(case.folder().join(".gitattributes"), attributes).expect("the .gitattributes can be written");
    }
    fs::create_dir_all(case.folder().join("ssh")).expect("the stand-in's folder can be made");
    routed(&case, &["*tool*"]);
    case
}

/// The name the case's checkout routes by.
fn repo_of(case: &Case) -> String {
    repo_name(&case.folder().file_name().expect("the case folder has a name").to_string_lossy())
}

/// Writes the routing file that sends the case's commands matching `patterns` to the builder with no output globs.
fn routed(case: &Case, patterns: &[&str]) {
    let repo = repo_of(case);
    let routing = json!({ "host": HOST, "slots": 2, "repos": { &repo: patterns }, "outputs": { &repo: [] } });
    write_remote_json(case, &routing.to_string());
}

/// The gate of a routed run of `command` logging to `out.log` through the client, with the seams `extra`: the builder
/// root `builder` in the case folder, each ssh call made through the stand-in, whose steps answer the preflight and the
/// run, the retry pause 1 ms and the ceiling an hour.
fn conversion_gate(case: &Case, extra: &[&str], command: &[&str]) -> Command {
    let steps = case.folder().join("ssh").join("ssh.steps");
    fs::write(&steps, "ok\nok").expect("the steps file can be written");
    let transport = format!("local:{}", shown(&case.folder().join("builder")));
    let mut seams = vec![
        "--test-remote-transport",
        transport.as_str(),
        "--test-remote-ssh",
        FAKE_SSH,
        "--test-remote-retry-pause-ms",
        "1",
    ];
    seams.extend(GIT_HANG_GUARDS);
    seams.extend_from_slice(extra);
    let mut words = heavy_words("out.log", &seams, command);
    assert_eq!(words[3], "--timeout", "the ceiling's value follows it");
    words[4] = "3600";
    let mut gate = case.gate(&words);
    gate.env(STEPS_VARIABLE, &steps)
        .env(GATE_VARIABLE, GATE)
        .env(crate::support::ROOT_WALK_STOP_VARIABLE, case.folder());
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

/// Writes `attributes` as the case's `.git/info/attributes`, which no snapshot sends.
fn local_attributes(case: &Case, attributes: &str) {
    let info = case.folder().join(".git").join("info");
    fs::create_dir_all(&info).expect("the info folder can be made");
    fs::write(info.join("attributes"), attributes).expect("the local attributes can be written");
}

/// `gate` made to read no global or system config and no global or system attributes, as git finds them through an
/// empty home under the case's `appdata`, so this PC's own `core.eol` and `* text=auto eol=lf` decide nothing.
fn isolated(case: &Case, gate: &mut Command) {
    let home = case.folder().join("appdata").join("home");
    fs::create_dir_all(&home).expect("the empty home can be made");
    gate.env("HOME", &home)
        .env("XDG_CONFIG_HOME", &home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_ATTR_NOSYSTEM", "1");
}

/// The back commit the builder holds for the case's run, from its bare repository's `refs/gate-back/<key>`, and the
/// key.
fn builder_back(case: &Case) -> (String, String) {
    let key = events(case)[0]["key"].as_str().expect("the queue line has a key").to_owned();
    let bare = shown(&case.folder().join("builder").join(format!("{}.git", repo_of(case))));
    let back = git_out(case, &["--git-dir", &bare, "rev-parse", &format!("refs/gate-back/{key}")]);
    (back, key)
}

/// The back commit of the case's run is kept at the key's -back ref, and its line ended with `see git diff <snapshot>
/// <back>`: that ending, the snapshot being the kept commit's parent.
fn assert_kept(case: &Case, outcome: &Outcome) -> String {
    let (back, key) = builder_back(case);
    let kept = format!("refs/gate-remote/{key}-back");
    assert_eq!(
        git_out(case, &["rev-parse", &kept]),
        back,
        "the changes are kept at the key's -back ref: {outcome:?}"
    );
    let snapshot = git_out(case, &["rev-parse", &format!("{kept}^")]);
    format!("see git diff {} {}", &snapshot[..8], &back[..8])
}

/// The case's run, refused: it failed a passing run with the line `gate: remote changes not applied: <said>; see git
/// diff <snapshot> <back>`, the back commit the builder holds is kept at the key's -back ref, and the worktree has no
/// change git sees, `before` being what `git status --porcelain` said before the run.
fn assert_refused(case: &Case, outcome: &Outcome, before: &str, said: &str) {
    assert_eq!(outcome.code, Some(1), "a passing run becomes 1: {outcome:?}");
    let line = format!("gate: remote changes not applied: {said}; {}", assert_kept(case, outcome));
    assert!(outcome.stderr.lines().any(|shown| shown == line), "{line:?} in {outcome:?}");
    assert_eq!(
        git_out(case, &["status", "--porcelain"]),
        before,
        "the worktree is untouched: {outcome:?}"
    );
}

/// The worktree of `case` has no file `path`, which the box's command wrote.
fn assert_absent(case: &Case, path: &str, outcome: &Outcome) {
    assert!(!case.folder().join(path).exists(), "the box's {path} is not here: {outcome:?}");
}

/// A back commit writing a path the tracked `.gitattributes` gives `filter=lfs` is kept: git-lfs would fetch whatever
/// object its pointer names.
#[test]
fn a_back_commit_writing_an_lfs_path_is_kept_and_fails_a_passing_run() {
    let case = conversion_case("syncback-conversions-lfs", "*.lfs filter=lfs diff=lfs merge=lfs -text\n");
    let before = git_out(&case, &["status", "--porcelain"]);

    let outcome = finish(conversion_gate(&case, &[], &[r".\tool.exe", "mark", "big.lfs"]));

    let said = "big.lfs is written through a checkout filter (filter=lfs), which a sync-back does not take";
    assert_refused(&case, &outcome, &before, said);
    assert_absent(&case, "big.lfs", &outcome);
}

/// Any filter driver is refused, not only LFS's: one the repository does not even define.
#[test]
fn a_back_commit_writing_a_path_of_a_custom_filter_is_kept() {
    let case = conversion_case("syncback-conversions-custom", "*.foo filter=foo\n");
    let before = git_out(&case, &["status", "--porcelain"]);

    let outcome = finish(conversion_gate(&case, &[], &[r".\tool.exe", "mark", "made.foo"]));

    let said = "made.foo is written through a checkout filter (filter=foo), which a sync-back does not take";
    assert_refused(&case, &outcome, &before, said);
    assert_absent(&case, "made.foo", &outcome);
}

/// `ident` expands each `$Id$` the apply writes, so a path it is set on is refused.
#[test]
fn a_back_commit_writing_an_ident_path_is_kept() {
    let case = conversion_case("syncback-conversions-ident", "*.id ident\n");
    let before = git_out(&case, &["status", "--porcelain"]);

    let outcome = finish(conversion_gate(&case, &[], &[r".\tool.exe", "mark", "made.id"]));

    let said = "made.id is written through a checkout filter (ident), which a sync-back does not take";
    assert_refused(&case, &outcome, &before, said);
    assert_absent(&case, "made.id", &outcome);
}

/// The attributes are the worktree's: a filter set in `.git/info/attributes`, which no snapshot sends, is caught too.
#[test]
fn a_filter_set_in_the_local_attributes_is_caught() {
    let case = conversion_case("syncback-conversions-local", "");
    local_attributes(&case, "*.cap filter=cap\n");
    let before = git_out(&case, &["status", "--porcelain"]);

    let outcome = finish(conversion_gate(&case, &[], &[r".\tool.exe", "mark", "made.cap"]));

    let said = "made.cap is written through a checkout filter (filter=cap), which a sync-back does not take";
    assert_refused(&case, &outcome, &before, said);
    assert_absent(&case, "made.cap", &outcome);
}

/// A back commit that deletes a filtered file is refused too, since `git apply` reads the file it deletes through the
/// filter's clean side: the file stays.
#[test]
fn a_back_commit_deleting_a_filtered_file_is_kept() {
    let case = conversion_case("syncback-conversions-delete", "*.x filter=foo\n");
    routed(&case, &["*tool*", "cmd *"]);
    fs::write(case.folder().join("made.x"), "kept\n").expect("the filtered file can be written");
    let before = git_out(&case, &["status", "--porcelain"]);

    let outcome = finish(conversion_gate(&case, &[], &["cmd", "/c", "del", "made.x"]));

    let said = "made.x is written through a checkout filter (filter=foo), which a sync-back does not take";
    assert_refused(&case, &outcome, &before, said);
    let content = fs::read_to_string(case.folder().join("made.x")).expect("the filtered file is still here");
    assert_eq!(content, "kept\n", "{outcome:?}");
}

/// The paths are fed to `git check-attr` in slices, a `test-seams` build's two at most: a filter on the third path the
/// back commit writes, in the second slice, is caught.
#[test]
fn a_filter_in_a_later_check_attr_slice_is_caught() {
    let case = conversion_case("syncback-conversions-slices", "*.foo filter=foo\n");
    let before = git_out(&case, &["status", "--porcelain"]);

    let outcome = finish(conversion_gate(
        &case,
        &[],
        &[r".\tool.exe", "mark", "a.txt", "mark", "b.txt", "mark", "c.foo"],
    ));

    let said = "c.foo is written through a checkout filter (filter=foo), which a sync-back does not take";
    assert_refused(&case, &outcome, &before, said);
    for path in ["a.txt", "b.txt", "c.foo"] {
        assert_absent(&case, path, &outcome);
    }
}

/// A `git check-attr` that fails ends the run with its failure and keeps the back commit. `GIT_ATTR_SOURCE` set to
/// `FETCH_HEAD^{tree}` reads the empty tree planted there, in the checkout for the snapshot and in the builder's bare
/// repository, made beforehand, for the fetch's pack; but once the fetch points the checkout's `FETCH_HEAD` at the back
/// commit, whose objects are still in the quarantine `git check-attr` does not read, git cannot find its tree.
#[test]
fn a_failing_check_attr_keeps_the_back_commit() {
    let case = conversion_case("syncback-conversions-check-fails", "");
    let bare = shown(&case.folder().join("builder").join(format!("{}.git", repo_of(&case))));
    git_out(&case, &["init", "--bare", "-q", "--", &bare]);
    for git_dir in [".git", bare.as_str()] {
        let empty = git_out(&case, &["--git-dir", git_dir, "hash-object", "-w", "-t", "tree", "--stdin"]);
        let planted = case.folder().join(git_dir).join("FETCH_HEAD");
        fs::write(planted, format!("{empty}\t\tplanted\n")).expect("FETCH_HEAD can be planted");
    }
    let mut gate = conversion_gate(&case, &[], &[r".\tool.exe", "mark", "made.txt"]);
    gate.env("GIT_ATTR_SOURCE", "FETCH_HEAD^{tree}");

    let outcome = finish(gate);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    let failed = outcome
        .stderr
        .lines()
        .find(|line| line.starts_with("gate: the remote run failed: git check-attr -z --stdin "));
    let failed = failed.unwrap_or_default();
    assert!(!failed.is_empty(), "the check-attr failure is the run's: {outcome:?}");
    let see = assert_kept(&case, &outcome);
    assert!(failed.ends_with(&format!("; the changes are kept: {see}")), "{failed:?}");
    assert_absent(&case, "made.txt", &outcome);
}

/// A `.lfsconfig`, which git-lfs reads from the worktree, is never written, at any depth.
#[test]
fn a_back_commit_writing_an_lfsconfig_is_kept() {
    let case = conversion_case("syncback-conversions-lfsconfig", "");
    let before = git_out(&case, &["status", "--porcelain"]);

    let outcome = finish(conversion_gate(&case, &[], &[r".\tool.exe", "write", "sub/.lfsconfig", "lfs"]));

    let said = "they touch sub/.lfsconfig, which a sync-back never writes";
    assert_refused(&case, &outcome, &before, said);
    assert_absent(&case, "sub/.lfsconfig", &outcome);
}

/// Under `core.autocrlf=true` a text file of 600,000 bytes, under the 1 MiB cap, is counted as the 1,200,000 bytes its
/// CRLF checkout may write, over it.
#[test]
fn a_text_file_under_autocrlf_counts_twice_toward_the_apply_cap() {
    let case = conversion_case("syncback-conversions-autocrlf", "");
    git(&case, &["config", "core.autocrlf", "true"]);
    let before = git_out(&case, &["status", "--porcelain"]);
    let mut gate = conversion_gate(
        &case,
        &["--test-remote-apply-cap-bytes", CAP],
        &[r".\tool.exe", "fill", "big.txt", "600000"],
    );
    isolated(&case, &mut gate);

    let outcome = finish(gate);

    assert_refused(&case, &outcome, &before, OVER_CAP);
    assert_absent(&case, "big.txt", &outcome);
}

/// With `core.autocrlf=false` and no `core.eol`, Git for Windows writes a `text=auto` file with its native CRLF line
/// ends: 600,000 bytes count as 1,200,000.
#[test]
fn a_text_auto_file_with_no_core_eol_counts_twice_toward_the_apply_cap() {
    let case = conversion_case("syncback-conversions-native", "*.txt text=auto\n");
    git(&case, &["config", "core.autocrlf", "false"]);
    let before = git_out(&case, &["status", "--porcelain"]);
    let mut gate = conversion_gate(
        &case,
        &["--test-remote-apply-cap-bytes", CAP],
        &[r".\tool.exe", "fill", "big.txt", "600000"],
    );
    isolated(&case, &mut gate);

    let outcome = finish(gate);

    assert_refused(&case, &outcome, &before, OVER_CAP);
    assert_absent(&case, "big.txt", &outcome);
}

/// The legacy `crlf` attribute, read when `text` is not given, makes a file text: with no `core.eol`, 600,000 bytes
/// count as 1,200,000.
#[test]
fn a_legacy_crlf_file_counts_twice_toward_the_apply_cap() {
    let case = conversion_case("syncback-conversions-legacy", "*.txt crlf\n");
    git(&case, &["config", "core.autocrlf", "false"]);
    let before = git_out(&case, &["status", "--porcelain"]);
    let mut gate = conversion_gate(
        &case,
        &["--test-remote-apply-cap-bytes", CAP],
        &[r".\tool.exe", "fill", "big.txt", "600000"],
    );
    isolated(&case, &mut gate);

    let outcome = finish(gate);

    assert_refused(&case, &outcome, &before, OVER_CAP);
    assert_absent(&case, "big.txt", &outcome);
}

/// A path with a `working-tree-encoding` counts four times: 300,000 bytes as 1,200,000, past the 1 MiB cap that twice
/// them would stay under. The attribute is local, so the box's own add, which wants a UTF-16 file to start with its
/// byte order mark, never reads it.
#[test]
fn an_encoded_file_counts_four_times_toward_the_apply_cap() {
    let case = conversion_case("syncback-conversions-encoding", "");
    git(&case, &["config", "core.autocrlf", "false"]);
    local_attributes(&case, "*.u16 working-tree-encoding=UTF-16\n");
    let before = git_out(&case, &["status", "--porcelain"]);

    let outcome = finish(conversion_gate(
        &case,
        &["--test-remote-apply-cap-bytes", CAP],
        &[r".\tool.exe", "fill", "big.u16", "300000"],
    ));

    assert_refused(&case, &outcome, &before, OVER_CAP);
    assert_absent(&case, "big.u16", &outcome);
}

/// With no conversion, a file of 600,000 bytes counts once and applies under the 1 MiB cap.
#[test]
fn a_file_with_no_conversion_counts_once_and_syncs_back() {
    let case = conversion_case("syncback-conversions-plain", "");
    git(&case, &["config", "core.autocrlf", "false"]);
    git(&case, &["config", "core.eol", "lf"]);
    let mut gate = conversion_gate(
        &case,
        &["--test-remote-apply-cap-bytes", CAP],
        &[r".\tool.exe", "fill", "plain.bin", "600000"],
    );
    isolated(&case, &mut gate);

    let outcome = finish(gate);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: synced back 1 file(s): plain.bin"), "{outcome:?}");
    let synced = fs::metadata(case.folder().join("plain.bin")).expect("the box's file is here").len();
    assert_eq!(synced, 600_000, "the whole file came back");
}
