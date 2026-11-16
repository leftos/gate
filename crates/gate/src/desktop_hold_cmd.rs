//! `gate desktop hold`: holds a builder's desktop lock for an agent that drives the builder's console desktop, so the
//! builder's locked desktop runs wait for it, naming its holder, or fail at their ceiling. It calls the box's `gate box
//! desktop-hold` over ssh in a kill-on-close job and keeps the call's standard input open: the box holds the lock until
//! that input ends. Ctrl+C closes it, and so does the end of this gate however it ends, through the job, which ends
//! ssh. The box's own lines are passed on as they come; the gate takes no slot and writes no journal line.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use gate_args::{BoxVerb, DesktopHoldRequest, DesktopHoldSeams};
use gate_router::{Builder, BuilderOs, Config, escaped};
use gate_win32::{Interrupt, PipedConsole, Running};
use gate_wire::stamp;

use crate::answer::{FAILED_STATUS, USAGE_STATUS};
use crate::console::Console;
use crate::remote::git;
use crate::remote::launch;
use crate::remote::transport::ssh::Ssh;
use crate::remote::transport::{Program, Transport, Wire};
use crate::remote::{SSH_FAILED, no_box, unnamed};
use crate::resolve::Environment;
use crate::route;
use crate::run::CONTROL_C_EXIT;

/// The command every line of the PC's own starts with.
const COMMAND: &str = "gate desktop hold";
/// What the box's held line starts with, on its standard output.
const HELD: &str = "gate: holding the builder's desktop as ";
/// What every line of the box's own starts with, once its stamp is taken off.
const BOX_LINE: &str = "gate:";
/// How often the relay looks at the console interrupt while it waits for the call's next line.
const LOOK: Duration = Duration::from_millis(50);
/// How long the call is given to end once Ctrl+C closed its input, before its job is ended.
const END_WAIT: Duration = Duration::from_secs(30);

/// Runs `gate desktop hold` and returns its exit status: the box's own (0 released, 124 at its ceiling, 75 for a desktop
/// that stayed busy, 1 for its failure), `CONTROL_C_EXIT` on Ctrl+C, 2 for a builder `remote.json` does not list or
/// a macOS builder, 1 for a builder that could not be reached or whose gate has no `desktop-hold`, and the box's own
/// status for a box that failed without a `gate:` line.
pub(crate) fn desktop_hold(request: &DesktopHoldRequest, environment: &Environment, console: &mut Console) -> u32 {
    let (builder, config) = match chosen(&request.builder, environment.local_app_data.as_deref()) {
        Ok(chosen) => chosen,
        Err((status, line)) => {
            console.err(&line);
            return u32::from(status);
        }
    };
    if matches!(builder.os, BuilderOs::Macos) {
        console.err(&format!(
            "{COMMAND}: {} is a macOS builder, which has no desktop",
            escaped(&builder.alias)
        ));
        return u32::from(USAGE_STATUS);
    }
    let transport = match transport(&request.seams, &config) {
        Ok(transport) => transport,
        Err(line) => {
            console.err(&line);
            return u32::from(FAILED_STATUS);
        }
    };
    let profile = transport.profile(&builder);
    // The holder's characters leave cmd nothing to read inside double quotes, and a Windows path holds no double quote.
    let line = format!(
        "{} --root \"{}\" --holder \"{}\" --ceiling {} --wait {}",
        profile.line(BoxVerb::DesktopHold),
        profile.works(),
        request.holder,
        request.ceiling,
        request.wait
    );
    let program = transport.call(&builder.alias, &line, Wire::Plain);
    let interrupt = Interrupt::install()
        .inspect_err(|error| {
            console.err(&format!(
                "gate: cannot install the Ctrl+C handler, so Ctrl+C ends the gate at once: {error}"
            ));
        })
        .ok();
    let raise = request.seams.interrupt_after_held.then_some(interrupt).flatten();
    match held(&program, interrupt, raise, console) {
        Ok(heard) => answered(&heard, &builder.alias, console),
        Err(why) => {
            console.err(&unreachable_line(&why));
            u32::from(FAILED_STATUS)
        }
    }
}

/// The builder `remote.json` under `local_app_data` lists as `named` ([`gate_router::Config::named_builder`]), and the
/// file read. Else the status and the line: 2 for a name it does not list, 1 for a routing file that is not there or
/// cannot be used.
fn chosen(named: &str, local_app_data: Option<&OsStr>) -> Result<(Builder, Config), (u8, String)> {
    let path = route::remote_json_path(local_app_data).ok_or_else(|| {
        (
            FAILED_STATUS,
            format!("{COMMAND}: no remote.json: LOCALAPPDATA is not an absolute folder"),
        )
    })?;
    let ignored = |reason: &str| (FAILED_STATUS, format!("{COMMAND}: ignored {}: {reason}", path.display()));
    let config = match route::read_remote_json(&path) {
        Ok(Some(config)) => config,
        Ok(None) => return Err((FAILED_STATUS, format!("{COMMAND}: no remote.json at {}", path.display()))),
        Err(problem) => return Err(ignored(&problem.reason)),
    };
    let builder = config
        .named_builder(named)
        .map_err(ignored)?
        .ok_or_else(|| (USAGE_STATUS, format!("{COMMAND}: {} is not a builder remote.json lists", escaped(named))))?;
    Ok((builder, config))
}

/// The way to the builder: the local transport `--test-remote-transport` names, else ssh ([`ssh`]).
#[cfg(feature = "test-seams")]
fn transport(seams: &DesktopHoldSeams, config: &Config) -> Result<Box<dyn Transport>, String> {
    match &seams.remote_transport {
        Some(named) => crate::remote::local_transport(named, seams.remote_ssh.as_deref()).map(|local| Box::new(local) as Box<dyn Transport>),
        None => ssh(config),
    }
}

/// The way to the builder: ssh ([`ssh`]).
#[cfg(not(feature = "test-seams"))]
fn transport(_seams: &DesktopHoldSeams, config: &Config) -> Result<Box<dyn Transport>, String> {
    ssh(config)
}

/// The ssh `config` names, else the one found along the gate's own `PATH`; else the line saying why it cannot be found.
fn ssh(config: &Config) -> Result<Box<dyn Transport>, String> {
    Ssh::find(config.ssh())
        .map(|ssh| Box::new(ssh) as Box<dyn Transport>)
        .map_err(|why| format!("{COMMAND}: {why}"))
}

/// The line saying the builder could not be reached, and `why`.
fn unreachable_line(why: &str) -> String {
    format!("{COMMAND}: the builder could not be reached: {why}")
}

/// Which of the call's streams a line came on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stream {
    Out,
    Err,
}

/// What a reader of one of the call's streams says.
enum Said {
    /// A line, its line end taken off.
    Line(Stream, String),
    /// The stream ended.
    Closed,
}

/// What the call said and how it ended.
#[derive(Default)]
struct Heard {
    /// Its exit status; `None` when it did not end within its wait or could not be waited for.
    status: Option<u32>,
    /// Every line of its standard output, as it came.
    out: Vec<String>,
    /// Every line of its standard error, as it came.
    err: Vec<String>,
    /// Whether Ctrl+C ended the hold.
    interrupted: bool,
}

/// Makes the call `program` in a kill-on-close job of its own, its standard input kept open until Ctrl+C (`interrupt`)
/// closes it, the box's lines passed on to `console` as they come, and `raise` raised once the box's held line has
/// come; what it said and how it ended, its job ended. Else why it could not start.
fn held(program: &Program, interrupt: Option<Interrupt>, raise: Option<Interrupt>, console: &mut Console) -> Result<Heard, String> {
    let shown = program.program.to_string_lossy().into_owned();
    let cannot = |error: String| format!("cannot run {}: {}", escaped(&shown), escaped(&error));
    let job = launch::call_job(false).map_err(|error| cannot(error.to_string()))?;
    let mut command = Command::new(&program.program);
    command.args(&program.args);
    // Hidden, so a Ctrl+C here reaches this gate alone, which closes the call's input; ssh in batch mode reads no
    // console.
    let launch = launch::piped_launch(&command, PipedConsole::Hidden).map_err(|error| cannot(error.to_string()))?;
    let (suspended, stdout, stderr, stdin) = launch::launched(&launch, &job, true).map_err(|error| cannot(error.to_string()))?;
    let running = suspended.assign_and_resume(&job).map_err(|error| cannot(error.to_string()))?;
    let (sender, lines) = mpsc::channel();
    read_lines(stdout, Stream::Out, sender.clone());
    read_lines(stderr, Stream::Err, sender);
    let mut call = Call {
        stdin,
        interrupt,
        raise,
        deadline: None,
        heard: Heard::default(),
    };
    call.relay(&lines, console);
    // A Ctrl+C that came as the call's streams ended is Ctrl+C all the same.
    call.heard.interrupted |= call.interrupt.is_some_and(Interrupt::is_raised);
    call.heard.status = call.ended(&running);
    // Ending the job ends ssh and anything it left, once the call has ended or its wait is over.
    drop(job);
    Ok(call.heard)
}

/// Reads `from`, the call's `stream`, on a thread of its own, sending each line to `to`, then that it ended.
fn read_lines(from: File, stream: Stream, to: Sender<Said>) {
    thread::spawn(move || {
        let mut reader = BufReader::new(from);
        let mut bytes = Vec::new();
        loop {
            bytes.clear();
            match reader.read_until(b'\n', &mut bytes) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&bytes).trim_end_matches(['\r', '\n']).to_owned();
                    if to.send(Said::Line(stream, line)).is_err() {
                        return;
                    }
                }
            }
        }
        // The relay may have stopped listening once its wait was over.
        let _ = to.send(Said::Closed);
    });
}

/// A call in flight: the write end of its standard input, open until Ctrl+C, the interrupts, the end of its wait
/// once its input is closed, and what it said so far.
struct Call {
    stdin: Option<File>,
    interrupt: Option<Interrupt>,
    raise: Option<Interrupt>,
    deadline: Option<Instant>,
    heard: Heard,
}

impl Call {
    /// Passes on the call's lines until both its streams end, closing its input once Ctrl+C is pressed and then
    /// giving it [`END_WAIT`] to end.
    fn relay(&mut self, lines: &Receiver<Said>, console: &mut Console) {
        let mut open = 2;
        while open > 0 {
            if self.interrupt.is_some_and(Interrupt::is_raised) && self.stdin.is_some() {
                // The box's input ends, which lets the builder's desktop go.
                self.stdin = None;
                self.deadline = Some(Instant::now() + END_WAIT);
                self.heard.interrupted = true;
            }
            if self.deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return;
            }
            match lines.recv_timeout(LOOK) {
                Ok(Said::Line(stream, line)) => self.heard_line(stream, line, console),
                Ok(Said::Closed) => open -= 1,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// Takes in `line` of `stream`: passed on, without its stamp, on the same stream when it is the box's own, and the
    /// raise made once the box's held line comes.
    fn heard_line(&mut self, stream: Stream, line: String, console: &mut Console) {
        let text = stamp::unstamped(&line).text;
        if text.starts_with(BOX_LINE) {
            match stream {
                Stream::Out => console.out(text),
                Stream::Err => console.err(text),
            }
        }
        if stream == Stream::Out
            && text.starts_with(HELD)
            && let Some(raise) = self.raise.take()
        {
            raise.raise();
        }
        match stream {
            Stream::Out => self.heard.out.push(line),
            Stream::Err => self.heard.err.push(line),
        }
    }

    /// The call's exit status, waited for until its deadline once Ctrl+C closed its input, else [`END_WAIT`] after its
    /// streams ended; `None` when it has not ended by then or cannot be waited for.
    fn ended(&self, running: &Running) -> Option<u32> {
        let wait = self
            .deadline
            .map_or(END_WAIT, |deadline| deadline.saturating_duration_since(Instant::now()));
        running.wait(wait).ok().flatten()
    }
}

/// The exit status of a hold the call `heard` came to, its line said: `CONTROL_C_EXIT` after Ctrl+C; the box's own
/// status, its lines already passed on, after the line saying the builder `alias` could not be reached when it failed
/// with no line of its own; else 1 after that line for ssh's own failure, or after the line saying its gate has no
/// `desktop-hold`.
fn answered(heard: &Heard, alias: &str, console: &mut Console) -> u32 {
    if heard.interrupted {
        return CONTROL_C_EXIT;
    }
    let Some(status) = heard.status else {
        console.err(&unreachable_line("the call did not end"));
        return u32::from(FAILED_STATUS);
    };
    let code = status.cast_signed();
    let errors = || heard.err.iter().map(|line| stamp::unstamped(line).text);
    if code == SSH_FAILED {
        let why = said_why(heard, alias).unwrap_or_else(|| format!("ssh exited {SSH_FAILED} with no message"));
        console.err(&unreachable_line(&why));
        return u32::from(FAILED_STATUS);
    }
    if code != 0 && no_box(code, &heard.out, errors()) {
        console.err(&format!("{COMMAND}: {} runs a gate without desktop-hold; run gate push", escaped(alias)));
        return u32::from(FAILED_STATUS);
    }
    let box_spoke = heard
        .out
        .iter()
        .chain(&heard.err)
        .any(|line| stamp::unstamped(line).text.starts_with(BOX_LINE));
    if code != 0 && !box_spoke {
        let why = said_why(heard, alias).unwrap_or_else(|| format!("the call exited {code} with no message"));
        console.err(&unreachable_line(&why));
    }
    status
}

/// The last line the call said on its standard error that is not the box's own, escaped and with the builder `alias`
/// masked ([`unnamed`]); `None` when it said none.
fn said_why(heard: &Heard, alias: &str) -> Option<String> {
    heard
        .err
        .iter()
        .rev()
        .map(|line| stamp::unstamped(line).text)
        .find(|line| !line.trim().is_empty() && !line.starts_with(BOX_LINE))
        .map(|line| unnamed(&escaped(&git::failure_text(&[line])), alias))
}
