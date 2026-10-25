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
    use gate_inputs::archive::{UnpackError, read_members};
    use gate_inputs::hash::sha256_of;
    use gate_inputs::manifest::{Held, Manifest, Stamp, read_sent};
    use gate_inputs::pack::Packer;
    use gate_wire::stamp::{Tag, unstamped};
    use gate_wire::{BoxKill, Refusal, adjusted_prefix, busy_line, escaped, killed_line, refused_line, started_line};
    use serde_json::{Value, json};

    const KEY: &str = "repo-0123abcd";
    const NONCE: &str = "0123456789abcdef0123456789abcdef";
    const SAMPLER_FAILED: &str = "gate: sampler failed: ";
    /// The program of a Godot install, inside its version's folder, where the box runs it from.
    const GODOT_PROGRAM: &str = "Godot.app/Contents/MacOS/Godot";

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
            // Mode 0700 as the box's preflight and prune demand of a root, whatever the umask.
            std::os::unix::fs::DirBuilderExt::mode(fs::DirBuilder::new().recursive(true), 0o700)
                .create(format!("{}/work", case.root()))
                .expect("the work folders' parent is made");
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
            self.snapshot_holding(&[])
        }

        /// [`Case::snapshot`], the commit also holding `files`, each a path and its text.
        fn snapshot_holding(&self, files: &[(&str, &str)]) -> String {
            let source = self.base.join("source");
            let source = source.to_str().expect("the temp folder is UTF-8");
            git(&["init", "-q", source]);
            fs::write(Path::new(source).join("a.txt"), "a\n").expect("the file is written");
            for (name, text) in files {
                let path = Path::new(source).join(name);
                fs::create_dir_all(path.parent().expect("a file has a folder")).expect("the file's folder is made");
                fs::write(path, text).expect("the file is written");
            }
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

        /// The keys of a pair of the leaf `repo` and the sibling `lib`, whose bare repository `<root>/lib.git` is made
        /// holding a commit of `b.txt`, pushed as `refs/gate/<key>`.
        fn pair_with_lib(&self) -> Value {
            let lib_bare = format!("{}/lib.git", self.root());
            git(&["init", "--bare", "-q", &lib_bare]);
            let lib_source = self.base.join("lib-source");
            let lib_source = lib_source.to_str().expect("the temp folder is UTF-8");
            git(&["init", "-q", lib_source]);
            fs::write(Path::new(lib_source).join("b.txt"), "b\n").expect("the sibling's file is written");
            git(&["-C", lib_source, "add", "-A"]);
            git(&[
                "-C",
                lib_source,
                "-c",
                "user.name=gate",
                "-c",
                "user.email=gate@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "sibling",
            ]);
            git(&["-C", lib_source, "push", "-q", &lib_bare, &format!("HEAD:refs/gate/{KEY}")]);
            let lib = git(&["-C", lib_source, "rev-parse", "HEAD"]);
            json!({ "Pair": { "Leaf": "repo", "Siblings": ["lib"] }, "SiblingSnapshots": { "lib": lib } })
        }

        /// The `box inputs` request word of `step` for this case's work folder: base64 of the JSON payload.
        fn inputs_request(&self, step: &str) -> String {
            let (root, work) = (self.root(), self.work());
            let payload = json!({
                "Key": KEY,
                "Bare": self.bare(),
                "Work": work,
                "Lock": format!("{work}.lock"),
                "Index": format!("{work}.index"),
                "OutputFolder": format!("{root}/outputs"),
                "Job": format!("gate-remote-{KEY}-0123456789ab"),
                "Nonce": NONCE,
                "Step": step,
            });
            STANDARD.encode(payload.to_string())
        }

        /// The inputs record, `<work>.inputs`.
        fn inputs_record(&self) -> String {
            format!("{}.inputs", self.work())
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
        /// Until the start marker and a heartbeat after it, then standard error closed: a client gone.
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

        /// Each line said on stderr, unstamped.
        fn err_texts(&self) -> Vec<&str> {
            texts(&self.err)
        }

        /// Checks every protocol line the run said, on either stream, is bare: written as it is, with no stamp.
        fn protocol_bare(&self) {
            for line in self.out.iter().chain(&self.err) {
                let read = unstamped(line);
                if read.text.starts_with("gate-remote-") {
                    assert_eq!(read.stamp, None, "a protocol line is never stamped: {line}");
                }
            }
        }

        /// The index and the text of each process line said on stderr, in order, unstamped.
        fn process_lines(&self) -> Vec<(usize, &str)> {
            self.err
                .iter()
                .enumerate()
                .filter_map(|(at, line)| {
                    let read = unstamped(line);
                    (read.tag == Some(Tag::Gat) && gate_wire::stamp::is_process_line(read.text)).then_some((at, read.text))
                })
                .collect()
        }
    }

    /// The text of each of `lines`, unstamped.
    fn texts(lines: &[String]) -> Vec<&str> {
        lines.iter().map(|line| unstamped(line).text).collect()
    }

    /// The value of the word `key=<value>` in `text`.
    fn word<'a>(text: &'a str, key: &str) -> Option<&'a str> {
        text.split(' ').find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
    }

    /// Starts `gate-box box --test-heartbeat-ms 50 --test-box-sample-ms 50 <seams> run --stdin`, writes `request` on
    /// its stdin, and reads it as `reading` says until it ends. Once the start marker and then a heartbeat were read,
    /// `beaten` is made, which a command can wait on.
    fn ran(seams: &[&str], request: &str, reading: Reading, beaten: &Path) -> Ended {
        ran_in(seams, request, reading, beaten, None)
    }

    /// [`ran`], the box's `SHELL`, which its login `PATH` is read from, set to `shell` when one is given.
    fn ran_in(seams: &[&str], request: &str, reading: Reading, beaten: &Path, shell: Option<&Path>) -> Ended {
        let mut words = vec!["box", "--test-heartbeat-ms", "50", "--test-box-sample-ms", "50"];
        words.extend(seams);
        words.extend(["run", "--stdin"]);
        let mut command = Command::new(env!("CARGO_BIN_EXE_gate-box"));
        if let Some(shell) = shell {
            command.env("SHELL", shell);
        }
        let mut child: Child = command
            .args(&words)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("gate-box starts");
        let mut stdin = child.stdin.take().expect("stdin is piped");
        stdin.write_all(format!("{request}\n").as_bytes()).expect("the request is written");
        drop(stdin);
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let stop = Arc::new(AtomicBool::new(false));
        let out_stop = Arc::clone(&stop);
        let out = thread::spawn(move || lines_of(stdout, reading, &out_stop));
        let beaten = beaten.to_owned();
        let err = thread::spawn(move || {
            let mut said = Vec::new();
            let marker = started_line(NONCE);
            let mut started = false;
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|read| read > 0) {
                let text = line.trim_end_matches('\n').to_owned();
                line.clear();
                started |= text == marker;
                let beat = text.starts_with("gate-remote-heartbeat:") && started;
                said.push(text);
                if beat {
                    if !beaten.exists() {
                        fs::write(&beaten, "").expect("the beaten marker is made");
                    }
                    if reading == Reading::CloseAfterStart {
                        stop.store(true, Ordering::SeqCst);
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

    /// How long a reader watching for a stop waits for bytes before it looks at the stop again.
    const READ_SLICE_MS: i32 = 20;

    /// The lines of `pipe`, read to its end; in [`Reading::CloseAfterStart`], until `stop` is set, when the reader
    /// stops and drops the pipe as a client that hung up drops both.
    fn lines_of(pipe: impl Read + AsRawFd, reading: Reading, stop: &AtomicBool) -> Vec<String> {
        let mut reader = BufReader::new(pipe);
        let mut said = Vec::new();
        let mut line = String::new();
        loop {
            if reading == Reading::CloseAfterStart {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                if !readable(reader.get_ref()) {
                    continue;
                }
            }
            match reader.read_line(&mut line) {
                Ok(read) if read > 0 => {
                    said.push(line.trim_end_matches('\n').to_owned());
                    line.clear();
                }
                _ => break,
            }
        }
        said
    }

    /// Whether `pipe` has bytes to read within a slice of the clock, so a reader watching for a stop never waits on a
    /// pipe that stays quiet; a broken pipe reads as ready, its read being what fails.
    fn readable(pipe: &impl AsRawFd) -> bool {
        let mut polled = libc::pollfd {
            fd: pipe.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `polled` is one `pollfd` that lives across the call, and its count is passed with it.
        unsafe { libc::poll(&mut polled, 1, READ_SLICE_MS) != 0 }
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
        let command = waits_for_a_beat("echo hi; echo there >&2", &beaten);
        let ended = ran(
            &[],
            &case.request(&commit, &json!({"Command": command, "Timeout": 3600})),
            Reading::All,
            &beaten,
        );
        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        assert_eq!(texts(&ended.out), ["hi"], "the command's output");
        let marker = started_line(NONCE);
        let err = ended.err_texts();
        let started = err.iter().position(|line| *line == marker).expect("the start marker on standard error");
        let there = err
            .iter()
            .position(|line| *line == "there")
            .expect("the command's line on standard error");
        assert!(
            started < there,
            "the start marker before the command's line on the one pipe that says both: {:?}",
            ended.err
        );
        ended.protocol_bare();
        assert!(beaten.exists(), "a heartbeat was read after the start marker: {:?}", ended.err);
        assert!(ended.adjusted(), "the adjusted line: {:?}", ended.err);
        // The login shell's PATH has a wall-clock bound a busy Mac can cross, which only notes it.
        let unread_path = "gate: remote path: could not read the login shell's PATH";
        assert!(
            !ended
                .err_texts()
                .iter()
                .any(|line| line.starts_with("gate: ") && !line.starts_with(unread_path)),
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
    fn a_pair_run_runs_in_the_main_tree_and_reads_its_sibling_beside_it() {
        let case = Case::new("pair");
        let commit = case.snapshot();
        let lib_bare = format!("{}/lib.git", case.root());
        let mut extra = case.pair_with_lib();
        extra["Command"] = json!(["/bin/sh", "-c", "cat a.txt ../lib/b.txt; echo c > c.txt; echo d > ../lib/d.txt"]);
        let ended = ran(&[], &case.request(&commit, &extra), Reading::All, &case.base.join("beaten"));
        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        assert_eq!(texts(&ended.out), ["a", "b"], "the command ran in the main tree, its sibling beside it");
        assert!(ended.err_has(&started_line(NONCE)), "the start marker on standard error: {:?}", ended.err);
        let main_back = case.back().expect("the main repo's back ref is pushed");
        let (lib_back, found) = git_said(&[
            &format!("--git-dir={lib_bare}"),
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/gate-back/{KEY}"),
        ]);
        assert!(found, "the sibling's back ref is pushed: {:?}", ended.err);
        let line = format!("gate: remote back commits: repo {main_back}, lib {lib_back}");
        assert_eq!(
            ended
                .err_texts()
                .into_iter()
                .filter(|said| said.starts_with("gate: remote back commits:"))
                .collect::<Vec<_>>(),
            [line.as_str()],
            "one line names both back commits: {:?}",
            ended.err
        );
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
    fn a_run_s_command_has_a_short_tmpdir_of_its_own_which_is_gone_after_the_run() {
        let case = Case::new("tmpdir");
        let commit = case.snapshot();
        let script = "printf '%s\\n' \"$TMPDIR\"; test -d \"$TMPDIR\" || exit 9; touch \"$TMPDIR/left\"";
        let request = case.request(&commit, &json!({"Command": ["/bin/sh", "-c", script]}));
        let ended = ran(&[], &request, Reading::All, &case.base.join("beaten"));
        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        assert_eq!(ended.out.len(), 1, "the TMPDIR: {:?}", ended.out);
        assert!(ended.err_has(&started_line(NONCE)), "the start marker on standard error: {:?}", ended.err);
        let tmpdir = ended.out.last().map(|line| unstamped(line).text).expect("the command said its TMPDIR");
        assert!(tmpdir.starts_with("/tmp/gate.") && tmpdir.ends_with('/') && tmpdir.len() < 38, "{tmpdir}");
        assert!(!Path::new(tmpdir).exists(), "the run's temp folder is gone: {tmpdir}");
        case.ended_clean();
    }

    #[test]
    fn a_failing_sampler_says_no_adjusted_line() {
        let case = Case::new("sampler-fail");
        let commit = case.snapshot();
        let request = case.request(&commit, &json!({"Timeout": 3600}));
        let ended = ran(&["--test-box-sampler-fail", "true"], &request, Reading::All, &case.base.join("beaten"));
        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        assert!(ended.err_texts().iter().any(|line| line.starts_with(SAMPLER_FAILED)), "{:?}", ended.err);
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
        ended.protocol_bare();
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
        assert!(ended.out.is_empty(), "nothing ran: {:?}", ended.out);
        assert!(!ended.err_has(&started_line(NONCE)), "no start marker: {:?}", ended.err);
        let tag = ended.err.iter().position(|line| *line == refused_line(NONCE, Refusal::Busy));
        assert!(
            tag.is_some_and(|tag| ended.err_texts().get(tag + 1) == Some(&busy_line(KEY).as_str())),
            "the busy tag, then the busy line: {:?}",
            ended.err
        );
        ended.protocol_bare();
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

    /// Runs `gate-box box prune <words>` with the variables `env` set; its status and its lines on stdout and stderr.
    fn pruned(words: &[&str], env: &[(&str, &str)]) -> Ended {
        let ran = Command::new(env!("CARGO_BIN_EXE_gate-box"))
            .args(["box", "prune"])
            .args(words)
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .output()
            .expect("gate-box runs");
        ended_of(&ran)
    }

    /// What a `gate-box` that has ended said, line by line, and its status.
    fn ended_of(ran: &std::process::Output) -> Ended {
        let lines = |bytes: &[u8]| String::from_utf8_lossy(bytes).lines().map(str::to_owned).collect();
        Ended {
            status: ran.status.code(),
            out: lines(&ran.stdout),
            err: lines(&ran.stderr),
        }
    }

    /// Sets the last write of `path`, a file or a folder, to `age` ago.
    fn backdated(path: &Path, age: std::time::Duration) {
        let when = std::time::SystemTime::now() - age;
        fs::File::open(path)
            .and_then(|file| file.set_times(fs::FileTimes::new().set_modified(when)))
            .expect("the last write is set");
    }

    #[test]
    fn an_age_prune_removes_an_idle_work_folder_and_a_leftover_run_folder() {
        let case = Case::new("prune-aged");
        let two_days = std::time::Duration::from_hours(48);
        let work = PathBuf::from(case.work());
        fs::create_dir(&work).expect("the work folder is made");
        fs::write(work.join("file"), "x").expect("a file is written in the work folder");
        backdated(&work.join("file"), two_days);
        backdated(&work, two_days);
        let tmp = case.base.join("tmp");
        let leftover = tmp.join("gate.Ab3dE6g8");
        fs::create_dir_all(&leftover).expect("the leftover run folder is made");
        backdated(&leftover, two_days);
        let tmp_root = tmp.to_str().expect("the temp folder is UTF-8");
        let free = (1000u64 << 30).to_string();

        let ended = pruned(
            &["--root", &case.root()],
            &[("GATE_BOX_TEST_TMP_ROOT", tmp_root), ("GATE_BOX_TEST_FREE_BYTES", &free)],
        );

        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        assert!(ended.err.is_empty(), "{:?}", ended.err);
        assert_eq!(
            ended.out,
            [
                format!("gate: remote pruned {KEY}: idle 48 h"),
                format!("gate: remote removed the leftover run folder {}: idle 48 h", leftover.display()),
                "gate: remote prune removed 1 of 1 work folders".to_owned(),
            ]
        );
        assert!(!work.exists() && !leftover.exists());
        assert!(Path::new(&case.lock()).is_file(), "the age prune keeps the lock");
    }

    #[test]
    fn a_keyed_prune_removes_the_key_s_folder_and_refs_and_leaves_no_lock_for_a_key_never_had() {
        let case = Case::new("prune-keyed");
        let commit = case.snapshot();
        git(&[&case.bare_dir(), "update-ref", &format!("refs/gate-bridge/{KEY}"), &commit]);
        let work = PathBuf::from(case.work());
        fs::create_dir(&work).expect("the work folder is made");
        let never = "never-0123abcd";

        let ended = pruned(&["--root", &case.root(), "--keys", KEY, never], &[]);

        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        assert!(ended.err.is_empty(), "{:?}", ended.err);
        assert_eq!(
            ended.out,
            [
                format!("gate: remote pruned {KEY} (worktree gone)"),
                format!("gate: remote pruned {never} (worktree gone)"),
            ]
        );
        assert_eq!(
            git(&[&case.bare_dir(), "for-each-ref", "--format=%(refname)"]),
            "",
            "the key's refs are gone"
        );
        assert!(!work.exists(), "the work folder is gone");
        let locks = [case.lock(), format!("{}/work/{never}.lock", case.root())];
        assert!(locks.iter().all(|lock| !Path::new(lock).exists()), "no lock is left");
    }

    /// Writes `script` at `path`, runnable.
    fn runnable(path: &Path, script: &str) {
        fs::write(path, script).expect("the script is written");
        fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o700)).expect("the script is made runnable");
    }

    #[test]
    fn a_run_with_a_tool_manifest_restores_its_tools_before_the_command() {
        let case = Case::new("tools");
        let shims = case.base.join("shims");
        fs::create_dir_all(&shims).expect("the shim folder is made");
        let log = case.base.join("dotnet.log");
        runnable(&shims.join("dotnet"), &format!("#!/bin/sh\necho \"$*\" >> '{}'\n", log.display()));
        // The login shell the box reads its PATH from puts the shim's folder first, then runs as `sh`.
        let shell = case.base.join("login-shell");
        runnable(
            &shell,
            &format!(
                "#!/bin/sh\nPATH='{}':\"$PATH\"\nexport PATH\nshift\nexec /bin/sh \"$@\"\n",
                shims.display()
            ),
        );
        let commit = case.snapshot_holding(&[(".config/dotnet-tools.json", "{\n  \"version\": 1\n}\n")]);
        // The command fails unless the restore ran before it.
        let command = json!(["/bin/sh", "-c", format!("test -s '{}'", log.display())]);
        let request = case.request(&commit, &json!({ "Command": command }));
        let ended = ran_in(&[], &request, Reading::All, &case.base.join("beaten"), Some(&shell));
        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        assert_eq!(fs::read_to_string(&log).expect("the shim ran"), "tool restore\n", "one restore");
        assert!(
            ended.err_texts().iter().any(|line| line.starts_with("gate: dotnet tool restore took ")),
            "{:?}",
            ended.err
        );
        assert!(ended.out.is_empty(), "the command says nothing on standard output: {:?}", ended.out);
        assert!(ended.err_has(&started_line(NONCE)), "the start marker on standard error: {:?}", ended.err);
        assert!(Path::new(&format!("{}.tools", case.work())).exists(), "the tools record is written");
        case.ended_clean();
    }

    /// The inputs of the inputs cases, each a path and its bytes: one in a folder of its own, one at the tree's top.
    const INPUTS: [(&str, &[u8]); 2] = [("cache/x.bin", b"cached\n"), ("in.txt", b"input\n")];
    /// The last write every packed input carries, in seconds since 1970.
    const WRITTEN: u64 = 1_700_000_000;

    /// The PC's inputs manifest of `files`.
    fn manifest_of(files: &[(&str, &[u8])]) -> Manifest {
        let mut manifest = Manifest::default();
        for (path, bytes) in files {
            let stamp = Stamp {
                size: bytes.len() as u64,
                modified: 0,
            };
            let sha256 = sha256_of(*bytes).expect("the input hashes").0;
            manifest.insert((*path).to_owned(), Held { stamp, sha256 });
        }
        manifest
    }

    /// The digest of the manifest of `files`, the run's `InputsDigest`.
    fn digest_of(files: &[(&str, &[u8])]) -> String {
        read_sent(manifest_of(files).line().into_bytes()).expect("the manifest reads").1
    }

    /// The inputs archive of `files`, packed as the PC packs it.
    fn archive_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut sink = Vec::new();
        let mut packer = Packer::new(&mut sink, 20).expect("a packer is made");
        for (path, bytes) in files {
            packer.add(path, bytes.len() as u64, WRITTEN, *bytes).expect("an input is packed");
        }
        packer.finish().expect("the archive ends");
        sink
    }

    /// Runs `gate-box box inputs --stdin` for `step` of the work folder of `case`, the request, the manifest of `files`
    /// and, for a put, their archive on its stdin; its status and its lines.
    fn inputs_answered(case: &Case, step: &str, files: &[(&str, &[u8])]) -> Ended {
        let mut input = format!("{}\n{}", case.inputs_request(step), manifest_of(files).line()).into_bytes();
        if step == "put" {
            input.extend(archive_of(files));
        }
        let mut child = Command::new(env!("CARGO_BIN_EXE_gate-box"))
            .args(["box", "inputs", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("gate-box starts");
        let mut stdin = child.stdin.take().expect("stdin is piped");
        // Written from a thread of its own, so the box's output is read however much it says, and a box that ends
        // before reading all its input still has its lines read.
        let writer = thread::spawn(move || stdin.write_all(&input));
        let ended = ended_of(&child.wait_with_output().expect("gate-box is waited for"));
        let written = writer.join().expect("the input's writer ends");
        assert!(
            written.is_ok() || ended.status != Some(0),
            "a box that passed read its whole input: {written:?} {:?}",
            ended.err
        );
        ended
    }

    /// [`INPUTS`] diffed and put into the work folder of `case`, each step checked.
    fn inputs_installed(case: &Case) {
        let diff = inputs_answered(case, "diff", &INPUTS);
        assert_eq!(diff.status, Some(0), "{:?}", diff.err);
        assert_eq!(
            diff.out,
            [
                "gate-remote-inputs: need 2",
                "gate-remote-inputs: path cache/x.bin",
                "gate-remote-inputs: path in.txt"
            ]
        );
        let put = inputs_answered(case, "put", &INPUTS);
        assert_eq!(put.status, Some(0), "{:?}", put.err);
        assert_eq!(put.out, ["gate-remote-inputs: installed 2"]);
    }

    /// The run of `commit` in the work folder of `case` running `script` in `sh`, with the digest of [`INPUTS`].
    fn ran_with_inputs(case: &Case, commit: &str, script: &str) -> Ended {
        let extra = json!({ "Command": ["/bin/sh", "-c", script], "InputsDigest": digest_of(&INPUTS) });
        ran(&[], &case.request(commit, &extra), Reading::All, &case.base.join("beaten"))
    }

    /// Checks `ended` was refused `inputs` 75 for `why`, its tag just before its line, before any start marker.
    fn assert_inputs_refused(ended: &Ended, why: &str) {
        assert_eq!(ended.status, Some(75), "{:?}", ended.err);
        assert!(ended.out.is_empty(), "nothing ran: {:?}", ended.out);
        assert!(!ended.err_has(&started_line(NONCE)), "no start marker: {:?}", ended.err);
        let line = format!("gate: remote inputs of work folder {KEY} are not the ones sent: {}", escaped(why));
        let tag = ended.err.iter().position(|said| *said == refused_line(NONCE, Refusal::Inputs));
        assert!(
            tag.is_some_and(|tag| ended.err_texts().get(tag + 1) == Some(&line.as_str())),
            "the inputs tag, then {line}: {:?}",
            ended.err
        );
    }

    #[test]
    fn inputs_diffed_and_put_let_a_run_with_their_digest_read_them() {
        let case = Case::new("inputs");
        let commit = case.snapshot();
        inputs_installed(&case);

        let ended = ran_with_inputs(&case, &commit, "cat in.txt cache/x.bin");

        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        assert_eq!(texts(&ended.out), ["input", "cached"]);
        assert!(ended.err_has(&started_line(NONCE)), "the start marker on standard error: {:?}", ended.err);
        case.ended_clean();
    }

    #[test]
    fn an_input_the_command_changed_stays_out_of_the_back_commit_and_refuses_the_next_run_inputs() {
        let case = Case::new("inputs-changed");
        let commit = case.snapshot();
        inputs_installed(&case);

        let ended = ran_with_inputs(&case, &commit, "echo b > a.txt; echo changed > in.txt");

        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        let back = case.back().expect("a back commit is pushed");
        assert_eq!(
            git(&[&case.bare_dir(), "ls-tree", "-r", "--name-only", &back]),
            "a.txt",
            "no input is committed back"
        );
        let next = ran_with_inputs(&case, &commit, "true");
        assert_inputs_refused(&next, "the input in.txt is not the copy installed");
        assert!(case.lock_free(), "the work lock is free");
    }

    #[test]
    fn an_input_changed_between_runs_refuses_the_next_run_inputs() {
        let case = Case::new("inputs-touched");
        let commit = case.snapshot();
        inputs_installed(&case);
        fs::write(format!("{}/cache/x.bin", case.work()), "other\n").expect("the input changes");

        let ended = ran_with_inputs(&case, &commit, "true");

        assert_inputs_refused(&ended, "the input cache/x.bin is not the copy installed");
    }

    #[test]
    fn a_layout_change_removes_the_inputs_record_and_the_next_run_with_the_digest_is_refused() {
        let case = Case::new("inputs-relaid");
        let commit = case.snapshot();
        inputs_installed(&case);
        let pair = case.request(&commit, &case.pair_with_lib());

        let ended = ran(&[], &pair, Reading::All, &case.base.join("beaten"));

        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        assert!(!Path::new(&case.inputs_record()).exists(), "the record goes with the layout");
        let next = ran_with_inputs(&case, &commit, "true");
        assert_inputs_refused(&next, &format!("{} does not exist", case.inputs_record()));
    }

    #[test]
    fn a_keyed_prune_removes_the_inputs_record_and_a_leftover_temp_of_it() {
        let case = Case::new("prune-inputs");
        inputs_installed(&case);
        let record = case.inputs_record();
        let temp = format!("{record}.gate-new-999999-0");
        fs::write(&temp, "{}").expect("a leftover temp of the record is written");

        let ended = pruned(&["--root", &case.root(), "--keys", KEY], &[]);

        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        assert!(!Path::new(&record).exists(), "the record is removed");
        assert!(!Path::new(&temp).exists(), "its leftover temp is removed");
        assert!(!Path::new(&case.work()).exists(), "the work folder is gone");
    }

    /// yaat's outputs glob: the TRX files its test runs leave below `TestResults`.
    const TRX_GLOB: &str = "tests/*/bin/*/*/TestResults/*.trx";

    /// The name of a case's outputs archive.
    fn archive_name() -> String {
        format!("{KEY}-{NONCE}.tar.zst")
    }

    /// The `gate-remote-outputs:` line of a pack of `members` files.
    fn packed_line(members: usize) -> String {
        format!("gate-remote-outputs: {members} {}", archive_name())
    }

    impl Case {
        /// The case's outputs archive, `<root>/outputs/<key>-<nonce>.tar.zst`.
        fn archive(&self) -> PathBuf {
            PathBuf::from(format!("{}/outputs/{}", self.root(), archive_name()))
        }

        /// The request word of a run of `snapshot` with `keys` set over it, followed by the PC's outputs manifest
        /// `manifest`, as the client sends a run with output globs.
        fn outputs_request(&self, snapshot: &str, keys: &Value, manifest: &Manifest) -> String {
            format!("{}\n{}", self.request(snapshot, keys), manifest.line().trim_end())
        }

        /// Runs `gate-box box outputs --stdin` for the archive `name` of this case's work folder; its status, the bytes
        /// it sent and its lines on stderr.
        fn outputs_sent(&self, name: &str) -> (Option<i32>, Vec<u8>, Vec<String>) {
            let (root, work) = (self.root(), self.work());
            let payload = json!({ "Key": KEY, "Work": work, "OutputFolder": format!("{root}/outputs"), "Name": name });
            let mut child = Command::new(env!("CARGO_BIN_EXE_gate-box"))
                .args(["box", "outputs", "--stdin"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("gate-box starts");
            let mut stdin = child.stdin.take().expect("stdin is piped");
            stdin
                .write_all(format!("{}\n", STANDARD.encode(payload.to_string())).as_bytes())
                .expect("the request is written");
            drop(stdin);
            let ran = child.wait_with_output().expect("gate-box is waited for");
            let err = String::from_utf8_lossy(&ran.stderr).lines().map(str::to_owned).collect();
            (ran.status.code(), ran.stdout, err)
        }
    }

    /// The keys of a run running `script` in `sh` and asking for the outputs `globs`.
    fn outputs_keys(script: &str, globs: &[&str]) -> Value {
        json!({ "Command": ["/bin/sh", "-c", script], "Outputs": globs })
    }

    /// The path of each member of the outputs archive `archive`, read as the PC's reader reads it, in its list's order.
    fn members_of(archive: &[u8]) -> Vec<String> {
        let items = read_members(archive, 1 << 30, 50_000, &Manifest::default().by_hash(&[]), |_, _, data| {
            data.read_to_end(&mut Vec::new()).map(drop).map_err(UnpackError::from)
        })
        .expect("the archive reads back");
        items.into_iter().map(|item| item.entry.path).collect()
    }

    /// The `gate-remote-outputs:` lines `ended` said.
    fn outputs_said(ended: &Ended) -> Vec<&str> {
        ended
            .err
            .iter()
            .filter(|line| line.starts_with("gate-remote-outputs:"))
            .map(String::as_str)
            .collect()
    }

    /// The keys of a run running `script` in `sh`, asking for the outputs `bin/**` and the TRX files below `bin`'s
    /// `TestResults`, and for a cross build for `win-x64` running `cross` in `sh`.
    fn cross_keys(script: &str, cross: &str) -> Value {
        json!({
            "Command": ["/bin/sh", "-c", script],
            "Outputs": ["bin/**", "bin/*/*/TestResults/*.trx"],
            "CrossBuild": { "Rid": "win-x64", "Words": ["/bin/sh", "-c", cross] },
        })
    }

    /// The folder a cross build for `win-x64` of a `Debug` build of `net10.0` writes to, and its native build's.
    const CROSS_FOLDER: &str = "bin/Debug/net10.0/win-x64";
    const NATIVE_FOLDER: &str = "bin/Debug/net10.0";
    /// A Mach-O 64-bit program's first bytes and more, as `printf` writes them.
    const MACH_O: &str = r"\317\372\355\376rest";
    /// A Windows program's first bytes, as `printf` writes them.
    const PE: &str = r"MZ\220\000";

    /// A cross build for `win-x64` that failed with `status`, as the box says it.
    fn cross_failed_line(status: u8) -> String {
        format!("gate: the cross build for win-x64 failed with exit {status}; the Windows outputs were not brought back")
    }

    /// The reasons a cross build's outputs are not brought back: it failed, or it left a program the PC cannot run.
    const CROSS_FAILED: &str = "the cross build failed";
    const CROSS_REFUSED: &str = "the cross build left a non-Windows file";

    /// `ended` said `line` and, right after it, that the outputs were not brought back for `reason`, and said no
    /// `gate-remote-outputs:` line.
    fn assert_said_then_not_brought_back(ended: &Ended, line: &str, reason: &str) {
        let texts = ended.err_texts();
        let not_brought = format!("gate: remote outputs not brought back: {reason}");
        assert!(
            texts.windows(2).any(|pair| pair[0] == line && pair[1] == not_brought),
            "{line} then {not_brought}: {:?}",
            ended.err
        );
        assert!(outputs_said(ended).is_empty(), "{:?}", ended.err);
    }

    #[test]
    fn cross_build_outputs_come_back_at_their_pc_paths_with_the_trx_and_a_native_program_stays_behind() {
        let case = Case::new("cross-built");
        let commit = case.snapshot_holding(&[(".gitignore", "bin/\n")]);
        let script = format!(
            "mkdir -p {NATIVE_FOLDER}/TestResults && echo '<TestRun/>' > {NATIVE_FOLDER}/TestResults/t.trx \
             && printf '{MACH_O}' > {NATIVE_FOLDER}/A && echo native > {NATIVE_FOLDER}/A.dll"
        );
        let cross = format!("mkdir -p {CROSS_FOLDER} && printf '{PE}' > {CROSS_FOLDER}/A.exe && printf '{PE}' > {CROSS_FOLDER}/A.dll");
        let request = case.outputs_request(&commit, &cross_keys(&script, &cross), &Manifest::default());

        let ended = ran(&[], &request, Reading::All, &case.base.join("beaten"));

        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        assert!(
            ended
                .err_texts()
                .iter()
                .any(|line| line.starts_with("gate: the cross build for win-x64 took ") && line.ends_with(" s")),
            "{:?}",
            ended.err
        );
        let archive = fs::read(case.archive()).expect("the archive is read");
        assert_eq!(
            members_of(&archive),
            [
                format!("{NATIVE_FOLDER}/A.dll"),
                format!("{NATIVE_FOLDER}/A.exe"),
                format!("{NATIVE_FOLDER}/TestResults/t.trx")
            ],
            "the cross build's files at their PC paths, one A.dll, and no Mach-O: {:?}",
            ended.err
        );
        assert_eq!(outputs_said(&ended), [packed_line(3)], "{:?}", ended.err);
        assert!(
            Path::new(&format!("{}/{NATIVE_FOLDER}/A", case.work())).exists(),
            "the native program stays on the builder"
        );
        case.ended_clean();
    }

    #[test]
    fn cross_build_that_fails_says_so_and_fails_the_outputs_bringing_nothing_back() {
        let case = Case::new("cross-failed");
        let commit = case.snapshot_holding(&[(".gitignore", "bin/\n")]);
        let script = format!("mkdir -p {NATIVE_FOLDER} && echo native > {NATIVE_FOLDER}/A.dll");
        let request = case.outputs_request(&commit, &cross_keys(&script, "echo cross said; exit 3"), &Manifest::default());

        let ended = ran(&[], &request, Reading::All, &case.base.join("beaten"));

        assert_eq!(ended.status, Some(1), "{:?}", ended.err);
        assert_said_then_not_brought_back(&ended, &cross_failed_line(3), CROSS_FAILED);
        assert!(texts(&ended.out).contains(&"cross said"), "its output is forwarded: {:?}", ended.out);
        assert!(outputs_said(&ended).is_empty(), "{:?}", ended.err);
        assert!(!case.archive().exists(), "nothing is packed");
        case.ended_clean();
    }

    #[test]
    fn cross_build_that_leaves_a_mach_o_among_its_outputs_brings_nothing_back() {
        let case = Case::new("cross-mach-o");
        let commit = case.snapshot_holding(&[(".gitignore", "bin/\n")]);
        let cross = format!("mkdir -p {CROSS_FOLDER} && printf '{PE}' > {CROSS_FOLDER}/A.exe && printf '{MACH_O}' > {CROSS_FOLDER}/A");
        let request = case.outputs_request(&commit, &cross_keys("true", &cross), &Manifest::default());

        let ended = ran(&[], &request, Reading::All, &case.base.join("beaten"));

        assert_eq!(ended.status, Some(1), "{:?}", ended.err);
        let refused =
            format!("gate: the cross build for win-x64 left a non-Windows file {NATIVE_FOLDER}/A; the Windows outputs were not brought back");
        assert_said_then_not_brought_back(&ended, &refused, CROSS_REFUSED);
        assert!(outputs_said(&ended).is_empty(), "{:?}", ended.err);
        assert!(!case.archive().exists(), "nothing is packed");
        case.ended_clean();
    }

    #[test]
    fn cross_build_does_not_run_after_a_failing_command() {
        let case = Case::new("cross-unrun");
        let commit = case.snapshot_holding(&[(".gitignore", "bin/\n")]);
        let marker = case.base.join("cross-ran");
        let cross = format!(": > '{}'", marker.display());
        let request = case.outputs_request(&commit, &cross_keys("exit 4", &cross), &Manifest::default());

        let ended = ran(&[], &request, Reading::All, &case.base.join("beaten"));

        assert_eq!(ended.status, Some(4), "{:?}", ended.err);
        assert!(!marker.exists(), "no cross build ran");
        assert!(
            !ended.err_texts().iter().any(|line| line.starts_with("gate: the cross build")),
            "{:?}",
            ended.err
        );
        case.ended_clean();
    }

    #[test]
    fn cross_build_is_ended_by_a_stop_and_brings_nothing_back() {
        let case = Case::new("cross-stopped");
        let commit = case.snapshot_holding(&[(".gitignore", "bin/\n")]);
        let marker = case.base.join("cross-started");
        let cross = format!(": > '{}'; sleep 600", marker.display());
        let keys = cross_keys("true", &cross);
        let request = case.outputs_request(&commit, &keys, &Manifest::default());
        let beaten = case.base.join("beaten");
        let run = thread::spawn(move || ran(&[], &request, Reading::All, &beaten));
        // A hang guard only: the wait is on the cross build's own marker.
        let guard = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while !marker.exists() && !run.is_finished() && std::time::Instant::now() < guard {
            thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(marker.exists(), "the cross build started");
        // The run adds the cross build's leader's start to its record once it started; only then may a stop signal it.
        let record = format!("{}.pgid", case.work());
        let names_a_start = || {
            fs::read(&record)
                .ok()
                .and_then(|bytes| gate_box_unix::lines::recorded_group(&bytes))
                .is_some_and(|recorded| recorded.started.is_some())
        };
        while !names_a_start() && !run.is_finished() && std::time::Instant::now() < guard {
            thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(names_a_start(), "the cross build's group and its leader's start are recorded");

        let mut stop = Command::new(env!("CARGO_BIN_EXE_gate-box"))
            .args(["box", "stop", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("gate-box starts");
        let mut stdin = stop.stdin.take().expect("stdin is piped");
        stdin
            .write_all(format!("{}\n", case.request(&commit, &keys)).as_bytes())
            .expect("the request is written");
        drop(stdin);
        let stopped = stop.wait_with_output().expect("the stop is waited for");
        let ended = run.join().expect("the run's reader ends");

        assert_eq!(stopped.status.code(), Some(0), "{}", String::from_utf8_lossy(&stopped.stderr));
        assert_eq!(ended.status, Some(1), "{:?}", ended.err);
        assert_said_then_not_brought_back(&ended, &cross_failed_line(143), CROSS_FAILED);
        assert!(outputs_said(&ended).is_empty(), "{:?}", ended.err);
        case.ended_clean();
    }

    #[test]
    fn a_run_with_yaat_s_trx_glob_packs_the_trx_and_box_outputs_sends_it_then_removes_it() {
        let case = Case::new("outputs-trx");
        let commit = case.snapshot_holding(&[(".gitignore", "bin/\n")]);
        let trx = "tests/Sim.Tests/bin/Release/net10.0/TestResults/sim.trx";
        let script = format!("mkdir -p \"$(dirname {trx})\" && echo '<TestRun/>' > {trx}");
        let request = case.outputs_request(&commit, &outputs_keys(&script, &[TRX_GLOB]), &Manifest::default());

        let ended = ran(&[], &request, Reading::All, &case.base.join("beaten"));

        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        assert_eq!(outputs_said(&ended), [packed_line(1)], "{:?}", ended.err);
        ended.protocol_bare();
        let (status, sent, err) = case.outputs_sent(&archive_name());
        assert_eq!(status, Some(0), "{err:?}");
        assert!(err.is_empty(), "{err:?}");
        assert!(!case.archive().exists(), "the archive sent is removed");
        assert_eq!(members_of(&sent), [trx], "the TRX below TestResults comes back");
        case.ended_clean();
    }

    #[test]
    fn a_file_the_pc_manifest_holds_unchanged_is_left_out_of_the_archive() {
        let case = Case::new("outputs-held");
        let commit = case.snapshot_holding(&[(".gitignore", "bin/\n")]);
        let script = "mkdir -p bin && echo same > bin/same.txt && echo new > bin/new.txt";
        let held = manifest_of(&[("bin/same.txt", b"same\n")]);
        let request = case.outputs_request(&commit, &outputs_keys(script, &["bin/**"]), &held);

        let ended = ran(&[], &request, Reading::All, &case.base.join("beaten"));

        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        assert_eq!(outputs_said(&ended), [packed_line(1)], "{:?}", ended.err);
        let archive = fs::read(case.archive()).expect("the archive is read");
        assert_eq!(members_of(&archive), ["bin/new.txt"], "the file the PC holds stays out");
        case.ended_clean();
    }

    #[test]
    fn a_pair_run_brings_back_from_its_main_tree_only() {
        let case = Case::new("outputs-pair");
        let commit = case.snapshot_holding(&[(".gitignore", "bin/\n")]);
        let script = "mkdir -p bin ../lib/bin && echo x > bin/x.txt && echo c > ../lib/c.txt && echo y > ../lib/bin/y.txt";
        let held = manifest_of(&[("a.txt", b"a\n")]);
        let mut keys = case.pair_with_lib();
        if let (Some(object), Some(outputs)) = (keys.as_object_mut(), outputs_keys(script, &["**/*.txt"]).as_object()) {
            object.extend(outputs.clone());
        }
        let request = case.outputs_request(&commit, &keys, &held);

        let ended = ran(&[], &request, Reading::All, &case.base.join("beaten"));

        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        assert_eq!(outputs_said(&ended), [packed_line(1)], "{:?}", ended.err);
        let back = ended
            .err
            .iter()
            .position(|line| unstamped(line).text.starts_with("gate: remote back commits: repo none, lib "));
        let packed = ended.err.iter().position(|line| *line == packed_line(1));
        assert!(
            back.is_some() && back < packed,
            "the back commits line, then the outputs line: {:?}",
            ended.err
        );
        let archive = fs::read(case.archive()).expect("the archive is read");
        assert_eq!(members_of(&archive), ["bin/x.txt"], "nothing of the sibling's tree comes back");
        case.ended_clean();
    }

    #[test]
    fn a_godot_console_command_with_a_version_runs_the_install_under_the_login_shell_s_godot_root() {
        let case = Case::new("godot");
        let root = case.base.join("godot root");
        let program = root.join("4.7.2").join(GODOT_PROGRAM);
        fs::create_dir_all(program.parent().expect("the program has a folder")).expect("the install is made");
        runnable(&program, "#!/bin/sh\nprintf '%s|%s\\n' \"$*\" \"$GODOT_PATH\"\n");
        // The login shell the box reads its PATH and Godot root from exports the root, then runs as `sh`.
        let shell = case.base.join("login-shell");
        runnable(
            &shell,
            &format!(
                "#!/bin/sh\nGATE_GODOT_ROOT='{}'\nexport GATE_GODOT_ROOT\nshift\nexec /bin/sh \"$@\"\n",
                root.display()
            ),
        );
        let commit = case.snapshot();
        let command = json!(["Godot_console", "--headless", "--path", "src/X", "--build-solutions", "--quit"]);
        let request = case.request(&commit, &json!({ "Command": command, "GodotVersion": "4.7.2" }));
        let ended = ran_in(&[], &request, Reading::All, &case.base.join("beaten"), Some(&shell));
        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        let said = format!("--headless --path src/X --build-solutions --quit|{}", program.display());
        assert_eq!(texts(&ended.out), [said.as_str()], "{:?}", ended.err);
        assert!(ended.err_has(&started_line(NONCE)), "the start marker on standard error: {:?}", ended.err);
        case.ended_clean();
    }

    #[test]
    fn every_line_of_the_command_is_stamped_out_or_err_and_the_protocol_lines_are_bare() {
        let case = Case::new("stamped");
        let commit = case.snapshot();
        let beaten = case.base.join("beaten");
        let command = waits_for_a_beat("echo one; echo two >&2; echo three", &beaten);
        let ended = ran(
            &[],
            &case.request(&commit, &json!({"Command": command, "Timeout": 3600})),
            Reading::All,
            &beaten,
        );
        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        assert!(ended.err_has(&started_line(NONCE)), "the start marker on standard error: {:?}", ended.err);
        let out: Vec<(Option<Tag>, &str)> = ended.out.iter().map(|line| (unstamped(line).tag, unstamped(line).text)).collect();
        assert_eq!(out, [(Some(Tag::Out), "one"), (Some(Tag::Out), "three")], "{:?}", ended.out);
        let two: Vec<Option<Tag>> = ended
            .err
            .iter()
            .filter(|line| unstamped(line).text == "two")
            .map(|line| unstamped(line).tag)
            .collect();
        assert_eq!(two, [Some(Tag::Err)], "{:?}", ended.err);
        for line in &ended.err {
            let read = unstamped(line);
            let protocol = read.text.starts_with("gate-remote-");
            assert!(protocol || read.tag.is_some(), "a line that is not a protocol line is stamped: {line}");
        }
        assert!(beaten.exists() && ended.adjusted(), "a heartbeat and the adjusted line: {:?}", ended.err);
        ended.protocol_bare();
        case.ended_clean();
    }

    #[test]
    fn the_root_line_comes_before_the_leader_s_spawn_whose_parent_is_the_box() {
        let case = Case::new("root");
        let commit = case.snapshot();
        let ended = ran(
            &[],
            &case.request(&commit, &json!({"Command": ["/bin/sh", "-c", "exit 0"]})),
            Reading::All,
            &case.base.join("beaten"),
        );
        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        let lines = ended.process_lines();
        let (root_at, root) = *lines.iter().find(|(_, text)| text.starts_with("gate pid=")).expect("a root line");
        let (spawn_at, spawn) = *lines.iter().find(|(_, text)| text.starts_with("spawn pid=")).expect("a spawn line");
        assert!(root_at < spawn_at, "the root line first: {:?}", ended.err);
        assert_eq!(
            word(root, "ppid"),
            Some(std::process::id().to_string().as_str()),
            "the root is the box this case started: {root}"
        );
        assert_eq!(word(spawn, "ppid"), word(root, "pid"), "the leader's parent is the box: {spawn}");
        assert!(spawn.ends_with(" sh"), "the leader's image is its program's file name: {spawn}");
        let leader = word(spawn, "pid").expect("the spawn names its pid");
        let exit = format!("exit pid={leader} code=0 ran=");
        assert!(
            lines.iter().any(|(at, text)| *at > spawn_at && text.starts_with(&exit)),
            "the leader's exit: {:?}",
            ended.err
        );
        case.ended_clean();
    }

    /// Checks each exit line of `lines` comes after the spawn line of its pid.
    fn exits_follow_spawns(lines: &[(usize, &str)]) {
        for (at, text) in lines.iter().filter(|(_, text)| text.starts_with("exit pid=")) {
            let pid = word(text, "pid");
            assert!(
                lines
                    .iter()
                    .any(|(spawned, spawn)| spawned < at && spawn.starts_with("spawn pid=") && word(spawn, "pid") == pid),
                "{text} has no spawn before it: {lines:?}"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_shell_s_background_sleep_has_its_spawn_and_exit_before_the_leader_s_exit() {
        let case = Case::new("descendant");
        let commit = case.snapshot();
        let command = json!(["/bin/sh", "-c", "sleep 0.2 & wait; exit 3"]);
        let ended = ran(
            &[],
            &case.request(&commit, &json!({"Command": command})),
            Reading::All,
            &case.base.join("beaten"),
        );
        assert_eq!(ended.status, Some(3), "{:?}", ended.err);
        let lines = ended.process_lines();
        let (_, leader) = *lines.iter().find(|(_, text)| text.starts_with("spawn pid=")).expect("the leader's spawn");
        let shell = word(leader, "pid").expect("the spawn names its pid");
        let found = lines
            .iter()
            .find(|(_, text)| text.starts_with("spawn pid=") && word(text, "ppid") == Some(shell))
            .copied();
        assert!(found.is_some(), "the sleep's spawn, its parent the shell: {:?}", ended.err);
        let (sleep_at, sleep) = found.expect("the sleep's spawn");
        let sleep_pid = word(sleep, "pid").expect("the spawn names its pid");
        let sleep_exit = lines
            .iter()
            .find(|(_, text)| text.starts_with(&format!("exit pid={sleep_pid} ")))
            .expect("the sleep's exit");
        assert!(sleep_exit.1.contains(" code=0 "), "{sleep_exit:?}");
        assert!(sleep_exit.0 > sleep_at);
        let sleep_said = lines.iter().filter(|(_, text)| word(text, "pid") == Some(sleep_pid)).count();
        assert_eq!(sleep_said, 2, "the sleep's spawn and exit, and no other line of it: {lines:?}");
        let leader_exit = lines
            .iter()
            .find(|(_, text)| text.starts_with(&format!("exit pid={shell} ")))
            .expect("the leader's exit");
        assert!(leader_exit.1.contains(" code=3 "), "{leader_exit:?}");
        assert!(leader_exit.0 > sleep_exit.0, "the sleep's exit before the leader's: {lines:?}");
        exits_follow_spawns(&lines);
        case.ended_clean();
    }

    #[test]
    fn a_killed_run_gives_the_leader_s_exit_by_signal_and_no_process_line_after_its_final_lines() {
        let case = Case::new("killed-lines");
        let commit = case.snapshot();
        let request = case.request(&commit, &json!({"Command": ["/bin/sh", "-c", "sleep 600"], "Timeout": 1}));
        // The whole machine free, so the ceiling's clock runs with wall time however loaded the machine is.
        let ended = ran(&["--test-box-free-percent", "100"], &request, Reading::All, &case.base.join("beaten"));
        assert_eq!(ended.status, Some(124), "{:?}", ended.err);
        let tag = ended
            .err
            .iter()
            .position(|line| *line == killed_line(NONCE, BoxKill::TimedOut))
            .expect("the kill tag, bare");
        let lines = ended.process_lines();
        let (_, leader) = *lines.iter().find(|(_, text)| text.starts_with("spawn pid=")).expect("the leader's spawn");
        let shell = word(leader, "pid").expect("the spawn names its pid");
        let exit = lines
            .iter()
            .find(|(_, text)| text.starts_with(&format!("exit pid={shell} signal=")))
            .expect("the leader's exit, by a signal");
        assert!(exit.0 < tag, "the leader's exit before the kill lines: {:?}", ended.err);
        assert!(
            lines.iter().all(|(at, _)| *at < tag),
            "no process line after the kill lines: {:?}",
            ended.err
        );
        exits_follow_spawns(&lines);
        ended.protocol_bare();
    }
}
