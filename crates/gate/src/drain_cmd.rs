//! `gate drain`: drains one builder before a redeploy. With no option it raises the builder's drain barrier (or raises
//! it again) through the box's `gate box drain` verb and waits, polling it, until the box says the builder is drained,
//! naming on stderr the runs it waits on; `--lift` lifts the barrier and `--status` says how the drain stands, changing
//! nothing. The builder is the one `remote.json`'s `host` names, or the one `--builder` names; no line names it.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::thread;
use std::time::{Duration, Instant};

use gate_args::{BoxVerb, DrainAction, DrainRequest, DrainSeams};
use gate_router::{Builder, escaped};
use gate_win32::{Interrupt, PipedConsole};
use gate_wire::stamp;
use serde_json::Value;

use crate::answer::{FAILED_STATUS, USAGE_STATUS};
use crate::console::Console;
use crate::environment::Variables;
use crate::heartbeat::Heartbeat;
use crate::remote::drain::utc_clock;
use crate::remote::git::{self, GitError, ProgramCall, ProgramOutput, ProgramPlace};
use crate::remote::launch;
use crate::remote::payload;
use crate::remote::transport::ssh::Ssh;
use crate::remote::transport::{Program, Transport, Wire};
use crate::remote::{SSH_FAILED, no_box};
use crate::resolve::Environment;
use crate::route;
use crate::run::CONTROL_C_EXIT;

/// How often the wait asks the box how the drain stands.
const POLL: Duration = Duration::from_secs(2);
/// How often the wait says it still waits.
const HEARTBEAT: Duration = Duration::from_secs(30);
/// How often the wait raises the barrier again, well inside its 30 minute lapse.
const RAISE: Duration = Duration::from_secs(300);
/// How long one call of the box's drain verb may take before it is ended: ssh's connection, and the box's drain lock,
/// which it retries for up to 10 s.
const BOUND: Duration = Duration::from_secs(120);
/// What every line of the box's drain verb starts with, once its stamp is taken off.
const BOX_LINE: &str = "gate: drain ";
/// What the box's line of a drain verb that failed starts with, on its standard error.
const BOX_FAILED: &str = "gate: drain failed: ";
/// The line of a wait whose barrier the box no longer holds.
const GONE: &str = "gate: drain: the barrier was lifted or lapsed while gate drain waited; gate drain raises it again";
/// The line of a drain Ctrl+C ended.
const INTERRUPTED: &str = "gate: drain: interrupted; the barrier stands; gate drain --lift lifts it, gate drain resumes the wait";
/// The line of a drain Ctrl+C ended during its first raise.
const INTERRUPTED_BEFORE: &str =
    "gate: drain: interrupted before the builder answered the raise; a barrier may stand; gate drain --status says, gate drain --lift lifts it";
/// What a line says in place of the builder's name.
const THE_BUILDER: &str = "the builder";

/// Runs `gate drain` and returns its exit status: 0 drained, lifted or reported, 1 a failure, 2 a builder `remote.json`
/// does not list, `CONTROL_C_EXIT` on Ctrl+C.
pub(crate) fn drain(request: &DrainRequest, environment: &Environment, console: &mut Console) -> u32 {
    let builder = match chosen(request.builder.as_deref(), environment.local_app_data.as_deref()) {
        Ok(builder) => builder,
        Err((status, line)) => {
            console.err(&line);
            return u32::from(status);
        }
    };
    let transport = match transport(&request.seams) {
        Ok(transport) => transport,
        Err(line) => {
            console.err(&line);
            return u32::from(FAILED_STATUS);
        }
    };
    let mut to = To {
        transport: &*transport,
        builder: &builder,
        interrupt: None,
    };
    match request.action {
        DrainAction::Status => status(&to, console),
        DrainAction::Lift => lift(&to, console),
        DrainAction::Raise => {
            to.interrupt = Interrupt::install()
                .inspect_err(|error| {
                    console.err(&format!(
                        "gate: cannot install the Ctrl+C handler, so Ctrl+C ends the gate at once: {error}"
                    ));
                })
                .ok();
            raise(&to, &request.seams, console)
        }
    }
}

/// The builder `remote.json` under `local_app_data` lists as `named`, else its `host`
/// ([`gate_router::Config::named_builder`]). Else the status and the line: 2 for a name it does not list, 1 for a
/// routing file that is not there or cannot be used, worded as `gate push` words it.
fn chosen(named: Option<&str>, local_app_data: Option<&OsStr>) -> Result<Builder, (u8, String)> {
    let path = route::remote_json_path(local_app_data)
        .ok_or_else(|| (FAILED_STATUS, "drain: no remote.json: LOCALAPPDATA is not an absolute folder".to_owned()))?;
    let ignored = |reason: &str| (FAILED_STATUS, format!("drain: ignored {}: {reason}", path.display()));
    let config = match route::read_remote_json(&path) {
        Ok(Some(config)) => config,
        Ok(None) => return Err((FAILED_STATUS, format!("drain: no remote.json at {}", path.display()))),
        Err(problem) => return Err(ignored(&problem.reason)),
    };
    let host = named.unwrap_or_else(|| config.host());
    config
        .named_builder(host)
        .map_err(ignored)?
        .ok_or_else(|| (USAGE_STATUS, format!("gate drain: {} is not a builder remote.json lists", escaped(host))))
}

/// The way to the builder: the local transport `--test-remote-transport` names, else ssh found along the gate's own
/// `PATH`. Else the line saying why the local transport cannot be used, or why ssh cannot be found.
#[cfg(feature = "test-seams")]
fn transport(seams: &DrainSeams) -> Result<Box<dyn Transport>, String> {
    match &seams.remote_transport {
        Some(named) => crate::remote::local_transport(named, seams.remote_ssh.as_deref()).map(|local| Box::new(local) as Box<dyn Transport>),
        None => ssh(),
    }
}

/// The way to the builder: ssh found along the gate's own `PATH`; else the line saying why it cannot be found.
#[cfg(not(feature = "test-seams"))]
fn transport(_seams: &DrainSeams) -> Result<Box<dyn Transport>, String> {
    ssh()
}

/// ssh found along the gate's own `PATH`; else the line saying why it cannot be found.
fn ssh() -> Result<Box<dyn Transport>, String> {
    Ssh::find()
        .map(|ssh| Box::new(ssh) as Box<dyn Transport>)
        .map_err(|why| format!("gate: drain: {why}"))
}

/// The builder a drain asks, the way to it, and the console interrupt that ends a call in flight.
struct To<'a> {
    transport: &'a dyn Transport,
    builder: &'a Builder,
    interrupt: Option<Interrupt>,
}

impl To<'_> {
    /// How the drain stands once the box's drain verb has taken `action`; else why it gave no answer, as [`classified`]
    /// reads it. No call is made once Ctrl+C was pressed.
    fn ask(&self, action: DrainAction) -> Result<Status, Unanswered> {
        if self.interrupted() {
            return Err(Unanswered::Interrupted);
        }
        let profile = self.transport.profile(self.builder);
        let program = self.transport.call(&self.builder.alias, &profile.line(BoxVerb::Drain), Wire::Plain);
        let input = payload::input(&payload::drain_word(profile.root(), action), None);
        let answer = called(&program, input.as_bytes(), self.interrupt).and_then(|output| answer_of(&output));
        classified(answer, self.interrupted(), &self.builder.alias)
    }

    /// Whether Ctrl+C was pressed.
    fn interrupted(&self) -> bool {
        self.interrupt.is_some_and(Interrupt::is_raised)
    }
}

/// Why a call of the box's drain verb gave no answer.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Unanswered {
    /// The builder could not be reached, the call outlived its bound, or the box said its drain failed: why, in the
    /// gate's or the box's own words.
    Unreachable(String),
    /// The call failed with a last line on its standard error, ssh's or the call's own, which may name the builder:
    /// that line, which [`classified`] masks into [`Unanswered::Unreachable`].
    Said(String),
    /// The builder's gate box does not take the drain verb: the call's status.
    TooOld(i32),
    /// Ctrl+C ended the call.
    Interrupted,
}

/// What a call that ended in `answer` comes to, Ctrl+C having been pressed by its end when `interrupted`: any failure is
/// then [`Unanswered::Interrupted`]; else a line ssh or the call said ([`Unanswered::Said`]) is said with the builder's
/// ssh `alias` masked ([`unnamed`]), and anything else is kept.
fn classified(answer: Result<Status, Unanswered>, interrupted: bool, alias: &str) -> Result<Status, Unanswered> {
    match answer {
        Ok(status) => Ok(status),
        Err(_) if interrupted => Err(Unanswered::Interrupted),
        Err(Unanswered::Said(line)) => Err(Unanswered::Unreachable(unnamed(&line, alias))),
        Err(other) => Err(other),
    }
}

/// Whether `byte` may be part of a host name, so a name next to it is not a whole word.
fn in_name(byte: Option<&u8>) -> bool {
    byte.is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'-' || *byte == b'.')
}

/// `why` with every whole-word occurrence of the builder's ssh `alias`, and of the host part of it after any `user@`,
/// said as "the builder", letter case aside, so no line names the builder; a word is bounded by anything but an ASCII
/// letter or digit, `-` or `.`.
fn unnamed(why: &str, alias: &str) -> String {
    let host = alias.rsplit_once('@').map_or(alias, |(_, host)| host);
    let mut names: Vec<String> = [alias, host]
        .iter()
        .filter(|name| !name.is_empty())
        .map(|name| name.to_ascii_lowercase())
        .collect();
    names.sort_by_key(|name| std::cmp::Reverse(name.len()));
    names.dedup();
    let lowered = why.to_ascii_lowercase();
    let bytes = lowered.as_bytes();
    let (mut text, mut copied, mut at) = (String::new(), 0, 0);
    while at < bytes.len() {
        let starts_word = at == 0 || !in_name(bytes.get(at - 1));
        let whole = |name: &&String| bytes[at..].starts_with(name.as_bytes()) && !in_name(bytes.get(at + name.len()));
        match names.iter().find(whole).filter(|_| starts_word) {
            Some(name) => {
                text.push_str(&why[copied..at]);
                text.push_str(THE_BUILDER);
                at += name.len();
                copied = at;
            }
            None => at += 1,
        }
    }
    text.push_str(&why[copied..]);
    text
}

/// Makes the one call `program`, fed `input` on its standard input, bounded at [`BOUND`] and ended by `interrupt`, as the
/// client's programs other than git run ([`git::program_output`]); what it said, else why it said nothing.
fn called(program: &Program, input: &[u8], interrupt: Option<Interrupt>) -> Result<ProgramOutput, Unanswered> {
    let shown = program.program.to_string_lossy();
    let words: Vec<String> = program.args.iter().map(|word| word.to_string_lossy().into_owned()).collect();
    let args: Vec<&str> = words.iter().map(String::as_str).collect();
    let call = ProgramCall {
        program: &shown,
        args: &args,
        limit: BOUND,
        drain: launch::DRAIN,
        interrupt,
        worktrees: &[],
        console: PipedConsole::Attached,
        stdin: Some(input),
        launch_guard: None,
    };
    let place = ProgramPlace::new(Variables::from_process()).map_err(Unanswered::Unreachable)?;
    let late = call.late_line();
    let limit = git::spelled_limit(BOUND);
    // A line never carries ssh's options: the whole call's words are shown as the program alone.
    let named = late.strip_suffix(&format!(" did not answer in {limit}")).unwrap_or(&late);
    match git::program_output(&call, &place) {
        Ok(Some(output)) => Ok(output),
        Ok(None) => Err(Unanswered::Unreachable(format!("cannot run {shown}: it is not found"))),
        Err(GitError::Interrupted) => Err(Unanswered::Interrupted),
        Err(GitError::Failed(why)) if why == late => Err(Unanswered::Unreachable(format!("no answer in {limit}"))),
        Err(GitError::Failed(why) | GitError::OverCap(why)) => Err(Unanswered::Unreachable(escaped(&why.replace(named, &shown)))),
    }
}

/// How the drain stands as the call that ended in `output` says: its status lines when it exited 0; else why it gave
/// no answer: the box's own failure line, a box that does not take the verb ([`no_box`]), or ssh's or the call's last
/// line.
fn answer_of(output: &ProgramOutput) -> Result<Status, Unanswered> {
    let lines: Vec<String> = String::from_utf8_lossy(&output.stdout).lines().map(str::to_owned).collect();
    let errors: Vec<String> = String::from_utf8_lossy(&output.stderr).lines().map(str::to_owned).collect();
    let code = output.status.code();
    if code == Some(0) {
        return status_of(lines.iter().map(String::as_str))
            .ok_or_else(|| Unanswered::Unreachable("the builder's answer says no drain state".to_owned()));
    }
    if let Some(why) = errors.iter().find_map(|line| stamp::unstamped(line).text.strip_prefix(BOX_FAILED)) {
        return Err(Unanswered::Unreachable(escaped(why)));
    }
    let Some(code) = code else {
        return Err(Unanswered::Unreachable("the call ended with no status".to_owned()));
    };
    if code != SSH_FAILED && no_box(code, &lines, errors.iter().map(String::as_str)) {
        return Err(Unanswered::TooOld(code));
    }
    let last = errors.iter().rev().find(|line| !line.trim().is_empty());
    if let Some(line) = last {
        return Err(Unanswered::Said(escaped(&git::failure_text(&[stamp::unstamped(line).text]))));
    }
    Err(Unanswered::Unreachable(if code == SSH_FAILED {
        format!("ssh exited {SSH_FAILED} with no message")
    } else {
        format!("the builder's drain call exited {code}")
    }))
}

/// How the builder's barrier stands, as the box's barrier line says it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Barrier {
    /// `barrier none`: none stands, and none lapsed since the last lift.
    None,
    /// `barrier {raised} {lapses}`: one stands, raised at the Unix second `raised` and lapsing at `lapses`, both by the
    /// builder's clock.
    Standing { raised: i64, lapses: i64 },
    /// `barrier lapsed {at}`: the last one lapsed at the Unix second `at` without a lift.
    Lapsed(i64),
}

/// The drain's state, as the box's state line says it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Drained,
    Waiting,
    Open,
}

impl State {
    fn word(self) -> &'static str {
        match self {
            State::Drained => "drained",
            State::Waiting => "waiting",
            State::Open => "open",
        }
    }
}

/// One thing the box lists that keeps the drain waiting.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Item {
    /// A live run: its age in seconds, and its PC, repo and command, each `None` when the box sent `null`.
    Session {
        age: u64,
        pc: Option<String>,
        repo: Option<String>,
        command: Option<String>,
    },
    /// A process the box counts: its pid and kind, `prune`, `git` or `job`.
    Process { pid: u32, kind: String },
    /// A run whose record the box cannot read: its key or record name.
    Unreadable(String),
    /// A Unix box that cannot list its processes: why.
    Unknown(String),
}

impl Item {
    /// The item with a session's age left out, as the wait compares one list with the next: an age grows between
    /// polls.
    fn unaged(&self) -> Item {
        match self {
            Item::Session { pc, repo, command, .. } => Item::Session {
                age: 0,
                pc: pc.clone(),
                repo: repo.clone(),
                command: command.clone(),
            },
            other => other.clone(),
        }
    }
}

/// How the drain stands, as the box's status lines say it: the barrier, what keeps it waiting, the live tickets and the
/// state.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Status {
    barrier: Barrier,
    items: Vec<Item>,
    tickets: u64,
    state: State,
}

/// One status line of the box, after `gate: drain `.
enum Said {
    Barrier(Barrier),
    Item(Item),
    Tickets(u64),
    State(State),
}

/// How the drain stands as the box's status `lines` say it, each read without its stamp; `None` when they say no
/// barrier or no state. A line of any other shape is passed over.
fn status_of<'l>(lines: impl IntoIterator<Item = &'l str>) -> Option<Status> {
    let (mut barrier, mut state, mut tickets, mut items) = (None, None, 0, Vec::new());
    for line in lines {
        let Some(rest) = stamp::unstamped(line).text.strip_prefix(BOX_LINE) else {
            continue;
        };
        match said_of(rest) {
            Some(Said::Barrier(said)) => barrier = Some(said),
            Some(Said::Item(item)) => items.push(item),
            Some(Said::Tickets(count)) => tickets = count,
            Some(Said::State(said)) => state = Some(said),
            None => {}
        }
    }
    Some(Status {
        barrier: barrier?,
        items,
        tickets,
        state: state?,
    })
}

/// What the status line `rest`, after `gate: drain `, says; `None` for any other line.
fn said_of(rest: &str) -> Option<Said> {
    let (word, tail) = rest.split_once(' ')?;
    match word {
        "barrier" => barrier_of(tail).map(Said::Barrier),
        "session" => session_of(tail).map(Said::Item),
        "process" => {
            let (pid, kind) = tail.split_once(' ')?;
            Some(Said::Item(Item::Process {
                pid: whole(pid)?,
                kind: kind.to_owned(),
            }))
        }
        "command" | "record" => tail.strip_suffix(" unreadable").map(|key| Said::Item(Item::Unreadable(key.to_owned()))),
        "processes" => tail.strip_prefix("unknown: ").map(|why| Said::Item(Item::Unknown(why.to_owned()))),
        "tickets" => whole(tail).map(Said::Tickets),
        "state" => match tail {
            "drained" => Some(Said::State(State::Drained)),
            "waiting" => Some(Said::State(State::Waiting)),
            "open" => Some(Said::State(State::Open)),
            _ => None,
        },
        _ => None,
    }
}

/// The barrier `tail`, after `gate: drain barrier `, says.
fn barrier_of(tail: &str) -> Option<Barrier> {
    if tail == "none" {
        return Some(Barrier::None);
    }
    if let Some(at) = tail.strip_prefix("lapsed ") {
        return whole(at).map(Barrier::Lapsed);
    }
    let (raised, lapses) = tail.split_once(' ')?;
    Some(Barrier::Standing {
        raised: whole(raised)?,
        lapses: whole(lapses)?,
    })
}

/// The session `tail`, after `gate: drain session `, says: its age, then one JSON object `{"Pc","Repo","Command"}`.
fn session_of(tail: &str) -> Option<Item> {
    let (age, fields) = tail.split_once(' ')?;
    let fields: Value = serde_json::from_str(fields).ok()?;
    let text = |name: &str| fields.get(name).and_then(Value::as_str).map(str::to_owned);
    Some(Item::Session {
        age: whole(age)?,
        pc: text("Pc"),
        repo: text("Repo"),
        command: text("Command"),
    })
}

/// `digits` as a whole number, when they are one or more ASCII digits and nothing else.
fn whole<T: std::str::FromStr>(digits: &str) -> Option<T> {
    (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

/// The list of what keeps the drain waiting: a header counting it, then one line each; a line of its own when the box
/// lists nothing.
fn list_lines(items: &[Item]) -> Vec<String> {
    if items.is_empty() {
        return vec!["gate: drain: waiting for the builder to finish what it has started".to_owned()];
    }
    let mut lines = vec![format!("gate: drain: waiting for {} run(s) on the builder:", items.len())];
    lines.extend(items.iter().map(item_line));
    lines
}

/// The list's line of `item`, each text from the builder escaped.
fn item_line(item: &Item) -> String {
    match item {
        Item::Session { age, pc, repo, command } => {
            format!(
                "gate:   {}  {}  {age} s  {}",
                field(pc.as_deref()),
                field(repo.as_deref()),
                field(command.as_deref())
            )
        }
        Item::Process { pid, kind } => format!("gate:   process {pid} ({})", escaped(kind)),
        Item::Unreadable(key) => format!("gate:   a run whose record cannot be read ({})", escaped(key)),
        Item::Unknown(why) => format!("gate:   the builder cannot list its processes: {}", escaped(why)),
    }
}

/// A session's field as its line shows it: escaped, or `?` for one the box sent as `null`.
fn field(text: Option<&str>) -> String {
    text.map_or_else(|| "?".to_owned(), escaped)
}

/// The line saying how the barrier stands.
fn barrier_line(barrier: Barrier) -> String {
    match barrier {
        Barrier::None => "gate: drain: no barrier stands on the builder".to_owned(),
        Barrier::Standing { lapses, .. } => format!("gate: drain: a barrier stands on the builder; it lapses at {} UTC", utc_clock(lapses)),
        Barrier::Lapsed(at) => format!("gate: drain: the last barrier lapsed at {} UTC without a lift", utc_clock(at)),
    }
}

/// The lines of `gate drain --status`: the barrier, the list while a barrier stands and something is going, the
/// tickets and the state.
fn status_lines(status: &Status) -> Vec<String> {
    let mut lines = vec![barrier_line(status.barrier)];
    let going = status.state == State::Waiting || !status.items.is_empty();
    if matches!(status.barrier, Barrier::Standing { .. }) && going {
        lines.extend(list_lines(&status.items));
    }
    lines.push(format!("gate: drain: {} waiting ticket(s)", status.tickets));
    lines.push(format!("gate: drain: state {}", status.state.word()));
    lines
}

/// Says why the first call of a drain gave no answer; its exit status.
fn unanswered(unanswered: Unanswered, console: &mut Console) -> u32 {
    match unanswered {
        Unanswered::Unreachable(why) | Unanswered::Said(why) => {
            console.err(&format!("gate: drain: the builder could not be reached: {why}"));
            u32::from(FAILED_STATUS)
        }
        Unanswered::TooOld(code) => too_old(code, console),
        Unanswered::Interrupted => {
            console.err(INTERRUPTED);
            CONTROL_C_EXIT
        }
    }
}

/// Says the builder's gate box does not take the drain verb, its call having ended with `code`; the status of a
/// failure.
fn too_old(code: i32, console: &mut Console) -> u32 {
    console.err(&format!(
        "gate: drain: the builder's gate box does not know drain (status {code}); run gate push"
    ));
    u32::from(FAILED_STATUS)
}

/// `gate drain --status`: one status call, said.
fn status(to: &To<'_>, console: &mut Console) -> u32 {
    match to.ask(DrainAction::Status) {
        Ok(status) => {
            for line in status_lines(&status) {
                console.err(&line);
            }
            0
        }
        Err(why) => unanswered(why, console),
    }
}

/// `gate drain --lift`: a status call, then a lift only when a barrier stands, so a lift of none says so.
fn lift(to: &To<'_>, console: &mut Console) -> u32 {
    let before = match to.ask(DrainAction::Status) {
        Ok(before) => before,
        Err(why) => return unanswered(why, console),
    };
    if !matches!(before.barrier, Barrier::Standing { .. }) {
        console.err("gate: drain: no barrier stands on the builder; nothing to lift");
        return 0;
    }
    match to.ask(DrainAction::Lift) {
        Ok(after) => {
            console.err(&format!(
                "gate: drain: barrier lifted; {} waiting run(s) start in arrival order",
                after.tickets
            ));
            0
        }
        Err(why) => unanswered(why, console),
    }
}

/// `gate drain`: raises the barrier, says so, and waits until the box says the builder is drained.
fn raise(to: &To<'_>, seams: &DrainSeams, console: &mut Console) -> u32 {
    let start = Instant::now();
    if seams.interrupt_before_raise
        && let Some(interrupt) = to.interrupt
    {
        interrupt.raise();
    }
    let first = match to.ask(DrainAction::Raise) {
        Ok(first) => first,
        Err(Unanswered::Interrupted) => {
            console.err(INTERRUPTED_BEFORE);
            return CONTROL_C_EXIT;
        }
        Err(why) => return unanswered(why, console),
    };
    let Barrier::Standing { raised, lapses } = first.barrier else {
        console.err(GONE);
        return u32::from(FAILED_STATUS);
    };
    console.err(&format!(
        "gate: drain: barrier raised on the builder; it lapses at {} UTC unless gate drain runs again",
        utc_clock(lapses)
    ));
    let mut wait = Wait::new(start, seams, raised, lapses);
    match wait.run(to, first, console) {
        Ended::Drained(lapses) => {
            console.err(&format!(
                "gate: drain: the builder is drained; gate drain --lift lifts it (it lapses at {} UTC; gate drain re-raises it)",
                utc_clock(lapses)
            ));
            0
        }
        Ended::Gone => {
            console.err(GONE);
            u32::from(FAILED_STATUS)
        }
        Ended::Lapsed(lapsed) => {
            console.err(&format!(
                "gate: drain: the builder has not answered and the barrier lapsed at {} UTC; gate drain raises it again",
                utc_clock(lapsed)
            ));
            u32::from(FAILED_STATUS)
        }
        Ended::TooOld(code) => too_old(code, console),
        Ended::Interrupted => {
            console.err(INTERRUPTED);
            CONTROL_C_EXIT
        }
    }
}

/// How a wait ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ended {
    /// The box said the builder is drained under the barrier that lapses at this Unix second.
    Drained(i64),
    /// The box said no barrier stands.
    Gone,
    /// The builder gave no answer until the barrier it last said, lapsing at this Unix second by its clock, lapsed.
    Lapsed(i64),
    /// The builder's gate box does not take the drain verb.
    TooOld(i32),
    /// Ctrl+C ended the wait.
    Interrupted,
}

/// A wait for the builder to drain: its timing, what it has said, and what the box last said.
struct Wait {
    start: Instant,
    poll: Duration,
    raise_every: Duration,
    heartbeat: Heartbeat,
    interrupt_after_list: bool,
    /// The items of the list said last, each [`Item::unaged`]; `None` before the first.
    listed: Option<Vec<Item>>,
    /// How many things the last list counted.
    count: usize,
    /// Each reason the builder gave no answer that was said.
    said: BTreeSet<String>,
    /// The `(raised, lapses)` Unix seconds, by the builder's clock, of the last standing barrier the box said, and when
    /// it lapses by this PC's own monotonic clock: as long after the first answer that said that pair as its lapse is
    /// after its raise.
    barrier: (i64, i64),
    deadline: Instant,
    /// When the last raise the box answered with a standing barrier was made.
    raised_at: Instant,
}

impl Wait {
    fn new(start: Instant, seams: &DrainSeams, raised: i64, lapses: i64) -> Wait {
        Wait {
            start,
            poll: seams.poll_ms.map_or(POLL, Duration::from_millis),
            raise_every: seams.raise_ms.map_or(RAISE, Duration::from_millis),
            heartbeat: Heartbeat::new(seams.heartbeat_ms.map_or(HEARTBEAT, Duration::from_millis)),
            interrupt_after_list: seams.interrupt_after_list,
            listed: None,
            count: 0,
            said: BTreeSet::new(),
            barrier: (raised, lapses),
            deadline: lapse_deadline(raised, lapses, Instant::now()),
            raised_at: start,
        }
    }

    /// Takes in `first`, the raise's answer, then asks the box every poll, raising the barrier again every
    /// re-raise period, until the wait ends.
    fn run(&mut self, to: &To<'_>, first: Status, console: &mut Console) -> Ended {
        let mut answer = Ok(first);
        loop {
            if let Some(ended) = self.took(answer, to.interrupt, console) {
                return ended;
            }
            if let Some(waited) = self.heartbeat.due(self.start.elapsed()) {
                console.err(&still_waiting(self.count, waited));
            }
            if to.interrupted() {
                return Ended::Interrupted;
            }
            thread::sleep(self.poll.min(self.heartbeat.until_due(self.start.elapsed())));
            if to.interrupted() {
                return Ended::Interrupted;
            }
            answer = self.asked(to);
        }
    }

    /// Takes in what the last call gave, `answer`: the end of the wait, or `None` to wait on.
    fn took(&mut self, answer: Result<Status, Unanswered>, interrupt: Option<Interrupt>, console: &mut Console) -> Option<Ended> {
        match answer {
            Ok(status) => self.answered(&status, Instant::now(), interrupt, console),
            Err(Unanswered::Unreachable(why) | Unanswered::Said(why)) => self.unanswered(why, console),
            Err(Unanswered::TooOld(code)) => Some(Ended::TooOld(code)),
            Err(Unanswered::Interrupted) => Some(Ended::Interrupted),
        }
    }

    /// The answer of the next call: a `raise` once the re-raise period has passed since the last raise that the box
    /// answered with a standing barrier, else a `status`, so a raise that failed is made again at the next poll.
    fn asked(&mut self, to: &To<'_>) -> Result<Status, Unanswered> {
        let raising = self.raised_at.elapsed() >= self.raise_every;
        let action = if raising { DrainAction::Raise } else { DrainAction::Status };
        let answer = to.ask(action);
        if raising
            && let Ok(Status {
                barrier: Barrier::Standing { .. },
                ..
            }) = &answer
        {
            self.raised_at = Instant::now();
        }
        answer
    }

    /// Takes in the box's answer `status`, given at `now`: the end of the wait when it says drained or no barrier,
    /// else the list, said when an item came, went or changed in anything but its age, and the console `interrupt`
    /// raised after the first under the seam. A barrier the wait has not seen yet moves the deadline.
    fn answered(&mut self, status: &Status, now: Instant, interrupt: Option<Interrupt>, console: &mut Console) -> Option<Ended> {
        let Barrier::Standing { raised, lapses } = status.barrier else {
            return Some(Ended::Gone);
        };
        if self.barrier != (raised, lapses) {
            self.barrier = (raised, lapses);
            self.deadline = lapse_deadline(raised, lapses, now);
        }
        match status.state {
            State::Drained => return Some(Ended::Drained(lapses)),
            State::Open => return Some(Ended::Gone),
            State::Waiting => {}
        }
        self.count = status.items.len();
        let unaged: Vec<Item> = status.items.iter().map(Item::unaged).collect();
        if self.listed.as_ref() == Some(&unaged) {
            return None;
        }
        for line in list_lines(&status.items) {
            console.err(&line);
        }
        let first = self.listed.replace(unaged).is_none();
        if first
            && self.interrupt_after_list
            && let Some(interrupt) = interrupt
        {
            interrupt.raise();
        }
        None
    }

    /// Takes in a call that gave no answer for `why`: `why` said the first time it is given, then the end of the wait
    /// once the last barrier the box said has lapsed by this PC's clock.
    fn unanswered(&mut self, why: String, console: &mut Console) -> Option<Ended> {
        let line = format!("gate: drain: the builder did not answer: {why}; still trying");
        if self.said.insert(why) {
            console.err(&line);
        }
        (Instant::now() > self.deadline).then_some(Ended::Lapsed(self.barrier.1))
    }
}

/// When a barrier the box said, raised at the Unix second `raised` and lapsing at `lapses` by its clock, lapses by this
/// PC's monotonic clock, said first at `now`: as long after `now` as its lapse is after its raise; `now` for one that
/// lapses before its raise.
fn lapse_deadline(raised: i64, lapses: i64, now: Instant) -> Instant {
    let lasts = u64::try_from(lapses.saturating_sub(raised)).unwrap_or(0);
    now + Duration::from_secs(lasts)
}

/// The still-waiting line, `waited` into the wait, with `count` things in the last list.
fn still_waiting(count: usize, waited: Duration) -> String {
    let seconds = waited.as_secs();
    if count == 0 {
        format!("gate: drain: still waiting for the builder to finish what it has started ({seconds} s)")
    } else {
        format!("gate: drain: still waiting for {count} run(s) ({seconds} s)")
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use gate_args::DrainSeams;

    use super::{Barrier, Item, State, Status, Unanswered, Wait, classified, list_lines, status_lines, status_of, still_waiting, unnamed};
    use crate::console::Console;

    fn status(lines: &[&str]) -> Option<Status> {
        status_of(lines.iter().copied())
    }

    /// A waiting answer under a standing barrier raised at `raised` and lapsing 1800 s later, that lists one live
    /// session of `age` seconds, and `more`.
    fn waiting_under(raised: i64, age: u64, more: &[Item]) -> Status {
        let mut items = vec![Item::Session {
            age,
            pc: Some("pc1".to_owned()),
            repo: Some("app".to_owned()),
            command: Some("cargo test".to_owned()),
        }];
        items.extend_from_slice(more);
        Status {
            barrier: Barrier::Standing {
                raised,
                lapses: raised + 1800,
            },
            items,
            tickets: 0,
            state: State::Waiting,
        }
    }

    /// [`waiting_under`] the barrier raised at 0.
    fn waiting(age: u64, more: &[Item]) -> Status {
        waiting_under(0, age, more)
    }

    #[test]
    fn a_list_that_changed_only_in_a_session_s_age_is_not_said_again() {
        let mut wait = Wait::new(Instant::now(), &DrainSeams::default(), 0, 1800);
        let mut console = Console::new();
        let now = Instant::now();

        assert_eq!(wait.answered(&waiting(5, &[]), now, None, &mut console), None);
        assert_eq!(wait.answered(&waiting(7, &[]), now, None, &mut console), None);
        let process = Item::Process {
            pid: 9,
            kind: "git".to_owned(),
        };
        assert_eq!(wait.answered(&waiting(9, &[process]), now, None, &mut console), None);

        assert_eq!(
            console.take_notes(),
            [
                "gate: drain: waiting for 1 run(s) on the builder:",
                "gate:   pc1  app  5 s  cargo test",
                "gate: drain: waiting for 2 run(s) on the builder:",
                "gate:   pc1  app  9 s  cargo test",
                "gate:   process 9 (git)",
            ],
            "the list is said again only when an item came"
        );
    }

    #[test]
    fn the_deadline_moves_only_for_a_barrier_the_wait_has_not_seen() {
        let mut wait = Wait::new(Instant::now(), &DrainSeams::default(), 0, 1800);
        let mut console = Console::new();
        let first = Instant::now();
        let much_later = first + Duration::from_hours(1);

        wait.answered(&waiting_under(100, 5, &[]), first, None, &mut console);
        let anchored = wait.deadline;
        wait.answered(&waiting_under(100, 6, &[]), much_later, None, &mut console);

        assert_eq!(anchored, first + Duration::from_mins(30), "a new raise is anchored to its first answer");
        assert_eq!(wait.deadline, anchored, "the same raise said again leaves the deadline");
        wait.answered(&waiting_under(200, 7, &[]), much_later, None, &mut console);
        assert_eq!(wait.deadline, much_later + Duration::from_mins(30), "the next raise moves it");
    }

    #[test]
    fn a_host_is_masked_only_as_a_whole_word() {
        assert_eq!(
            unnamed("ssh: connect to host builder-1 port 22: Connection refused", "builder-1"),
            "ssh: connect to host the builder port 22: Connection refused"
        );
        assert_eq!(unnamed("ssh: mac (MAC) runs macOS", "mac"), "ssh: the builder (the builder) runs macOS");
        assert_eq!(
            unnamed("ssh: Could not resolve hostname Builder.lan", "me@builder.lan"),
            "ssh: Could not resolve hostname the builder"
        );
        assert_eq!(unnamed("ssh: builder-10 refused", "builder-1"), "ssh: builder-10 refused");
    }

    #[test]
    fn a_failed_call_is_interrupted_after_ctrl_c_and_masked_only_where_ssh_said_it() {
        let refused = || Err(Unanswered::Said("ssh: connect to host builder port 22: Connection refused".to_owned()));
        let own = || Err(Unanswered::Unreachable("the builder's answer says no drain state".to_owned()));
        assert_eq!(classified(refused(), true, "builder"), Err(Unanswered::Interrupted));
        assert_eq!(classified(own(), true, "builder"), Err(Unanswered::Interrupted));
        assert_eq!(classified(Err(Unanswered::TooOld(2)), true, "builder"), Err(Unanswered::Interrupted));
        let answered = waiting(5, &[]);
        assert_eq!(classified(Ok(answered.clone()), true, "builder"), Ok(answered));
        assert_eq!(
            classified(refused(), false, "builder"),
            Err(Unanswered::Unreachable(
                "ssh: connect to host the builder port 22: Connection refused".to_owned()
            ))
        );
        assert_eq!(classified(own(), false, "builder"), own(), "the gate's own words are never masked");
        assert_eq!(
            classified(Err(Unanswered::Said("ssh: macOS said no".to_owned())), false, "mac"),
            Err(Unanswered::Unreachable("ssh: macOS said no".to_owned()))
        );
    }

    #[test]
    fn the_still_waiting_line_counts_runs_or_says_what_the_builder_has_started() {
        let waited = Duration::from_secs(31);
        assert_eq!(still_waiting(2, waited), "gate: drain: still waiting for 2 run(s) (31 s)");
        assert_eq!(
            still_waiting(0, waited),
            "gate: drain: still waiting for the builder to finish what it has started (31 s)"
        );
    }

    #[test]
    fn the_box_s_status_lines_are_read_stamped_or_not() {
        let said = status(&[
            "2026-10-25T00:00:00.000000+00:00 out gate: drain barrier 100 1900",
            r#"gate: drain session 12 {"Pc":"pc1","Repo":null,"Command":"cargo test"}"#,
            "gate: drain process 42 git",
            "gate: drain command k-1 unreadable",
            "gate: drain record s9.json unreadable",
            "gate: drain processes unknown: ps failed",
            "gate: drain tickets 3",
            "gate: drain state waiting",
        ]);
        assert_eq!(
            said,
            Some(Status {
                barrier: Barrier::Standing { raised: 100, lapses: 1900 },
                items: vec![
                    Item::Session {
                        age: 12,
                        pc: Some("pc1".to_owned()),
                        repo: None,
                        command: Some("cargo test".to_owned()),
                    },
                    Item::Process {
                        pid: 42,
                        kind: "git".to_owned()
                    },
                    Item::Unreadable("k-1".to_owned()),
                    Item::Unreadable("s9.json".to_owned()),
                    Item::Unknown("ps failed".to_owned()),
                ],
                tickets: 3,
                state: State::Waiting,
            })
        );
    }

    #[test]
    fn an_answer_with_no_barrier_or_no_state_is_no_status() {
        assert_eq!(status(&["gate: drain barrier none", "gate: drain tickets 0"]), None);
        assert_eq!(status(&["gate: drain tickets 0", "gate: drain state open"]), None);
        let lapsed = status(&["gate: drain barrier lapsed 1900", "gate: drain state open"]).map(|said| said.barrier);
        assert_eq!(lapsed, Some(Barrier::Lapsed(1900)));
        assert_eq!(
            status(&["gate: drain barrier 1 x", "gate: drain state open"]),
            None,
            "a malformed barrier line is no barrier"
        );
    }

    #[test]
    fn the_list_counts_every_item_and_escapes_what_the_builder_sent() {
        let items = [
            Item::Session {
                age: 5,
                pc: None,
                repo: Some("app".to_owned()),
                command: Some("evil\u{1b}[2K".to_owned()),
            },
            Item::Process {
                pid: 7,
                kind: "job".to_owned(),
            },
            Item::Unreadable("k".to_owned()),
            Item::Unknown("no ps".to_owned()),
        ];
        assert_eq!(
            list_lines(&items),
            [
                "gate: drain: waiting for 4 run(s) on the builder:",
                r"gate:   ?  app  5 s  evil\u{1b}[2K",
                "gate:   process 7 (job)",
                "gate:   a run whose record cannot be read (k)",
                "gate:   the builder cannot list its processes: no ps",
            ]
        );
        assert_eq!(list_lines(&[]), ["gate: drain: waiting for the builder to finish what it has started"]);
    }

    #[test]
    fn a_status_lists_only_under_a_standing_barrier_with_something_going() {
        let drained = Status {
            barrier: Barrier::Standing { raised: 0, lapses: 1800 },
            items: Vec::new(),
            tickets: 0,
            state: State::Drained,
        };
        assert_eq!(
            status_lines(&drained),
            [
                "gate: drain: a barrier stands on the builder; it lapses at 00:30 UTC",
                "gate: drain: 0 waiting ticket(s)",
                "gate: drain: state drained",
            ]
        );
        let lapsed = Status {
            barrier: Barrier::Lapsed(60),
            items: Vec::new(),
            tickets: 2,
            state: State::Open,
        };
        assert_eq!(
            status_lines(&lapsed),
            [
                "gate: drain: the last barrier lapsed at 00:01 UTC without a lift",
                "gate: drain: 2 waiting ticket(s)",
                "gate: drain: state open",
            ]
        );
    }
}
