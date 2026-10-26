//! `gate push` through the suite's stand-in for ssh, whose push call runs the builder script in this PC's pwsh with a
//! LOCALAPPDATA of the builder's own, `builders\<host>` in the case folder. Each case's installed gate is a copy of the
//! built gate in the case's own LOCALAPPDATA, its desktop helper a copy of the suite's stand-in helper, and a builder's
//! old gate a copy of the stand-in, which says its own version, [`OLD_VERSION`]. Each builder's registration of the
//! desktop task runs the pushed gate's own seam, so no case touches this PC's Task Scheduler.

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
use crate::support::{
    Case, FAKE_DESKTOP_HELPER, FAKE_TOOL, GATE, Outcome, beside, finish, git, junction, names, shown, start, version_of, write_remote_json,
};

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
/// The desktop helper's name beside gate.exe, on this PC and on a builder.
const HELPER: &str = "gate-desktop-helper.exe";
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

/// The desktop helper the case's push sends: `<LOCALAPPDATA>\gate\bin\gate-desktop-helper.exe`.
fn installed_helper(case: &Case) -> PathBuf {
    installed_gate(case).with_file_name(HELPER)
}

/// Installs a copy of the built gate as the case's installed gate, and a copy of the suite's stand-in helper beside it.
fn install_gate(case: &Case) {
    install_gate_alone(case);
    fs::copy(FAKE_DESKTOP_HELPER, installed_helper(case)).expect("the helper can be installed");
}

/// Installs a copy of the built gate as the case's installed gate, with no desktop helper beside it.
fn install_gate_alone(case: &Case) {
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

/// `gate push` for `case`, each builder's call made through `ssh` acting on the steps file `steps`, `extra` after; each
/// builder's registration is `--test-push-register ok` unless `extra` gives that seam, so no case reaches this PC's
/// Task Scheduler.
fn push_command(case: &Case, steps: &Path, ssh: &str, extra: &[&str]) -> Command {
    let mut words = vec!["push", "--test-push-ssh", ssh];
    let registers = |word: &&str| *word == "--test-push-register" || word.starts_with("--test-push-register=");
    if !extra.iter().any(registers) {
        words.extend(["--test-push-register", "ok"]);
    }
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

/// The lines a push that installed gate.exe and the helper on `host` and registered the task there says on stdout.
fn installed_lines(host: &str) -> String {
    let version = version();
    format!(
        "push: {host}: sending gate {version}\npush: {host}: installed gate {version}\n{}",
        desktop_lines(host)
    )
}

/// The lines a push that installed the helper on `host` and registered the task there says after its gate lines.
fn desktop_lines(host: &str) -> String {
    format!("push: {host}: installed the desktop helper\npush: {host}: registered the desktop task\n")
}

/// Asserts `outcome` is a push that installed the built gate and the helper on [`HOST`] and registered the task there,
/// with nothing on stderr.
fn assert_installed(case: &Case, outcome: &Outcome) {
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(unix(&outcome.stdout), installed_lines(HOST), "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_holds(
        &builder_bin(case, HOST).join("gate.exe"),
        &fs::read(installed_gate(case)).expect("the installed gate can be read"),
    );
    assert_holds(
        &builder_bin(case, HOST).join(HELPER),
        &fs::read(installed_helper(case)).expect("the installed helper can be read"),
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
        [HELPER, "gate.exe", "push.lock"],
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
    let lines: Vec<&str> = text.trim_end().split('\n').collect();
    assert_eq!(lines.len(), 3, "the push fed the script's line, then the exe's, then the helper's");
    let gzip = STANDARD.decode(lines[1]).expect("the push fed base64");
    let mut unpacked = Vec::new();
    GzDecoder::new(gzip.as_slice()).read_to_end(&mut unpacked).expect("the push fed gzip");
    assert!(unpacked == exe, "what the push fed decompresses to the installed exe");
}

#[test]
fn a_push_to_an_identical_gate_says_already_installed_and_registers_again() {
    let case = push_case("a_push_to_an_identical_gate_says_already_installed_and_registers_again");
    let theirs = builder_file(&case, HOST, "gate.exe", &fs::read(GATE).expect("the built gate can be read"));
    builder_file(
        &case,
        HOST,
        HELPER,
        &fs::read(FAKE_DESKTOP_HELPER).expect("the stand-in helper can be read"),
    );
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
        format!(
            "push: {HOST}: sending gate {version}\npush: {HOST}: gate {version} already installed\n\
             push: {HOST}: registered the desktop task\n"
        ),
        "an identical gate and helper still register the task again: {outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_eq!(modified(&theirs), before, "the builder's gate was not written");
    assert_eq!(
        names(&builder_bin(&case, HOST)),
        [HELPER, "gate.exe", "push.lock"],
        "nothing was replaced"
    );
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
        format!(
            "push: {HOST}: sending gate {version}\npush: {HOST}: replaced gate {version}\npush: {HOST}: installed gate {version}\n{}",
            desktop_lines(HOST)
        ),
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
        format!(
            "push: {HOST}: sending gate {version}\npush: {HOST}: replaced gate {OLD_VERSION}\npush: {HOST}: installed gate {version}\n{}",
            desktop_lines(HOST)
        ),
        "the replaced version is read from the builder's old gate before the move: {outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    let bin = builder_bin(&case, HOST);
    assert_holds(
        &bin.join("gate.exe"),
        &fs::read(installed_gate(&case)).expect("the installed gate can be read"),
    );
    let names = names(&bin);
    assert_eq!(
        names.len(),
        4,
        "the helper, the gate, one aside copy and the lock, no temp copy: {names:?}"
    );
    assert_eq!(names[3], "push.lock", "{names:?}");
    let aside = &names[2];
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
        format!(
            "push: {HOST}: sending gate {version}\npush: {HOST}: replaced gate {OLD_VERSION}\npush: {HOST}: installed gate {version}\n{}",
            desktop_lines(HOST)
        ),
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    let bin = builder_bin(&case, HOST);
    let names = names(&bin);
    assert_eq!(names.len(), 4, "the helper, the gate, its own aside copy and the lock: {names:?}");
    assert_eq!(
        (names[0].as_str(), names[1].as_str(), names[3].as_str()),
        (HELPER, "gate.exe", "push.lock"),
        "{names:?}"
    );
    let own = &names[2];
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
             push: {OTHER_HOST}: sending gate {version}\npush: {OTHER_HOST}: installed gate {version}\n{}",
            desktop_lines(OTHER_HOST)
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
        [HELPER, "gate.exe", ASIDE, "push.lock"],
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
    assert_eq!(
        names(&builder_bin(&case, HOST)),
        [HELPER, "gate.exe", "push.lock"],
        "the aside copy was deleted"
    );
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
        [HELPER, "gate.exe", "push.lock"],
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
    junction(&bin, &elsewhere);
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
    junction(&lock, &elsewhere);
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

#[test]
fn a_push_with_no_installed_desktop_helper_tries_no_builder() {
    let case = Case::new("a_push_with_no_installed_desktop_helper_tries_no_builder");
    install_gate_alone(&case);
    route(&case, &json!({ "host": HOST, "repos": {} }));
    let steps = steps(&case, &["ok"]);

    let outcome = push(&case, &steps);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(
        unix(&outcome.stderr),
        format!(
            "push: no installed desktop helper at {}; run tools/install.ps1\n",
            shown(&installed_helper(&case))
        ),
        "{outcome:?}"
    );
    assert_eq!(calls(&steps), Vec::<String>::new(), "no builder was called");
}

#[test]
fn a_failed_registration_is_said_on_the_builder_s_line_and_push_exits_0() {
    let case = push_case("a_failed_registration_is_said_on_the_builder_s_line_and_push_exits_0");
    let steps = steps(&case, &["ok"]);

    let outcome = finish(push_command(&case, &steps, FAKE_SSH, &["--test-push-register", "fail"]));

    let version = version();
    assert_eq!(outcome.code, Some(0), "a failed registration alone fails no push: {outcome:?}");
    assert_eq!(
        unix(&outcome.stdout),
        format!(
            "push: {HOST}: sending gate {version}\npush: {HOST}: installed gate {version}\n\
             push: {HOST}: installed the desktop helper\npush: {HOST}: desktop task not registered: the test seam refused\n"
        ),
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_holds(
        &builder_bin(&case, HOST).join(HELPER),
        &fs::read(installed_helper(&case)).expect("the installed helper can be read"),
    );
}

/// A builder whose helper name is a folder, which no file read or move takes, fails the helper's install after gate.exe
/// is in place: the push fails that builder and exits 1, and no registration is said.
#[test]
fn a_failed_helper_install_fails_the_push() {
    let case = push_case("a_failed_helper_install_fails_the_push");
    let helper = builder_bin(&case, HOST).join(HELPER);
    fs::create_dir_all(&helper).expect("a folder can be made at the helper's name");
    let steps = steps(&case, &["ok"]);

    let outcome = push(&case, &steps);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(
        unix(&outcome.stdout),
        format!("push: {HOST}: sending gate {}\n", version()),
        "{outcome:?}"
    );
    let stderr = unix(&outcome.stderr);
    assert!(
        stderr.starts_with(&format!("push: {HOST} failed: could not install {}: ", shown(&helper))) && stderr.lines().count() == 1,
        "one failure line naming the helper: {outcome:?}"
    );
    assert!(helper.is_dir(), "the folder is left as it was");
}

/// The root the routing file gives every macOS builder; the stand-in makes it the builder's own `root` folder.
const MAC_ROOT: &str = "/gate-mac";
/// The macOS builders a case's `builders` entries add.
const MAC_HOST: &str = "pushmac-a";
const OTHER_MAC: &str = "pushmac-c";
/// The variable naming the macOS builders' root to the stand-in.
const UNIX_ROOT_VARIABLE: &str = "GATE_FAKE_SSH_UNIX_ROOT";
/// The variable naming the version the stand-in says to `--version`, which a macOS case's installed gate says.
const VERSION_VARIABLE: &str = "GATE_FAKE_SSH_VERSION";
/// The macOS calls of a push that builds: the prepare, the git push and the build.
const MAC_CALLS: [&str; 3] = ["mac-prepare", "git-receive-pack", "mac-build"];

/// A macOS case: its folder, the commit its repo holds, which `source.json` names, and the version its installed gate
/// says, `0.1.0+<the commit's first 12 digits>`.
struct MacCase {
    case: Case,
    commit: String,
    version: String,
}

impl MacCase {
    /// The build id `source.json` names.
    fn id(&self) -> &str {
        &self.commit[..12]
    }

    /// Writes `source.json` naming the case's repo, its commit and `build_id`, `dirty` as given.
    fn write_source(&self, build_id: &str, dirty: bool) {
        self.write_source_of(&self.case.folder().join("repo"), build_id, dirty);
    }

    /// Writes `source.json` naming `repo`, the case's commit and `build_id`, `dirty` as given.
    fn write_source_of(&self, repo: &Path, build_id: &str, dirty: bool) {
        let source = json!({ "repoRoot": shown(repo), "commit": self.commit, "buildId": build_id, "dirty": dirty });
        fs::write(source_path(&self.case), source.to_string()).expect("source.json can be written");
    }
}

/// A case whose installed gate is a copy of the stand-in, saying the version of a build of a commit of the case's own
/// repo, with the stand-in helper beside it, `source.json` naming that repo, commit and id, and `routing` as its
/// `remote.json`.
fn mac_case(name: &str, routing: &Value) -> MacCase {
    let case = Case::new(name);
    let exe = installed_gate(&case);
    fs::create_dir_all(exe.parent().expect("the installed gate has a folder")).expect("the install folder can be made");
    fs::copy(FAKE_SSH, &exe).expect("the stand-in can be installed as the gate");
    fs::copy(FAKE_DESKTOP_HELPER, installed_helper(&case)).expect("the helper can be installed");
    git(&case, &["init", "--quiet", "repo"]);
    fs::write(case.folder().join("repo").join("file.txt"), "built\n").expect("the repo's file can be written");
    git(&case, &["-C", "repo", "add", "file.txt"]);
    git(&case, &["-C", "repo", "commit", "--quiet", "-m", "built"]);
    let commit = git(&case, &["-C", "repo", "rev-parse", "HEAD"]);
    route(&case, routing);
    let version = format!("0.1.0+{}", &commit[..12]);
    let mac = MacCase { case, commit, version };
    mac.write_source(mac.id(), false);
    mac
}

/// The case's `source.json`.
fn source_path(case: &Case) -> PathBuf {
    installed_gate(case).with_file_name("source.json")
}

/// A routing file naming [`HOST`], `hosts` as its `hosts`, and the macOS builders `macs` as `builders` entries in that
/// order.
fn mac_routing(hosts: &[&str], macs: &[&str]) -> Value {
    let entries: Vec<Value> = macs
        .iter()
        .map(|mac| json!({ "host": mac, "os": "macos", "root": MAC_ROOT, "repos": { "app": ["*"] } }))
        .collect();
    json!({ "host": HOST, "hosts": hosts, "builders": entries, "repos": {} })
}

/// `gate push` for the macOS case `mac` through the stand-in acting on `steps`.
fn mac_push(mac: &MacCase, steps: &Path) -> Outcome {
    let mut command = push_command(&mac.case, steps, FAKE_SSH, &[]);
    let root = MAC_ROOT.strip_prefix('/').expect("the macOS root is absolute");
    command.env(UNIX_ROOT_VARIABLE, root).env(VERSION_VARIABLE, &mac.version);
    // The user's wrappers and flags, which the builder's build must not take.
    command
        .env("RUSTC_WRAPPER", "a-wrapper")
        .env("RUSTC_WORKSPACE_WRAPPER", "a-wrapper")
        .env("RUSTFLAGS", "-Dwarnings");
    finish(command)
}

/// The macOS builder `host`'s stand-in root.
fn mac_root(case: &Case, host: &str) -> PathBuf {
    builders(case).join(host).join("root")
}

/// Writes a `gate-box` on the macOS builder `host` that says `gate <version>`; its path.
fn mac_box(case: &Case, host: &str, version: &str) -> PathBuf {
    let bin = mac_root(case, host).join("bin");
    fs::create_dir_all(&bin).expect("the builder's bin folder can be made");
    let path = bin.join("gate-box");
    fs::write(&path, format!("#!/bin/sh\necho 'gate {version}'\n")).expect("the builder's box can be written");
    path
}

/// The lines a push says on stdout that installed the case's gate on [`HOST`], then for each macOS builder and its lines
/// in `macs`, `push: <mac>: sending gate-box <version>` and those lines.
fn mac_stdout(version: &str, macs: &[(&str, &str)]) -> String {
    let host = format!(
        "push: {HOST}: sending gate {version}\npush: {HOST}: installed gate {version}\n{}",
        desktop_lines(HOST)
    );
    let parts: Vec<String> = macs.iter().map(|(mac, lines)| mac_lines(mac, version, lines)).collect();
    host + &parts.concat()
}

/// The macOS builder `mac`'s lines when it is sent `version`: its sending line, then `lines`.
fn mac_lines(mac: &str, version: &str, lines: &str) -> String {
    format!("push: {mac}: sending gate-box {version}\n{lines}")
}

/// The lines of the macOS builder `mac` after its sending line when it built and installed `version`, having replaced
/// the box `replaced` names.
fn built_lines(mac: &str, version: &str, replaced: Option<&str>) -> String {
    let replaced = replaced.map_or_else(String::new, |old| format!("push: {mac}: replaced gate-box {old}\n"));
    format!("push: {mac}: building gate-box {version}\n{replaced}push: {mac}: installed gate-box {version}\n")
}

/// The commit the macOS builder `host`'s stand-in bare repo holds as the pushed ref.
fn pushed_commit(case: &Case, host: &str) -> String {
    let bare = mac_root(case, host).join("gate-push").join("src.git");
    git(case, &["--git-dir", &shown(&bare), "rev-parse", "refs/gate-push/src"])
}

/// `["push"]` and [`MAC_CALLS`]: the call order of a push to [`HOST`] and a macOS builder that builds.
fn host_and_mac_calls() -> Vec<&'static str> {
    std::iter::once("push").chain(MAC_CALLS).collect()
}

#[test]
fn a_push_builds_the_box_on_a_macos_builder_after_host() {
    let mac = mac_case("a_push_builds_the_box_on_a_macos_builder_after_host", &mac_routing(&[], &[MAC_HOST]));
    // A repo that signs its pushes by its own config still pushes to the builder, which takes no signed push.
    git(&mac.case, &["-C", "repo", "config", "push.gpgSign", "true"]);
    let steps = steps(&mac.case, &["ok", "ok", "ok", "ok"]);
    assert!(!mac_root(&mac.case, MAC_HOST).exists(), "the builder has no root yet");

    let outcome = mac_push(&mac, &steps);

    let version = &mac.version;
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let bare = mac_root(&mac.case, MAC_HOST).join("gate-push").join("src.git");
    assert_eq!(
        git(&mac.case, &["--git-dir", &shown(&bare), "config", "core.sharedRepository"]),
        "0600",
        "the bare repo keeps its files to the builder's user"
    );
    for (key, value) in [("receive.autogc", "false"), ("gc.auto", "0")] {
        assert_eq!(
            git(&mac.case, &["--git-dir", &shown(&bare), "config", "--local", key]),
            value,
            "no gc a git push starts prunes objects the checkout borrows"
        );
    }
    let env = fs::read_to_string(builders(&mac.case).join(MAC_HOST).join("cargo.env")).expect("cargo's wrappers were logged");
    assert_eq!(
        env.trim_end(),
        "RUSTC_WRAPPER=[] RUSTC_WORKSPACE_WRAPPER=[] RUSTFLAGS=[] PWD=[/]",
        "the user's wrappers and flags are set empty, which overrides cargo's config, and cargo runs from /, so it reads no \
         config above the checkout"
    );
    assert_eq!(
        unix(&outcome.stdout),
        mac_stdout(version, &[(MAC_HOST, &built_lines(MAC_HOST, version, None))]),
        "a first install says no replaced line: {outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_eq!(calls(&steps), host_and_mac_calls());
    assert_eq!(pushed_commit(&mac.case, MAC_HOST), mac.commit, "the commit landed as the pushed ref");
    let root = mac_root(&mac.case, MAC_HOST);
    assert_eq!(names(&root.join("bin")), ["gate-box"], "no temporary copy is left");
    assert_eq!(
        names(&root.join("gate-push")),
        ["build.log", "push.lock", "src", "src.git", "target"],
        "every file the push keeps is in its own folder, and the building marker is gone"
    );
    let built = fs::read_to_string(root.join("bin").join("gate-box")).expect("the box can be read");
    assert!(built.contains(&format!("gate {version}")), "the installed box is the one built: {built}");
    let cargo = fs::read_to_string(builders(&mac.case).join(MAC_HOST).join("cargo.args")).expect("cargo's call was logged");
    let cargo = cargo.trim_end();
    let start = format!("GATE_BUILD_ID={} build --release --locked --manifest-path /", mac.id());
    assert!(
        cargo.starts_with(&start)
            && cargo.contains("/gate-push/src/Cargo.toml -p gate-box-unix --target-dir /")
            && cargo.ends_with("/gate-push/target --message-format=json")
            && cargo.lines().count() == 1,
        "one locked build of the box alone with the build id and no features: {cargo}"
    );
}

/// The routing file refuses an alias both `hosts` and `builders` name, so only file order is left to see here.
#[test]
fn a_push_reaches_macos_builders_after_host_and_hosts_in_file_order() {
    let mac = mac_case(
        "a_push_reaches_macos_builders_after_host_and_hosts_in_file_order",
        &mac_routing(&[OTHER_HOST], &[OTHER_MAC, MAC_HOST]),
    );
    let steps = steps(&mac.case, &["ok"; 8]);

    let outcome = mac_push(&mac, &steps);

    let version = &mac.version;
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let host_lines = mac_stdout(version, &[]);
    let hosts_lines = format!(
        "push: {OTHER_HOST}: sending gate {version}\npush: {OTHER_HOST}: installed gate {version}\n{}",
        desktop_lines(OTHER_HOST)
    );
    let macs: Vec<String> = [OTHER_MAC, MAC_HOST]
        .iter()
        .map(|mac| mac_lines(mac, version, &built_lines(mac, version, None)))
        .collect();
    let mac_lines = macs.concat();
    assert_eq!(
        unix(&outcome.stdout),
        format!("{host_lines}{hosts_lines}{mac_lines}"),
        "host, then hosts, then the macOS entries in file order: {outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    let expected: Vec<&str> = ["push", "push"].into_iter().chain(MAC_CALLS).chain(MAC_CALLS).collect();
    assert_eq!(calls(&steps), expected);
    assert_eq!(pushed_commit(&mac.case, OTHER_MAC), mac.commit);
    assert_eq!(pushed_commit(&mac.case, MAC_HOST), mac.commit);
}

/// Asserts a push of the macOS case `mac`, with the `source.json` the case left, refused its macOS builder with `reason`
/// before any call to it, and still pushed to [`HOST`].
fn assert_source_refused(mac: &MacCase, reason: &str) {
    let steps = steps(&mac.case, &["ok"]);

    let outcome = mac_push(mac, &steps);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(unix(&outcome.stdout), mac_stdout(&mac.version, &[(MAC_HOST, "")]), "{outcome:?}");
    assert_eq!(unix(&outcome.stderr), format!("push: {MAC_HOST} failed: {reason}\n"), "{outcome:?}");
    assert_eq!(calls(&steps), ["push"], "the macOS builder got no call");
    assert!(!builders(&mac.case).join(MAC_HOST).exists(), "nothing reached the macOS builder");
}

#[test]
fn a_push_with_no_source_json_refuses_a_macos_builder() {
    let mac = mac_case("a_push_with_no_source_json_refuses_a_macos_builder", &mac_routing(&[], &[MAC_HOST]));
    fs::remove_file(source_path(&mac.case)).expect("source.json can be removed");
    assert_source_refused(&mac, "no source.json beside the installed gate; reinstall it with tools/install.ps1");
}

#[test]
fn a_push_with_an_unreadable_source_json_refuses_a_macos_builder() {
    let mac = mac_case(
        "a_push_with_an_unreadable_source_json_refuses_a_macos_builder",
        &mac_routing(&[], &[MAC_HOST]),
    );
    fs::write(source_path(&mac.case), "[1]").expect("source.json can be written");
    assert_source_refused(&mac, "source.json is unreadable: it is not a JSON object");
}

#[test]
fn a_push_of_a_dirty_build_refuses_a_macos_builder() {
    let mac = mac_case("a_push_of_a_dirty_build_refuses_a_macos_builder", &mac_routing(&[], &[MAC_HOST]));
    mac.write_source(mac.id(), true);
    assert_source_refused(&mac, "the installed gate was built from uncommitted changes; commit them and reinstall");
}

#[test]
fn a_push_whose_source_json_names_another_build_refuses_a_macos_builder() {
    let mac = mac_case(
        "a_push_whose_source_json_names_another_build_refuses_a_macos_builder",
        &mac_routing(&[], &[MAC_HOST]),
    );
    let other = mac.commit[..13].to_owned();
    mac.write_source(&other, false);
    let reason = format!(
        "source.json does not match the installed gate ({other} vs {}); reinstall it with tools/install.ps1",
        mac.version
    );
    assert_source_refused(&mac, &reason);
}

#[test]
fn a_push_whose_source_json_names_no_build_refuses_a_macos_builder() {
    let mac = mac_case(
        "a_push_whose_source_json_names_no_build_refuses_a_macos_builder",
        &mac_routing(&[], &[MAC_HOST]),
    );
    mac.write_source("", false);
    let reason = format!(
        "source.json does not match the installed gate ( vs {}); reinstall it with tools/install.ps1",
        mac.version
    );
    assert_source_refused(&mac, &reason);
}

#[test]
fn a_push_whose_repo_lacks_the_commit_refuses_a_macos_builder() {
    let mac = mac_case(
        "a_push_whose_repo_lacks_the_commit_refuses_a_macos_builder",
        &mac_routing(&[], &[MAC_HOST]),
    );
    let elsewhere = mac.case.folder().join("elsewhere");
    git(&mac.case, &["init", "--quiet", "elsewhere"]);
    mac.write_source_of(&elsewhere, mac.id(), false);
    assert_source_refused(&mac, &format!("the repo {} does not hold commit {}", shown(&elsewhere), mac.commit));
}

#[test]
fn a_push_replaces_an_older_box_on_a_macos_builder() {
    let mac = mac_case("a_push_replaces_an_older_box_on_a_macos_builder", &mac_routing(&[], &[MAC_HOST]));
    let old = "0.1.0+aaaaaaaaaaaa";
    mac_box(&mac.case, MAC_HOST, old);
    let steps = steps(&mac.case, &["ok", "ok", "ok", "ok"]);

    let outcome = mac_push(&mac, &steps);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        unix(&outcome.stdout),
        mac_stdout(&mac.version, &[(MAC_HOST, &built_lines(MAC_HOST, &mac.version, Some(old)))]),
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_eq!(calls(&steps), host_and_mac_calls());
}

#[test]
fn a_macos_builder_whose_box_is_the_gate_s_build_gets_no_git_push_and_no_build() {
    let mac = mac_case(
        "a_macos_builder_whose_box_is_the_gate_s_build_gets_no_git_push_and_no_build",
        &mac_routing(&[], &[MAC_HOST]),
    );
    mac_box(&mac.case, MAC_HOST, &mac.version);
    let steps = steps(&mac.case, &["ok", "ok"]);

    let outcome = mac_push(&mac, &steps);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let lines = format!("push: {MAC_HOST}: gate-box {} already installed\n", mac.version);
    assert_eq!(unix(&outcome.stdout), mac_stdout(&mac.version, &[(MAC_HOST, &lines)]), "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_eq!(calls(&steps), ["push", "mac-prepare"], "one prepare call only");
}

#[test]
fn a_macos_builder_another_push_is_installing_on_says_so() {
    let mac = mac_case("a_macos_builder_another_push_is_installing_on_says_so", &mac_routing(&[], &[MAC_HOST]));
    let steps = steps(&mac.case, &["ok", "lock-busy"]);

    let outcome = mac_push(&mac, &steps);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(unix(&outcome.stdout), mac_stdout(&mac.version, &[(MAC_HOST, "")]), "{outcome:?}");
    assert_eq!(
        unix(&outcome.stderr),
        format!("push: {MAC_HOST} failed: another gate push is installing on this builder\n"),
        "{outcome:?}"
    );
    assert_eq!(calls(&steps), ["push", "mac-prepare"]);
}

#[test]
fn a_failed_build_on_a_macos_builder_fails_the_push_and_leaves_its_box() {
    let mac = mac_case(
        "a_failed_build_on_a_macos_builder_fails_the_push_and_leaves_its_box",
        &mac_routing(&[], &[MAC_HOST]),
    );
    let existing = mac_box(&mac.case, MAC_HOST, "0.1.0+aaaaaaaaaaaa");
    let before = fs::read(&existing).expect("the box can be read");
    let steps = steps(&mac.case, &["ok", "ok", "ok", "cargo-fail"]);

    let outcome = mac_push(&mac, &steps);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    let building = format!("push: {MAC_HOST}: building gate-box {}\n", mac.version);
    assert_eq!(unix(&outcome.stdout), mac_stdout(&mac.version, &[(MAC_HOST, &building)]), "{outcome:?}");
    let stderr = unix(&outcome.stderr);
    assert!(
        stderr.starts_with(&format!("push: {MAC_HOST} failed: cargo build exited 101; see /"))
            && stderr.ends_with("/gate-push/build.log\n")
            && stderr.lines().count() == 1,
        "one failure line naming the build log: {outcome:?}"
    );
    assert_holds(&existing, &before);
    assert_eq!(
        names(&mac_root(&mac.case, MAC_HOST).join("bin")),
        ["gate-box"],
        "no temporary copy is left"
    );
    assert_eq!(calls(&steps), host_and_mac_calls());
}

/// A build its own bound killed leaves no `building` marker.
#[test]
fn a_macos_build_killed_at_its_bound_leaves_no_building_marker() {
    let mac = mac_case(
        "a_macos_build_killed_at_its_bound_leaves_no_building_marker",
        &mac_routing(&[], &[MAC_HOST]),
    );
    let steps = steps(&mac.case, &["ok", "ok", "ok", "cargo-bound"]);

    let outcome = mac_push(&mac, &steps);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    let stderr = unix(&outcome.stderr);
    assert!(
        stderr.starts_with(&format!("push: {MAC_HOST} failed: cargo build did not finish in 1800 s; see /"))
            && stderr.ends_with("/gate-push/build.log\n")
            && stderr.lines().count() == 1,
        "one failure line naming the bound: {outcome:?}"
    );
    let push_folder = mac_root(&mac.case, MAC_HOST).join("gate-push");
    assert!(push_folder.join("build.log").exists(), "the build ran: {outcome:?}");
    assert!(!push_folder.join("building").exists(), "the bound's kill leaves no building marker");
}

/// Asserts a push of the macOS case `mac`, whose builder's prepare call takes the step `step`, failed that builder with
/// stderr's one line `push: <mac> failed: ` and what `reason` accepts, after that call alone, and still pushed to
/// [`HOST`].
fn assert_prepare_refused(mac: &MacCase, step: &str, reason: impl Fn(&str) -> bool) {
    let steps = steps(&mac.case, &["ok", step]);

    let outcome = mac_push(mac, &steps);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(unix(&outcome.stdout), mac_stdout(&mac.version, &[(MAC_HOST, "")]), "{outcome:?}");
    let stderr = unix(&outcome.stderr);
    let failed = stderr
        .strip_prefix(&format!("push: {MAC_HOST} failed: "))
        .and_then(|rest| rest.strip_suffix('\n'));
    assert!(failed.is_some_and(|failed| !failed.contains('\n') && reason(failed)), "{outcome:?}");
    assert_eq!(calls(&steps), ["push", "mac-prepare"], "no git push and no build");
}

#[test]
fn a_macos_builder_without_perl_is_refused_before_the_git_push() {
    let mac = mac_case(
        "a_macos_builder_without_perl_is_refused_before_the_git_push",
        &mac_routing(&[], &[MAC_HOST]),
    );
    assert_prepare_refused(&mac, "no-perl", |failed| failed == "the builder has no /usr/bin/perl");
    assert!(!mac_root(&mac.case, MAC_HOST).exists(), "nothing was made on the builder");
}

#[test]
fn a_macos_builder_whose_root_another_account_may_enter_is_refused_before_the_git_push() {
    let mac = mac_case(
        "a_macos_builder_whose_root_another_account_may_enter_is_refused_before_the_git_push",
        &mac_routing(&[], &[MAC_HOST]),
    );
    // The builder's user is the case's own account, named as the stand-in's `id -un` says it.
    assert_prepare_refused(&mac, "root-open", |failed| {
        failed.contains("/root is not owned by ") && failed.ends_with(" or is open to others")
    });
}

#[test]
fn a_macos_builder_without_cargo_is_refused_before_the_git_push() {
    let mac = mac_case(
        "a_macos_builder_without_cargo_is_refused_before_the_git_push",
        &mac_routing(&[], &[MAC_HOST]),
    );
    assert_prepare_refused(&mac, "no-cargo", |failed| {
        failed == r#"the builder has no cargo; run install-macos.sh --only "Rust""#
    });
    assert!(!mac_root(&mac.case, MAC_HOST).exists(), "nothing was made on the builder");
}

#[test]
fn a_macos_builder_whose_build_log_is_a_hard_link_is_refused() {
    let mac = mac_case(
        "a_macos_builder_whose_build_log_is_a_hard_link_is_refused",
        &mac_routing(&[], &[MAC_HOST]),
    );
    let push_folder = mac_root(&mac.case, MAC_HOST).join("gate-push");
    fs::create_dir_all(&push_folder).expect("the push folder can be made");
    let other = mac.case.folder().join("other.txt");
    fs::write(&other, "another file\n").expect("the other file can be written");
    fs::hard_link(&other, push_folder.join("build.log")).expect("build.log can be made a second name of the other file");

    assert_prepare_refused(&mac, "ok", |failed| {
        failed.starts_with('/') && failed.ends_with("/gate-push/build.log is a link")
    });
    assert_eq!(
        fs::read_to_string(&other).expect("the other file can be read"),
        "another file\n",
        "it is untouched"
    );
}

#[test]
fn a_macos_builder_whose_box_is_a_folder_is_refused_before_the_build() {
    let mac = mac_case(
        "a_macos_builder_whose_box_is_a_folder_is_refused_before_the_build",
        &mac_routing(&[], &[MAC_HOST]),
    );
    let folder = mac_root(&mac.case, MAC_HOST).join("bin").join("gate-box");
    fs::create_dir_all(&folder).expect("the box's name can be made a folder");
    let steps = steps(&mac.case, &["ok", "ok", "ok", "ok"]);

    let outcome = mac_push(&mac, &steps);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    let building = format!("push: {MAC_HOST}: building gate-box {}\n", mac.version);
    assert_eq!(unix(&outcome.stdout), mac_stdout(&mac.version, &[(MAC_HOST, &building)]), "{outcome:?}");
    let stderr = unix(&outcome.stderr);
    assert!(
        stderr.starts_with(&format!("push: {MAC_HOST} failed: /")) && stderr.ends_with("/bin/gate-box is a folder\n"),
        "{outcome:?}"
    );
    assert_eq!(calls(&steps), host_and_mac_calls());
    assert!(names(&folder).is_empty(), "nothing was moved into the folder");
    assert!(!builders(&mac.case).join(MAC_HOST).join("cargo.args").exists(), "nothing was built");
}

/// Makes a new commit in the macOS case's repo, its file saying `text`, and points `source.json` and the case's version
/// at it.
fn commit_again(mac: &mut MacCase, text: &str) {
    fs::write(mac.case.folder().join("repo").join("file.txt"), text).expect("the repo's file can be changed");
    git(&mac.case, &["-C", "repo", "commit", "--quiet", "-am", "again"]);
    mac.commit = git(&mac.case, &["-C", "repo", "rev-parse", "HEAD"]);
    mac.version = format!("0.1.0+{}", &mac.commit[..12]);
    mac.write_source(mac.id(), false);
}

/// The script's no-hooks and no-replace-refs settings have no plant that reaches the fresh `.git` this case shows (its
/// git reads no user config or template), so script-text tests in `push.rs` pin them.
#[test]
fn a_macos_build_makes_its_checkout_s_git_folder_afresh() {
    let mut mac = mac_case("a_macos_build_makes_its_checkout_s_git_folder_afresh", &mac_routing(&[], &[MAC_HOST]));
    let steps = steps(&mac.case, &["ok"; 8]);
    let first = mac_push(&mac, &steps);
    assert_eq!(first.code, Some(0), "{first:?}");
    let replaced = mac.version.clone();
    commit_again(&mut mac, "built again\n");
    let push_folder = mac_root(&mac.case, MAC_HOST).join("gate-push");
    let planted = push_folder.join("src").join(".git").join("planted");
    fs::write(&planted, "left by an earlier build\n").expect("a file can be planted in the checkout's .git");

    let outcome = mac_push(&mac, &steps);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let lines = mac_lines(MAC_HOST, &mac.version, &built_lines(MAC_HOST, &mac.version, Some(&replaced)));
    assert!(unix(&outcome.stdout).ends_with(&lines), "{outcome:?}");
    assert_eq!(outcome.stderr, "", "the checkout was readied once: {outcome:?}");
    assert_eq!(
        fs::read_to_string(push_folder.join("src").join("file.txt")).expect("the checkout's file can be read"),
        "built again\n",
        "the pushed commit's tree is checked out"
    );
    assert!(!planted.exists(), "the checkout's .git was made afresh");
}

#[test]
fn a_macos_prepare_deletes_old_ref_locks_a_cut_push_left() {
    let mut mac = mac_case("a_macos_prepare_deletes_old_ref_locks_a_cut_push_left", &mac_routing(&[], &[MAC_HOST]));
    let steps = steps(&mac.case, &["ok"; 8]);
    let first = mac_push(&mac, &steps);
    assert_eq!(first.code, Some(0), "{first:?}");
    let bare = mac_root(&mac.case, MAC_HOST).join("gate-push").join("src.git");
    let refs = bare.join("refs");
    let old = [
        refs.join("gate-push").join("src.lock"),
        bare.join("packed-refs.lock"),
        bare.join("HEAD.lock"),
    ];
    let hour_ago = SystemTime::now() - Duration::from_secs(3600);
    for lock in &old {
        fs::write(lock, "").expect("a lock can be written");
        File::options()
            .write(true)
            .open(lock)
            .and_then(|file| file.set_modified(hour_ago))
            .expect("the lock can be made an hour old");
    }
    let replaced = mac.version.clone();
    let young = refs.join("young.lock");
    fs::write(&young, "").expect("a young ref lock can be written");
    commit_again(&mut mac, "built again\n");

    let outcome = mac_push(&mac, &steps);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let lines = mac_lines(MAC_HOST, &mac.version, &built_lines(MAC_HOST, &mac.version, Some(&replaced)));
    assert!(unix(&outcome.stdout).ends_with(&lines), "{outcome:?}");
    assert_eq!(
        outcome.stderr, "",
        "the locks alone were swept and the checkout was readied once: {outcome:?}"
    );
    assert_eq!(pushed_commit(&mac.case, MAC_HOST), mac.commit, "the new commit landed as the pushed ref");
    for lock in &old {
        assert!(!lock.exists(), "the old lock {} is gone", lock.display());
    }
    assert!(young.exists(), "a ref lock a push may still hold is kept");
}

/// Writes an empty `path` last modified an hour ago.
fn old_file(path: &Path) {
    fs::write(path, "").expect("the file can be written");
    File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(SystemTime::now() - Duration::from_secs(3600)))
        .expect("the file can be made an hour old");
}

#[test]
fn a_macos_push_sweeps_an_old_config_lock_before_it_turns_gc_off() {
    let mut mac = mac_case(
        "a_macos_push_sweeps_an_old_config_lock_before_it_turns_gc_off",
        &mac_routing(&[], &[MAC_HOST]),
    );
    let steps = steps(&mac.case, &["ok"; 8]);
    let first = mac_push(&mac, &steps);
    assert_eq!(first.code, Some(0), "{first:?}");
    let bare = mac_root(&mac.case, MAC_HOST).join("gate-push").join("src.git");
    git(&mac.case, &["--git-dir", &shown(&bare), "config", "receive.autogc", "true"]);
    let lock = bare.join("config.lock");
    old_file(&lock);
    let replaced = mac.version.clone();
    commit_again(&mut mac, "built again\n");

    let outcome = mac_push(&mac, &steps);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let lines = mac_lines(MAC_HOST, &mac.version, &built_lines(MAC_HOST, &mac.version, Some(&replaced)));
    assert!(unix(&outcome.stdout).ends_with(&lines), "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert!(!lock.exists(), "the old config lock was swept");
    assert_eq!(
        git(&mac.case, &["--git-dir", &shown(&bare), "config", "--local", "receive.autogc"]),
        "false",
        "gc was turned off again once the lock was gone"
    );
}

/// A `config.lock` younger than the sweep's 30 minutes would fail any config write, so a push that succeeds past one
/// wrote none.
#[test]
fn a_macos_push_writes_no_config_when_gc_is_already_off() {
    let mut mac = mac_case("a_macos_push_writes_no_config_when_gc_is_already_off", &mac_routing(&[], &[MAC_HOST]));
    let steps = steps(&mac.case, &["ok"; 8]);
    let first = mac_push(&mac, &steps);
    assert_eq!(first.code, Some(0), "{first:?}");
    let lock = mac_root(&mac.case, MAC_HOST).join("gate-push").join("src.git").join("config.lock");
    fs::write(&lock, "").expect("a fresh config lock can be written");
    let replaced = mac.version.clone();
    commit_again(&mut mac, "built again\n");

    let outcome = mac_push(&mac, &steps);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let lines = mac_lines(MAC_HOST, &mac.version, &built_lines(MAC_HOST, &mac.version, Some(&replaced)));
    assert!(unix(&outcome.stdout).ends_with(&lines), "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert!(lock.exists(), "a config lock a writer may still hold is kept");
}

/// An interrupted `git init --bare` leaves a `src.git` that is no repository, which the next push makes again.
#[test]
fn a_macos_push_makes_a_src_git_that_is_no_repository_again() {
    let mac = mac_case("a_macos_push_makes_a_src_git_that_is_no_repository_again", &mac_routing(&[], &[MAC_HOST]));
    let bare = mac_root(&mac.case, MAC_HOST).join("gate-push").join("src.git");
    fs::create_dir_all(bare.join("objects")).expect("a half-made src.git can be planted");
    let steps = steps(&mac.case, &["ok", "ok", "ok", "ok"]);

    let outcome = mac_push(&mac, &steps);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        unix(&outcome.stdout),
        mac_stdout(&mac.version, &[(MAC_HOST, &built_lines(MAC_HOST, &mac.version, None))]),
        "{outcome:?}"
    );
    let stderr = unix(&outcome.stderr);
    assert!(
        stderr.starts_with(&format!("push: {MAC_HOST}: /"))
            && stderr.ends_with("/gate-push/src.git is not a repository; making it again\n")
            && stderr.lines().count() == 1,
        "one note that src.git was made again: {outcome:?}"
    );
    assert_eq!(pushed_commit(&mac.case, MAC_HOST), mac.commit, "the commit landed in the remade src.git");
}

/// A `src.git` that is no folder is refused as every push folder is, before the remake could touch it.
#[test]
fn a_macos_src_git_that_is_a_plain_file_is_refused_and_kept() {
    let mac = mac_case("a_macos_src_git_that_is_a_plain_file_is_refused_and_kept", &mac_routing(&[], &[MAC_HOST]));
    let bare = mac_root(&mac.case, MAC_HOST).join("gate-push").join("src.git");
    fs::create_dir_all(bare.parent().expect("src.git has a folder")).expect("the push folder can be made");
    fs::write(&bare, "not a repository\n").expect("a file can be planted as src.git");

    assert_prepare_refused(&mac, "ok", |failed| {
        failed.starts_with("could not create the folder /") && failed.ends_with("/gate-push/src.git: it is not a folder")
    });

    assert_holds(&bare, b"not a repository\n");
}

/// A `src.git` broken after the prepare call fails the build with its own line; only the prepare call makes one again.
#[test]
fn a_macos_build_meeting_a_broken_src_git_fails_and_leaves_it() {
    let mac = mac_case(
        "a_macos_build_meeting_a_broken_src_git_fails_and_leaves_it",
        &mac_routing(&[], &[MAC_HOST]),
    );
    let existing = mac_box(&mac.case, MAC_HOST, "0.1.0+aaaaaaaaaaaa");
    let before = fs::read(&existing).expect("the box can be read");
    let steps = steps(&mac.case, &["ok", "ok", "ok", "broken-src-git"]);

    let outcome = mac_push(&mac, &steps);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    let building = format!("push: {MAC_HOST}: building gate-box {}\n", mac.version);
    assert_eq!(unix(&outcome.stdout), mac_stdout(&mac.version, &[(MAC_HOST, &building)]), "{outcome:?}");
    let stderr = unix(&outcome.stderr);
    assert!(
        stderr.starts_with(&format!("push: {MAC_HOST} failed: /"))
            && stderr.ends_with("/gate-push/src.git is not a repository\n")
            && stderr.lines().count() == 1,
        "one failure line naming src.git: {outcome:?}"
    );
    let bare = mac_root(&mac.case, MAC_HOST).join("gate-push").join("src.git");
    assert!(
        bare.join("refs").join("gate-push").join("src").exists(),
        "the broken src.git keeps the pushed ref"
    );
    assert!(!bare.join("HEAD").exists(), "src.git was not made again");
    assert_holds(&existing, &before);
    assert_eq!(calls(&steps), host_and_mac_calls());
}
