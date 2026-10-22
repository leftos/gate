//! The words `gate-box` is called with: `--version`, or the hidden `box <verb> --stdin` the PC's client sends, read
//! by `gate-args` as `gate` reads them; then the verb's answer and the status the process exits with.

use std::ffi::OsString;

use gate_args::{BoxRequest, BoxVerb, Invocation, Problem};

/// What the words ask for.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// `box <verb> --stdin`, or a verb that reads no request (`prune`, with its root and keys), with the hidden test
    /// seams given before the verb (all defaults in a box built without the `test-seams` feature).
    Verb(Box<BoxRequest>),
    /// `--version`: say `gate <version>`, as `gate --version` does.
    Version,
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
        Ok(Invocation::Box(request)) => Action::Verb(Box::new(request)),
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
        BoxVerb::Preflight | BoxVerb::Boot | BoxVerb::Stop | BoxVerb::Run | BoxVerb::Prune => None,
        BoxVerb::Outputs | BoxVerb::Inflight | BoxVerb::Inputs | BoxVerb::Lfs | BoxVerb::RegisterDesktop => {
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

/// Answers `action` through `console`, reading a verb's request from stdin, but `prune`'s, which reads none; the
/// status.
#[cfg(unix)]
fn answer(action: Action, console: &mut crate::console::Console) -> u8 {
    let request = match action {
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
        Action::Verb(request) => *request,
    };
    let verb = request.verb;
    if let Some(line) = unbuilt(verb) {
        console.err(&line);
        return crate::lines::USAGE_STATUS;
    }
    if verb == BoxVerb::Prune {
        // A verb with no request never reads stdin.
        let Some(root) = request.root.as_deref() else {
            // The parse gives `prune` its root always; a request without one is the box's usage error.
            console.err(&Problem::BoxUsage.to_string());
            return crate::lines::USAGE_STATUS;
        };
        return crate::prune::prune(root, &request.keys, std::time::SystemTime::now(), console);
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
        BoxVerb::Run => {
            // The run says its lines through an output its threads share; the console says nothing more.
            let output = std::sync::Arc::new(crate::console::Output::new(crate::console::StdioSink));
            crate::run::run(&payload, &output, crate::run::Watches::of(&request.seams))
        }
        // Every other verb was refused as unbuilt above.
        _ => crate::lines::USAGE_STATUS,
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use gate_args::{BoxRequest, BoxSeams, BoxVerb, Problem};

    use super::{Action, action, unbuilt, version_text};

    fn words(text: &str) -> Vec<OsString> {
        text.split(' ').map(OsString::from).collect()
    }

    /// The action of the verb `verb` given `seams`, the prune's `root` and `keys`.
    fn verb_action(verb: BoxVerb, seams: BoxSeams, root: Option<&str>, keys: &[&str]) -> Action {
        Action::Verb(Box::new(BoxRequest {
            verb,
            seams,
            root: root.map(PathBuf::from),
            keys: keys.iter().map(|key| (*key).to_owned()).collect(),
        }))
    }

    #[test]
    fn version_says_the_gate_s_version_text() {
        assert_eq!(action(words("--version")), Action::Version);
        assert_eq!(version_text(), format!("gate {}", gate_wire::build_version()));
    }

    #[test]
    fn a_box_verb_with_stdin_is_that_verb() {
        let verb = |verb| verb_action(verb, BoxSeams::default(), None, &[]);
        assert_eq!(action(words("box preflight --stdin")), verb(BoxVerb::Preflight));
        assert_eq!(action(words("box boot --stdin")), verb(BoxVerb::Boot));
        assert_eq!(action(words("box stop --stdin")), verb(BoxVerb::Stop));
        assert_eq!(action(words("box run --stdin")), verb(BoxVerb::Run));
    }

    #[cfg(feature = "test-seams")]
    #[test]
    fn the_seams_given_before_the_verb_reach_the_verb() {
        let seams = BoxSeams {
            heartbeat_ms: Some(50),
            ..BoxSeams::default()
        };
        assert_eq!(
            action(words("box --test-heartbeat-ms 50 run --stdin")),
            verb_action(BoxVerb::Run, seams, None, &[])
        );
    }

    #[test]
    fn a_prune_with_a_root_is_that_verb_and_reads_no_request() {
        assert_eq!(
            action(words("box prune --root /r")),
            verb_action(BoxVerb::Prune, BoxSeams::default(), Some("/r"), &[])
        );
        // Answered by the prune with no request read from stdin, which the test runner leaves empty: an answer that read
        // one would say the request's usage line. A relative root is refused before the prune opens anything, so no
        // sweep runs.
        #[cfg(unix)]
        {
            let mut console = crate::console::Console::captured();
            let relative = verb_action(BoxVerb::Prune, BoxSeams::default(), Some("gate-remote"), &[]);
            let status = super::answer(relative, &mut console);
            assert_eq!(status, crate::lines::USAGE_STATUS);
            assert_eq!(console.err, [r#"gate: box prune --root is not an absolute path: "gate-remote""#]);
            assert!(console.out.is_empty(), "{:?}", console.out);
        }
    }

    #[test]
    fn a_prune_with_keys_carries_them() {
        assert_eq!(
            action(words("box prune --root /r --keys a B")),
            verb_action(BoxVerb::Prune, BoxSeams::default(), Some("/r"), &["a", "B"])
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
    fn the_built_verbs_are_answered_and_the_others_are_not_built_here() {
        for verb in [BoxVerb::Preflight, BoxVerb::Boot, BoxVerb::Stop, BoxVerb::Run, BoxVerb::Prune] {
            assert_eq!(unbuilt(verb), None, "{verb:?}");
        }
        for verb in [
            BoxVerb::Outputs,
            BoxVerb::Inputs,
            BoxVerb::Lfs,
            BoxVerb::Inflight,
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
