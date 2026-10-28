//! Tailscale's path from this PC to a builder: `tailscale status --json` read once for the tailnet peer the builder's
//! alias names, matched by its host name, its DNS name whole or its first label, or one of its tailnet addresses, and,
//! when none matches, by the host name `ssh -G` resolves the alias to. A tailnet that is not running is said as that.
//! Each call is bounded at [`LIMIT`], judged against the worktree as the transport's ssh is, and nothing is cached.
//! `gate check --builder` says the path as an item, and a routed run journals it on its release line, read on a thread
//! of its own ([`PendingPath`]) so the read never holds the run's slot or its result.

use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use gate_win32::{Interrupt, PipedConsole};
use serde_json::Value;

use crate::environment::Variables;
use crate::remote::git::{self, LaunchGuard, ProgramCall, ProgramOutput, ProgramPlace};
use crate::remote::launch;
use crate::remote::transport::ssh::Ssh;

/// How long each `tailscale status --json` and `ssh -G` call may take.
const LIMIT: Duration = Duration::from_secs(3);
/// The program read for the tailnet's status, found along `PATH`.
const TAILSCALE: &str = "tailscale";
/// The variable naming the status file a test-seams gate reads when `--test-tailscale-status` is not given, so the
/// black-box suite never reaches this PC's `tailscale` or `ssh -G`.
#[cfg(feature = "test-seams")]
const STATUS_VARIABLE: &str = "GATE_TEST_TAILSCALE_STATUS";
/// The start of every line saying the status JSON cannot be read.
const UNREADABLE: &str = "tailscale status printed JSON that cannot be read";

/// How Tailscale reaches a tailnet peer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PeerPath {
    /// A direct path is in use (`CurAddr` set).
    Direct,
    /// Relayed through the DERP region named.
    Derp(String),
    /// Relayed through a peer relay.
    PeerRelay,
    /// No path is in use yet.
    NoPath,
}

/// What reading the path to a builder found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Found {
    /// The matched peer's path.
    Path(PeerPath),
    /// `tailscale` is not found along `PATH`.
    NotInstalled,
    /// No tailnet peer, or several none of them online, matches the builder's alias or the host name `ssh -G` resolves
    /// it to.
    NoPeer,
    /// Several online tailnet peers match the host word named: the alias without any `user@` part, or the host name
    /// `ssh -G` resolved it to.
    Several(String),
    /// The tailnet is not running, a call failed, timed out or was interrupted, or the status JSON cannot be read: why,
    /// one line.
    Failed(String),
}

impl Found {
    /// The journal's `path` value: `direct`, `derp:{region}`, `peer-relay` or `none`; `None` when the path is unknown.
    pub(crate) fn journal_word(&self) -> Option<String> {
        match self {
            Found::Path(PeerPath::Direct) => Some("direct".to_owned()),
            Found::Path(PeerPath::Derp(region)) => Some(format!("derp:{region}")),
            Found::Path(PeerPath::PeerRelay) => Some("peer-relay".to_owned()),
            Found::Path(PeerPath::NoPath) => Some("none".to_owned()),
            Found::NotInstalled | Found::NoPeer | Found::Several(_) | Found::Failed(_) => None,
        }
    }
}

/// Where the reads' programs run: the worktree none of them may be found inside, the interrupt that ends them, and the
/// launch guard each is started under, `None` for a read whose caller waits for it.
#[derive(Clone, Copy)]
pub(crate) struct Reads<'a> {
    pub worktree: &'a Path,
    pub interrupt: Option<Interrupt>,
    pub launch_guard: Option<&'a LaunchGuard>,
}

/// A read's calls: where they run, and the one [`ProgramPlace`] both are started from.
#[derive(Clone, Copy)]
struct Calls<'a> {
    place: &'a ProgramPlace,
    reads: Reads<'a>,
}

/// The path to the builder `alias`. The status is read from the file `status_flag` names in place of running
/// `tailscale` when given, else in a test-seams build from the file [`STATUS_VARIABLE`] names when set; a file that
/// does not exist stands for a PC without Tailscale, and an alias no peer matches is then not resolved with `ssh -G`.
pub(crate) fn path_to(alias: &str, status_flag: Option<&Path>, reads: Reads<'_>) -> Found {
    if let Some(file) = seam_file(status_flag) {
        return match seam_text(&file) {
            Ok(text) => found_in(&text, alias, None),
            Err(found) => found,
        };
    }
    let place = match ProgramPlace::new(Variables::from_process()) {
        Ok(place) => place,
        Err(why) => return Found::Failed(why),
    };
    let calls = Calls { place: &place, reads };
    match status_text(calls) {
        Ok(Some(text)) => found_in(&text, alias, Some(calls)),
        Ok(None) => Found::NotInstalled,
        Err(why) => Found::Failed(why),
    }
}

/// The status the seam's `file` holds; else a PC without Tailscale for a file that does not exist, or why it cannot be
/// read.
fn seam_text(file: &Path) -> Result<String, Found> {
    std::fs::read_to_string(file).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => Found::NotInstalled,
        _ => Found::Failed(format!("cannot read {}: {error}", file.display())),
    })
}

/// The path to the builder `alias` the status JSON `status` gives, an alias no peer matches resolved with `ssh -G`
/// through `calls` when given.
fn found_in(status: &str, alias: &str, calls: Option<Calls<'_>>) -> Found {
    let peers = match peers_of(status) {
        Ok(peers) => peers,
        Err(why) => return Found::Failed(why),
    };
    match (path_among(&peers, host_word(alias)), calls) {
        (Found::NoPeer, Some(calls)) => resolved(&peers, alias, resolved_host(alias, calls)),
        (found, _) => found,
    }
}

/// The file `--test-tailscale-status` names, else the one [`STATUS_VARIABLE`] names.
#[cfg(feature = "test-seams")]
fn seam_file(status_flag: Option<&Path>) -> Option<PathBuf> {
    status_flag
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os(STATUS_VARIABLE).filter(|value| !value.is_empty()).map(PathBuf::from))
}

/// The file `--test-tailscale-status` names, which a gate built without the test seams never reads.
#[cfg(not(feature = "test-seams"))]
fn seam_file(status_flag: Option<&Path>) -> Option<PathBuf> {
    status_flag.map(Path::to_path_buf)
}

/// What a routed run's path read is given: the builder's alias, the status file of `--test-tailscale-status`, the
/// run's worktree root, its interrupt, how long the read sleeps first (`--test-tailscale-delay-ms`), and how long each
/// of its calls holds its launch guard between the create and the resume (`--test-tailscale-hold-start-ms`).
pub(crate) struct ReadOf {
    pub alias: String,
    pub status_flag: Option<PathBuf>,
    pub worktree: String,
    pub interrupt: Option<Interrupt>,
    pub delay: Option<Duration>,
    pub hold_start: Option<Duration>,
}

/// A path read running on a thread of its own, never joined: the process may end while it runs, and a path it finds
/// once nothing takes it is dropped. Its calls start their programs under a [`LaunchGuard`] it shares with the read.
pub(crate) struct PendingPath(mpsc::Receiver<Found>, Arc<LaunchGuard>);

impl PendingPath {
    /// Starts the read `read` on a thread of its own; else why the thread could not be started.
    pub(crate) fn start(read: ReadOf) -> Result<PendingPath, String> {
        let (sender, receiver) = mpsc::channel();
        let guard = Arc::new(LaunchGuard::new(read.hold_start));
        let shared = Arc::clone(&guard);
        thread::Builder::new()
            .name("tailscale-path".to_owned())
            .spawn(move || {
                if let Some(delay) = read.delay {
                    thread::sleep(delay);
                }
                let reads = Reads {
                    worktree: Path::new(&read.worktree),
                    interrupt: read.interrupt,
                    launch_guard: Some(&shared),
                };
                // A release line already written without the path took nothing, and its receiver is gone.
                let _untaken = sender.send(path_to(&read.alias, read.status_flag.as_deref(), reads));
            })
            .map_err(|error| format!("cannot start its thread: {error}"))?;
        Ok(PendingPath(receiver, guard))
    }

    /// What the read found if it has finished, waiting up to `wait` for it when given and else not at all; a read whose
    /// thread ended without an answer, as one that panicked does, is taken as ended at once, waited on or not.
    pub(crate) fn taken(&self, wait: Option<Duration>) -> Taken {
        match wait {
            Some(wait) => match self.0.recv_timeout(wait) {
                Ok(found) => Taken::Finished(found),
                Err(mpsc::RecvTimeoutError::Timeout) => Taken::Running,
                Err(mpsc::RecvTimeoutError::Disconnected) => Taken::Ended,
            },
            None => match self.0.try_recv() {
                Ok(found) => Taken::Finished(found),
                Err(mpsc::TryRecvError::Empty) => Taken::Running,
                Err(mpsc::TryRecvError::Disconnected) => Taken::Ended,
            },
        }
    }
}

impl Drop for PendingPath {
    /// Closes the read's launch guard, so the process never ends between a call's creating its program suspended and
    /// putting it in its kill-on-close job: a call in that window is waited out, and a later one starts nothing.
    fn drop(&mut self) {
        self.1.close();
    }
}

/// What a release line takes of a [`PendingPath`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Taken {
    /// The read finished, finding this.
    Finished(Found),
    /// The read is still running.
    Running,
    /// The read's thread ended without an answer.
    Ended,
}

impl Taken {
    /// The journal's `path` value ([`Found::journal_word`]); `None` for a read that did not finish.
    pub(crate) fn journal_word(&self) -> Option<String> {
        match self {
            Taken::Finished(found) => found.journal_word(),
            Taken::Running | Taken::Ended => None,
        }
    }

    /// The line the run's log notes: why a read failed (`gate: tailscale path not read: <why>`), that it did not
    /// finish in time, or that its thread ended without an answer (`gate: tailscale path read failed`); `None` for a
    /// read that found a path, no Tailscale, no matching peer or several.
    pub(crate) fn note(&self) -> Option<String> {
        match self {
            Taken::Finished(Found::Failed(why)) => Some(format!("gate: tailscale path not read: {why}")),
            Taken::Finished(_) => None,
            Taken::Running => Some("gate: tailscale path not read in time for the release line".to_owned()),
            Taken::Ended => Some("gate: tailscale path read failed".to_owned()),
        }
    }
}

/// The path to the builder `alias`, whose host word matched no peer of `peers`, once `ssh -G` answered `resolved`: the
/// host name it printed matched against the same peers, unless it is the alias's own host word, which already matched
/// none; else why `ssh -G` gave no host name.
pub(crate) fn resolved(peers: &[Value], alias: &str, resolved: Result<String, String>) -> Found {
    match resolved {
        Err(why) => Found::Failed(format!("ssh -G {alias} {why}")),
        Ok(name) if same_host(&name, host_word(alias)) => Found::NoPeer,
        Ok(name) => path_among(peers, &name),
    }
}

/// The builder's alias without any `user@` part.
pub(crate) fn host_word(alias: &str) -> &str {
    alias.rsplit_once('@').map_or(alias, |(_, host)| host)
}

/// Whether the host words `one` and `other` are the same, in any case and with any trailing dot.
fn same_host(one: &str, other: &str) -> bool {
    let one = one.trim_end_matches('.');
    !one.is_empty() && one.eq_ignore_ascii_case(other.trim_end_matches('.'))
}

/// The peers of the status JSON `status`, read once; else why it cannot be read, or the state of a tailnet whose
/// `BackendState` is not `Running` (`tailscale is <state>`). A status with no `Peer` has none.
pub(crate) fn peers_of(status: &str) -> Result<Vec<Value>, String> {
    let read: Value = serde_json::from_str(status).map_err(|error| format!("{UNREADABLE}: {error}"))?;
    let Value::Object(mut top) = read else {
        return Err(format!("{UNREADABLE}: it is not an object"));
    };
    match top.get("BackendState") {
        None => {}
        Some(Value::String(state)) if state == "Running" => {}
        Some(Value::String(state)) => return Err(format!("tailscale is {state}")),
        Some(_) => return Err(format!("{UNREADABLE}: its BackendState is not text")),
    }
    match top.remove("Peer") {
        Some(Value::Object(peers)) => Ok(peers.into_iter().map(|(_, peer)| peer).collect()),
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(_) => Err(format!("{UNREADABLE}: its Peer is not an object")),
    }
}

/// The path to the one peer of `peers` that `host` names ([`names`]); of several, the one online. No peer when none
/// matches, or several do and none is online; [`Found::Several`], naming `host`, when more than one online does.
pub(crate) fn path_among(peers: &[Value], host: &str) -> Found {
    let matched: Vec<&Value> = peers.iter().filter(|peer| names(peer, host)).collect();
    if let [one] = matched.as_slice() {
        return Found::Path(path_of(one));
    }
    let online: Vec<&Value> = matched
        .into_iter()
        .filter(|peer| peer.get("Online") == Some(&Value::Bool(true)))
        .collect();
    match online.as_slice() {
        [] => Found::NoPeer,
        [one] => Found::Path(path_of(one)),
        _ => Found::Several(host.to_owned()),
    }
}

/// Whether `peer` is named by `host`, in any case and with any trailing dot: its `HostName`, its `DNSName` whole or its
/// first label, or one of its `TailscaleIPs`.
fn names(peer: &Value, host: &str) -> bool {
    let text = |key: &str| peer.get(key).and_then(Value::as_str).unwrap_or_default();
    let same = |word: &str| same_host(word, host);
    let dns = text("DNSName");
    let label = dns.split('.').next().unwrap_or_default();
    let address = peer
        .get("TailscaleIPs")
        .and_then(Value::as_array)
        .is_some_and(|ips| ips.iter().filter_map(Value::as_str).any(same));
    same(text("HostName")) || same(dns) || same(label) || address
}

/// The path `peer` is reached by: `CurAddr` set is direct; else its `Relay` region; else a peer relay; else none.
fn path_of(peer: &Value) -> PeerPath {
    let set = |key: &str| peer.get(key).and_then(Value::as_str).filter(|value| !value.is_empty());
    if set("CurAddr").is_some() {
        PeerPath::Direct
    } else if let Some(region) = set("Relay") {
        PeerPath::Derp(region.to_owned())
    } else if set("PeerRelay").is_some() {
        PeerPath::PeerRelay
    } else {
        PeerPath::NoPath
    }
}

/// The value of the first `hostname ` line of `ssh -G`'s `output`; `None` when it has none.
pub(crate) fn ssh_hostname(output: &str) -> Option<&str> {
    output
        .lines()
        .find_map(|line| line.strip_prefix("hostname "))
        .map(str::trim)
        .filter(|host| !host.is_empty())
}

/// The host name an `ssh -G` that ended with the exit code `code`, writing `stdout` and `stderr`, resolved its alias
/// to; else why it gave none: its exit, or output with no `hostname` line.
pub(crate) fn ssh_answer(code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> Result<String, String> {
    if code != Some(0) {
        return Err(exited(code, stderr));
    }
    ssh_hostname(&String::from_utf8_lossy(stdout))
        .map(str::to_owned)
        .ok_or_else(|| "printed no hostname line".to_owned())
}

/// `exited <code>`, with the lines `stderr` says that are not blank as a failure line carries them: git's failure text
/// ([`git::git_failure`]) with `exited` for its `status`.
fn exited(code: Option<i32>, stderr: &[u8]) -> String {
    let failure = git::git_failure(code, stderr);
    let said = failure.strip_prefix("status ").unwrap_or(&failure);
    format!("exited {said}")
}

/// What `tailscale status --json` printed; `None` when `tailscale` is not found along `PATH`; else why the call failed.
fn status_text(calls: Calls<'_>) -> Result<Option<String>, String> {
    let Some(output) = bounded(TAILSCALE, &["status", "--json"], calls)? else {
        return Ok(None);
    };
    if !output.status.success() {
        return Err(format!("tailscale status {}", exited(output.status.code(), &output.stderr)));
    }
    Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
}

/// The host name `ssh -G -- <alias>` resolves `alias` to, with ssh found as the transport finds it; else why it gave
/// none, its own words cut from the front of a failure line.
fn resolved_host(alias: &str, calls: Calls<'_>) -> Result<String, String> {
    let ssh = Ssh::find().map_err(|why| format!("could not start: {why}"))?;
    let program = ssh.program.to_string_lossy();
    let args = ["-G", "--", alias];
    let named = format!("{program} {} ", args.join(" "));
    let output = bounded(&program, &args, calls)
        .map_err(|why| why.strip_prefix(&named).map_or_else(|| why.clone(), str::to_owned))?
        .ok_or_else(|| "could not start: it is not found".to_owned())?;
    ssh_answer(output.status.code(), &output.stdout, &output.stderr)
}

/// Runs `program` with `args` on a hidden console from the calls' place, bounded at [`LIMIT`] and ended by the reads'
/// interrupt, a program found inside the reads' worktree refused ([`git::program_output`]); `None` when it is not found
/// along `PATH`; else why it could not be run, did not end in time or was interrupted.
fn bounded(program: &str, args: &[&str], calls: Calls<'_>) -> Result<Option<ProgramOutput>, String> {
    let call = ProgramCall {
        program,
        args,
        limit: LIMIT,
        drain: launch::DRAIN,
        interrupt: calls.reads.interrupt,
        worktrees: &[calls.reads.worktree],
        console: PipedConsole::Hidden,
        stdin: None,
        launch_guard: calls.reads.launch_guard,
    };
    git::program_output(&call, calls.place).map_err(String::from)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    use crate::remote::git::LaunchGuard;

    use super::{Found, PeerPath, PendingPath, Taken, host_word, path_among, peers_of, resolved, ssh_answer, ssh_hostname};

    /// What the status JSON `status` gives for `host`, as [`super::path_to`] reads it.
    fn path_in(status: &str, host: &str) -> Result<Found, String> {
        Ok(path_among(&peers_of(status)?, host))
    }

    /// A read that found `path`.
    #[expect(clippy::unnecessary_wraps, reason = "it stands for path_in's answer, which the cases compare it with")]
    fn found(path: PeerPath) -> Result<Found, String> {
        Ok(Found::Path(path))
    }

    /// A status of the peers `peers`, each a JSON object's fields.
    fn status(peers: &[&str]) -> String {
        let entries: Vec<String> = peers
            .iter()
            .enumerate()
            .map(|(index, fields)| format!(r#""nodekey:{index}": {{{fields}}}"#))
            .collect();
        format!(
            r#"{{"BackendState": "Running", "Self": {{"HostName": "this-pc"}}, "Peer": {{{}}}}}"#,
            entries.join(", ")
        )
    }

    /// A peer named `host`, at `100.64.0.{n}`, online as `online`, with the path fields `path`.
    fn peer(host: &str, n: u8, online: bool, path: &str) -> String {
        let names = format!(r#""HostName": "{host}", "DNSName": "{host}.tail0000.ts.net.""#);
        let addresses = format!(r#""TailscaleIPs": ["100.64.0.{n}", "fd7a:115c:a1e0::{n}"]"#);
        format!(r#"{names}, {addresses}, "Online": {online}, {path}"#)
    }

    const DIRECT: &str = r#""CurAddr": "192.0.2.10:41641", "Relay": "fra", "PeerRelay": """#;
    const DERP: &str = r#""CurAddr": "", "Relay": "fra", "PeerRelay": """#;
    const PEER_RELAY: &str = r#""CurAddr": "", "Relay": "", "PeerRelay": "100.64.0.9:7777""#;
    const NO_PATH: &str = r#""CurAddr": "", "Relay": "", "PeerRelay": """#;

    #[test]
    fn each_path_is_read_from_the_matched_peer() {
        for (fields, path) in [
            (DIRECT, PeerPath::Direct),
            (DERP, PeerPath::Derp("fra".to_owned())),
            (PEER_RELAY, PeerPath::PeerRelay),
            (NO_PATH, PeerPath::NoPath),
        ] {
            let text = status(&[&peer("other-box", 2, true, DIRECT), &peer("build-box", 1, true, fields)]);
            assert_eq!(path_in(&text, "build-box"), found(path.clone()), "{path:?}");
        }
    }

    #[test]
    fn a_peer_is_matched_by_host_name_dns_label_or_address_in_any_case() {
        let named = concat!(
            r#""HostName": "Build-Box", "DNSName": "other-name.tail0000.ts.net.", "#,
            r#""TailscaleIPs": ["100.64.0.1"], "Online": true, "CurAddr": "192.0.2.10:41641""#
        );
        let text = status(&[named]);
        for host in ["build-box", "BUILD-BOX", "other-name", "Other-Name", "100.64.0.1"] {
            assert_eq!(path_in(&text, host), found(PeerPath::Direct), "{host}");
        }
        for host in ["build", "other-name.tail0000", "100.64.0.10", "tail0000", "tail0000.ts.net", ""] {
            assert_eq!(path_in(&text, host), Ok(Found::NoPeer), "{host}");
        }
    }

    /// A tailnet DNS name matches the peer's whole `DNSName`, with or without its trailing dot, in any case.
    #[test]
    fn a_peer_is_matched_by_its_full_dns_name_with_or_without_the_trailing_dot() {
        let text = status(&[&peer("build-box", 1, true, DERP)]);
        for host in ["build-box.tail0000.ts.net", "Build-Box.TAIL0000.ts.net.", "build-box.tail0000.ts.net."] {
            assert_eq!(path_in(&text, host), found(PeerPath::Derp("fra".to_owned())), "{host}");
        }
        assert_eq!(path_in(&text, "build-box.tail9999.ts.net"), Ok(Found::NoPeer), "another tailnet");
    }

    #[test]
    fn a_user_part_of_the_alias_is_cut() {
        assert_eq!(host_word("builder@build-box"), "build-box");
        assert_eq!(host_word("build-box"), "build-box");
        let text = status(&[&peer("build-box", 1, true, DERP)]);
        assert_eq!(path_in(&text, host_word("ci@Build-Box")), found(PeerPath::Derp("fra".to_owned())));
    }

    /// Of several matching peers the one online wins; several online are said as several, and none online as none.
    #[test]
    fn of_several_matches_the_one_online_wins_and_several_online_are_said() {
        let one_online = status(&[&peer("build-box", 1, false, DIRECT), &peer("build-box", 2, true, DERP)]);
        assert_eq!(path_in(&one_online, "build-box"), found(PeerPath::Derp("fra".to_owned())));
        let two_online = status(&[&peer("build-box", 1, true, DIRECT), &peer("build-box", 2, true, DERP)]);
        assert_eq!(path_in(&two_online, "build-box"), Ok(Found::Several("build-box".to_owned())));
        let none_online = status(&[&peer("build-box", 1, false, DIRECT), &peer("build-box", 2, false, DERP)]);
        assert_eq!(path_in(&none_online, "build-box"), Ok(Found::NoPeer));
    }

    #[test]
    fn no_matching_peer_is_none() {
        assert_eq!(path_in(&status(&[&peer("other-box", 2, true, DIRECT)]), "build-box"), Ok(Found::NoPeer));
        assert_eq!(
            path_in(r#"{"BackendState": "Running"}"#, "build-box"),
            Ok(Found::NoPeer),
            "no peers at all"
        );
    }

    #[test]
    fn json_that_cannot_be_read_says_why() {
        for text in ["", "{not json", "[]", r#"{"Peer": []}"#, r#"{"BackendState": 1}"#] {
            let read = path_in(text, "build-box");
            let said = matches!(&read, Err(why)
                if why.starts_with("tailscale status printed JSON that cannot be read: ") && !why.contains('\n'));
            assert!(said, "{text:?}: {read:?}");
        }
    }

    /// A tailnet that is not running says its state, whatever its peers.
    #[test]
    fn a_tailnet_that_is_not_running_says_its_state() {
        for state in ["Stopped", "NeedsLogin"] {
            let text = status(&[&peer("build-box", 1, true, DIRECT)]).replace("\"Running\"", &format!("\"{state}\""));
            assert_eq!(path_in(&text, "build-box"), Err(format!("tailscale is {state}")), "{state}");
        }
    }

    /// The host name `ssh -G` printed is matched against the same peers; one that is the alias's own host word, or
    /// matches none, is no peer, and an `ssh -G` that gave none says why, naming the alias.
    #[test]
    fn the_host_name_ssh_g_resolves_is_matched_and_its_failure_said() {
        let Ok(peers) = peers_of(&status(&[&peer("build-box", 1, true, DERP)])) else {
            unreachable!("the status reads");
        };
        let relayed = Found::Path(PeerPath::Derp("fra".to_owned()));
        assert_eq!(resolved(&peers, "ci@gate-mac", Ok("100.64.0.1".to_owned())), relayed);
        assert_eq!(resolved(&peers, "gate-mac", Ok("build-box.tail0000.ts.net".to_owned())), relayed);
        assert_eq!(
            resolved(&peers, "gate-mac", Ok("Gate-Mac.".to_owned())),
            Found::NoPeer,
            "the alias itself"
        );
        assert_eq!(resolved(&peers, "gate-mac", Ok("elsewhere.example".to_owned())), Found::NoPeer);
        let Ok(twins) = peers_of(&status(&[&peer("build-box", 1, true, DIRECT), &peer("build-box", 2, true, DERP)])) else {
            unreachable!("the status reads");
        };
        assert_eq!(
            resolved(&twins, "ci@gate-mac", Ok("Build-Box.tail0000.ts.net".to_owned())),
            Found::Several("Build-Box.tail0000.ts.net".to_owned()),
            "several are named by the host name ssh -G printed"
        );
        assert_eq!(
            path_among(&twins, host_word("ci@Build-Box")),
            Found::Several("Build-Box".to_owned()),
            "several are named by the alias without its user"
        );
        assert_eq!(
            resolved(&peers, "ci@gate-mac", Err("did not answer in 3 s".to_owned())),
            Found::Failed("ssh -G ci@gate-mac did not answer in 3 s".to_owned())
        );
    }

    /// `ssh -G` answers its `hostname` line only when it exits 0 with one; else its exit, or the missing line.
    #[test]
    fn an_ssh_g_that_fails_or_prints_no_hostname_says_why() {
        assert_eq!(ssh_answer(Some(0), b"user ci\nhostname 100.64.0.1\n", b""), Ok("100.64.0.1".to_owned()));
        assert_eq!(
            ssh_answer(Some(255), b"", b"ssh: bad configuration option\r\n\n"),
            Err("exited 255: ssh: bad configuration option".to_owned())
        );
        assert_eq!(ssh_answer(None, b"hostname x\n", b""), Err("exited unknown".to_owned()));
        assert_eq!(
            ssh_answer(Some(0), b"user ci\nport 22\n", b""),
            Err("printed no hostname line".to_owned())
        );
    }

    #[test]
    fn the_journal_word_of_each_path_and_none_when_unknown() {
        assert_eq!(Found::Path(PeerPath::Direct).journal_word().as_deref(), Some("direct"));
        assert_eq!(Found::Path(PeerPath::Derp("fra".to_owned())).journal_word().as_deref(), Some("derp:fra"));
        assert_eq!(Found::Path(PeerPath::PeerRelay).journal_word().as_deref(), Some("peer-relay"));
        assert_eq!(Found::Path(PeerPath::NoPath).journal_word().as_deref(), Some("none"));
        for unknown in [
            Found::NotInstalled,
            Found::NoPeer,
            Found::Several("x".to_owned()),
            Found::Failed("x".to_owned()),
        ] {
            assert_eq!(unknown.journal_word(), None, "{unknown:?}");
        }
    }

    /// What the release line takes of a read, and the note the log gets: a path journalled with none, a failed read's
    /// reason, a read not finished, and a read whose thread ended without an answer.
    #[test]
    fn each_taken_read_gives_its_journal_word_and_its_log_note() {
        let cases = [
            (Taken::Finished(Found::Path(PeerPath::Direct)), Some("direct"), None),
            (Taken::Finished(Found::NoPeer), None, None),
            (
                Taken::Finished(Found::Failed("tailscale is Stopped".to_owned())),
                None,
                Some("gate: tailscale path not read: tailscale is Stopped"),
            ),
            (Taken::Running, None, Some("gate: tailscale path not read in time for the release line")),
            (Taken::Ended, None, Some("gate: tailscale path read failed")),
        ];
        for (taken, word, note) in cases {
            assert_eq!((taken.journal_word().as_deref(), taken.note().as_deref()), (word, note), "{taken:?}");
        }
    }

    /// A read whose thread ended without sending, as one that panicked does, is taken as ended at once, waited on or not.
    #[test]
    fn a_read_whose_thread_ended_without_an_answer_is_taken_as_ended_at_once() {
        let (sender, receiver) = mpsc::channel::<Found>();
        drop(sender);
        let pending = PendingPath(receiver, Arc::new(LaunchGuard::new(None)));
        assert_eq!(pending.taken(None), Taken::Ended);
        assert_eq!(pending.taken(Some(Duration::from_secs(600))), Taken::Ended);
    }

    #[test]
    fn ssh_g_s_hostname_line_is_read_among_the_others() {
        let output = "user ci\nhostname build-box.tail0000.ts.net\nport 22\nhostname later\n";
        assert_eq!(ssh_hostname(output), Some("build-box.tail0000.ts.net"));
        assert_eq!(ssh_hostname("user ci\r\nhostname 100.64.0.1\r\nport 22\r\n"), Some("100.64.0.1"));
        assert_eq!(ssh_hostname("user ci\nport 22\nhostnamealias x\n"), None);
        assert_eq!(ssh_hostname(""), None);
    }
}
