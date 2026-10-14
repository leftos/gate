//! `gate push` through the suite's stand-in for ssh, whose push call runs the builder script in this PC's pwsh with a
//! LOCALAPPDATA of the builder's own, `builders\<host>` in the case folder. Each case's installed gate is a copy of the
//! built gate in the case's own LOCALAPPDATA, and a builder's old gate a copy of the stand-in, which says its own
//! version, [`OLD_VERSION`].

use std::fs::{self, File};
use std::io::Read as _;
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use flate2::read::GzDecoder;
use serde_json::{Value, json};

use crate::install::unix;
use crate::support::{Case, FAKE_TOOL, GATE, Outcome, beside, finish, names, shown, start, version_of, write_remote_json};

/// The suite's stand-in for ssh, which `--test-push-ssh` names.
const FAKE_SSH: &str = env!("CARGO_BIN_EXE_fake-ssh");
/// The variable naming the stand-in's steps file; the stand-in logs each call's verb to `<steps>.log`.
const STEPS_VARIABLE: &str = "GATE_FAKE_SSH_STEPS";
/// The variable naming the folder that holds each builder's LOCALAPPDATA, a folder named as its host.
const BUILDERS_VARIABLE: &str = "GATE_FAKE_SSH_BUILDERS";
/// The builder `remote.json`'s `host` names.
const HOST: &str = "pushbox-a";
/// The builder a `hosts` entry adds.
const OTHER_HOST: &str = "pushbox-b";
/// An aside copy an earlier install left, named as the script names one.
const ASIDE: &str = "gate.exe.20200101000000.4.old";
/// What the stand-in says after `gate ` when it is started with `--version` alone, as a builder's old gate.
const OLD_VERSION: &str = "0.0.0-old";
/// What `fake-ssh`'s `fail-other` step says on standard error.
const REFUSED: &str = "ssh: connect to host builder port 22: Connection refused";
/// Only guards against a stuck case; no case asserts how long anything took.
const HANG_GUARD: Duration = Duration::from_secs(600);
/// How often a wait looks again.
const POLL: Duration = Duration::from_millis(10);

/// A case whose LOCALAPPDATA holds a copy of the built gate as its installed gate and a routing file naming [`HOST`].
fn push_case(name: &str) -> Case {
    let case = Case::new(name);
    install_gate(&case);
    route(&case, &json!({ "host": HOST, "repos": {} }));
    case
}

/// Writes `routing` as the case's `remote.json`.
fn route(case: &Case, routing: &Value) {
    write_remote_json(case, &routing.to_string());
}

/// The gate the case's push sends: `<LOCALAPPDATA>\gate\bin\gate.exe`.
fn installed_gate(case: &Case) -> PathBuf {
    case.local_app_data().join("gate").join("bin").join("gate.exe")
}

/// Installs a copy of the built gate as the case's installed gate.
fn install_gate(case: &Case) {
    let exe = installed_gate(case);
    fs::create_dir_all(exe.parent().expect("the installed gate has a folder")).expect("the install folder can be made");
    fs::copy(GATE, &exe).expect("the gate can be installed");
}

/// The folder the stand-in makes each builder's LOCALAPPDATA in.
fn builders(case: &Case) -> PathBuf {
    case.folder().join("builders")
}

/// The builder `host`'s install folder, `<its LOCALAPPDATA>\gate\bin`.
fn builder_bin(case: &Case, host: &str) -> PathBuf {
    builders(case).join(host).join("gate").join("bin")
}

/// Writes `bytes` in the builder `host`'s install folder as `name`, making the folder; its path.
fn builder_file(case: &Case, host: &str, name: &str, bytes: &[u8]) -> PathBuf {
    let bin = builder_bin(case, host);
    fs::create_dir_all(&bin).expect("the builder's install folder can be made");
    let path = bin.join(name);
    fs::write(&path, bytes).expect("the builder's file can be written");
    path
}

/// A builder's old gate: the stand-in's bytes, which say `gate` [`OLD_VERSION`] to `--version`, so a line naming that
/// version was read on the builder.
fn old_gate() -> Vec<u8> {
    fs::read(FAKE_SSH).expect("the stand-in can be read")
}

/// Whether `name` is an aside copy's name as the script makes one, `gate.exe.<yyyyMMddHHmmss>.<pid>.old`.
fn is_aside_name(name: &str) -> bool {
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    name.strip_prefix("gate.exe.")
        .and_then(|rest| rest.strip_suffix(".old"))
        .and_then(|rest| rest.split_once('.'))
        .is_some_and(|(stamp, pid)| stamp.len() == 14 && digits(stamp) && digits(pid))
}

/// Writes `steps`, one a line, as the stand-in's steps file in the case folder; its path.
fn steps(case: &Case, steps: &[&str]) -> PathBuf {
    let path = case.folder().join("push.steps");
    fs::write(&path, steps.join("\n")).expect("the steps file can be written");
    path
}

/// The verbs the stand-in of the steps file `steps` was called with, in order; none before its first call.
fn calls(steps: &Path) -> Vec<String> {
    fs::read_to_string(beside(steps, ".log")).map_or_else(|_| Vec::new(), |text| text.lines().map(str::to_owned).collect())
}

/// What the stand-in of the steps file `steps` was fed on its standard input by its last push call that ran the script.
fn pushed_input(steps: &Path) -> Vec<u8> {
    fs::read(beside(steps, ".input")).expect("the stand-in kept the push's input")
}

/// `gate push` for `case`, each builder's call made through `ssh` acting on the steps file `steps`, `extra` after.
fn push_command(case: &Case, steps: &Path, ssh: &str, extra: &[&str]) -> Command {
    let mut words = vec!["push", "--test-push-ssh", ssh];
    words.extend(extra);
    let mut command = case.gate_without_state_dir(&words);
    command.env(STEPS_VARIABLE, steps).env(BUILDERS_VARIABLE, builders(case));
    command
}

/// Runs `gate push` for `case` through the stand-in acting on `steps`.
fn push(case: &Case, steps: &Path) -> Outcome {
    finish(push_command(case, steps, FAKE_SSH, &[]))
}

/// Runs `command` to its end, or kills it after the hang guard, its stdout and stderr one file of the case, so the
/// lines of both are read in the order they were written: its status code and that text, with `\n` line ends.
fn merged(case: &Case, mut command: Command) -> (Option<i32>, String) {
    let path = case.folder().join("merged.txt");
    let file = File::create(&path).expect("the output file can be made");
    command
        .stdin(Stdio::null())
        .stdout(file.try_clone().expect("the output file's handle can be copied"))
        .stderr(file);
    let mut child = command.spawn().expect("the gate starts");
    drop(command);
    let deadline = Instant::now() + HANG_GUARD;
    let status = loop {
        if let Some(status) = child.try_wait().expect("the gate's status can be read") {
            break Some(status);
        }
        if Instant::now() >= deadline {
            child.kill().expect("a stuck gate can be killed");
            child.wait().expect("a killed gate can be waited for");
            break None;
        }
        thread::sleep(POLL);
    };
    let text = fs::read_to_string(&path).expect("the output file can be read");
    (status.and_then(|status| status.code()), unix(&text))
}

/// What the built gate prints after `gate `, read once: the version every case's installed gate says.
static VERSION: LazyLock<String> = LazyLock::new(|| version_of(Path::new(GATE)));

/// [`VERSION`].
fn version() -> &'static str {
    &VERSION
}

/// Asserts the file at `path` holds `bytes`, without printing either.
fn assert_holds(path: &Path, bytes: &[u8]) {
    let held = fs::read(path).expect("the file can be read");
    assert!(held == bytes, "{} holds other bytes", shown(path));
}

/// The lines a push that installed on `host` says on stdout.
fn installed_lines(host: &str) -> String {
    let version = version();
    format!("push: {host}: sending gate {version}\npush: {host}: installed gate {version}\n")
}

/// Asserts `outcome` is a push that installed the built gate on [`HOST`], with nothing on stderr.
fn assert_installed(case: &Case, outcome: &Outcome) {
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(unix(&outcome.stdout), installed_lines(HOST), "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_holds(
        &builder_bin(case, HOST).join("gate.exe"),
        &fs::read(installed_gate(case)).expect("the installed gate can be read"),
    );
}

/// Asserts `outcome` is a push that failed on [`HOST`] with `reason`, after saying it was sending.
fn assert_failed(outcome: &Outcome, reason: &str) {
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(
        unix(&outcome.stdout),
        format!("push: {HOST}: sending gate {}\n", version()),
        "{outcome:?}"
    );
    assert_eq!(unix(&outcome.stderr), format!("push: {HOST} failed: {reason}\n"), "{outcome:?}");
}

#[test]
fn a_push_installs_on_a_builder_with_no_gate() {
    let case = push_case("a_push_installs_on_a_builder_with_no_gate");
    let steps = steps(&case, &["ok"]);

    let outcome = push(&case, &steps);

    assert_installed(&case, &outcome);
    assert_eq!(
        names(&builder_bin(&case, HOST)),
        ["gate.exe", "push.lock"],
        "nothing else is left in the builder's install folder, the lock's plain file aside"
    );
    assert_eq!(calls(&steps), ["push"]);
    let words = fs::read_to_string(beside(&steps, ".words")).expect("the stand-in kept the call's words");
    assert!(words.starts_with("push -o RequestTTY=no "), "the push asks for no terminal: {words}");
}

/// The exe crosses ssh compressed: what the builder's call is fed is smaller than the uncompressed exe's base64, and
/// the script decompresses it to the exe whole before its check and install.
#[test]
fn a_push_sends_the_exe_compressed_and_installs_it_whole() {
    let case = push_case("a_push_sends_the_exe_compressed_and_installs_it_whole");
    let steps = steps(&case, &["ok"]);

    let outcome = push(&case, &steps);

    assert_installed(&case, &outcome);
    let exe = fs::read(installed_gate(&case)).expect("the installed gate can be read");
    let sent = pushed_input(&steps);
    let uncompressed = exe.len().div_ceil(3) * 4;
    assert!(
        sent.len() < uncompressed,
        "the push fed {} bytes, no fewer than the {uncompressed} of the exe's base64",
        sent.len()
    );
    let text = String::from_utf8(sent).expect("the push fed text");
    let (_, exe_text) = text.split_once('\n').expect("the push fed the script's line, then the exe");
    let gzip = STANDARD.decode(exe_text.trim_end()).expect("the push fed base64");
    let mut unpacked = Vec::new();
    GzDecoder::new(gzip.as_slice()).read_to_end(&mut unpacked).expect("the push fed gzip");
    assert!(unpacked == exe, "what the push fed decompresses to the installed exe");
}

#[test]
fn a_push_to_an_identical_gate_says_already_installed() {
    let case = push_case("a_push_to_an_identical_gate_says_already_installed");
    let theirs = builder_file(&case, HOST, "gate.exe", &fs::read(GATE).expect("the built gate can be read"));
    let modified = |path: &Path| -> SystemTime {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .expect("the builder's gate has a write time")
    };
    let before = modified(&theirs);
    let steps = steps(&case, &["ok"]);

    let outcome = push(&case, &steps);

    let version = version();
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        unix(&outcome.stdout),
        format!("push: {HOST}: sending gate {version}\npush: {HOST}: gate {version} already installed\n"),
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_eq!(modified(&theirs), before, "the builder's gate was not written");
    assert_eq!(names(&builder_bin(&case, HOST)), ["gate.exe", "push.lock"], "nothing was replaced");
}

#[test]
fn a_push_reports_the_build_it_sends() {
    let case = push_case("a_push_reports_the_build_it_sends");
    // The builder holds this version's own bytes with one byte appended: another build of the same package version, so
    // its SHA-256 differs from the sent copy's and the push replaces it rather than reading it as already installed.
    let mut other = fs::read(GATE).expect("the built gate can be read");
    other.push(b'\0');
    let theirs = builder_file(&case, HOST, "gate.exe", &other);
    let steps = steps(&case, &["ok"]);

    let outcome = push(&case, &steps);

    let version = version();
    assert!(
        version.starts_with(&format!("{}+", env!("CARGO_PKG_VERSION"))),
        "the push names the package version and the build it sends: {version}"
    );
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        unix(&outcome.stdout),
        format!("push: {HOST}: sending gate {version}\npush: {HOST}: replaced gate {version}\npush: {HOST}: installed gate {version}\n"),
        "another build of this version is not already installed: {outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_holds(&theirs, &fs::read(installed_gate(&case)).expect("the installed gate can be read"));
}

#[test]
fn a_push_replaces_an_older_gate() {
    let case = push_case("a_push_replaces_an_older_gate");
    let old = old_gate();
    builder_file(&case, HOST, "gate.exe", &old);
    let steps = steps(&case, &["ok"]);

    let outcome = push(&case, &steps);

    let version = version();
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        unix(&outcome.stdout),
        format!("push: {HOST}: sending gate {version}\npush: {HOST}: replaced gate {OLD_VERSION}\npush: {HOST}: installed gate {version}\n"),
        "the replaced version is read from the builder's old gate before the move: {outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    let bin = builder_bin(&case, HOST);
    assert_holds(
        &bin.join("gate.exe"),
        &fs::read(installed_gate(&case)).expect("the installed gate can be read"),
    );
    let names = names(&bin);
    assert_eq!(names.len(), 3, "the gate, one aside copy and the lock, no temp copy: {names:?}");
    assert_eq!(names[2], "push.lock", "{names:?}");
    let aside = &names[1];
    assert!(is_aside_name(aside), "the old gate is left aside for a later push to delete: {names:?}");
    assert_holds(&bin.join(aside), &old);
}

#[test]
fn a_verified_install_deletes_older_aside_copies_but_keeps_its_own() {
    let case = push_case("a_verified_install_deletes_older_aside_copies_but_keeps_its_own");
    let old = old_gate();
    builder_file(&case, HOST, "gate.exe", &old);
    builder_file(&case, HOST, ASIDE, b"an old gate no process runs");
    let steps = steps(&case, &["ok"]);

    let outcome = push(&case, &steps);

    let version = version();
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        unix(&outcome.stdout),
        format!("push: {HOST}: sending gate {version}\npush: {HOST}: replaced gate {OLD_VERSION}\npush: {HOST}: installed gate {version}\n"),
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    let bin = builder_bin(&case, HOST);
    let names = names(&bin);
    assert_eq!(names.len(), 3, "the gate, its own aside copy and the lock: {names:?}");
    assert_eq!((names[0].as_str(), names[2].as_str()), ("gate.exe", "push.lock"), "{names:?}");
    let own = &names[1];
    assert!(
        own != ASIDE && is_aside_name(own),
        "the older aside copy was deleted, the push's own kept: {names:?}"
    );
    assert_holds(&bin.join(own), &old);
}

#[test]
fn a_push_tries_every_builder_and_fails_if_one_did() {
    let case = Case::new("a_push_tries_every_builder_and_fails_if_one_did");
    install_gate(&case);
    route(&case, &json!({ "host": HOST, "hosts": [OTHER_HOST], "repos": {} }));
    let steps = steps(&case, &["fail-other", "ok"]);

    let (code, text) = merged(&case, push_command(&case, &steps, FAKE_SSH, &[]));

    let version = version();
    assert_eq!(code, Some(1), "{text}");
    assert_eq!(
        text,
        format!(
            "push: {HOST}: sending gate {version}\npush: {HOST} failed: {REFUSED}\n\
             push: {OTHER_HOST}: sending gate {version}\npush: {OTHER_HOST}: installed gate {version}\n"
        ),
        "the builder after the failed one is tried too"
    );
    assert_eq!(calls(&steps), ["push", "push"]);
    assert!(!builders(&case).join(HOST).exists(), "the failed call ran nothing");
    assert_holds(
        &builder_bin(&case, OTHER_HOST).join("gate.exe"),
        &fs::read(installed_gate(&case)).expect("the installed gate can be read"),
    );
}

#[test]
fn a_held_aside_copy_is_left_silently() {
    let case = push_case("a_held_aside_copy_is_left_silently");
    let aside = builder_file(&case, HOST, ASIDE, &fs::read(FAKE_TOOL).expect("fake-tool can be read"));
    let go = case.folder().join("go");
    let running = start({
        let mut command = Command::new(&aside);
        command.args(["wait", &shown(&go), "exit", "0"]);
        command
    });
    let steps = steps(&case, &["ok"]);

    let outcome = push(&case, &steps);

    // The aside copy's process is let go and seen to end before any assertion, so a failing one leaves none waiting.
    fs::write(&go, "").expect("the go file can be written");
    let ran = running.finish();

    assert_installed(&case, &outcome);
    assert_eq!(ran.code, Some(0), "the process running the aside copy ran to its end: {ran:?}");
    assert_eq!(
        names(&builder_bin(&case, HOST)),
        ["gate.exe", ASIDE, "push.lock"],
        "an aside copy a process runs is left, and left silently, as install leaves it"
    );
}

#[test]
fn an_unheld_aside_copy_is_cleaned_up_after_a_verified_install() {
    let case = push_case("an_unheld_aside_copy_is_cleaned_up_after_a_verified_install");
    builder_file(&case, HOST, ASIDE, b"an old gate no process runs");
    let steps = steps(&case, &["ok"]);

    let outcome = push(&case, &steps);

    assert_installed(&case, &outcome);
    assert_eq!(names(&builder_bin(&case, HOST)), ["gate.exe", "push.lock"], "the aside copy was deleted");
}

#[test]
fn a_stale_temp_copy_does_not_stop_a_push() {
    let case = push_case("a_stale_temp_copy_does_not_stop_a_push");
    builder_file(&case, HOST, "gate.exe.4.tmp", b"half a gate an earlier push left");
    let steps = steps(&case, &["ok"]);

    let outcome = push(&case, &steps);

    assert_installed(&case, &outcome);
    assert_eq!(
        names(&builder_bin(&case, HOST)),
        ["gate.exe", "push.lock"],
        "the stale temp copy was deleted"
    );
}

#[test]
fn a_linked_bin_folder_is_refused() {
    let case = push_case("a_linked_bin_folder_is_refused");
    let elsewhere = case.folder().join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("the link's target can be made");
    let bin = builder_bin(&case, HOST);
    fs::create_dir_all(bin.parent().expect("the install folder has a parent")).expect("the builder's gate folder can be made");
    let linked = Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(&bin)
        .arg(&elsewhere)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .expect("cmd starts");
    assert!(linked.success(), "the junction is made");
    let steps = steps(&case, &["ok"]);

    let outcome = push(&case, &steps);

    assert_failed(&outcome, &format!("{} is a link", shown(&bin)));
    assert!(names(&elsewhere).is_empty(), "nothing was written through the link");
}

/// A push that finds the builder's lock held, as another push installing there holds it, fails and touches nothing:
/// the old gate and an aside copy the cleanup would delete are both left.
#[test]
fn a_push_while_another_holds_the_builder_lock_touches_nothing() {
    let case = push_case("a_push_while_another_holds_the_builder_lock_touches_nothing");
    let old = old_gate();
    builder_file(&case, HOST, "gate.exe", &old);
    builder_file(&case, HOST, ASIDE, b"an old gate no process runs");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(builder_bin(&case, HOST).join("push.lock"))
        .expect("the lock can be held");
    let steps = steps(&case, &["ok"]);

    let outcome = push(&case, &steps);

    drop(lock);
    assert_failed(&outcome, "another gate push is installing on this builder");
    let bin = builder_bin(&case, HOST);
    assert_eq!(names(&bin), ["gate.exe", ASIDE, "push.lock"], "nothing was cleaned up or replaced");
    assert_holds(&bin.join("gate.exe"), &old);
}

/// A push that finds this PC's lock held, as `tools/install.ps1` holds it while it replaces and checks the installed
/// gate, reads nothing and calls no builder.
#[test]
fn a_push_while_an_install_holds_the_local_lock_sends_nothing() {
    let case = push_case("a_push_while_an_install_holds_the_local_lock_sends_nothing");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(installed_gate(&case).with_file_name("push.lock"))
        .expect("the lock can be held");
    let steps = steps(&case, &["ok"]);

    let outcome = push(&case, &steps);

    drop(lock);
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(
        unix(&outcome.stderr),
        "push: an install is in progress on this PC; nothing was sent\n",
        "{outcome:?}"
    );
    assert_eq!(calls(&steps), Vec::<String>::new(), "no builder was called");
}

/// A `push.lock` on this PC that is a second name of another file is refused as a link is, and nothing is sent.
#[test]
fn a_push_lock_that_is_a_hard_link_is_refused() {
    let case = push_case("a_push_lock_that_is_a_hard_link_is_refused");
    let other = case.folder().join("other.txt");
    fs::write(&other, b"another file").expect("the other file is written");
    let lock = installed_gate(&case).with_file_name("push.lock");
    fs::hard_link(&other, &lock).expect("push.lock is made a second name of the other file");
    let steps = steps(&case, &["ok"]);

    let outcome = push(&case, &steps);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(
        unix(&outcome.stderr),
        format!("push: {} is a link; nothing was sent\n", lock.display()),
        "{outcome:?}"
    );
    assert_eq!(calls(&steps), Vec::<String>::new(), "no builder was called");
    assert_eq!(
        fs::read(&other).expect("the other file is read"),
        b"another file",
        "the other file is untouched"
    );
}

/// A `push.lock` on this PC that is a junction, which cannot be opened as a file, is refused as a link is, and nothing
/// is sent.
#[test]
fn a_push_lock_that_is_a_junction_is_refused() {
    let case = push_case("a_push_lock_that_is_a_junction_is_refused");
    let elsewhere = case.folder().join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("the link's target can be made");
    let lock = installed_gate(&case).with_file_name("push.lock");
    let linked = Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(&lock)
        .arg(&elsewhere)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .expect("cmd starts");
    assert!(linked.success(), "the junction is made");
    let steps = steps(&case, &["ok"]);

    let outcome = push(&case, &steps);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(
        unix(&outcome.stderr),
        format!("push: {} is a link; nothing was sent\n", lock.display()),
        "{outcome:?}"
    );
    assert_eq!(calls(&steps), Vec::<String>::new(), "no builder was called");
    assert!(names(&elsewhere).is_empty(), "nothing was written through the link");
}

#[test]
fn a_push_past_its_bound_says_timed_out() {
    let case = push_case("a_push_past_its_bound_says_timed_out");
    let never = case.folder().join("never");
    let steps = steps(&case, &[&format!("hold {}", shown(&never))]);

    let outcome = finish(push_command(&case, &steps, FAKE_SSH, &["--test-push-bound-ms", "200"]));

    assert_failed(&outcome, "timed out after 1 s");
    // The call holds until the 200 ms bound kills it, which may be before the stand-in has logged it.
    let calls = calls(&steps);
    assert!(calls.is_empty() || calls == ["push"], "at most the one push call: {calls:?}");
    assert!(!builder_bin(&case, HOST).join("gate.exe").exists(), "nothing was installed");
}

#[test]
fn a_push_whose_ssh_cannot_be_found_fails_that_builder() {
    let case = push_case("a_push_whose_ssh_cannot_be_found_fails_that_builder");
    let missing = shown(&case.folder().join("no-such-ssh.exe"));
    let steps = steps(&case, &["ok"]);

    let outcome = finish(push_command(&case, &steps, &missing, &[]));

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(
        unix(&outcome.stdout),
        format!("push: {HOST}: sending gate {}\n", version()),
        "{outcome:?}"
    );
    let stderr = unix(&outcome.stderr);
    assert!(
        stderr.starts_with(&format!("push: {HOST} failed: cannot start {missing}: ")) && stderr.lines().count() == 1,
        "one failure line naming the program: {outcome:?}"
    );
    assert_eq!(calls(&steps), Vec::<String>::new(), "no builder was called");
}

#[test]
fn a_push_with_no_remote_json_tries_no_builder() {
    let case = Case::new("a_push_with_no_remote_json_tries_no_builder");
    install_gate(&case);
    let steps = steps(&case, &["ok"]);

    let outcome = push(&case, &steps);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    let routing = case.local_app_data().join("gate").join("remote.json");
    assert_eq!(
        unix(&outcome.stderr),
        format!("push: no remote.json at {}\n", shown(&routing)),
        "{outcome:?}"
    );
    assert_eq!(calls(&steps), Vec::<String>::new(), "no builder was called");
}

#[test]
fn a_push_with_a_bad_hosts_entry_tries_no_builder() {
    let case = Case::new("a_push_with_a_bad_hosts_entry_tries_no_builder");
    install_gate(&case);
    route(&case, &json!({ "host": HOST, "hosts": [""], "repos": {} }));
    let steps = steps(&case, &["ok"]);

    let outcome = push(&case, &steps);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    let routing = case.local_app_data().join("gate").join("remote.json");
    assert_eq!(
        unix(&outcome.stderr),
        format!("push: ignored {}: hosts entries must each be a non-empty string\n", shown(&routing)),
        "the router's own reason: {outcome:?}"
    );
    assert_eq!(calls(&steps), Vec::<String>::new(), "no builder was called");
}

#[test]
fn a_push_with_no_installed_gate_tries_no_builder() {
    let case = Case::new("a_push_with_no_installed_gate_tries_no_builder");
    route(&case, &json!({ "host": HOST, "repos": {} }));
    let steps = steps(&case, &["ok"]);

    let outcome = push(&case, &steps);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(
        unix(&outcome.stderr),
        format!("push: no installed gate at {}; run tools/install.ps1\n", shown(&installed_gate(&case))),
        "{outcome:?}"
    );
    assert_eq!(calls(&steps), Vec::<String>::new(), "no builder was called");
}
