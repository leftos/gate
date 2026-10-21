//! The words `gate-box` is called with: `--version`, or the hidden `box <verb> --stdin` the PC's client sends, read
//! by `gate-args` as `gate` reads them; then the verb's answer and the status the process exits with.

use std::ffi::OsString;

use gate_args::{BoxSeams, BoxVerb, Invocation, Problem};

/// What the words ask for.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// `--version`: say `gate <version>`, as `gate --version` does.
    Version,
    /// `box <verb> --stdin`, or a verb that reads no request, with the hidden test seams given before the verb (all
    /// defaults in a box built without the `test-seams` feature).
    Verb(BoxVerb, Box<BoxSeams>),
    /// Words the box does not take: say these lines on stderr and exit [`crate::lines::USAGE_STATUS`].
    Usage(Vec<String>),
}

/// What `args`, the words after the program name, ask for. Any `gate` subcommand but the hidden `box` is the box usage.
#[must_use]
pub fn action(args: Vec<OsString>) -> Action {
    if !matches!(args.first().and_then(|word| word.to_str()), Some("box" | "--version")) {
        return Action::Usage(vec![Problem::BoxUsage.to_string()]);
    }
    match gate_args::parse(args) {
        Ok(Invocation::Version) => Action::Version,
        Ok(Invocation::Box(request)) => Action::Verb(request.verb, Box::new(request.seams)),
        Ok(_) => Action::Usage(vec![Problem::BoxUsage.to_string()]),
        Err(problems) if problems.is_empty() => Action::Usage(vec![Problem::BoxUsage.to_string()]),
        Err(problems) => Action::Usage(problems.iter().map(ToString::to_string).collect()),
    }
}

/// The line refusing a verb this builder has not built, which exits [`crate::lines::USAGE_STATUS`] without reading stdin; `None` for
/// a verb it answers.
#[must_use]
pub fn unbuilt(verb: BoxVerb) -> Option<String> {
    match verb {
        BoxVerb::Preflight | BoxVerb::Boot | BoxVerb::Stop => None,
        BoxVerb::Run => Some("gate: box run is not built yet".to_owned()),
        BoxVerb::Outputs | BoxVerb::Inflight | BoxVerb::Inputs | BoxVerb::Lfs | BoxVerb::RegisterDesktop | BoxVerb::Prune => {
            Some(format!("gate: box {} is not built on this builder", verb.as_str()))
        }
    }
}

/// `gate <version>`, the text `gate --version` says.
#[must_use]
pub fn version_text() -> String {
    format!("gate {}", gate_wire::build_version())
}

/// Answers `args` on the process's stdout and stderr, reading the request from stdin; the status to exit with.
#[cfg(unix)]
#[must_use]
pub fn main(args: Vec<OsString>) -> std::process::ExitCode {
    // An ignored SIGCHLD, inherited from whatever started the box, has the kernel reap its children unasked, so the
    // runner could not see a program's exit (`waitid` fails with `ECHILD`) and its group's id could be another's.
    // SAFETY: `signal` with `SIG_DFL` installs no handler and touches no memory; no thread of the box runs yet.
    unsafe {
        libc::signal(libc::SIGCHLD, libc::SIG_DFL);
    }
    let mut console = crate::console::Console::stdio();
    std::process::ExitCode::from(answer(action(args), &mut console))
}

/// Answers `action` through `console`, reading a verb's request from stdin; the status.
#[cfg(unix)]
fn answer(action: Action, console: &mut crate::console::Console) -> u8 {
    let (verb, _seams) = match action {
        Action::Version => {
            console.out(&version_text());
            return 0;
        }
        Action::Usage(lines) => {
            for line in &lines {
                console.err(line);
            }
            return crate::lines::USAGE_STATUS;
        }
        Action::Verb(verb, seams) => (verb, seams),
    };
    if let Some(line) = unbuilt(verb) {
        console.err(&line);
        return crate::lines::USAGE_STATUS;
    }
    let payload = match crate::request::read_request(std::io::stdin().lock()) {
        Ok(payload) => payload,
        Err(line) => {
            console.err(&line);
            return crate::lines::USAGE_STATUS;
        }
    };
    match verb {
        BoxVerb::Preflight => crate::preflight::preflight(&payload, console),
        BoxVerb::Stop => crate::stop::stop(&payload, console),
        BoxVerb::Boot => crate::boot::status(crate::boot::say(console)),
        // Every other verb was refused as unbuilt above.
        _ => crate::lines::USAGE_STATUS,
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use gate_args::{BoxVerb, Problem};

    use super::{Action, action, unbuilt, version_text};

    fn words(text: &str) -> Vec<OsString> {
        text.split(' ').map(OsString::from).collect()
    }

    #[test]
    fn version_says_the_gate_s_version_text() {
        assert_eq!(action(words("--version")), Action::Version);
        assert_eq!(version_text(), format!("gate {}", gate_wire::build_version()));
    }

    #[test]
    fn a_box_verb_with_stdin_is_that_verb() {
        let verb = |verb| Action::Verb(verb, Box::default());
        assert_eq!(action(words("box preflight --stdin")), verb(BoxVerb::Preflight));
        assert_eq!(action(words("box boot --stdin")), verb(BoxVerb::Boot));
        assert_eq!(action(words("box stop --stdin")), verb(BoxVerb::Stop));
        assert_eq!(action(words("box run --stdin")), verb(BoxVerb::Run));
    }

    #[cfg(feature = "test-seams")]
    #[test]
    fn the_seams_given_before_the_verb_reach_the_verb() {
        use gate_args::BoxSeams;

        let seams = BoxSeams {
            heartbeat_ms: Some(50),
            ..BoxSeams::default()
        };
        assert_eq!(
            action(words("box --test-heartbeat-ms 50 run --stdin")),
            Action::Verb(BoxVerb::Run, Box::new(seams))
        );
    }

    #[test]
    fn any_other_words_are_the_box_usage() {
        let usage = Action::Usage(vec![Problem::BoxUsage.to_string()]);
        assert_eq!(action(words("box preflight")), usage, "no --stdin");
        assert_eq!(action(words("box nope --stdin")), usage);
        assert_eq!(action(Vec::new()), usage, "no words");
        assert_eq!(action(words("run -- x")), usage, "another gate subcommand");
    }

    #[test]
    fn run_is_not_built_yet_and_the_windows_only_verbs_are_not_built_here() {
        for verb in [BoxVerb::Preflight, BoxVerb::Boot, BoxVerb::Stop] {
            assert_eq!(unbuilt(verb), None, "{verb:?}");
        }
        assert_eq!(unbuilt(BoxVerb::Run).as_deref(), Some("gate: box run is not built yet"));
        for verb in [
            BoxVerb::Outputs,
            BoxVerb::Inputs,
            BoxVerb::Lfs,
            BoxVerb::Inflight,
            BoxVerb::Prune,
            BoxVerb::RegisterDesktop,
        ] {
            assert_eq!(
                unbuilt(verb),
                Some(format!("gate: box {} is not built on this builder", verb.as_str())),
                "{verb:?}"
            );
        }
    }
}
