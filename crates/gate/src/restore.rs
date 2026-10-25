//! The `dotnet restore` a routed dotnet build that brought its outputs back runs here when the project or solution it
//! names has no `obj\project.assets.json` on this PC, so a later `--no-build` run here keeps its word: the restore's
//! words, read from the command's own ([`restore_words`]), and its run, bounded, in a kill-on-close job of its own, its
//! output in a side file next to the run's log ([`run_restore`]).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::slice;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use gate_router::{DotnetTarget, dotnet_target, escaped, names_program};
use gate_win32::{Interrupt, Job, Launch, Running};

use crate::remote::git::spelled_limit;

/// How long a restore here may take before its process tree is ended, unless the routed command's ceiling leaves less.
pub(crate) const RESTORE_LIMIT: Duration = Duration::from_secs(300);
/// The least of the routed command's ceiling that must be left for a restore here to start.
pub(crate) const RESTORE_LEAST_LEFT: Duration = Duration::from_secs(10);
/// How often a running restore is looked at for its limit and the console interrupt.
const POLL: Duration = Duration::from_millis(100);
/// How long a restore whose job was ended is waited for to exit.
const KILL_WAIT: Duration = Duration::from_secs(30);
/// How long the restore's output readers get to end once its job is ended.
const DRAIN: Duration = Duration::from_secs(2);
/// The exit code a stopped restore's job ends its processes with.
const STOPPED: u32 = 124;
/// What every line of a restore that did not pass ends with.
const BUILDS_FIRST: &str = "a --no-build run here builds first";
/// How much of a stream a reader takes from its pipe at a time.
const CHUNK: usize = 8 * 1024;
/// The program name of a dotnet word.
const DOTNET: &str = "dotnet";
/// The word that ends a command's own words; the words after it go to the program the command runs.
const SEPARATOR: &str = "--";
/// The first character of a response-file word, whose words the gate cannot see.
const RESPONSE_FILE: char = '@';
/// The switch that ends every restore's words, so it leaves no build server running.
const DISABLE_BUILD_SERVERS: &str = "--disable-build-servers";
/// The options a restore carries as written, with the next word as their value or their value after `=` or `:`.
const CARRIED_WITH_VALUE: [&str; 3] = ["-r", "--runtime", "--source"];
/// The switches a restore carries as they are.
const CARRIED_SWITCHES: [&str; 1] = ["--use-current-runtime"];
/// The starts of the one-word property forms a restore carries as they are.
const PROPERTY_PREFIXES: [&str; 6] = ["-p:", "/p:", "--property:", "--property=", "-property:", "/property:"];
/// The long property option, whose next word is the property in its two-word form.
const LONG_PROPERTY: &str = "--property";
/// The short property option, whose next word is the property in its two-word form when it holds `=`; else it is
/// `dotnet run`'s old `-p <project>`, which a restore does not carry.
const SHORT_PROPERTY: &str = "-p";
/// The configuration options, which a restore rejects and is given as a property.
const CONFIGURATION_OPTIONS: [&str; 2] = ["-c", "--configuration"];
/// The options a restore does not carry that take the next word as their value, so it is never read as the target.
const VALUE_OPTIONS: [&str; 25] = [
    "-f",
    "--framework",
    "-o",
    "--output",
    "-v",
    "--verbosity",
    "-s",
    "--settings",
    "-l",
    "--logger",
    "--filter",
    "--results-directory",
    "--collect",
    "--version-suffix",
    "-lp",
    "--launch-profile",
    "--project",
    "-e",
    "--environment",
    "-d",
    "--diag",
    "--test-adapter-path",
    "--blame-hang-timeout",
    "--blame-hang-dump-type",
    "--blame-crash-dump-type",
];
/// The self-contained options, whose next word is their value when it is `true` or `false`, any case.
const SELF_CONTAINED_OPTIONS: [&str; 2] = ["--sc", "--self-contained"];
/// The values a self-contained option takes as its next word.
const SELF_CONTAINED_VALUES: [&str; 2] = ["true", "false"];
/// The extensions of the project and solution files a restore here runs for, compared ignoring case.
const RESTORABLE_EXTENSIONS: [&str; 5] = ["csproj", "fsproj", "vbproj", "sln", "slnx"];

/// The `dotnet restore` of a dotnet command: the command's own dotnet word, the target word as given, the target the
/// assets check reads, and the restore's other words.
pub(crate) struct RestorePlan {
    program: String,
    target: String,
    check: DotnetTarget,
    args: Vec<String>,
}

impl RestorePlan {
    /// The restore's whole command: its program, `restore`, its target, then its other words.
    pub(crate) fn words(&self) -> Vec<String> {
        let mut words = vec![self.program.clone(), "restore".to_owned(), self.target.clone()];
        words.extend(self.args.iter().cloned());
        words
    }

    /// The target word as the command gave it.
    pub(crate) fn target(&self) -> &str {
        &self.target
    }

    /// The project, solution or folder the assets check reads ([`crate::no_build::named_without_assets`]).
    pub(crate) fn check(&self) -> &DotnetTarget {
        &self.check
    }
}

/// The `dotnet restore` of the dotnet command `words`, run in `folder`; `None` for any other command, for one wrapped
/// in a script (`pwsh ... -- dotnet ...`, whose script may set its own SDK or feeds), for one with no verb, for one
/// whose own words name a response file, and for one that names no target. Its program is the command's own dotnet
/// word; its target the project or solution the command names ([`dotnet_target`]), else the first of its own words
/// after the verb that starts with neither `-` nor `/`, is no value of an option the gate knows and names a file or
/// folder that exists, read against `folder`. It carries the runtime (`-r`, `--runtime`, `--use-current-runtime`), the
/// long `--source`, and every property the command sets, its configuration as `-p:Configuration=X`, and never its
/// framework (`-f` is `--force` to a restore) or a word after its own `--`; `--disable-build-servers` ends it.
pub(crate) fn restore_words(words: &[String], folder: &Path) -> Option<RestorePlan> {
    let own = direct_dotnet_own(words)?;
    let verb = 1 + own[1..].iter().position(|word| !word.is_empty())?;
    let (mut args, positional) = carried(&own[verb + 1..], folder);
    args.push(DISABLE_BUILD_SERVERS.to_owned());
    let (target, check) = match dotnet_target(words) {
        Some(DotnetTarget::Project(target)) if !target.is_empty() => (target.clone(), DotnetTarget::Project(target)),
        Some(DotnetTarget::Solution(target)) if !target.is_empty() => (target.clone(), DotnetTarget::Solution(target)),
        _ => positional.map(|target| (target.clone(), DotnetTarget::Project(target)))?,
    };
    Some(RestorePlan {
        program: own[0].clone(),
        target,
        check,
        args,
    })
}

/// The own words of a dotnet command run directly: from its first word, which names `dotnet`, up to, not including,
/// its first `--`. `None` for any other command, a dotnet command a script runs among them, and for one whose own
/// words name a response file.
fn direct_dotnet_own(words: &[String]) -> Option<&[String]> {
    let start = words.iter().position(|word| !word.is_empty())?;
    if !names_program(&words[start], DOTNET) {
        return None;
    }
    let end = words[start + 1..]
        .iter()
        .position(|word| word == SEPARATOR)
        .map_or(words.len(), |at| start + 1 + at);
    let own = &words[start..end];
    (!own.iter().any(|word| word.starts_with(RESPONSE_FILE))).then_some(own)
}

/// The words a restore carries from a dotnet command's own words after its verb, `rest`, in their order, and the first
/// of them that names a file or folder in `folder` and is no option or option value.
fn carried(rest: &[String], folder: &Path) -> (Vec<String>, Option<String>) {
    let mut args = Vec::new();
    let mut positional = None;
    let mut words = rest.iter();
    while let Some(word) = words.next() {
        if let Some(kept) = carried_word(word, &mut words) {
            args.extend(kept);
        } else if !skips_value(word, &mut words) && positional.is_none() && names_on_disk(word, folder) {
            positional = Some(word.clone());
        }
    }
    (args, positional)
}

/// Whether `word` is an option a restore does not carry, its value, the next of `rest`, taken with it: always for one
/// of [`VALUE_OPTIONS`], and for one of [`SELF_CONTAINED_OPTIONS`] when the next word is one of
/// [`SELF_CONTAINED_VALUES`].
fn skips_value(word: &str, rest: &mut slice::Iter<'_, String>) -> bool {
    if VALUE_OPTIONS.contains(&word) {
        rest.next();
        return true;
    }
    if !SELF_CONTAINED_OPTIONS.contains(&word) {
        return false;
    }
    let valued = rest
        .as_slice()
        .first()
        .is_some_and(|next| SELF_CONTAINED_VALUES.iter().any(|value| next.eq_ignore_ascii_case(value)));
    if valued {
        rest.next();
    }
    true
}

/// What a restore carries for `word`, the next of `rest` taken when it is the word's value; `None` when `word` is no
/// option a restore reads, and an empty list for one it reads and drops.
fn carried_word(word: &str, rest: &mut slice::Iter<'_, String>) -> Option<Vec<String>> {
    if let Some(value) = valued(word, rest, &CONFIGURATION_OPTIONS) {
        return Some(value.iter().map(|value| format!("-p:Configuration={value}")).collect());
    }
    if CARRIED_SWITCHES.contains(&word) || PROPERTY_PREFIXES.iter().any(|prefix| word.starts_with(prefix)) {
        return Some(vec![word.to_owned()]);
    }
    two_word_property(word, rest).or_else(|| kept(word, rest, &CARRIED_WITH_VALUE))
}

/// The two-word property `word` starts (`--property Name=Value`, `-p Name=Value`), its value the next of `rest`; an
/// empty list for a `-p` whose value holds no `=`, `dotnet run`'s old project option. `None` for any other word.
fn two_word_property(word: &str, rest: &mut slice::Iter<'_, String>) -> Option<Vec<String>> {
    if word != LONG_PROPERTY && word != SHORT_PROPERTY {
        return None;
    }
    let value = rest.next()?;
    let is_property = word == LONG_PROPERTY || value.contains('=');
    Some(if is_property { vec![word.to_owned(), value.clone()] } else { Vec::new() })
}

/// `word` and its value as written, when `word` is one of `spellings`: with the next of `rest`, or alone when its value
/// follows it after `=` or `:`; empty for a spelling with no word after it.
fn kept(word: &str, rest: &mut slice::Iter<'_, String>, spellings: &[&str]) -> Option<Vec<String>> {
    if spellings.contains(&word) {
        return Some(rest.next().map(|value| vec![word.to_owned(), value.clone()]).unwrap_or_default());
    }
    inline_value(word, spellings).map(|_| vec![word.to_owned()])
}

/// The values of `word` when it is one of `spellings`: the next of `rest`, none when there is none, or the value after
/// its `=` or `:`.
fn valued(word: &str, rest: &mut slice::Iter<'_, String>, spellings: &[&str]) -> Option<Vec<String>> {
    if spellings.contains(&word) {
        return Some(rest.next().cloned().into_iter().collect());
    }
    inline_value(word, spellings).map(|value| vec![value.to_owned()])
}

/// The value of `word` written as one of `spellings` with its value after `=` or `:` (`-c=X`, `-c:X`).
fn inline_value<'a>(word: &'a str, spellings: &[&str]) -> Option<&'a str> {
    spellings
        .iter()
        .find_map(|spelling| word.strip_prefix(spelling).and_then(|after| after.strip_prefix(['=', ':'])))
}

/// Whether `word` names a file or folder that exists, read against `folder`, and starts with neither `-` nor `/`.
fn names_on_disk(word: &str, folder: &Path) -> bool {
    !word.is_empty() && !word.starts_with(['-', '/']) && folder.join(word).exists()
}

/// How a restore here ended once it started.
#[derive(Debug)]
pub(crate) enum Restored {
    /// It exited 0.
    Passed,
    /// It exited with this other code.
    Failed(u32),
    /// It ran past its limit and its process tree was ended.
    Stopped,
    /// A Ctrl+C ended it.
    Interrupted,
}

/// How a restore here that started went: how it ended, and a note of its output that could not be written to the side
/// file, `None` when all of it was.
#[derive(Debug)]
pub(crate) struct Ran {
    pub(crate) restored: Restored,
    pub(crate) unwritten: Option<String>,
}

/// The side file a restore here writes its output to: the run's `log` path with `.restore.log` appended.
pub(crate) fn side_log(log: &Path) -> PathBuf {
    let mut side = log.as_os_str().to_owned();
    side.push(".restore.log");
    PathBuf::from(side)
}

/// The side file at `side`, made anew: whatever was at its name removed first (a link itself, not its target), then
/// created as a file no other name had.
///
/// # Errors
///
/// Why the name could not be removed or the file created.
fn fresh_side(side: &Path) -> Result<File, String> {
    match fs::remove_file(side) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            return Err(format!("cannot remove the earlier {}: {error}", side.display()));
        }
        _ => {}
    }
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(side)
        .map_err(|error| format!("cannot create {}: {error}", side.display()))
}

/// Runs `launch`, a restore with its output piped back, in an anonymous kill-on-close job of its own, its stdout and
/// stderr copied into the side file `side`, made anew first ([`fresh_side`]), and ends its whole job past `limit` or on
/// the console `interrupt`. How it went; whatever it left running ends with its job. A reader that failed or did not
/// end in time leaves the outcome as the process's exit, and is noted at the end of the side file.
///
/// # Errors
///
/// Why it could not run, could not be waited for or stopped, or its side file could not be made.
pub(crate) fn run_restore(launch: &Launch, side: &Path, limit: Duration, interrupt: Option<Interrupt>) -> Result<Ran, String> {
    let file = Arc::new(Mutex::new(fresh_side(side)?));
    let job = Job::anonymous_kill_on_close().map_err(|error| format!("cannot create the restore's job: {error}"))?;
    let mut suspended = launch.start_suspended().map_err(|error| error.to_string())?;
    let (stdout, stderr) = suspended
        .take_output()
        .ok_or_else(|| "the restore's output pipes were not made".to_owned())?;
    let running = suspended.assign_and_resume(&job).map_err(|error| error.to_string())?;
    let readers = [copying(stdout, Arc::clone(&file)), copying(stderr, Arc::clone(&file))];
    let restored = waited(&running, &job, limit, interrupt);
    // Whatever the restore left running, which would hold its pipes open, ends with its job.
    drop(job);
    let restored = restored?;
    let problems = drained(&readers);
    let unwritten = (!problems.is_empty()).then(|| noted(&file, &problems)).flatten();
    Ok(Ran { restored, unwritten })
}

/// The problems of the restore's output readers, none when each wrote its stream whole before [`DRAIN`] passed.
fn drained(readers: &[Receiver<io::Result<()>>]) -> Vec<String> {
    let until = Instant::now() + DRAIN;
    readers
        .iter()
        .filter_map(|reader| match reader.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(format!("gate: the restore's output could not all be written here: {error}")),
            Err(_) => Some(format!(
                "gate: the restore's output did not end within {} s of its end, so this file may lack its last lines",
                DRAIN.as_secs()
            )),
        })
        .collect()
}

/// Appends `problems` to the end of the side file `file`, a line each; why they could not be, `None` when they were.
fn noted(file: &Mutex<File>, problems: &[String]) -> Option<String> {
    let mut text = String::new();
    for problem in problems {
        text.push_str(problem);
        text.push('\n');
    }
    let written = match file.lock() {
        Ok(mut file) => file.write_all(text.as_bytes()),
        Err(_) => Err(io::Error::other("a reader of the restore failed mid-write")),
    };
    written.err().map(|error| format!("{} ({error})", problems.join("; ")))
}

/// How the running restore ended: its exit, a Ctrl+C, which ends its job, or its limit, past which its job is ended.
fn waited(running: &Running, job: &Job, limit: Duration, interrupt: Option<Interrupt>) -> Result<Restored, String> {
    let deadline = Instant::now() + limit;
    let interrupted = || interrupt.is_some_and(Interrupt::is_raised);
    loop {
        if interrupted() {
            return stopped(running, job).map(|()| Restored::Interrupted);
        }
        let slice = POLL.min(deadline.saturating_duration_since(Instant::now()));
        let exited = running.wait(slice).map_err(|error| format!("cannot wait for dotnet restore: {error}"))?;
        match exited {
            Some(code) => return Ok(exit_ended(code, interrupted())),
            None if Instant::now() >= deadline => return stopped(running, job).map(|()| Restored::Stopped),
            None => {}
        }
    }
}

/// How a restore that exited with `code` ended: interrupted, whatever the code, when the console interrupt was raised
/// by then (`interrupted`).
fn exit_ended(code: u32, interrupted: bool) -> Restored {
    match code {
        _ if interrupted => Restored::Interrupted,
        0 => Restored::Passed,
        code => Restored::Failed(code),
    }
}

/// Ends every process of the restore's job and waits [`KILL_WAIT`] for its own to exit; else why it could not.
fn stopped(running: &Running, job: &Job) -> Result<(), String> {
    job.terminate(STOPPED)
        .map_err(|error| format!("dotnet restore could not be stopped: {error}"))?;
    match running.wait(KILL_WAIT) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(format!("dotnet restore did not end within {} s of its kill", KILL_WAIT.as_secs())),
        Err(error) => Err(format!("dotnet restore could not be stopped: {error}")),
    }
}

/// Copies `pipe` into `side` on a thread of its own, to the pipe's end; how the copy ended, once it has.
fn copying(mut pipe: File, side: Arc<Mutex<File>>) -> Receiver<io::Result<()>> {
    let (sent, received) = mpsc::channel();
    thread::spawn(move || {
        // The receiver is gone only once the restore stopped waiting on this stream.
        drop(sent.send(copied(&mut pipe, &side)));
    });
    received
}

/// Writes what `pipe` holds into `side` as it comes, to the pipe's end.
fn copied(pipe: &mut File, side: &Mutex<File>) -> io::Result<()> {
    let mut chunk = vec![0_u8; CHUNK];
    loop {
        let read = pipe.read(&mut chunk)?;
        if read == 0 {
            return Ok(());
        }
        let mut file = side
            .lock()
            .map_err(|_| io::Error::other("another reader of the restore failed mid-write"))?;
        file.write_all(&chunk[..read])?;
    }
}

/// The lines a restore here of `plan` says once it ended as `ran`, `took` after it was started, under `limit`, its
/// output in `side`: how it ended, then why part of its output is missing from `side` when it is; one that could not
/// run says why. The target and the side file's path are escaped.
pub(crate) fn said(plan: &RestorePlan, side: &Path, took: Duration, limit: Duration, ran: Result<Ran, String>) -> Vec<String> {
    let target = escaped(plan.target());
    let side = escaped(&side.to_string_lossy());
    let ran = match ran {
        Ok(ran) => ran,
        Err(error) => return vec![cannot_run(&error)],
    };
    let ended = match ran.restored {
        Restored::Passed => format!(
            "gate: restored {target} here in {:.1} s, so a --no-build run here can use the routed build's outputs",
            took.as_secs_f64()
        ),
        Restored::Failed(code) => format!("gate: dotnet restore of {target} here failed with exit {code} ({side}); {BUILDS_FIRST}"),
        Restored::Stopped => format!(
            "gate: dotnet restore of {target} here ran past {} and was stopped ({side}); {BUILDS_FIRST}",
            spelled_limit(limit)
        ),
        Restored::Interrupted => format!("gate: dotnet restore of {target} here was interrupted ({side}); {BUILDS_FIRST}"),
    };
    let mut lines = vec![ended];
    lines.extend(
        ran.unwritten
            .map(|why| format!("gate: cannot write the end of {side}: {}", escaped(&why))),
    );
    lines
}

/// Whether the restore's target `target` is a project or solution file: its extension one of
/// [`RESTORABLE_EXTENSIONS`], any case.
pub(crate) fn names_restorable(target: &str) -> bool {
    Path::new(target)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| RESTORABLE_EXTENSIONS.iter().any(|known| extension.eq_ignore_ascii_case(known)))
}

/// Why a restore here that is due does not start, as [`not_restoring`] says it.
pub(crate) enum NotRestoring {
    /// Its target is a folder or a file other than a project or solution file ([`names_restorable`]).
    NotProjectOrSolution,
    /// The console interrupt was raised before it started.
    Interrupted,
    /// Under [`RESTORE_LEAST_LEFT`] of the routed command's ceiling is left.
    TooLittleLeft,
}

/// The line a restore here of `plan` that does not start says, for `why`; its target escaped.
pub(crate) fn not_restoring(plan: &RestorePlan, why: &NotRestoring) -> String {
    let reason = match why {
        NotRestoring::NotProjectOrSolution => "it is not a project or solution file".to_owned(),
        NotRestoring::Interrupted => "Ctrl+C was pressed".to_owned(),
        NotRestoring::TooLittleLeft => format!("under {} s of the routed command's ceiling is left", RESTORE_LEAST_LEFT.as_secs()),
    };
    format!("gate: not restoring {} here: {reason}; {BUILDS_FIRST}", escaped(plan.target()))
}

/// The line of a restore here that could not run, for `error`, escaped.
pub(crate) fn cannot_run(error: &str) -> String {
    format!("gate: cannot run dotnet restore here: {}; {BUILDS_FIRST}", escaped(error))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its folders must fail loudly")]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{Restored, exit_ended, restore_words};

    /// A folder of this test process's own named `name`, holding the folder `tests/X`.
    fn folder(name: &str) -> PathBuf {
        let folder = crate::test_support::temp_dir().join(format!("gate-restore-{name}-{}", std::process::id()));
        if folder.exists() {
            fs::remove_dir_all(&folder).expect("a folder left by an earlier run can be removed");
        }
        fs::create_dir_all(folder.join("tests").join("X")).expect("the folder can be made");
        folder
    }

    /// The restore of `command`, split on spaces, run in `folder`: its whole words and its target.
    fn restore_of(command: &str, folder: &Path) -> Option<(Vec<String>, String)> {
        let words: Vec<String> = command.split(' ').map(str::to_owned).collect();
        restore_words(&words, folder).map(|plan| (plan.words(), plan.target().to_owned()))
    }

    /// Asserts that `command`, run in a folder holding `tests/X`, restores with `expected`'s words, split on spaces,
    /// and the target `target`.
    fn assert_restores(name: &str, command: &str, expected: &str, target: &str) {
        let folder = folder(name);
        let expected: Vec<String> = expected.split(' ').map(str::to_owned).collect();
        assert_eq!(restore_of(command, &folder), Some((expected, target.to_owned())), "{command}");
        fs::remove_dir_all(&folder).expect("the test folder can be removed");
    }

    /// Asserts that `command`, run in a folder holding `tests/X`, restores nothing.
    fn assert_restores_nothing(name: &str, command: &str) {
        let folder = folder(name);
        assert_eq!(restore_of(command, &folder), None, "{command}");
        fs::remove_dir_all(&folder).expect("the test folder can be removed");
    }

    #[test]
    fn a_build_s_configuration_becomes_a_property() {
        assert_restores(
            "configuration",
            "dotnet build app/App.csproj -c Release",
            "dotnet restore app/App.csproj -p:Configuration=Release --disable-build-servers",
            "app/App.csproj",
        );
    }

    #[test]
    fn a_test_s_framework_and_filter_are_not_carried() {
        assert_restores(
            "framework",
            "dotnet test tests/X -c Debug -f net10.0 --filter A",
            "dotnet restore tests/X -p:Configuration=Debug --disable-build-servers",
            "tests/X",
        );
    }

    #[test]
    fn runtime_and_properties_are_carried() {
        assert_restores(
            "runtime",
            "dotnet build app.csproj -r win-x64 -p:Foo=1 --property:Bar=2",
            "dotnet restore app.csproj -r win-x64 -p:Foo=1 --property:Bar=2 --disable-build-servers",
            "app.csproj",
        );
    }

    #[test]
    fn a_wrapped_dotnet_command_restores_nothing() {
        assert_restores_nothing("wrapped", "pwsh -NoProfile -File t.ps1 -- dotnet build a.csproj");
    }

    #[test]
    fn words_after_the_dotnet_command_s_separator_are_never_carried() {
        assert_restores(
            "separator",
            "dotnet test tests/X -- --filter-class X",
            "dotnet restore tests/X --disable-build-servers",
            "tests/X",
        );
    }

    #[test]
    fn a_response_file_command_restores_nothing() {
        assert_restores_nothing("response", "dotnet build @args.rsp");
    }

    #[test]
    fn the_dotnet_word_is_kept_as_given() {
        assert_restores(
            "program",
            r"C:\sdk\dotnet.exe build a.csproj",
            r"C:\sdk\dotnet.exe restore a.csproj --disable-build-servers",
            "a.csproj",
        );
    }

    #[test]
    fn a_command_that_names_no_target_restores_nothing() {
        assert_restores_nothing("no-target", "dotnet build -r win-x64 missing");
    }

    #[test]
    fn an_environment_value_is_no_target() {
        assert_restores(
            "environment",
            "dotnet test -e FOO=1 tests/X",
            "dotnet restore tests/X --disable-build-servers",
            "tests/X",
        );
    }

    #[test]
    fn a_diagnostics_file_is_no_target() {
        assert_restores(
            "diag",
            "dotnet build --diag d.log app/App.csproj",
            "dotnet restore app/App.csproj --disable-build-servers",
            "app/App.csproj",
        );
    }

    #[test]
    fn every_property_form_is_carried() {
        assert_restores(
            "properties",
            "dotnet build a.csproj -p Foo=1 --property Bar=2 /property:Baz=3 /p:Qux=4",
            "dotnet restore a.csproj -p Foo=1 --property Bar=2 /property:Baz=3 /p:Qux=4 --disable-build-servers",
            "a.csproj",
        );
    }

    #[test]
    fn the_long_source_is_carried_and_the_short_s_is_not() {
        assert_restores(
            "source",
            "dotnet test a.csproj --source feed1 --source=feed2 -s x.runsettings",
            "dotnet restore a.csproj --source feed1 --source=feed2 --disable-build-servers",
            "a.csproj",
        );
    }

    /// Asserts that `command`, run in a folder holding `tests/X` and a folder named for each of `decoys`, restores
    /// `tests/X` with `expected`'s words, split on spaces.
    fn assert_restores_past(name: &str, command: &str, decoys: &[&str], expected: &str) {
        let folder = folder(name);
        for decoy in decoys {
            fs::create_dir_all(folder.join(decoy)).expect("the decoy folder can be made");
        }
        let expected: Vec<String> = expected.split(' ').map(str::to_owned).collect();
        assert_eq!(restore_of(command, &folder), Some((expected, "tests/X".to_owned())), "{command}");
        fs::remove_dir_all(&folder).expect("the test folder can be removed");
    }

    #[test]
    fn a_dump_type_value_is_not_the_target() {
        for option in ["--blame-hang-dump-type", "--blame-crash-dump-type"] {
            assert_restores_past(
                "dump-type",
                &format!("dotnet test {option} full tests/X"),
                &["full"],
                "dotnet restore tests/X --disable-build-servers",
            );
        }
    }

    #[test]
    fn a_self_contained_value_is_not_the_target() {
        for (option, value) in [("--sc", "true"), ("--self-contained", "false"), ("--sc", "TRUE")] {
            assert_restores_past(
                "self-contained",
                &format!("dotnet build {option} {value} tests/X"),
                &["true", "false", "TRUE"],
                "dotnet restore tests/X --disable-build-servers",
            );
        }
    }

    #[test]
    fn a_ctrl_c_during_a_restore_that_exits_1_is_interrupted() {
        assert!(matches!(exit_ended(1, true), Restored::Interrupted));
        assert!(matches!(exit_ended(1, false), Restored::Failed(1)));
        assert!(matches!(exit_ended(0, false), Restored::Passed));
    }
}
