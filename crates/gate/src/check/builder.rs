//! The check of a builder `remote.json` lists: the repo's `.gate/toolchain.toml` and root `global.json` read from disk
//! as this PC's check reads them, then one bounded call of the builder's `gate box preflight`, as a routed run's
//! preflight is made, with the repo's own work-folder key and the manifest's tools as its `Needs`. The preflight
//! readies only the work folder the next routed run of the repo would ready, and its answer is read with the remote
//! client's own readers: the builder's gate build, the folders it created, its SDK list and what it found of each tool.

use std::path::Path;
use std::time::Duration;

use gate_args::{BoxVerb, CheckRequest, CheckSeams};
use gate_router::{Place, repo_name, work_folder_key};
use gate_win32::PipedConsole;

use crate::answer::{FAILED_STATUS, USAGE_STATUS};
use crate::boxside::payload::escaped;
use crate::check::local::{self, Declared};
use crate::check::report::{self, At, Checked, Tool};
use crate::console::Console;
use crate::environment::Variables;
use crate::remote::git::{self, GitError, ProgramCall, ProgramOutput, ProgramPlace};
use crate::remote::payload::{self, Location};
use crate::remote::toolchain::{self, Reported};
use crate::remote::transport::ssh::Ssh;
use crate::remote::transport::{Program, Transport, Wire};
use crate::remote::{BOX_USAGE, CREATED, NOT_RECOGNIZED, SSH_FAILED, other_build, sdks_of, their_version};
use crate::route;
use crate::version::build_version;

/// How long the builder's call may take before it is ended: the box's tool probes and SDK list take up to 30 s, and
/// ssh's connection up to 10 s more.
const BOUND: Duration = Duration::from_secs(120);
/// What a failure to end a call that outlived its bound says after the late line, before why.
const NOT_STOPPED: &str = "could not be stopped: ";

/// Checks the builder `host` for the repo the working folder is in and says what it found through `console`; returns
/// the exit status: 0 when nothing is missing, 1 when something is or the check could not run, 2 for a host
/// `remote.json` does not list.
pub(crate) fn check(request: &CheckRequest, host: &str, console: &mut Console) -> u32 {
    let host = match listed(host) {
        Ok(host) => host,
        Err((status, line)) => {
            console.err(&line);
            return u32::from(status);
        }
    };
    let place = match local::placed() {
        Ok(Some(place)) => place,
        Ok(None) => return failed(console, "check: not in a git repository"),
        Err(why) => return failed(console, &format!("check: {why}")),
    };
    let mut transport = match transport(&request.seams, &host) {
        Ok(transport) => transport,
        Err(line) => return failed(console, &line),
    };
    // The worktree is known only now: a transport whose program lies inside it starts nothing.
    if let Err(why) = transport.judge(Path::new(&place.root)) {
        return failed(console, &format!("check: {host}: {why}"));
    }
    let bound = request.seams.remote_timeout_ms.map_or(BOUND, Duration::from_millis);
    match asked(&*transport, &host, &place, bound) {
        Ok(asked) => {
            let at = At::Builder {
                host: &host,
                key: &asked.key,
            };
            super::say(&report::said(at, &asked.checked), console)
        }
        Err(line) => failed(console, &line),
    }
}

/// Says `line` on stderr; the status of a check that could not run.
fn failed(console: &mut Console, line: &str) -> u32 {
    console.err(line);
    u32::from(FAILED_STATUS)
}

/// The builder `remote.json` lists as `host` or in `hosts` ([`gate_router::Config::builders`]) that is `host` in any
/// ASCII letter case, as the file spells it. Else the status and the line: 2 for a host it does not list, 1 for a
/// routing file that is not there or cannot be used.
fn listed(host: &str) -> Result<String, (u8, String)> {
    let cannot = |why: String| (FAILED_STATUS, format!("check: {}: remote.json {why}", escaped(host)));
    let path = route::remote_json_path(std::env::var_os("LOCALAPPDATA").as_deref())
        .ok_or_else(|| cannot("cannot be found: LOCALAPPDATA is not an absolute folder".to_owned()))?;
    let config = match route::read_remote_json(&path) {
        Ok(Some(config)) => config,
        Ok(None) => return Err(cannot(format!("not found at {}", path.display()))),
        Err(problem) => return Err(cannot(format!("is ignored: {}", problem.reason))),
    };
    let builders = config.builders().map_err(|why| cannot(format!("is ignored: {why}")))?;
    builders
        .iter()
        .find(|builder| builder.eq_ignore_ascii_case(host))
        .cloned()
        .ok_or_else(|| (USAGE_STATUS, format!("gate check: {} is not a builder remote.json lists", escaped(host))))
}

/// The way to the builder `host`: the local transport `--test-remote-transport` names, else ssh found along the gate's
/// own `PATH`. Else the line saying why the local transport cannot be used, or why ssh cannot be found.
#[cfg(feature = "test-seams")]
fn transport(seams: &CheckSeams, host: &str) -> Result<Box<dyn Transport>, String> {
    match &seams.remote_transport {
        Some(named) => crate::remote::local_transport(named, seams.remote_ssh.as_deref()).map(|local| Box::new(local) as Box<dyn Transport>),
        None => ssh(host),
    }
}

/// The way to the builder `host`: ssh found along the gate's own `PATH`; else the line saying why it cannot be found.
#[cfg(not(feature = "test-seams"))]
fn transport(_seams: &CheckSeams, host: &str) -> Result<Box<dyn Transport>, String> {
    ssh(host)
}

/// ssh found along the gate's own `PATH`; else the line of the builder `host` saying why it cannot be found.
fn ssh(host: &str) -> Result<Box<dyn Transport>, String> {
    Ssh::find()
        .map(|ssh| Box::new(ssh) as Box<dyn Transport>)
        .map_err(|why| format!("check: {host}: {why}"))
}

/// A builder that answered: what it has, and the work-folder key it was asked about.
struct Asked {
    checked: Checked,
    key: String,
}

/// What the builder `host`, reached over `transport` within `bound`, has of what the repo at `place` declares; else
/// the line saying why it could not be checked.
fn asked(transport: &dyn Transport, host: &str, place: &Place, bound: Duration) -> Result<Asked, String> {
    let root = Path::new(&place.root);
    let declared = local::declared(root);
    let needs = toolchain::needs_of(&declared.manifest);
    let repo = repo_name(&place.checkout);
    let key = work_folder_key(&place.root);
    let location = Location::under(transport.root(), &repo, &key, &payload::job_nonce());
    let input = payload::input(&payload::preflight_word(&location, &needs), None);
    let sent: Vec<String> = needs.into_iter().map(|(name, _)| name).collect();
    let program = transport.call(host, &payload::line(BoxVerb::Preflight), Wire::Plain);
    let heard = called(&program, bound, input.as_bytes()).map_err(|failure| failure.line(host))?;
    if let Some(line) = refusal(host, transport.root(), &heard) {
        return Err(line);
    }
    let answer = Answer {
        host,
        sent: &sent,
        lines: &heard.lines,
        last_error: heard.last_error.as_deref(),
    };
    Ok(Asked {
        checked: answer.checked(declared, repo, place.root.clone(), local::declares_siblings(root)),
        key,
    })
}

/// What the builder's call said: its exit status, its standard output's lines, and the last line of its standard error
/// that is not blank.
struct Heard {
    status: Option<i32>,
    lines: Vec<String>,
    last_error: Option<String>,
}

/// Why the builder's call gave no answer: an unreachable builder, or any other failure of the call.
#[derive(Debug, PartialEq, Eq)]
enum Failure {
    Unreachable(String),
    Other(String),
}

impl Failure {
    /// The line of the builder `host` saying it.
    fn line(&self, host: &str) -> String {
        match self {
            Failure::Unreachable(why) => format!("check: {host}: unreachable: {why}"),
            Failure::Other(why) => format!("check: {host}: {why}"),
        }
    }
}

/// Makes the one call `program`, fed `input` on its standard input, bounded at `bound` and never made again, as the
/// client's programs other than git run ([`git::program_output`]); else why it could not be made or did not end in
/// time.
fn called(program: &Program, bound: Duration, input: &[u8]) -> Result<Heard, Failure> {
    let shown = program.program.to_string_lossy();
    let words: Vec<String> = program.args.iter().map(|word| word.to_string_lossy().into_owned()).collect();
    let args: Vec<&str> = words.iter().map(String::as_str).collect();
    let call = ProgramCall {
        program: &shown,
        args: &args,
        limit: bound,
        interrupt: None,
        worktree: None,
        console: PipedConsole::Attached,
        stdin: Some(input),
    };
    let place = ProgramPlace::new(Variables::from_process()).map_err(Failure::Other)?;
    match git::program_output(&call, &place) {
        Ok(Some(output)) => Ok(heard(&output)),
        Ok(None) => Err(Failure::Other(format!("cannot run {shown}: it is not found"))),
        Err(error) => Err(failure(error, &call.late_line(), &shown, bound)),
    }
}

/// Why a call ended in `error` gave no answer, its program shown as `shown`: an unreachable builder for the line it is
/// ended with past its `bound`, `late`, alone; any other failure as it is, the whole call's words, which `late` starts
/// with, shown as the program alone, so no line carries ssh's options. A late call whose job could not be
/// ended says why, read from what follows `late` alone, in either form git's calls say it: `, and could not be
/// stopped: <why>` or `, and <program> could not be stopped: <why>`.
fn failure(error: GitError, late: &str, shown: &str, bound: Duration) -> Failure {
    let limit = git::spelled_limit(bound);
    let named = late.strip_suffix(&format!(" did not answer in {limit}")).unwrap_or(late);
    match error {
        GitError::Failed(why) if why == late => Failure::Unreachable(format!("no answer in {limit}")),
        GitError::Failed(why) => match not_stopped(&why, late, shown) {
            Some(kill) => Failure::Other(format!("no answer in {limit}, and ssh {NOT_STOPPED}{kill}")),
            None => Failure::Other(why.replace(named, shown)),
        },
        GitError::OverCap(why) => Failure::Other(why.replace(named, shown)),
        GitError::Interrupted => Failure::Other("the call was interrupted".to_owned()),
    }
}

/// Why the job of a call that outlived its bound could not be ended, from `why`, its end: what follows the late line
/// `late` and `, and `, then `could not be stopped: ` or the program `shown` and it; `None` for any other end.
fn not_stopped<'a>(why: &'a str, late: &str, shown: &str) -> Option<&'a str> {
    let rest = why.strip_prefix(late)?.strip_prefix(", and ")?;
    rest.strip_prefix(NOT_STOPPED)
        .or_else(|| rest.strip_prefix(shown)?.strip_prefix(' ')?.strip_prefix(NOT_STOPPED))
}

/// What `output` says, read as text.
fn heard(output: &ProgramOutput) -> Heard {
    let lines = String::from_utf8_lossy(&output.stdout).lines().map(str::to_owned).collect();
    let last_error = String::from_utf8_lossy(&output.stderr)
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(str::to_owned);
    Heard {
        status: output.status.code(),
        lines,
        last_error,
    }
}

/// The one line of a builder `host`, whose root is `root`, that cannot be checked, as its call `heard` ended: ssh's
/// failure, a builder with no `gate box` that takes the call, a preflight that failed, or a builder that runs another
/// gate build than this PC's; `None` for one that answered.
fn refusal(host: &str, root: &str, heard: &Heard) -> Option<String> {
    let last = heard.last_error.as_deref().map(|line| git::failure_text(&[line]));
    match heard.status {
        Some(0) => {}
        Some(SSH_FAILED) => return Some(format!("check: {host}: unreachable: {}", last.unwrap_or_default())),
        Some(status @ (NOT_RECOGNIZED | BOX_USAGE)) => {
            return Some(format!(
                "check: {host}: has no gate box that takes this call (status {status}); run gate push"
            ));
        }
        status => {
            let status = status.map_or_else(|| "unknown".to_owned(), |status| status.to_string());
            let why = last.map(|last| format!(": {last}")).unwrap_or_default();
            return Some(format!("check: {host}: preparing {root} failed (status {status}){why}"));
        }
    }
    let ours = build_version();
    other_build(their_version(&heard.lines), &ours).map(|theirs| format!("check: {host}: runs gate {theirs}, this PC runs {ours}; run gate push"))
}

/// The preflight's answer: the builder `host`, the tools `sent` in its `Needs`, the lines its standard output said, and
/// the last line its standard error said.
struct Answer<'a> {
    host: &'a str,
    sent: &'a [String],
    lines: &'a [String],
    last_error: Option<&'a str>,
}

impl Answer<'_> {
    /// What the builder has of what the repo named `repo`, whose worktree root is `root`, `declared`, and whether it
    /// `declares_siblings`, which a builder is not checked for.
    fn checked(&self, declared: Declared, repo: String, root: String, declares_siblings: bool) -> Checked {
        let reported = toolchain::reported(self.lines, self.sent);
        let tools = declared
            .manifest
            .tools
            .iter()
            .map(|need| Tool {
                need: need.clone(),
                reported: Self::found(&reported, &need.name),
            })
            .collect();
        let sdk = local::sdk_listed(declared.pin.as_ref(), || match sdks_of(self.lines) {
            Some(sdks) => Ok(sdks.iter().map(|sdk| escaped(&shown(sdk))).collect()),
            None => Err(self.no_sdk_list()),
        });
        let created = self
            .lines
            .iter()
            .filter_map(|line| line.strip_prefix(CREATED))
            .map(|folder| shown(folder.trim()))
            .collect();
        Checked {
            repo,
            root,
            declares_toolchain: declared.declares_toolchain,
            unreadable: declared.unreadable(false),
            problems: declared.problems,
            tools,
            sdk,
            workloads: declared.manifest.workloads,
            consent_problem: None,
            siblings: Vec::new(),
            created,
            siblings_skipped: declares_siblings,
        }
    }

    /// Why the SDK pin is skipped when the builder sent no SDK list: that, and the last line its call said on standard
    /// error when it said one, cut as a line shows it.
    fn no_sdk_list(&self) -> String {
        let why = self.last_error.map(|line| format!(": {}", shown(line))).unwrap_or_default();
        format!("{} sent no SDK list{why}", self.host)
    }

    /// What the builder said of the tool `name`, as `reported` reads its lines, its texts cut as a line shows them.
    fn found(reported: &[(String, Reported)], name: &str) -> Reported {
        match reported.iter().find(|(sent, _)| sent == name).map(|(_, answer)| answer) {
            Some(Reported::Version(found)) => Reported::Version(shown(found)),
            Some(Reported::Skipped(why)) => Reported::Skipped(shown(why)),
            Some(answer) => answer.clone(),
            None => Reported::Skipped(toolchain::NO_ANSWER.to_owned()),
        }
    }
}

/// A text the builder said, cut to its first [`git::FAILURE_BYTES`] so a builder cannot flood the console. The report
/// escapes every such text it says, but the SDK names, which its caller escapes.
fn shown(text: &str) -> String {
    git::first_bytes(text, git::FAILURE_BYTES).to_owned()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Answer, Failure, Heard, failure, refusal};
    use crate::check::local::Declared;
    use crate::check::report::{At, Checked, Line, Sdk, said};
    use crate::remote::git::GitError;
    use crate::remote::parity::pin_of;
    use crate::remote::toolchain::{self, Manifest, Reported, ToolNeed, Version};
    use crate::version::build_version;

    const HOST: &str = "builder-1";
    const AT: At<'static> = At::Builder {
        host: HOST,
        key: "app-1a2b3c4d",
    };

    fn need(name: &str, min: &str) -> ToolNeed {
        ToolNeed {
            name: name.to_owned(),
            min: Version::parse(min),
            known: true,
        }
    }

    /// What a repo declaring the tools `tools` and pinning `pin`, when given, reads as.
    fn declared(tools: Vec<ToolNeed>, pin: Option<&str>) -> Declared {
        Declared {
            declares_toolchain: true,
            manifest: Manifest {
                tools,
                ..Manifest::default()
            },
            pin: pin.map(|version| pin_of(Some(&format!(r#"{{"sdk": {{"version": "{version}"}}}}"#)))),
            problems: Vec::new(),
            toolchain_unreadable: false,
            pin_unreadable: false,
        }
    }

    fn lines(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| (*text).to_owned()).collect()
    }

    /// What the builder's `answer` of the tools `sent` is read as against `declared`.
    fn checked(declared: Declared, sent: &[&str], answer: &[String], last_error: Option<&str>) -> Checked {
        let sent: Vec<String> = sent.iter().map(|name| (*name).to_owned()).collect();
        Answer {
            host: HOST,
            sent: &sent,
            lines: answer,
            last_error,
        }
        .checked(declared, "app".to_owned(), "X:/dev/app".to_owned(), false)
    }

    fn reported(checked: &Checked) -> Vec<(&str, &Reported)> {
        checked.tools.iter().map(|tool| (tool.need.name.as_str(), &tool.reported)).collect()
    }

    #[test]
    fn a_tool_is_what_the_builder_reported_and_one_it_did_not_answer_is_skipped() {
        let answer = lines(&[
            "gate: remote version x",
            "gate: remote tool node 22.1.0",
            "gate: remote tool ffmpeg missing",
        ]);
        let checked = checked(
            declared(vec![need("node", "24"), need("pnpm", "10"), need("ffmpeg", "")], None),
            &["node", "pnpm", "ffmpeg"],
            &answer,
            None,
        );
        assert_eq!(
            reported(&checked),
            [
                ("node", &Reported::Version("22.1.0".to_owned())),
                ("pnpm", &Reported::Skipped(toolchain::NO_ANSWER.to_owned())),
                ("ffmpeg", &Reported::Missing),
            ]
        );
    }

    #[test]
    fn a_pin_with_no_sdk_line_is_skipped_saying_the_builder_sent_none() {
        let answer = lines(&["gate: remote version x"]);
        let quiet = checked(declared(Vec::new(), Some("10.0.401")), &[], &answer, None);
        assert!(
            matches!(&quiet.sdk, Some(Sdk::Skipped(why)) if why == "builder-1 sent no SDK list"),
            "{:?}",
            quiet.sdk
        );
        let said_why = checked(
            declared(Vec::new(), Some("10.0.401")),
            &[],
            &answer,
            Some("gate: remote could not read the SDK list: dotnet hung"),
        );
        assert!(
            matches!(&said_why.sdk, Some(Sdk::Skipped(why))
                if why == "builder-1 sent no SDK list: gate: remote could not read the SDK list: dotnet hung"),
            "{:?}",
            said_why.sdk
        );
    }

    #[test]
    fn a_builder_s_sdk_names_cannot_forge_a_line() {
        let answer = lines(&["gate: remote sdks 8.0.404\u{1b}[2K\rcheck: builder-1: nothing missing,10.0.300\u{202e}"]);
        let checked = checked(declared(Vec::new(), Some("10.0.401")), &[], &answer, None);
        let texts: Vec<String> = said(AT, &checked)
            .lines
            .into_iter()
            .map(|line| match line {
                Line::Out(text) | Line::Err(text) => text,
            })
            .collect();
        assert!(
            texts.iter().all(|text| !text.contains(['\u{1b}', '\r', '\u{202e}'])),
            "no raw mark is said: {texts:?}"
        );
        assert!(
            texts.contains(
                &r"check: builder-1: .NET SDK 10.0.401 (rollForward patch) missing (builder-1 has 8.0.404\u{1b}[2K\rcheck: builder-1: nothing missing, 10.0.300\u{202e})"
                    .to_owned()
            ),
            "{texts:?}"
        );
    }

    fn heard(status: Option<i32>, lines: Vec<String>, last_error: Option<&str>) -> Heard {
        Heard {
            status,
            lines,
            last_error: last_error.map(str::to_owned),
        }
    }

    #[test]
    fn each_status_of_the_call_is_said_as_it_ended() {
        let root = r"D:\gate-remote";
        let ours = lines(&[&format!("gate: remote version {}", build_version())]);
        assert_eq!(refusal(HOST, root, &heard(Some(0), ours, None)), None, "the same build answered");
        assert_eq!(
            refusal(HOST, root, &heard(Some(0), lines(&["gate: remote version 0.0.0-old"]), None)),
            Some(format!(
                "check: builder-1: runs gate 0.0.0-old, this PC runs {}; run gate push",
                build_version()
            ))
        );
        assert_eq!(
            refusal(
                HOST,
                root,
                &heard(Some(255), Vec::new(), Some("ssh: connect to host builder-1 port 22: Connection refused"))
            ),
            Some("check: builder-1: unreachable: ssh: connect to host builder-1 port 22: Connection refused".to_owned())
        );
        for status in [9009, 2] {
            assert_eq!(
                refusal(HOST, root, &heard(Some(status), Vec::new(), None)),
                Some(format!(
                    "check: builder-1: has no gate box that takes this call (status {status}); run gate push"
                ))
            );
        }
        assert_eq!(
            refusal(
                HOST,
                root,
                &heard(
                    Some(1),
                    Vec::new(),
                    Some("gate: remote could not create the folder D:\\gate-remote\\work")
                )
            ),
            Some(
                r"check: builder-1: preparing D:\gate-remote failed (status 1): gate: remote could not create the folder D:\gate-remote\work"
                    .to_owned()
            )
        );
        assert_eq!(
            refusal(HOST, root, &heard(Some(3), Vec::new(), None)),
            Some(r"check: builder-1: preparing D:\gate-remote failed (status 3)".to_owned())
        );
    }

    #[test]
    fn only_a_call_ended_at_its_bound_is_unreachable() {
        let bound = Duration::from_millis(500);
        let named = "C:\\ssh.exe -o BatchMode=yes -- builder-1 \"gate.exe\" box preflight eyJLZXkiOiJhcHAifQ==";
        let late = format!("{named} did not answer in 500 ms");
        let shown = "C:\\ssh.exe";
        assert_eq!(
            failure(GitError::Failed(late.clone()), &late, shown, bound),
            Failure::Unreachable("no answer in 500 ms".to_owned())
        );
        assert_eq!(
            failure(
                GitError::Failed(format!("{late}, and could not be stopped: access denied")),
                &late,
                shown,
                bound
            ),
            Failure::Other("no answer in 500 ms, and ssh could not be stopped: access denied".to_owned())
        );
        assert_eq!(
            failure(
                GitError::Failed(format!("{late}, and {shown} could not be stopped: access denied")),
                &late,
                shown,
                bound
            ),
            Failure::Other("no answer in 500 ms, and ssh could not be stopped: access denied".to_owned())
        );
        let odd_named = "C:\\could not be stopped: x\\ssh.exe -- builder-1 \"gate.exe\" box preflight eyJLZXkiOiJhcHAifQ==";
        let odd_late = format!("{odd_named} did not answer in 500 ms");
        let odd_shown = "C:\\could not be stopped: x\\ssh.exe";
        assert_eq!(
            failure(
                GitError::Failed(format!("{odd_late}, and could not be stopped: access denied")),
                &odd_late,
                odd_shown,
                bound
            ),
            Failure::Other("no answer in 500 ms, and ssh could not be stopped: access denied".to_owned()),
            "only what follows the late line is read for why the kill failed"
        );
        assert_eq!(
            failure(GitError::Failed(format!("{odd_late}; then something else")), &odd_late, odd_shown, bound),
            Failure::Other(format!("{odd_shown} did not answer in 500 ms; then something else")),
            "a late line with no kill failure after it never shows the payload"
        );
        assert_eq!(
            failure(GitError::OverCap(format!("{named} wrote more than 16 MiB")), &late, shown, bound),
            Failure::Other("C:\\ssh.exe wrote more than 16 MiB".to_owned()),
            "the line's payload is never said"
        );
        assert_eq!(
            failure(GitError::Failed("cannot run C:\\ssh.exe: access denied".to_owned()), &late, shown, bound),
            Failure::Other("cannot run C:\\ssh.exe: access denied".to_owned())
        );
        assert_eq!(
            Failure::Other("cannot run C:\\ssh.exe: access denied".to_owned()).line(HOST),
            "check: builder-1: cannot run C:\\ssh.exe: access denied"
        );
        assert_eq!(
            Failure::Unreachable("no answer in 500 ms".to_owned()).line(HOST),
            "check: builder-1: unreachable: no answer in 500 ms"
        );
    }
}
