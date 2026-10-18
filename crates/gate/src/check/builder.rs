//! The check of a builder `remote.json` lists: the repo's `.gate/toolchain.toml`, `.gate/siblings.toml` and root
//! `global.json` read from disk as this PC's check reads them, then one bounded call of the builder's `gate box
//! preflight`, as a routed run's preflight is made, with the repo's own work-folder key and the manifest's tools as its
//! `Needs`. The preflight readies only the work folder the next routed run of the repo would ready, and its answer is
//! read with the remote client's own readers: the builder's gate build, the folders it created, its SDK list, what it
//! found of each tool, and whether its work folder is warm for the worktree's warm-up file. The warm-up's items are
//! this PC's, its record said only when it names the builder checked.

use std::path::Path;
use std::time::Duration;

use gate_args::{BoxVerb, CheckRequest, CheckSeams};
use gate_router::{Place, repo_name, work_folder_key};
use gate_win32::PipedConsole;

use crate::answer::{FAILED_STATUS, USAGE_STATUS};
use crate::boxside::payload::escaped;
use crate::check::local::{self, Declared, SiblingsFile, WarmSources, Warmed};
use crate::check::report::{self, At, Checked, Problem, Tool, Warmup};
use crate::console::Console;
use crate::environment::Variables;
use crate::remote::git::{self, GitError, ProgramCall, ProgramOutput, ProgramPlace};
use crate::remote::payload::{self, Location};
use crate::remote::toolchain::{self, Reported};
use crate::remote::transport::ssh::Ssh;
use crate::remote::transport::{Program, Transport, Wire};
use crate::remote::{BOX_USAGE, CREATED, NOT_RECOGNIZED, SSH_FAILED, Warm, install_why, other_build, sdks_of, their_version, warm_of};
use crate::route;
use gate_desktop::version::build_version;

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
    let mut declared = local::declared(root);
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
    let siblings = local::read_siblings_file(root, &mut declared.problems);
    let sources = WarmSources::from_process(&repo);
    let warmed = warm_items(&place.root, sources, host, &heard.lines, &mut declared.problems);
    let mut checked = answer.checked(declared, repo, place.root.clone(), &siblings, warmed.unreadable);
    checked.warmup = warmed.items;
    Ok(Asked { checked, key })
}

/// The warm-up of a builder check of `host` for the worktree root `root`, from `sources`: this PC's
/// ([`local::warmup`]), the record said only when it names `host` or cannot be read, then, when the repo consents and
/// its warm-up file parsed, what the preflight's `lines` say of the builder's work folder against that file's hash.
/// Each problem of the file is added to `problems`.
fn warm_items(root: &str, sources: WarmSources, host: &str, lines: &[String], problems: &mut Vec<Problem>) -> Warmed {
    let mut warmed = local::warmup(root, sources, Some(host), problems);
    let builder = warmed.hash.as_deref().map(|hash| builder_warmth(lines, hash));
    warmed.items.extend(builder);
    warmed
}

/// What the builder's preflight `lines` say of its work folder's warm-up, as the client reads them ([`warm_of`]): warm
/// only when its `.warm` holds `hash`, the hash of the worktree's warm-up file.
fn builder_warmth(lines: &[String], hash: &str) -> Warmup {
    Warmup::Builder {
        warm: matches!(warm_of(lines), Warm::Hash(said) if said == hash),
    }
}

/// What the builder's call said: its exit status, its standard output's lines, its standard error's lines, and the last
/// of those that is not blank.
struct Heard {
    status: Option<i32>,
    lines: Vec<String>,
    errors: Vec<String>,
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
        worktrees: &[],
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
    let errors: Vec<String> = String::from_utf8_lossy(&output.stderr).lines().map(str::to_owned).collect();
    let last_error = errors.iter().rev().find(|line| !line.trim().is_empty()).cloned();
    Heard {
        status: output.status.code(),
        lines,
        errors,
        last_error,
    }
}

/// The one line of a builder `host`, whose root is `root`, that cannot be checked, as its call `heard` ended: ssh's
/// failure, a builder with no `gate box` that takes the call, a preflight that failed, or a builder that runs another
/// gate build than this PC's; `None` for one that answered.
fn refusal(host: &str, root: &str, heard: &Heard) -> Option<String> {
    let last = heard.last_error.as_deref().map(|line| git::failure_text(&[line]));
    let said = heard.lines.iter().chain(&heard.errors).map(String::as_str);
    if let Some(why) = heard.status.and_then(|status| install_why(status, said)) {
        return Some(format!(
            "check: {host}: its gate install is not trusted ({}); reinstall with gate push",
            escaped(why)
        ));
    }
    match heard.status {
        Some(0) => {}
        Some(SSH_FAILED) => {
            let why = last
                .filter(|shown| !shown.trim().is_empty())
                .unwrap_or_else(|| format!("ssh exited {SSH_FAILED} with no message"));
            return Some(format!("check: {host}: unreachable: {why}"));
        }
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
    /// What the builder has of what the repo named `repo`, whose worktree root is `root`, `declared`, what its
    /// `siblings` file declares (the sibling folders are not checked on a builder), and whether its warm-up file is
    /// there but `warmup_unreadable` whole.
    fn checked(&self, declared: Declared, repo: String, root: String, siblings: &SiblingsFile, warmup_unreadable: bool) -> Checked {
        let reported = toolchain::reported(self.lines, self.sent);
        let tools = declared
            .manifest
            .tools
            .iter()
            .map(|need| Tool {
                need: need.clone(),
                reported: Self::found(&reported, &need.name),
                not_checked: None,
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
            unreadable: declared.unreadable(siblings.unreadable, warmup_unreadable),
            problems: declared.problems,
            tools,
            godot: declared.godot,
            unreal: declared.unreal,
            sdk,
            workloads: declared.manifest.workloads,
            consent_problem: None,
            hooks_problem: None,
            siblings: Vec::new(),
            created,
            siblings_skipped: siblings.declares,
            warmup: Vec::new(),
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

    use std::fs;
    use std::path::{Path, PathBuf};

    use gate_router::warm::warmup_hash;
    use gate_router::work_folder_key;
    use gate_state::v1::{self, WarmRecord, WarmState};
    use jiff::Timestamp;

    use super::{Answer, Failure, Heard, builder_warmth, failure, refusal, warm_items};
    use crate::check::local::{Consent, Declared, SiblingsFile, WarmSources};
    use crate::check::report::{At, Checked, Line, Sdk, Warmup, said};
    use crate::remote::git::GitError;
    use crate::remote::install_untrusted_line;
    use crate::remote::parity::pin_of;
    use crate::remote::toolchain::{self, Manifest, Pin, Reported, ToolNeed, Version};
    use gate_desktop::version::build_version;

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
            godot: None,
            unreal: None,
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
        .checked(declared, "app".to_owned(), "X:/dev/app".to_owned(), &SiblingsFile::default(), false)
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

    /// A builder's godot is judged against the manifest's exact pin, as a routed run judges it: another version is
    /// missing, and a builder that gives no answer is missing for its text.
    #[test]
    fn a_pinned_godot_on_a_builder_is_missing_when_it_does_not_serve_the_pin() {
        let Some(pin) = Pin::parse("4.7.2") else {
            unreachable!("4.7.2 is a pin");
        };
        let mut serving = declared(vec![need("godot", "4.7.2")], None);
        serving.godot = Some(vec![pin]);
        let answer = lines(&["gate: remote tool godot 4.7.3"]);
        let other = checked(serving, &["godot"], &answer, None);
        assert_eq!(
            said(AT, &other).lines,
            [
                Line::Out("check: builder-1: app (work folder app-1a2b3c4d)".to_owned()),
                Line::Out("check: builder-1: godot missing (needs 4.7.2 exactly: 4.7.3)".to_owned()),
                Line::Out("check: builder-1: 1 missing, 0 too old".to_owned()),
            ]
        );
        assert_eq!(said(AT, &other).status, 1);

        let mut silent = declared(vec![need("godot", "4.7.2")], None);
        silent.godot = Some(vec![pin]);
        let quiet = checked(silent, &["godot"], &lines(&[]), None);
        assert_eq!(
            said(AT, &quiet).lines,
            [
                Line::Out("check: builder-1: app (work folder app-1a2b3c4d)".to_owned()),
                Line::Out("check: builder-1: godot missing (needs 4.7.2 exactly: not probed (the builder gave no answer))".to_owned()),
                Line::Out("check: builder-1: 1 missing, 0 too old".to_owned()),
            ]
        );
        assert_eq!(said(AT, &quiet).status, 1);
    }

    /// A builder's unreal is judged against the manifest's exact pin, as a routed run judges it: a later engine is
    /// missing, and a builder whose probe was skipped is missing saying why.
    #[test]
    fn a_pinned_unreal_on_a_builder_is_missing_when_it_does_not_serve_the_pin() {
        let Some(pin) = Pin::parse("5.8") else {
            unreachable!("5.8 is a pin");
        };
        let mut later = declared(vec![need("unreal", "5.8")], None);
        later.unreal = Some(vec![pin]);
        let answer = lines(&["gate: remote tool unreal 5.9.0"]);
        let other = checked(later, &["unreal"], &answer, None);
        assert_eq!(
            said(AT, &other).lines,
            [
                Line::Out("check: builder-1: app (work folder app-1a2b3c4d)".to_owned()),
                Line::Out("check: builder-1: unreal missing (needs 5.8 exactly: 5.9.0)".to_owned()),
                Line::Out("check: builder-1: 1 missing, 0 too old".to_owned()),
            ]
        );
        assert_eq!(said(AT, &other).status, 1);

        let mut skipped = declared(vec![need("unreal", "5.8")], None);
        skipped.unreal = Some(vec![pin]);
        let answer = lines(&["gate: remote tool unreal skipped: GATE_UNREAL_ROOT u is not an absolute path on a drive"]);
        let refused = checked(skipped, &["unreal"], &answer, None);
        assert_eq!(
            said(AT, &refused).lines,
            [
                Line::Out("check: builder-1: app (work folder app-1a2b3c4d)".to_owned()),
                Line::Out(
                    "check: builder-1: unreal missing (needs 5.8 exactly: not probed (GATE_UNREAL_ROOT u is not an absolute path on a drive))"
                        .to_owned()
                ),
                Line::Out("check: builder-1: 1 missing, 0 too old".to_owned()),
            ]
        );
        assert_eq!(said(AT, &refused).status, 1);
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

    /// The builder's `.warm` is warm only when its preflight's warm line holds the warm-up file's hash: another hash and
    /// `none` are not warm, and neither counts against the check.
    #[test]
    fn the_builder_s_work_folder_is_warm_only_for_the_warm_up_file_s_hash() {
        const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
        let version = format!("gate: remote version {}", build_version());
        for (name, warm_line, line) in [
            (
                "warm",
                format!("gate: remote warm {HASH}"),
                "check: builder-1: warm-up: the work folder is warm",
            ),
            (
                "another hash",
                format!("gate: remote warm {OTHER}"),
                "check: builder-1: warm-up: the work folder is not warm",
            ),
            (
                "none",
                "gate: remote warm none".to_owned(),
                "check: builder-1: warm-up: the work folder is not warm",
            ),
        ] {
            let answer = lines(&[version.as_str(), warm_line.as_str()]);
            let mut checked = checked(declared(Vec::new(), None), &[], &answer, None);
            checked.warmup = vec![builder_warmth(&answer, HASH)];
            assert_eq!(
                said(AT, &checked).lines,
                [
                    Line::Out("check: builder-1: app (work folder app-1a2b3c4d)".to_owned()),
                    Line::Out(line.to_owned()),
                    Line::Out("check: builder-1: nothing missing".to_owned()),
                ],
                "{name}"
            );
            assert_eq!(said(AT, &checked).status, 0, "{name}");
        }
        assert_eq!(
            builder_warmth(&lines(&["gate: remote warm none"]), HASH),
            Warmup::Builder { warm: false },
            "a warm line with no version line before it is unseen"
        );
    }

    /// A warm-up file that reads.
    const WARMUP_FILE: &[u8] = b"commands = [[\"dotnet\", \"build\"]]\n";

    /// A scratch worktree root holding [`WARMUP_FILE`] as its `.gate/warmup.toml`, and a state root beside it, both
    /// made new for the test `name`.
    #[expect(clippy::expect_used, reason = "a test that cannot make its folders must fail loudly")]
    fn warm_scratch(name: &str) -> (String, PathBuf) {
        let folder = crate::test_support::temp_dir().join(format!("gate-check-builder-{name}-{}", std::process::id()));
        if folder.exists() {
            fs::remove_dir_all(&folder).expect("an earlier scratch folder can be removed");
        }
        let root = folder.join("app");
        fs::create_dir_all(root.join(".gate")).expect("the .gate folder can be made");
        fs::write(root.join(".gate").join("warmup.toml"), WARMUP_FILE).expect("the warm-up file can be written");
        let state = folder.join("state");
        fs::create_dir_all(v1::warm_dir(&state)).expect("the warm-up folder can be made");
        (root.to_string_lossy().into_owned(), state)
    }

    /// The sources of a repo consenting to warm-ups on `builder-1`, its state root `state`.
    fn consenting(state: &Path) -> WarmSources {
        WarmSources {
            consent: Consent::Given { host: HOST.to_owned() },
            state: Ok(state.to_path_buf()),
        }
    }

    /// A preflight's lines whose warm line holds `hash`.
    fn warm_said(hash: &str) -> Vec<String> {
        lines(&[
            format!("gate: remote version {}", build_version()).as_str(),
            format!("gate: remote warm {hash}").as_str(),
        ])
    }

    /// The builder's line is judged against the hash of the worktree's own warm-up file, read from disk.
    #[test]
    fn the_builder_s_warm_line_is_compared_with_the_hash_of_the_worktree_s_file() {
        let (root, state) = warm_scratch("hash");
        let mut problems = Vec::new();
        let file_hash = warmup_hash(WARMUP_FILE);
        assert_eq!(
            warm_items(&root, consenting(&state), HOST, &warm_said(&file_hash), &mut problems).items,
            [Warmup::Builder { warm: true }],
            "the hash of the file's bytes"
        );
        let other = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
        assert_eq!(
            warm_items(&root, consenting(&state), HOST, &warm_said(other), &mut problems).items,
            [Warmup::Builder { warm: false }],
            "another hash"
        );
        assert!(problems.is_empty(), "{problems:?}");
    }

    /// This PC's record is said on a builder check when it names the builder checked, and not when it names another.
    #[test]
    #[expect(clippy::expect_used, reason = "a test that cannot write its record must fail loudly")]
    fn a_pc_record_is_said_on_a_builder_check_only_when_it_names_that_builder() {
        let (root, state) = warm_scratch("record");
        let at = Timestamp::from_second(1_700_000_000).expect("a time in range");
        let record = WarmRecord {
            host: HOST.to_owned(),
            state: WarmState::Warm,
            at,
            hash: warmup_hash(WARMUP_FILE),
            gate: build_version(),
            pid: 1,
            started: at,
            failure: None,
            reason: None,
        };
        v1::write_warm_record(&v1::warm_record_path(&state, &work_folder_key(&root)), &record).expect("the record can be written");
        let said = warm_said(&warmup_hash(WARMUP_FILE));
        let mut problems = Vec::new();
        assert_eq!(
            warm_items(&root, consenting(&state), HOST, &said, &mut problems).items,
            [Warmup::Warm { host: HOST.to_owned() }, Warmup::Builder { warm: true }],
            "a record of the builder checked"
        );
        assert_eq!(
            warm_items(&root, consenting(&state), "builder-2", &said, &mut problems).items,
            [Warmup::Builder { warm: true }],
            "a record of another builder"
        );
    }

    fn heard(status: Option<i32>, lines: Vec<String>, last_error: Option<&str>) -> Heard {
        Heard {
            status,
            lines,
            errors: last_error.iter().map(|line| (*line).to_owned()).collect(),
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

    /// A box that refused its own install is said as that, with its why, in place of a failed preparation; another
    /// status with the same line is not that refusal.
    #[test]
    fn a_box_that_refuses_its_install_is_said_as_untrusted() {
        let root = r"D:\gate-remote";
        let line = install_untrusted_line(r"C:\gate\gate.exe is owned by S-1-5-21-1, not SYSTEM");
        assert_eq!(
            refusal(HOST, root, &heard(Some(1), Vec::new(), Some(&line))),
            Some(
                r"check: builder-1: its gate install is not trusted (C:\gate\gate.exe is owned by S-1-5-21-1, not SYSTEM); reinstall with gate push"
                    .to_owned()
            )
        );
        assert_eq!(
            refusal(HOST, root, &heard(Some(3), Vec::new(), Some(&line))),
            Some(format!(r"check: builder-1: preparing D:\gate-remote failed (status 3): {line}"))
        );
        let mut followed = heard(Some(1), Vec::new(), Some("ssh: a later line"));
        followed.errors.insert(0, line);
        assert_eq!(
            refusal(HOST, root, &followed),
            Some(
                r"check: builder-1: its gate install is not trusted (C:\gate\gate.exe is owned by S-1-5-21-1, not SYSTEM); reinstall with gate push"
                    .to_owned()
            ),
            "every line is read, as the client reads them, not the last alone"
        );
    }

    /// An ssh that exits 255 with nothing on its standard error still says why the builder is unreachable.
    #[test]
    fn an_ssh_failure_with_no_message_says_ssh_exited_255() {
        let root = r"D:\gate-remote";
        assert_eq!(
            refusal(HOST, root, &heard(Some(255), Vec::new(), None)),
            Some("check: builder-1: unreachable: ssh exited 255 with no message".to_owned())
        );
        assert_eq!(
            refusal(HOST, root, &heard(Some(255), Vec::new(), Some("ssh: connection lost\r"))),
            Some("check: builder-1: unreachable: ssh exited 255 with no message".to_owned()),
            "a line a carriage return ends shows as nothing"
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
