//! `gate-box box run` end to end: the built binary, given the heartbeat and sample seams, a payload on stdin as the PC's
//! client sends it and a local bare repository, run to its end. Every wait is on the program or on a line it says,
//! never on the clock.

#[cfg(unix)]
#[expect(
    clippy::expect_used,
    reason = "a case whose folder, repository or process cannot be made must fail loudly"
)]
#[expect(
    clippy::disallowed_methods,
    reason = "the spawn lock is the Windows gate's; this starts the Unix box and sets a case's repository up"
)]
mod unix {
    use std::fs::{self, OpenOptions};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use gate_wire::{BoxKill, Refusal, adjusted_prefix, busy_line, killed_line, refused_line, started_line};
    use serde_json::{Value, json};

    const KEY: &str = "repo-0123abcd";
    const NONCE: &str = "0123456789abcdef0123456789abcdef";
    const SAMPLER_FAILED: &str = "gate: sampler failed: ";

    /// Runs git with `args`; its standard output, trimmed, and whether it exited 0.
    fn git_said(args: &[&str]) -> (String, bool) {
        let ran = Command::new("git")
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .expect("git starts");
        (String::from_utf8_lossy(&ran.stdout).trim().to_owned(), ran.status.success())
    }

    /// Runs git with `args`; its standard output, trimmed. Fails the case unless git exits 0.
    fn git(args: &[&str]) -> String {
        let (said, passed) = git_said(args);
        assert!(passed, "git {args:?}");
        said
    }

    /// A work root `<base>/gate-remote` private to this process, with its `<root>/work` and an empty bare repository,
    /// removed when the case ends.
    struct Case {
        base: PathBuf,
    }

    impl Case {
        fn new(name: &str) -> Case {
            let base = std::env::temp_dir().join(format!("gate-box-unix-bb-{}-{name}", std::process::id()));
            if base.exists() {
                fs::remove_dir_all(&base).expect("a stale case folder is removed");
            }
            let case = Case { base };
            fs::create_dir_all(format!("{}/work", case.root())).expect("the work folders' parent is made");
            git(&["init", "--bare", "-q", &case.bare()]);
            case
        }

        fn root(&self) -> String {
            self.base.join("gate-remote").to_str().expect("the temp folder is UTF-8").to_owned()
        }

        fn work(&self) -> String {
            format!("{}/work/{KEY}", self.root())
        }

        fn bare(&self) -> String {
            format!("{}/repo.git", self.root())
        }

        fn lock(&self) -> String {
            format!("{}.lock", self.work())
        }

        /// A commit holding `a.txt`, pushed to the bare repository as `refs/gate/<key>`, the back ref pointed at it as
        /// an earlier run's; its sha.
        fn snapshot(&self) -> String {
            let source = self.base.join("source");
            let source = source.to_str().expect("the temp folder is UTF-8");
            git(&["init", "-q", source]);
            fs::write(Path::new(source).join("a.txt"), "a\n").expect("the file is written");
            git(&["-C", source, "add", "-A"]);
            git(&[
                "-C",
                source,
                "-c",
                "user.name=gate",
                "-c",
                "user.email=gate@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "snapshot",
            ]);
            git(&["-C", source, "push", "-q", &self.bare(), &format!("HEAD:refs/gate/{KEY}")]);
            let commit = git(&["-C", source, "rev-parse", "HEAD"]);
            git(&[&self.bare_dir(), "update-ref", &format!("refs/gate-back/{KEY}"), &commit]);
            commit
        }

        fn bare_dir(&self) -> String {
            format!("--git-dir={}", self.bare())
        }

        /// The back ref's commit, `None` when there is none.
        fn back(&self) -> Option<String> {
            let (said, found) = git_said(&[&self.bare_dir(), "rev-parse", "--verify", "-q", &format!("refs/gate-back/{KEY}")]);
            found.then_some(said)
        }

        /// The request word of a run checking out `snapshot`, with `extra` keys set over it, as the PC's client sends
        /// it: base64 of the JSON payload.
        fn request(&self, snapshot: &str, extra: &Value) -> String {
            let (root, work) = (self.root(), self.work());
            let mut payload = json!({
                "Key": KEY,
                "Bare": self.bare(),
                "Work": work,
                "Lock": format!("{work}.lock"),
                "Index": format!("{work}.index"),
                "OutputFolder": format!("{root}/outputs"),
                "Job": format!("gate-remote-{KEY}-0123456789ab"),
                "Nonce": NONCE,
                "Host": "mac-test",
                "Snapshot": snapshot,
                "Identity": {
                    "GIT_AUTHOR_NAME": "Box Author",
                    "GIT_AUTHOR_EMAIL": "author@example.com",
                    "GIT_COMMITTER_NAME": "Box Committer",
                    "GIT_COMMITTER_EMAIL": "committer@example.com",
                },
                "Command": ["true"],
            });
            if let (Some(object), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
                object.extend(extra.clone());
            }
            STANDARD.encode(payload.to_string())
        }

        /// Whether the work lock can be taken now, so no run holds it.
        fn lock_free(&self) -> bool {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(self.lock())
                .expect("the lock file opens");
            // SAFETY: `file` is open for the call; a lock taken is let go when it closes.
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
        }

        /// The run left no interrupted-command record nor group record, and let its work lock go.
        fn ended_clean(&self) {
            assert!(!Path::new(&format!("{}.command", self.work())).exists(), "no .command left");
            assert!(!Path::new(&format!("{}.pgid", self.work())).exists(), "no .pgid left");
            assert!(self.lock_free(), "the work lock is free");
        }
    }

    impl Drop for Case {
        fn drop(&mut self) {
            // Best effort: a folder left behind is in the temp folder and named for this process.
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    /// How a case reads the box's output.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Reading {
        /// Every line, to the end.
        All,
        /// Until the start marker and a heartbeat after it, then both pipes closed: a client gone.
        CloseAfterStart,
    }

    /// What a run said and how it ended.
    struct Ended {
        status: Option<i32>,
        out: Vec<String>,
        err: Vec<String>,
    }

    impl Ended {
        fn err_has(&self, wanted: &str) -> bool {
            self.err.iter().any(|line| line == wanted)
        }

        fn adjusted(&self) -> bool {
            let prefix = adjusted_prefix(NONCE);
            self.err.iter().any(|line| line.starts_with(&prefix))
        }
    }

    /// Starts `gate-box box --test-heartbeat-ms 50 --test-box-sample-ms 50 <seams> run --stdin`, writes `request` on
    /// its stdin, and reads it as `reading` says until it ends. Once the start marker and then a heartbeat were read,
    /// `beaten` is made, which a command can wait on.
    fn ran(seams: &[&str], request: &str, reading: Reading, beaten: &Path) -> Ended {
        let mut words = vec!["box", "--test-heartbeat-ms", "50", "--test-box-sample-ms", "50"];
        words.extend(seams);
        words.extend(["run", "--stdin"]);
        let mut child: Child = Command::new(env!("CARGO_BIN_EXE_gate-box"))
            .args(&words)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("gate-box starts");
        let mut stdin = child.stdin.take().expect("stdin is piped");
        stdin.write_all(format!("{request}\n").as_bytes()).expect("the request is written");
        drop(stdin);
        let started = Arc::new(AtomicBool::new(false));
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let out_started = Arc::clone(&started);
        let out = thread::spawn(move || lines_of(stdout, reading, |line| line == started_line(NONCE), &out_started));
        let err_started = Arc::clone(&started);
        let beaten = beaten.to_owned();
        let err = thread::spawn(move || {
            let mut said = Vec::new();
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|read| read > 0) {
                let text = line.trim_end_matches('\n').to_owned();
                line.clear();
                let beat = text.starts_with("gate-remote-heartbeat:") && err_started.load(Ordering::SeqCst);
                said.push(text);
                if beat && !beaten.exists() {
                    fs::write(&beaten, "").expect("the beaten marker is made");
                    if reading == Reading::CloseAfterStart {
                        break;
                    }
                }
            }
            said
        });
        let out = out.join().expect("the stdout reader ends");
        let err = err.join().expect("the stderr reader ends");
        let status = child.wait().expect("gate-box is waited for").code();
        Ended { status, out, err }
    }

    /// The lines of `pipe`, read to its end, or for [`Reading::CloseAfterStart`] up to the line `marks` takes for the
    /// start marker, which sets `started`.
    fn lines_of(pipe: impl Read, reading: Reading, marks: impl Fn(&str) -> bool, started: &AtomicBool) -> Vec<String> {
        let mut said = Vec::new();
        for line in BufReader::new(pipe).lines() {
            let Ok(line) = line else { break };
            let start = marks(&line);
            said.push(line);
            if start {
                started.store(true, Ordering::SeqCst);
                if reading == Reading::CloseAfterStart {
                    break;
                }
            }
        }
        said
    }

    /// A command that writes `file` and waits until `beaten` is made, so a heartbeat is read while it runs.
    fn waits_for_a_beat(script: &str, beaten: &Path) -> Value {
        json!([
            "/bin/sh",
            "-c",
            format!("{script}; while [ ! -e '{}' ]; do sleep 0.05; done", beaten.display())
        ])
    }

    #[test]
    fn a_run_says_its_start_before_the_commands_output_and_beats_while_it_runs() {
        let case = Case::new("beats");
        let commit = case.snapshot();
        let beaten = case.base.join("beaten");
        let command = waits_for_a_beat("echo hi", &beaten);
        let ended = ran(
            &[],
            &case.request(&commit, &json!({"Command": command, "Timeout": 3600})),
            Reading::All,
            &beaten,
        );
        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        assert_eq!(
            ended.out,
            [started_line(NONCE), "hi".to_owned()],
            "the start marker before the command's output"
        );
        assert!(beaten.exists(), "a heartbeat was read after the start marker: {:?}", ended.err);
        assert!(ended.adjusted(), "the adjusted line: {:?}", ended.err);
        // The login shell's PATH has a wall-clock bound a busy Mac can cross, which only notes it.
        let unread_path = "gate: remote path: could not read the login shell's PATH";
        assert!(
            !ended.err.iter().any(|line| line.starts_with("gate: ") && !line.starts_with(unread_path)),
            "no gate line: {:?}",
            ended.err
        );
        assert_eq!(case.back(), None, "nothing changed, so the earlier back ref is gone and none is pushed");
        case.ended_clean();
    }

    #[test]
    fn a_run_that_changes_the_tree_pushes_a_back_commit_parented_on_the_snapshot() {
        let case = Case::new("changes");
        let commit = case.snapshot();
        let request = case.request(&commit, &json!({"Command": ["/bin/sh", "-c", "echo new > new.txt"]}));
        let ended = ran(&[], &request, Reading::All, &case.base.join("beaten"));
        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        let back = case.back().expect("a back commit is pushed");
        assert_ne!(back, commit);
        let shown = git(&[&case.bare_dir(), "show", "-s", "--format=%P|%an|%cn|%s", &back]);
        assert_eq!(shown, format!("{commit}|Box Author|Box Committer|gate-remote back"));
        assert_eq!(git(&[&case.bare_dir(), "show", &format!("{back}:new.txt")]), "new");
        case.ended_clean();
    }

    #[test]
    fn a_run_that_changes_nothing_pushes_no_back_ref() {
        let case = Case::new("unchanged");
        let commit = case.snapshot();
        let ended = ran(&[], &case.request(&commit, &json!({})), Reading::All, &case.base.join("beaten"));
        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        assert_eq!(case.back(), None, "the earlier back ref is deleted and none pushed");
        case.ended_clean();
    }

    #[test]
    fn a_command_that_exits_nonzero_still_syncs_back() {
        let case = Case::new("nonzero");
        let commit = case.snapshot();
        let request = case.request(&commit, &json!({"Command": ["/bin/sh", "-c", "echo y > y.txt; exit 3"]}));
        let ended = ran(&[], &request, Reading::All, &case.base.join("beaten"));
        assert_eq!(ended.status, Some(3), "{:?}", ended.err);
        let back = case.back().expect("a back commit is pushed");
        assert_eq!(git(&[&case.bare_dir(), "show", &format!("{back}:y.txt")]), "y");
        case.ended_clean();
    }

    #[test]
    fn a_failing_sampler_says_no_adjusted_line() {
        let case = Case::new("sampler-fail");
        let commit = case.snapshot();
        let request = case.request(&commit, &json!({"Timeout": 3600}));
        let ended = ran(&["--test-box-sampler-fail", "true"], &request, Reading::All, &case.base.join("beaten"));
        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        assert!(ended.err.iter().any(|line| line.starts_with(SAMPLER_FAILED)), "{:?}", ended.err);
        assert!(!ended.adjusted(), "no adjusted line: {:?}", ended.err);
        case.ended_clean();
    }

    #[test]
    fn a_command_past_its_ceiling_ends_124_with_the_tag_and_no_back_ref() {
        let case = Case::new("ceiling");
        let commit = case.snapshot();
        let request = case.request(&commit, &json!({"Command": ["/bin/sh", "-c", "echo x > x.txt; sleep 600"], "Timeout": 1}));
        // The whole machine free, so the ceiling's clock runs with wall time however loaded the machine is.
        let ended = ran(&["--test-box-free-percent", "100"], &request, Reading::All, &case.base.join("beaten"));
        assert_eq!(ended.status, Some(124), "{:?}", ended.err);
        assert!(ended.err_has(&killed_line(NONCE, BoxKill::TimedOut)), "{:?}", ended.err);
        assert_eq!(case.back(), None, "no back commit, and the earlier back ref is deleted");
        assert!(!Path::new(&format!("{}.pgid", case.work())).exists(), "no .pgid left");
        assert!(case.lock_free(), "the work lock is free");
    }

    #[test]
    fn a_held_work_lock_refuses_busy_75() {
        let case = Case::new("busy");
        let commit = case.snapshot();
        let held = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(case.lock())
            .expect("the lock file opens");
        // SAFETY: `held` stays open, and its lock held, until the case ends.
        let locked = unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(locked, 0, "the case holds the work lock");
        let ended = ran(&[], &case.request(&commit, &json!({})), Reading::All, &case.base.join("beaten"));
        assert_eq!(ended.status, Some(75), "{:?}", ended.err);
        assert!(ended.out.is_empty(), "no start marker: {:?}", ended.out);
        let tag = ended.err.iter().position(|line| *line == refused_line(NONCE, Refusal::Busy));
        assert!(
            tag.is_some_and(|tag| ended.err.get(tag + 1) == Some(&busy_line(KEY))),
            "the busy tag, then the busy line: {:?}",
            ended.err
        );
        drop(held);
        assert!(case.lock_free(), "the box let go of nothing it did not hold");
    }

    #[test]
    fn a_client_that_closes_its_input_early_ends_255() {
        let case = Case::new("gone");
        let commit = case.snapshot();
        let request = case.request(&commit, &json!({"Command": ["/bin/sh", "-c", "echo z > z.txt; sleep 600"]}));
        let ended = ran(&[], &request, Reading::CloseAfterStart, &case.base.join("beaten"));
        assert_eq!(ended.status, Some(255), "{:?}", ended.err);
        assert_eq!(case.back(), None, "no back commit for a client gone");
        assert!(!Path::new(&format!("{}.pgid", case.work())).exists(), "no .pgid left");
        assert!(case.lock_free(), "the work lock is free");
    }
}
