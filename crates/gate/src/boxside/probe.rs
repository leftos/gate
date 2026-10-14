//! The builder's tool probes: for each tool a routed run needs, what the builder has, read by running the tool's own
//! version command from this module's table, never a program the caller names. Every probe of a batch that runs a
//! program runs at once, under one deadline the batch shares, in the system folder and on a hidden console of its own,
//! as [`git::program_output`] runs a program; a probe still running at the deadline is ended with its job. Nothing here
//! writes to the console or reads a payload: the caller hands over the names and says the lines.

use std::any::Any;
use std::path::Path;
use std::thread::{self, Scope, ScopedJoinHandle};
use std::time::{Duration, Instant};

use gate_win32::PipedConsole;

use crate::boxside::payload::escaped;
use crate::environment::Variables;
use crate::remote::git::{self, GitError, ProgramCall, ProgramOutput, ProgramPlace};

/// How long a batch of probes may take together, unless a test seam says otherwise.
pub(crate) const PROBE_LIMIT: Duration = Duration::from_secs(10);

/// The most digits a part of a version may have: a longer one is no version the client could compare.
const MAX_PART_DIGITS: usize = 10;

/// What a probe found of one tool.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Probed {
    /// It ran, and its version reads as this text.
    Version(String),
    /// It ran, and no version reads in what it wrote.
    Present,
    /// It is not found.
    Missing,
    /// It was not probed to an answer, for this reason.
    Skipped(String),
}

/// How the table probes a tool it knows.
enum ToolProbe {
    /// Runs `program` with `args`.
    Runs { program: &'static str, args: &'static [&'static str] },
    /// The tool is known, but no command probes it.
    Unprobed,
}

/// The table's probe of the tool `name`; `None` for a name it does not know. cargo-nextest is run as the plugin itself,
/// never through cargo, which would read a `.cargo\config.toml` in every folder above its working folder and run the
/// alias one defines.
fn probe_of(name: &str) -> Option<ToolProbe> {
    let runs = |program, args| Some(ToolProbe::Runs { program, args });
    match name {
        "rust" => runs("rustc", &["--version"]),
        "cargo-nextest" => runs("cargo-nextest", &["nextest", "--version"]),
        "node" => runs("node", &["--version"]),
        "pnpm" => runs("pnpm", &["--version"]),
        "pwsh" => runs("pwsh", &["--version"]),
        "ffmpeg" => runs("ffmpeg", &["-version"]),
        "godot" => Some(ToolProbe::Unprobed),
        _ => None,
    }
}

/// A probe of the batch: found without running anything, or running on a thread of its own.
enum Pending<'scope> {
    Found(Probed),
    Running(ScopedJoinHandle<'scope, Probed>),
}

/// Probes each tool of `names`, all within `limit` from now, and gives back each name with what its probe found, in
/// the order of `names`. The probes that run a program run at once, each on a thread of its own, sharing the system
/// folder and the environment `variables` for the batch, whose programs are looked for along its `PATH`; with a
/// `worktree`, a program found that lies inside it is refused ([`git::judged`]). Environment `variables` that could
/// not be made, for the reason given, skip every probe that runs a program.
pub(crate) fn probe(names: &[String], limit: Duration, variables: Result<Variables, String>, worktree: Option<&Path>) -> Vec<(String, Probed)> {
    let deadline = Instant::now() + limit;
    let place = variables.and_then(ProgramPlace::new);
    thread::scope(|scope| {
        let pending: Vec<Pending<'_>> = names.iter().map(|name| started(scope, name, &place, worktree, deadline, limit)).collect();
        names
            .iter()
            .zip(pending)
            .map(|(name, pending)| {
                let found = match pending {
                    Pending::Found(found) => found,
                    Pending::Running(probe) => probe.join().unwrap_or_else(|panic| Probed::Skipped(panic_reason(&*panic))),
                };
                (name.clone(), found)
            })
            .collect()
    })
}

/// The probe of the tool `name`: what is found without running anything, or the thread on `scope` that runs its
/// program in `place`, never one inside `worktree`, by `deadline`, the end of the batch's `limit`.
fn started<'scope, 'env>(
    scope: &'scope Scope<'scope, 'env>,
    name: &str,
    place: &'env Result<ProgramPlace, String>,
    worktree: Option<&'env Path>,
    deadline: Instant,
    limit: Duration,
) -> Pending<'scope> {
    match probe_of(name) {
        None => Pending::Found(Probed::Skipped("unknown tool".to_owned())),
        Some(ToolProbe::Unprobed) => Pending::Found(Probed::Skipped(format!("no probe for {name}"))),
        Some(ToolProbe::Runs { program, args }) => match place {
            Err(why) => Pending::Found(Probed::Skipped(format!("cannot run {program}: {why}"))),
            Ok(place) => Pending::Running(scope.spawn(move || ran(&Run { program, args, worktree }, place, deadline, limit))),
        },
    }
}

/// A probe's program, its arguments, and the worktree it must not be found inside.
struct Run<'a> {
    program: &'a str,
    args: &'a [&'a str],
    worktree: Option<&'a Path>,
}

/// What running `run`'s program in `place` finds by `deadline`, the end of the batch's `limit`; nothing is run once
/// the deadline has passed.
fn ran(run: &Run<'_>, place: &ProgramPlace, deadline: Instant, limit: Duration) -> Probed {
    let Run { program, args, worktree } = *run;
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Probed::Skipped(format!("no answer in {}", git::spelled_limit(limit)));
    }
    let call = ProgramCall {
        program,
        args,
        limit: remaining,
        interrupt: None,
        worktree,
        console: PipedConsole::Hidden,
    };
    match git::program_output(&call, place) {
        Ok(None) => Probed::Missing,
        Ok(Some(output)) => read(&output),
        Err(GitError::Failed(why)) => Probed::Skipped(late_reason(&why, &call.late_line(), limit).unwrap_or(why)),
        Err(error) => Probed::Skipped(String::from(error)),
    }
}

/// The reason a probe that failed with `why` is skipped for, when `why` is its `late` line, alone or followed by why
/// it could not be stopped: `no answer in <limit>` with the batch's `limit`, and what followed. `None` for another
/// failure.
fn late_reason(why: &str, late: &str, limit: Duration) -> Option<String> {
    why.strip_prefix(late)
        .map(|rest| format!("no answer in {}{rest}", git::spelled_limit(limit)))
}

/// The reason a probe whose thread panicked with `panic` is skipped for, its message when it has one.
pub(crate) fn panic_reason(panic: &(dyn Any + Send)) -> String {
    let message = panic
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned());
    message.map_or_else(|| "the probe failed".to_owned(), |message| format!("the probe failed: {message}"))
}

/// What a probe that ran to its exit with `output` found: present when it exited non-zero or its first line holds no
/// version; else that version.
fn read(output: &ProgramOutput) -> Probed {
    if !output.status.success() {
        return Probed::Present;
    }
    version_of(&String::from_utf8_lossy(&output.stdout), &String::from_utf8_lossy(&output.stderr)).map_or(Probed::Present, Probed::Version)
}

/// The version a probe wrote: the first version of the first non-blank line of its standard output, or of its standard
/// error when its standard output holds nothing but blanks. Only that line is read: a tool's later lines may name
/// another program's version, as ffmpeg's `built with gcc 14.2.0` does.
fn version_of(stdout: &str, stderr: &str) -> Option<String> {
    let chosen = if stdout.trim().is_empty() { stderr } else { stdout };
    chosen.lines().find(|line| !line.trim().is_empty()).and_then(first_version)
}

/// The first version `text` spells: the first run of two or more dot-separated runs of ASCII digits, after an optional
/// `v`, that starts no word or number already begun (so not after a letter, a digit or a `.`), cut to its first three
/// numbers; what follows it, such as a `-` build label, is no part of it. `None` when there is no such run, or when a
/// number of the first one has more than [`MAX_PART_DIGITS`] digits.
pub(crate) fn first_version(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let parts = (0..bytes.len())
        .filter(|&at| starts_number(bytes, at))
        .find_map(|at| dotted(&text[at..]))?;
    let readable = parts.iter().all(|part| part.len() <= MAX_PART_DIGITS);
    readable.then(|| parts[..parts.len().min(3)].join("."))
}

/// Whether a version may start at `at` in `bytes`: a digit not after a letter, a digit or a `.`, or after a `v` that
/// is itself not after one of them.
fn starts_number(bytes: &[u8], at: usize) -> bool {
    let free = |before: Option<&u8>| before.is_none_or(|byte| !byte.is_ascii_alphanumeric() && *byte != b'.');
    let before = at.checked_sub(1).and_then(|index| bytes.get(index));
    bytes[at].is_ascii_digit()
        && match before {
            Some(b'v' | b'V') => free(at.checked_sub(2).and_then(|index| bytes.get(index))),
            _ => free(before),
        }
}

/// The dot-separated runs of digits `text` starts with, up to the first empty one, when there are two or more; `None`
/// when there are fewer.
fn dotted(text: &str) -> Option<Vec<&str>> {
    let end = text
        .find(|character: char| !character.is_ascii_digit() && character != '.')
        .unwrap_or(text.len());
    let parts: Vec<&str> = text[..end].split('.').take_while(|part| !part.is_empty()).collect();
    (parts.len() >= 2).then_some(parts)
}

/// The line the preflight says for the tool `name`, as its probe found it: `gate: remote tool <name> <version>`,
/// `present`, `missing` or `skipped: <why>`, the name and the reason escaped, as either may come from outside.
pub(crate) fn tool_line(name: &str, probed: &Probed) -> String {
    let name = escaped(name);
    match probed {
        Probed::Version(version) => format!("gate: remote tool {name} {version}"),
        Probed::Present => format!("gate: remote tool {name} present"),
        Probed::Missing => format!("gate: remote tool {name} missing"),
        Probed::Skipped(why) => format!("gate: remote tool {name} skipped: {}", escaped(why)),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Probed, first_version, late_reason, panic_reason, probe, tool_line, version_of};
    use crate::environment::Variables;

    #[test]
    fn the_version_is_the_first_dotted_number_after_an_optional_v() {
        for (text, version) in [
            ("rustc 1.99.0 (b940084d7 2026-09-28)", "1.99.0"),
            ("v24.19.0", "24.19.0"),
            ("12.5.1", "12.5.1"),
            ("PowerShell 7.6.6", "7.6.6"),
            ("ffmpeg version 9.0.2-full_build-www.gyan.dev Copyright (c) 2000-2026", "9.0.2"),
            ("cargo-nextest 0.9.100 (abc 2026-01-01)", "0.9.100"),
            ("cargo-nextest 0.9.148 (cd1d6d546 2026-10-07)", "0.9.148"),
            ("10.12", "10.12"),
            ("tool 1.2.3.4", "1.2.3"),
            ("tool 1.2.", "1.2"),
            ("tool 1234567890.1", "1234567890.1"),
        ] {
            assert_eq!(first_version(text).as_deref(), Some(version), "{text:?}");
        }
    }

    #[test]
    fn text_with_no_dotted_number_has_no_version() {
        for text in [
            "ffmpeg version N-112345-gabcdef",
            "",
            "24",
            "x1.2 rev2.3",
            "abc1.2.3",
            "1..2",
            "release 7.",
        ] {
            assert_eq!(first_version(text), None, "{text:?}");
        }
    }

    #[test]
    fn a_number_of_more_than_ten_digits_is_no_readable_version() {
        for text in [
            "tool 12345678901.2",
            "tool 1.12345678901",
            "tool 1.2.3.12345678901",
            "v99999999999.0 then 1.2.3",
        ] {
            assert_eq!(first_version(text), None, "{text:?}");
        }
    }

    #[test]
    fn only_the_first_line_of_the_chosen_stream_is_read() {
        let gyan = "ffmpeg version 9.0.2-full_build-www.gyan.dev Copyright (c) 2000-2026\r\nbuilt with gcc 14.2.0\r\n";
        assert_eq!(version_of(gyan, "").as_deref(), Some("9.0.2"));
        let git_build = "ffmpeg version N-117456-gabc Copyright (c) 2000-2026\r\nbuilt with gcc 14.2.0\r\n";
        assert_eq!(version_of(git_build, ""), None, "the compiler's version is not the tool's");
        assert_eq!(version_of("\r\n  \r\nv24.19.0\r\n", "").as_deref(), Some("24.19.0"), "blank lines first");
        assert_eq!(
            version_of(" \r\n", "PowerShell 7.6.6\r\n").as_deref(),
            Some("7.6.6"),
            "stderr when stdout is blank"
        );
        assert_eq!(
            version_of("no version here\r\n", "7.6.6\r\n"),
            None,
            "stderr is not read when stdout has text"
        );
    }

    #[test]
    fn a_late_probe_is_said_with_the_batch_limit_and_why_it_could_not_be_stopped() {
        let late = "node --version did not answer in 9873 ms";
        let limit = Duration::from_secs(10);
        assert_eq!(late_reason(late, late, limit).as_deref(), Some("no answer in 10 s"));
        assert_eq!(
            late_reason(&format!("{late}, and could not be stopped: access denied"), late, limit).as_deref(),
            Some("no answer in 10 s, and could not be stopped: access denied")
        );
        assert_eq!(
            late_reason(late, late, Duration::from_millis(200)).as_deref(),
            Some("no answer in 200 ms")
        );
        assert_eq!(late_reason("cannot run node: access denied", late, limit), None);
    }

    #[test]
    fn a_panicking_probe_says_its_message() {
        assert_eq!(panic_reason(&"boom"), "the probe failed: boom");
        assert_eq!(panic_reason(&"bang".to_owned()), "the probe failed: bang");
        assert_eq!(panic_reason(&7_u8), "the probe failed");
    }

    #[test]
    fn each_finding_says_its_line() {
        assert_eq!(
            tool_line("node", &Probed::Version("24.19.0".to_owned())),
            "gate: remote tool node 24.19.0"
        );
        assert_eq!(tool_line("ffmpeg", &Probed::Present), "gate: remote tool ffmpeg present");
        assert_eq!(tool_line("pnpm", &Probed::Missing), "gate: remote tool pnpm missing");
        assert_eq!(
            tool_line("node", &Probed::Skipped("no answer in 10 s".to_owned())),
            "gate: remote tool node skipped: no answer in 10 s"
        );
        assert_eq!(
            tool_line("a\nb", &Probed::Skipped("why\r".to_owned())),
            r"gate: remote tool a\nb skipped: why\r",
            "a name and a reason from outside are escaped"
        );
    }

    #[test]
    fn a_name_the_table_does_not_know_or_cannot_probe_runs_nothing() {
        let names = ["nodejs".to_owned(), "godot".to_owned(), "Node".to_owned()];
        assert_eq!(
            probe(&names, Duration::from_secs(1), Ok(Variables::from_process()), None),
            [
                ("nodejs".to_owned(), Probed::Skipped("unknown tool".to_owned())),
                ("godot".to_owned(), Probed::Skipped("no probe for godot".to_owned())),
                ("Node".to_owned(), Probed::Skipped("unknown tool".to_owned())),
            ]
        );
    }

    #[test]
    fn a_batch_with_no_time_left_runs_nothing_and_says_so() {
        let names = ["node".to_owned(), "rust".to_owned()];
        assert_eq!(
            probe(&names, Duration::ZERO, Ok(Variables::from_process()), None),
            [
                ("node".to_owned(), Probed::Skipped("no answer in 0 s".to_owned())),
                ("rust".to_owned(), Probed::Skipped("no answer in 0 s".to_owned())),
            ]
        );
    }
}
