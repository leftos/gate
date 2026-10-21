//! The preflight's tool probes and SDK list: each program found along the login shell's `PATH` and run in the bounded
//! runner from `/`, so no project file of the ssh session's folder (a `rust-toolchain.toml`, a `global.json`) bends
//! what it says.

use std::io;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use crate::lines::{Probed, no_answer, probed, sdk_list};
use crate::process::{NotRan, run_bounded, status_text, with_path};

/// The deadline every probe of one preflight shares.
pub const PROBE_LIMIT: Duration = Duration::from_secs(10);
/// How long `dotnet --list-sdks` may take.
pub const DOTNET_LIMIT: Duration = Duration::from_secs(30);
/// The most a probe or `dotnet --list-sdks` may write on its standard output.
const OUTPUT_CAP: usize = 16 * 1024 * 1024;
/// The folder every probe runs in.
const RUN_FOLDER: &str = "/";

/// How a tool is probed.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// Run the program with the arguments and read its version.
    Runs { program: &'static str, args: &'static [&'static str] },
    /// A tool this builder never has: a run that needs it is not routed here.
    Missing,
    /// A name the box knows no probe for.
    Unknown,
}

/// The probe of the tool `name`.
fn plan(name: &str) -> Plan {
    let runs = |program, args| Plan::Runs { program, args };
    match name {
        "rust" => runs("rustc", &["--version"]),
        "cargo-nextest" => runs("cargo-nextest", &["nextest", "--version"]),
        "node" => runs("node", &["--version"]),
        "pnpm" => runs("pnpm", &["--version"]),
        "pwsh" => runs("pwsh", &["--version"]),
        "ffmpeg" => runs("ffmpeg", &["-version"]),
        "git-lfs" => runs("git-lfs", &["version"]),
        "godot" | "unreal" | "netfx-sdk" => Plan::Missing,
        _ => Plan::Unknown,
    }
}

/// What each tool of `names` is, in their order, probed in parallel along `path` (the box's own `PATH` when `None`)
/// under one shared [`PROBE_LIMIT`].
#[must_use]
pub fn probe_all(names: &[String], path: Option<&str>) -> Vec<Probed> {
    let deadline = Instant::now() + PROBE_LIMIT;
    thread::scope(|scope| {
        let probes: Vec<_> = names.iter().map(|name| scope.spawn(move || probe(name, path, deadline))).collect();
        probes
            .into_iter()
            .map(|probe| probe.join().unwrap_or_else(|_| Probed::Skipped("the probe failed".to_owned())))
            .collect()
    })
}

/// What the tool `name` is, its program run along `path` until `deadline`.
fn probe(name: &str, path: Option<&str>, deadline: Instant) -> Probed {
    let (program, args) = match plan(name) {
        Plan::Runs { program, args } => (program, args),
        Plan::Missing => return Probed::Missing,
        Plan::Unknown => return Probed::Skipped("unknown tool".to_owned()),
    };
    if Instant::now() >= deadline {
        return Probed::Skipped(no_answer(PROBE_LIMIT));
    }
    let mut command = Command::new(program);
    with_path(command.args(args).current_dir(RUN_FOLDER), path);
    match run_bounded(command, OUTPUT_CAP, deadline) {
        Ok(ran) => probed(ran.status.success(), &ran.stdout, &ran.stderr),
        Err(NotRan::Start(error)) if error.kind() == io::ErrorKind::NotFound => Probed::Missing,
        Err(NotRan::Start(error)) => Probed::Skipped(format!("cannot run {program}: {error}")),
        Err(NotRan::TimedOut { .. }) => Probed::Skipped(no_answer(PROBE_LIMIT)),
        Err(NotRan::OverCap) => Probed::Skipped(format!("{program} {} wrote more than 16 MiB", args.join(" "))),
        Err(NotRan::Failed(why)) => Probed::Skipped(format!("cannot run {program}: {why}")),
    }
}

/// The SDK list of `dotnet --list-sdks` run along `path` within [`DOTNET_LIMIT`] ([`sdk_list`]); `none` when no
/// dotnet is found. Else why it could not be read, for [`crate::lines::sdks_failure`].
///
/// # Errors
///
/// Why the list could not be read: dotnet ran past the limit, wrote more than 16 MiB, failed, or could not start.
pub fn sdks(path: Option<&str>) -> Result<String, String> {
    let mut command = Command::new("dotnet");
    with_path(command.arg("--list-sdks").current_dir(RUN_FOLDER), path);
    match run_bounded(command, OUTPUT_CAP, Instant::now() + DOTNET_LIMIT) {
        Ok(ran) if ran.status.success() => Ok(sdk_list(&String::from_utf8_lossy(&ran.stdout))),
        Ok(ran) => Err(format!("dotnet --list-sdks exited with {}", status_text(ran.status))),
        Err(NotRan::Start(error)) if error.kind() == io::ErrorKind::NotFound => Ok("none".to_owned()),
        Err(NotRan::Start(error)) => Err(format!("cannot run dotnet: {error}")),
        Err(NotRan::TimedOut { .. }) => Err(format!("dotnet --list-sdks did not answer in {} s", DOTNET_LIMIT.as_secs())),
        Err(NotRan::OverCap) => Err("dotnet --list-sdks wrote more than 16 MiB".to_owned()),
        Err(NotRan::Failed(why)) => Err(format!("cannot run dotnet: {why}")),
    }
}

#[cfg(test)]
mod tests {
    use super::{Plan, plan, probe_all};
    use crate::lines::Probed;

    #[test]
    fn each_tool_has_its_probe() {
        assert_eq!(
            plan("rust"),
            Plan::Runs {
                program: "rustc",
                args: &["--version"]
            }
        );
        assert_eq!(
            plan("cargo-nextest"),
            Plan::Runs {
                program: "cargo-nextest",
                args: &["nextest", "--version"]
            }
        );
        assert_eq!(
            plan("ffmpeg"),
            Plan::Runs {
                program: "ffmpeg",
                args: &["-version"]
            }
        );
        assert_eq!(
            plan("git-lfs"),
            Plan::Runs {
                program: "git-lfs",
                args: &["version"]
            }
        );
        for name in ["godot", "unreal", "netfx-sdk"] {
            assert_eq!(plan(name), Plan::Missing, "{name}");
        }
        assert_eq!(plan("Rust"), Plan::Unknown, "names are exact");
    }

    #[test]
    fn the_probes_answer_in_the_order_asked() {
        let names = ["godot", "bogus", "netfx-sdk"].map(str::to_owned);
        assert_eq!(
            probe_all(&names, None),
            [Probed::Missing, Probed::Skipped("unknown tool".to_owned()), Probed::Missing]
        );
    }
}
