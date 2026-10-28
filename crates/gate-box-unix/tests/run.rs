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
    use std::os::unix::process::CommandExt;
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
            self.inputs_request_with(step, &json!({}))
        }

        /// [`Case::inputs_request`], with `extra` keys set over it.
        fn inputs_request_with(&self, step: &str, extra: &Value) -> String {
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
                "Step": step,
            });
            if let (Some(object), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
                object.extend(extra.clone());
            }
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
        unsafe { libc::poll(&raw mut polled, 1, READ_SLICE_MS) != 0 }
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
    fn the_command_makes_files_under_a_022_umask_while_the_box_s_git_makes_its_own_private() {
        let case = Case::new("umask");
        let commit = case.snapshot();
        let request = case.request(&commit, &json!({"Command": ["/bin/sh", "-c", "echo x > made.txt"]}));
        let ended = ran(&[], &request, Reading::All, &case.base.join("beaten"));
        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        let mode = |path: String| std::os::unix::fs::MetadataExt::mode(&fs::symlink_metadata(&path).expect("it is looked at")) & 0o777;
        assert_eq!(mode(format!("{}/made.txt", case.work())), 0o644, "the command's file");
        assert_eq!(
            mode(format!("{}/.git/config", case.work())),
            0o600,
            "the fresh tree's config, made by the box's git"
        );
    }

    #[test]
    fn a_tracked_file_the_checkout_writes_is_readable_as_on_the_pc() {
        let case = Case::new("checkout-umask");
        let commit = case.snapshot();
        let ended = ran(&[], &case.request(&commit, &json!({})), Reading::All, &case.base.join("beaten"));
        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        let looked = fs::symlink_metadata(format!("{}/a.txt", case.work())).expect("the tracked file is looked at");
        assert_eq!(std::os::unix::fs::MetadataExt::mode(&looked) & 0o777, 0o644, "the checkout's file");
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
        pruned_with(&[], words, env)
    }

    /// [`pruned`] with the box's test seams `seams` given before the verb.
    fn pruned_with(seams: &[&str], words: &[&str], env: &[(&str, &str)]) -> Ended {
        let ran = Command::new(env!("CARGO_BIN_EXE_gate-box"))
            .arg("box")
            .args(seams)
            .arg("prune")
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

    /// A login shell for `case` whose `PATH` starts with a folder holding only the runnable `program`, `script` its
    /// text; the shell's path.
    fn shell_with_shim(case: &Case, program: &str, script: &str) -> PathBuf {
        let shims = case.base.join("shims");
        fs::create_dir_all(&shims).expect("the shim folder is made");
        runnable(&shims.join(program), script);
        let shell = case.base.join("login-shell");
        runnable(
            &shell,
            &format!(
                "#!/bin/sh\nPATH='{}':\"$PATH\"\nexport PATH\nshift\nexec /bin/sh \"$@\"\n",
                shims.display()
            ),
        );
        shell
    }

    /// The lines of the shim's log at `log`; none when the shim never wrote it.
    fn logged(log: &Path) -> Vec<String> {
        fs::read_to_string(log)
            .map(|text| text.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    /// The package files of a node project of `lockfile`, its `node_modules` ignored.
    fn node_project(lockfile: &'static str) -> [(&'static str, &'static str); 3] {
        [(lockfile, "{}\n"), ("package.json", "{}\n"), (".gitignore", "node_modules/\n")]
    }

    #[test]
    fn a_pnpm_run_restores_its_packages_before_the_command_and_skips_them_the_next_time() {
        let case = Case::new("pnpm");
        let log = case.base.join("pnpm.log");
        let shim = format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\nif [ \"$1\" = install ]; then mkdir -p node_modules; fi\n",
            log.display()
        );
        let shell = shell_with_shim(&case, "pnpm", &shim);
        let commit = case.snapshot_holding(&node_project("pnpm-lock.yaml"));
        let request = case.request(&commit, &json!({ "Command": ["pnpm", "test"] }));
        let ended = ran_in(&[], &request, Reading::All, &case.base.join("beaten"), Some(&shell));
        assert_eq!(ended.status, Some(0), "the command's status: {:?}", ended.err);
        assert_eq!(logged(&log), ["install --frozen-lockfile", "test"], "the install, then the command");
        assert!(
            ended.err_texts().iter().any(|line| line.starts_with("gate: pnpm install took ")),
            "{:?}",
            ended.err
        );
        let record = fs::read_to_string(format!("{}.packages", case.work())).expect("the packages record is written");
        assert!(
            record.lines().any(|line| line
                .strip_prefix(".\t")
                .is_some_and(|digest| digest.len() == 64 && digest.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')))),
            "the work folder's line, a SHA-256 in hex: {record:?}"
        );
        case.ended_clean();

        let again = ran_in(&[], &request, Reading::All, &case.base.join("beaten-again"), Some(&shell));
        assert_eq!(again.status, Some(0), "the command's status: {:?}", again.err);
        assert_eq!(
            logged(&log),
            ["install --frozen-lockfile", "test", "test"],
            "the command alone, no second install: {:?}",
            again.err
        );
        case.ended_clean();
    }

    #[test]
    fn a_failing_npm_install_refuses_the_run_75_with_the_tools_tag_and_never_runs_the_command() {
        let case = Case::new("npm-fails");
        let log = case.base.join("npm.log");
        let shim = format!("#!/bin/sh\nif [ \"$1\" = ci ]; then exit 1; fi\necho \"$*\" >> '{}'\n", log.display());
        let shell = shell_with_shim(&case, "npm", &shim);
        let commit = case.snapshot_holding(&node_project("package-lock.json"));
        let request = case.request(&commit, &json!({ "Command": ["npm", "test"] }));
        let ended = ran_in(&[], &request, Reading::All, &case.base.join("beaten"), Some(&shell));
        assert_eq!(ended.status, Some(75), "{:?}", ended.err);
        let tag = ended.err.iter().position(|line| *line == refused_line(NONCE, Refusal::Tools));
        let packages = format!("gate: remote work folder {KEY} could not restore its npm packages");
        assert!(
            tag.is_some_and(|tag| ended.err_texts().get(tag + 1) == Some(&packages.as_str())),
            "the tools tag, then the packages line: {:?}",
            ended.err
        );
        assert!(
            !logged(&log).iter().any(|line| line == "test"),
            "the command never ran: {:?}",
            logged(&log)
        );
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
            self.outputs_sent_as(name, &json!({}), &[])
        }

        /// [`Case::outputs_sent`], with `extra` keys set over the payload and the box given the seams `seams`.
        fn outputs_sent_as(&self, name: &str, extra: &Value, seams: &[&str]) -> (Option<i32>, Vec<u8>, Vec<String>) {
            let (root, work) = (self.root(), self.work());
            let mut payload = json!({ "Key": KEY, "Work": work, "OutputFolder": format!("{root}/outputs"), "Name": name });
            if let (Some(object), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
                object.extend(extra.clone());
            }
            let mut child = Command::new(env!("CARGO_BIN_EXE_gate-box"))
                .arg("box")
                .args(seams)
                .args(["outputs", "--stdin"])
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

    /// `ended` said, on stderr, the start line of a cross build for `win-x64` running `cross`, before the first line
    /// `then` holds.
    fn assert_started_cross_build(ended: &Ended, cross: &str, then: impl Fn(&str) -> bool, what: &str) {
        let texts = ended.err_texts();
        let start = format!("gate: cross build for win-x64: /bin/sh -c {cross}");
        let at = texts.iter().position(|said| *said == start.as_str());
        let after = texts.iter().position(|said| then(said));
        assert!(
            matches!((at, after), (Some(at), Some(after)) if at < after),
            "{start} before {what}: {:?}",
            ended.err
        );
    }

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
        assert_started_cross_build(
            &ended,
            &cross,
            |said: &str| said.starts_with("gate: the cross build for win-x64 took "),
            "the passed line",
        );
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
    fn cross_build_that_fails_says_so_and_brings_nothing_back_keeping_the_command_s_status() {
        let case = Case::new("cross-failed");
        let commit = case.snapshot_holding(&[(".gitignore", "bin/\n")]);
        let script = format!("mkdir -p {NATIVE_FOLDER} && echo native > {NATIVE_FOLDER}/A.dll");
        let request = case.outputs_request(&commit, &cross_keys(&script, "echo cross said; exit 3"), &Manifest::default());

        let ended = ran(&[], &request, Reading::All, &case.base.join("beaten"));

        assert_eq!(ended.status, Some(0), "the command's own status: {:?}", ended.err);
        assert_said_then_not_brought_back(&ended, &cross_failed_line(3), CROSS_FAILED);
        assert_started_cross_build(
            &ended,
            "echo cross said; exit 3",
            |said: &str| cross_failed_line(3) == said,
            "the failure line",
        );
        assert!(texts(&ended.out).contains(&"cross said"), "its output is forwarded: {:?}", ended.out);
        assert!(outputs_said(&ended).is_empty(), "{:?}", ended.err);
        assert!(!case.archive().exists(), "nothing is packed");
        case.ended_clean();
    }

    #[test]
    fn cross_build_that_leaves_a_mach_o_among_its_outputs_brings_nothing_back_keeping_the_command_s_status() {
        let case = Case::new("cross-mach-o");
        let commit = case.snapshot_holding(&[(".gitignore", "bin/\n")]);
        let cross = format!("mkdir -p {CROSS_FOLDER} && printf '{PE}' > {CROSS_FOLDER}/A.exe && printf '{MACH_O}' > {CROSS_FOLDER}/A");
        let request = case.outputs_request(&commit, &cross_keys("true", &cross), &Manifest::default());

        let ended = ran(&[], &request, Reading::All, &case.base.join("beaten"));

        assert_eq!(ended.status, Some(0), "the command's own status: {:?}", ended.err);
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
        assert_eq!(
            outputs_said(&ended),
            ["gate-remote-outputs: 0"],
            "its TestResults glob matched nothing: {:?}",
            ended.err
        );
        assert!(!case.archive().exists(), "nothing is packed");
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

    /// The Unix second the drain cases judge at, through `--test-box-now`.
    const DRAIN_NOW: u64 = 1_800_000_000;

    /// The lines of a drain with no barrier, no ticket and nothing going.
    const OPEN_LINES: [&str; 3] = ["gate: drain barrier none", "gate: drain tickets 0", "gate: drain state open"];

    /// Runs `gate-box box --test-box-now <DRAIN_NOW> --test-box-drain-readable-only true drain --stdin` asking `action`
    /// of the root `root`; its status and its lines. The process scan counts only what it can read, so a process of
    /// another case caught mid-exec never holds this case's drain.
    fn drained(root: &str, action: &str) -> Ended {
        drained_at(root, action, DRAIN_NOW)
    }

    /// [`drained`] at the Unix second `now`.
    fn drained_at(root: &str, action: &str, now: u64) -> Ended {
        let request = STANDARD.encode(json!({ "Root": root, "Action": action }).to_string());
        let now = now.to_string();
        let mut child = Command::new(env!("CARGO_BIN_EXE_gate-box"))
            .args([
                "box",
                "--test-box-now",
                &now,
                "--test-box-drain-readable-only",
                "true",
                "drain",
                "--stdin",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("gate-box starts");
        let mut stdin = child.stdin.take().expect("stdin is piped");
        // A box that refuses the verb before reading its request closes stdin early; its lines are read all the same.
        let _ = stdin.write_all(format!("{request}\n").as_bytes());
        drop(stdin);
        ended_of(&child.wait_with_output().expect("gate-box is waited for"))
    }

    /// The JSON of the drain record `name` under the root `root`.
    fn drain_record(root: &str, name: &str) -> Value {
        let text = fs::read_to_string(format!("{root}/drain/{name}")).expect("the drain record is read");
        serde_json::from_str(&text).expect("the drain record is JSON")
    }

    /// The permission bits of `path`, looked at as itself.
    fn mode_of(path: &str) -> u32 {
        std::os::unix::fs::MetadataExt::mode(&fs::symlink_metadata(path).expect("the path is looked at")) & 0o7777
    }

    #[test]
    fn drain_raise_status_lift_round_trip() {
        let case = Case::new("drain-round-trip");
        let root = case.root();
        let raised = format!("gate: drain barrier {DRAIN_NOW} {}", DRAIN_NOW + 1800);
        let drained_lines = [raised.as_str(), "gate: drain tickets 0", "gate: drain state drained"];

        let raise = drained(&root, "raise");
        assert_eq!(raise.status, Some(0), "{:?}", raise.err);
        assert_eq!(raise.out, drained_lines);
        assert!(raise.err.is_empty(), "{:?}", raise.err);
        assert_eq!(
            drain_record(&root, "barrier"),
            json!({ "Raised": DRAIN_NOW, "Lapses": DRAIN_NOW + 1800, "Generation": 1 })
        );
        for folder in ["drain", "drain/tickets", "drain/sessions"] {
            assert_eq!(mode_of(&format!("{root}/{folder}")), 0o700, "{folder}");
        }

        let status = drained(&root, "status");
        assert_eq!(status.status, Some(0), "{:?}", status.err);
        assert_eq!(status.out, drained_lines);
        let again = drained(&root, "raise");
        assert_eq!(again.status, Some(0), "{:?}", again.err);
        assert_eq!(
            drain_record(&root, "barrier")["Generation"],
            json!(2),
            "a raise while one stands is one generation on"
        );

        let lift = drained(&root, "lift");
        assert_eq!(lift.status, Some(0), "{:?}", lift.err);
        assert_eq!(lift.out, OPEN_LINES);
        assert!(!Path::new(&format!("{root}/drain/barrier")).exists(), "the lift removes the barrier");
        assert_eq!(drain_record(&root, "lifted"), json!({ "At": DRAIN_NOW }));
        let after = drained(&root, "status");
        assert_eq!(after.status, Some(0), "{:?}", after.err);
        assert_eq!(after.out, OPEN_LINES);
    }

    #[test]
    fn drain_status_on_a_missing_root_reads_open() {
        let case = Case::new("drain-missing-root");
        let missing = case.base.join("missing");
        let root = missing.join("gate-remote");
        for action in ["status", "lift"] {
            let ended = drained(root.to_str().expect("the temp folder is UTF-8"), action);
            assert_eq!(ended.status, Some(0), "{action}: {:?}", ended.err);
            assert_eq!(ended.out, OPEN_LINES, "{action}");
            assert!(ended.err.is_empty(), "{action}: {:?}", ended.err);
        }
        assert!(!missing.exists(), "no root is made");

        let without_drain = drained(&case.root(), "status");
        assert_eq!(without_drain.status, Some(0), "{:?}", without_drain.err);
        assert_eq!(without_drain.out, OPEN_LINES);
        assert!(!Path::new(&format!("{}/drain", case.root())).exists(), "a root with no drain/ is left so");
    }

    /// A planted shape's name, how it is planted, and whether a file type is that shape.
    type Shape<'a> = (&'a str, &'a dyn Fn(), fn(&fs::FileType) -> bool);

    #[test]
    fn a_planted_drain_link_file_or_open_folder_is_moved_aside() {
        use std::os::unix::fs::PermissionsExt;
        let case = Case::new("drain-planted");
        let root = case.root();
        let drain = format!("{root}/drain");
        let moved = format!("{root}/drain.gate-moved-1");
        let standing = json!({ "Raised": DRAIN_NOW, "Lapses": DRAIN_NOW + 1800, "Generation": 1 }).to_string();
        let elsewhere = case.base.join("elsewhere");
        fs::create_dir(&elsewhere).expect("the link's target is made");
        fs::write(elsewhere.join("barrier"), &standing).expect("the target's barrier is written");
        let moved_line = format!(
            "gate: drain moved aside {} to drain.gate-moved-1: it was not the box's protected folder",
            escaped(&drain)
        );

        let plant_link = || std::os::unix::fs::symlink(&elsewhere, &drain).expect("the link is planted");
        let plant_file = || fs::write(&drain, &standing).expect("the file is planted");
        let plant_open_folder = || {
            fs::create_dir(&drain).expect("the folder is planted");
            fs::write(format!("{drain}/barrier"), &standing).expect("the folder's barrier is written");
            fs::set_permissions(&drain, fs::Permissions::from_mode(0o755)).expect("the folder is opened to others");
        };
        let shapes: [Shape<'_>; 3] = [
            ("link", &plant_link, fs::FileType::is_symlink),
            ("file", &plant_file, fs::FileType::is_file),
            ("open folder", &plant_open_folder, fs::FileType::is_dir),
        ];
        for (shape, plant, is_shape) in shapes {
            plant();
            let ended = drained(&root, "status");
            assert_eq!(ended.status, Some(0), "{shape}: {:?}", ended.err);
            assert_eq!(ended.out, OPEN_LINES, "{shape}: the planted barrier is never read");
            assert_eq!(ended.err, [moved_line.as_str()], "{shape}");
            let aside = fs::symlink_metadata(&moved).expect("the planted drain is moved aside");
            assert!(is_shape(&aside.file_type()), "{shape}: moved aside as itself");
            assert!(fs::symlink_metadata(&drain).expect("a drain is made").is_dir(), "{shape}");
            assert_eq!(mode_of(&drain), 0o700, "{shape}");

            fs::remove_dir_all(&drain).expect("the made drain is removed");
            if aside.is_dir() {
                fs::remove_dir_all(&moved).expect("the moved folder is removed");
            } else {
                fs::remove_file(&moved).expect("the moved link or file is removed");
            }
        }
        assert_eq!(
            fs::read_to_string(elsewhere.join("barrier")).expect("the link's target is untouched"),
            standing
        );
    }

    #[test]
    fn a_lapsed_barrier_reads_lapsed_and_is_recorded() {
        let case = Case::new("drain-lapsed");
        let root = case.root();
        let raise = drained(&root, "raise");
        assert_eq!(raise.status, Some(0), "{:?}", raise.err);
        let lapses = DRAIN_NOW + 1800;
        let lapsed = format!("gate: drain barrier lapsed {lapses}");
        let lapsed_lines = [lapsed.as_str(), "gate: drain tickets 0", "gate: drain state open"];

        let standing = drained_at(&root, "status", lapses - 1);
        assert_eq!(
            standing.out[0],
            format!("gate: drain barrier {DRAIN_NOW} {lapses}"),
            "it stands until its lapse"
        );
        let at_lapse = drained_at(&root, "status", lapses);
        assert_eq!(at_lapse.status, Some(0), "{:?}", at_lapse.err);
        assert_eq!(at_lapse.out, lapsed_lines);
        assert!(!Path::new(&format!("{root}/drain/barrier")).exists(), "the lapsed barrier is removed");
        assert_eq!(drain_record(&root, "lapsed"), json!({ "At": lapses }));
        let later = drained_at(&root, "status", lapses + 600);
        assert_eq!(later.out, lapsed_lines, "read again from the lapsed record");
    }

    #[test]
    fn an_unreadable_barrier_stands_as_raised() {
        let case = Case::new("drain-unreadable");
        let root = case.root();
        let raise = drained(&root, "raise");
        assert_eq!(raise.status, Some(0), "{:?}", raise.err);
        let barrier = format!("{root}/drain/barrier");
        fs::write(&barrier, "{not json").expect("the barrier is damaged");
        let written = fs::metadata(&barrier)
            .and_then(|metadata| metadata.modified())
            .expect("the barrier's last write is read")
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after 1970")
            .as_secs();

        let within = drained_at(&root, "status", written + 1799);
        assert_eq!(within.status, Some(0), "{:?}", within.err);
        let stands = format!("gate: drain barrier {written} {}", written + 1800);
        assert_eq!(within.out, [stands.as_str(), "gate: drain tickets 0", "gate: drain state drained"]);
        let past = drained_at(&root, "status", written + 1800);
        assert_eq!(
            past.out[0],
            format!("gate: drain barrier lapsed {}", written + 1800),
            "for one lapse and no longer"
        );
    }

    #[test]
    fn a_planted_tickets_or_sessions_folder_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let case = Case::new("drain-planted-inner");
        let root = case.root();
        let raise = drained(&root, "raise");
        assert_eq!(raise.status, Some(0), "{:?}", raise.err);
        let (tickets, sessions) = (format!("{root}/drain/tickets"), format!("{root}/drain/sessions"));

        fs::remove_dir(&tickets).expect("the tickets folder is removed");
        fs::write(&tickets, "").expect("a file is planted at tickets");
        let as_file = drained(&root, "status");
        fs::remove_file(&tickets).expect("the planted file is removed");
        fs::set_permissions(&sessions, fs::Permissions::from_mode(0o755)).expect("sessions is opened to others");
        let as_open = drained(&root, "status");

        let refused = |path: &str| format!("gate: drain failed: {} is not the box user's own folder at mode 0700", escaped(path));
        assert_eq!(as_file.status, Some(1), "{:?}", as_file.err);
        assert_eq!(as_file.err, [refused(&tickets)]);
        assert!(as_file.out.is_empty(), "{:?}", as_file.out);
        assert_eq!(as_open.status, Some(1), "{:?}", as_open.err);
        assert_eq!(as_open.err, [refused(&sessions)]);
        assert_eq!(mode_of(&sessions), 0o755, "a planted folder within drain/ is left as it is");
    }

    #[test]
    fn a_drain_on_an_untrusted_root_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let case = Case::new("drain-untrusted");
        let root = case.root();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).expect("the root is opened to others");

        let ended = drained(&root, "raise");

        assert_eq!(ended.status, Some(1), "{:?}", ended.err);
        assert_eq!(ended.err.len(), 1, "{:?}", ended.err);
        let wanted = format!("gate: drain failed: the builder's work root is not locked down: {root} is not owned by ");
        assert!(ended.err[0].starts_with(&wanted), "{:?}", ended.err);
        assert!(ended.err[0].contains(" or is open to others; run the installer's "), "{:?}", ended.err);
        assert!(!Path::new(&format!("{root}/drain")).exists(), "nothing is made under a root not trusted");
    }

    /// Runs `gate-box box <words> --stdin` with `input` on its stdin; its status and its lines. A box that ends before
    /// reading all its input has its lines read all the same.
    fn boxed(words: &[&str], input: Vec<u8>) -> Ended {
        let mut child = Command::new(env!("CARGO_BIN_EXE_gate-box"))
            .arg("box")
            .args(words)
            .arg("--stdin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("gate-box starts");
        let mut stdin = child.stdin.take().expect("stdin is piped");
        let writer = thread::spawn(move || stdin.write_all(&input));
        let ended = ended_of(&child.wait_with_output().expect("gate-box is waited for"));
        // A box refused before it reads its input closes stdin early, and the write fails; that is no failure here.
        drop(writer.join().expect("the input's writer ends"));
        ended
    }

    /// The payload keys naming the drain session `id` of the PC `pc1` and the repo `repo`.
    fn session_keys(id: &str) -> Value {
        json!({ "Session": id, "Pc": "pc1", "Repo": "repo" })
    }

    /// Runs `gate-box box --test-box-now <DRAIN_NOW> preflight --stdin` for the work folder of `case`, with `extra` keys
    /// set over its payload; its status and its lines.
    fn preflighted(case: &Case, extra: &Value) -> Ended {
        let now = DRAIN_NOW.to_string();
        boxed(
            &["--test-box-now", &now, "preflight"],
            format!("{}\n", case.request("", extra)).into_bytes(),
        )
    }

    /// The lines a call refused by the barrier raised at [`DRAIN_NOW`] says, at `place` in line.
    fn drain_refusal(place: &str) -> [String; 3] {
        [
            refused_line(NONCE, Refusal::Drain),
            format!("gate: drain place {place}"),
            format!("gate: drain barrier {DRAIN_NOW} {}", DRAIN_NOW + 1800),
        ]
    }

    /// The names in the drain folder `folder` of the root `root`, in order.
    fn drain_names(root: &str, folder: &str) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(format!("{root}/drain/{folder}"))
            .expect("the drain folder is listed")
            .map(|entry| entry.expect("an entry is read").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Raises the barrier on the root `root` at [`DRAIN_NOW`].
    fn raised(root: &str) {
        let raise = drained(root, "raise");
        assert_eq!(raise.status, Some(0), "{:?}", raise.err);
    }

    #[test]
    fn a_preflight_under_a_barrier_refuses_75_after_its_version_line() {
        let case = Case::new("drain-preflight");
        let root = case.root();
        raised(&root);

        let ended = preflighted(&case, &session_keys("s1"));

        assert_eq!(ended.status, Some(75), "{:?}", ended.err);
        assert_eq!(ended.out.len(), 1, "only the version line: {:?}", ended.out);
        assert!(ended.out[0].starts_with("gate: remote version "), "{:?}", ended.out);
        assert_eq!(ended.err, drain_refusal("1"));
        assert_eq!(
            drain_record(&root, "tickets/00000001-s1"),
            json!({ "Session": "s1", "Polled": DRAIN_NOW }),
            "the refused session takes ticket 1"
        );
        assert!(!Path::new(&case.work()).exists(), "a refused preflight makes no work folder");
    }

    #[test]
    fn a_no_ticket_preflight_under_a_barrier_refuses_and_takes_no_place() {
        let case = Case::new("drain-no-ticket");
        let root = case.root();
        raised(&root);

        let ended = preflighted(&case, &json!({ "Session": "s1", "Pc": "pc1", "Repo": "repo", "NoTicket": true }));

        assert_eq!(ended.status, Some(75), "{:?}", ended.err);
        assert_eq!(ended.err, drain_refusal("1"));
        assert!(drain_names(&root, "tickets").is_empty(), "no ticket is taken");
        let next = preflighted(&case, &session_keys("s2"));
        assert_eq!(next.err, drain_refusal("1"), "no place was taken ahead of the next session");
    }

    #[test]
    fn a_waiting_session_is_admitted_by_its_preflight_after_the_lift() {
        let case = Case::new("drain-after-lift");
        let root = case.root();
        raised(&root);
        let waiting = preflighted(&case, &session_keys("s1"));
        assert_eq!(waiting.status, Some(75), "{:?}", waiting.err);
        let lift = drained(&root, "lift");
        assert_eq!(lift.status, Some(0), "{:?}", lift.err);

        let admitted = preflighted(&case, &session_keys("s1"));

        assert_eq!(admitted.status, Some(0), "{:?}", admitted.err);
        assert!(
            admitted.out.first().is_some_and(|line| line.starts_with("gate: remote version ")),
            "{:?}",
            admitted.out
        );
        assert!(drain_names(&root, "tickets").is_empty(), "the admitted session gives up its ticket");
        assert_eq!(
            drain_record(&root, "sessions/s1"),
            json!({ "Pc": "pc1", "Repo": "repo", "Command": "", "Job": "", "Called": DRAIN_NOW })
        );
        assert_eq!(drain_names(&root, "sessions"), ["s1"], "the holder is removed at the call's end");
    }

    #[test]
    fn a_run_of_a_dead_session_under_a_barrier_is_refused_drain() {
        let case = Case::new("drain-run-dead");
        let commit = case.snapshot();
        let root = case.root();
        raised(&root);
        let now = DRAIN_NOW.to_string();

        let ended = ran(
            &["--test-box-now", &now],
            &case.request(&commit, &session_keys("s9")),
            Reading::All,
            &case.base.join("beaten"),
        );

        assert_eq!(ended.status, Some(75), "{:?}", ended.err);
        assert!(ended.out.is_empty(), "nothing ran: {:?}", ended.out);
        assert!(!ended.err_has(&started_line(NONCE)), "no start marker: {:?}", ended.err);
        let refusal = drain_refusal("1");
        let tag = ended.err.iter().position(|line| *line == refusal[0]);
        assert!(
            tag.is_some_and(|tag| ended.err.get(tag..tag + 3) == Some(&refusal[..])),
            "the drain tag, then the place and barrier lines, bare: {:?}",
            ended.err
        );
        ended.protocol_bare();
        assert!(drain_names(&root, "tickets").is_empty(), "a run takes no ticket");
        assert!(case.lock_free(), "the work lock is free");
    }

    #[test]
    fn a_run_of_a_live_session_under_a_barrier_runs() {
        let case = Case::new("drain-run-live");
        let commit = case.snapshot();
        let root = case.root();
        raised(&root);
        let record = json!({ "Pc": "pc1", "Repo": "repo", "Command": "", "Job": "", "Called": DRAIN_NOW });
        fs::write(format!("{root}/drain/sessions/s1"), record.to_string()).expect("the session's record is written");
        let later = (DRAIN_NOW + 60).to_string();

        let ended = ran(
            &["--test-box-now", &later],
            &case.request(&commit, &session_keys("s1")),
            Reading::All,
            &case.base.join("beaten"),
        );

        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        assert!(ended.err_has(&started_line(NONCE)), "the run started: {:?}", ended.err);
        assert_eq!(
            drain_record(&root, "sessions/s1"),
            json!({
                "Pc": "pc1",
                "Repo": "repo",
                "Command": "true",
                "Job": format!("gate-remote-{KEY}-0123456789ab"),
                "Called": DRAIN_NOW + 60
            }),
            "the run records its command and job, and its end"
        );
        assert_eq!(drain_names(&root, "sessions"), ["s1"], "the holder is removed at the run's end");
        case.ended_clean();
    }

    #[test]
    fn an_inputs_call_under_a_barrier_is_refused_before_its_manifest_is_read() {
        let case = Case::new("drain-inputs");
        let root = case.root();
        raised(&root);
        let now = DRAIN_NOW.to_string();
        let request = case.inputs_request_with("diff", &session_keys("s9"));

        let ended = boxed(&["--test-box-now", &now, "inputs"], format!("{request}\nnot a manifest\n").into_bytes());

        assert_eq!(ended.status, Some(75), "{:?}", ended.err);
        assert_eq!(ended.err, drain_refusal("1"), "refused drain, the manifest never read");
        assert!(ended.out.is_empty(), "{:?}", ended.out);
        assert!(!Path::new(&case.work()).exists(), "nothing is laid out");
    }

    #[test]
    fn outputs_of_a_dead_session_under_a_barrier_keeps_the_archive() {
        let case = Case::new("drain-outputs");
        let root = case.root();
        raised(&root);
        std::os::unix::fs::DirBuilderExt::mode(&mut fs::DirBuilder::new(), 0o700)
            .create(format!("{root}/outputs"))
            .expect("the output folder is made");
        fs::write(case.archive(), b"archive bytes").expect("the archive is written");
        let now = DRAIN_NOW.to_string();

        let (status, sent, err) = case.outputs_sent_as(&archive_name(), &json!({ "Session": "s9" }), &["--test-box-now", &now]);

        assert_eq!(status, Some(0), "{err:?}");
        assert_eq!(sent, b"archive bytes", "the archive is sent");
        assert_eq!(err, ["gate: remote outputs kept: the builder is drained"]);
        assert!(case.archive().exists(), "and kept");
    }

    #[test]
    fn a_holder_is_not_inherited_by_the_routed_command() {
        let case = Case::new("drain-holder-cloexec");
        let commit = case.snapshot();
        let root = case.root();
        let (held, open) = (case.base.join("held.txt"), case.base.join("open.txt"));
        // The command lists the holders while it runs, then the files its own shell holds open.
        let script = format!(
            "ls '{root}/drain/sessions' > '{}'; /usr/sbin/lsof -a -p $$ -Fn > '{}'",
            held.display(),
            open.display()
        );
        let mut keys = session_keys("s1");
        keys["Command"] = json!(["/bin/sh", "-c", script]);

        let ended = ran(&[], &case.request(&commit, &keys), Reading::All, &case.base.join("beaten"));

        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        let holders = fs::read_to_string(&held).expect("the holders were listed");
        assert!(
            holders.lines().any(|name| name.starts_with("s1.")),
            "the run held its holder while its command ran: {holders:?}"
        );
        let files = fs::read_to_string(&open).expect("the open files were listed");
        assert!(
            files.lines().any(|line| line.starts_with('n')),
            "lsof listed the shell's files: {files:?}"
        );
        assert!(!files.contains("/drain/"), "the command holds no file of the drain open: {files:?}");
        assert_eq!(drain_names(&root, "sessions"), ["s1"], "the holder is removed at the run's end");
    }

    #[test]
    fn a_session_with_a_job_is_not_removed_on_its_lease_alone() {
        let case = Case::new("drain-job-session");
        let root = case.root();
        raised(&root);
        let called_long_ago = |job: &str| json!({ "Pc": "pc1", "Repo": "repo", "Command": "x", "Job": job, "Called": DRAIN_NOW - 1000 });
        fs::write(format!("{root}/drain/sessions/s1"), called_long_ago("gate-remote-job").to_string()).expect("a record naming a job is written");
        fs::write(format!("{root}/drain/sessions/s2"), called_long_ago("").to_string()).expect("a record naming no job is written");
        // The job is live by its work folder's group record, which names it and a group still its run's: the leader of a
        // group of its own, started before the record was written.
        let mut leader = Command::new("/bin/sleep")
            .arg("600")
            .process_group(0)
            .spawn()
            .expect("the group's leader starts");
        fs::write(format!("{}.pgid", case.work()), format!("gate-remote-job\n{}\n", leader.id())).expect("the group record is written");

        let status = drained(&root, "status");
        leader.kill().expect("the leader is killed");
        leader.wait().expect("the leader is waited for");

        assert_eq!(status.status, Some(0), "{:?}", status.err);
        let sessions: Vec<&String> = status.out.iter().filter(|line| line.starts_with("gate: drain session ")).collect();
        assert_eq!(sessions.len(), 1, "{:?}", status.out);
        assert!(sessions[0].starts_with("gate: drain session 1000 "), "{:?}", status.out);
        assert_eq!(status.out.last().map(String::as_str), Some("gate: drain state waiting"));
        assert_eq!(drain_names(&root, "sessions"), ["s1"], "only the record naming no job is removed");
    }

    // The process scan is built for macOS only.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_process_naming_the_root_keeps_the_drain_waiting() {
        let case = Case::new("drain-process");
        let root = case.root();
        raised(&root);
        // The shell waits on its input itself, so no child of it outlives it, and its last argument names the root.
        let mut held = Command::new("/bin/sh")
            .args(["-c", "read line", "sh", &case.work()])
            .stdin(Stdio::piped())
            .spawn()
            .expect("the shell starts");
        let pid = held.id();

        let waiting = drained(&root, "status");
        held.kill().expect("the shell is killed");
        held.wait().expect("the shell is waited for");
        let after = drained(&root, "status");

        assert_eq!(waiting.status, Some(0), "{:?}", waiting.err);
        assert!(
            waiting.out.contains(&format!("gate: drain process {pid} git")),
            "the shell naming the root is counted: {:?}",
            waiting.out
        );
        assert_eq!(waiting.out.last().map(String::as_str), Some("gate: drain state waiting"));
        assert_eq!(after.status, Some(0), "{:?}", after.err);
        assert_eq!(after.out.last().map(String::as_str), Some("gate: drain state drained"), "{:?}", after.out);
    }

    // The process scan is built for macOS only.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_box_preflight_polling_during_the_status_does_not_hold_the_drain() {
        let case = Case::new("drain-box-poll");
        let root = case.root();
        raised(&root);
        // A program at the box's own path, as each box call runs it, waiting on its input; its arguments name the root.
        std::os::unix::fs::DirBuilderExt::mode(&mut fs::DirBuilder::new(), 0o700)
            .create(format!("{root}/bin"))
            .expect("the box's folder is made");
        let program = format!("{root}/bin/gate-box");
        // `cat` reads its input first, so it waits there before it would read the work folder it names.
        fs::copy("/bin/cat", &program).expect("a stand-in for the box is copied");
        let mut polling = Command::new(&program)
            .args(["-", &case.work()])
            .stdin(Stdio::piped())
            .spawn()
            .expect("the stand-in starts");

        let status = drained(&root, "status");
        polling.kill().expect("the stand-in is killed");
        polling.wait().expect("the stand-in is waited for");

        assert_eq!(status.status, Some(0), "{:?}", status.err);
        assert_eq!(
            status.out.last().map(String::as_str),
            Some("gate: drain state drained"),
            "{:?}",
            status.out
        );
    }

    #[test]
    fn an_inputs_call_on_an_untrusted_root_touches_no_drain() {
        use std::os::unix::fs::PermissionsExt;
        let case = Case::new("drain-inputs-untrusted");
        let root = case.root();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).expect("the root is opened to others");
        let now = DRAIN_NOW.to_string();
        let request = case.inputs_request_with("diff", &session_keys("s1"));

        let ended = boxed(&["--test-box-now", &now, "inputs"], format!("{request}\nnot a manifest\n").into_bytes());

        assert_ne!(ended.status, Some(0), "{:?}", ended.err);
        assert!(
            ended.err.iter().any(|line| line.contains("work root is not locked down")),
            "the root's own line: {:?}",
            ended.err
        );
        assert!(!Path::new(&format!("{root}/drain")).exists(), "nothing is made under drain/");
    }

    // The process scan is built for macOS only.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_run_without_a_session_keeps_the_drain_waiting() {
        let case = Case::new("drain-sessionless-run");
        let commit = case.snapshot();
        let root = case.root();
        // Two pipes the command waits on, outside the root: it says it is up on one, then waits to be let go on the other.
        let (up, go) = (case.base.join("up"), case.base.join("go"));
        for pipe in [&up, &go] {
            let made = Command::new("mkfifo").arg(pipe).status().expect("mkfifo runs");
            assert!(made.success(), "the pipe {} is made", pipe.display());
        }
        let script = r#"echo up > "$0"; read line < "$1""#;
        let keys = json!({ "Command": ["/bin/sh", "-c", script, up.to_str().expect("UTF-8"), go.to_str().expect("UTF-8")] });
        let request = case.request(&commit, &keys);
        let beaten = case.base.join("beaten");
        let run = thread::spawn(move || ran(&[], &request, Reading::All, &beaten));
        // Opened once the command writes to it: the run is going, its group recorded.
        fs::read_to_string(&up).expect("the command says it is up");
        raised(&root);

        let waiting = drained(&root, "status");
        fs::write(&go, "go\n").expect("the command is let go");
        let ended = run.join().expect("the run's thread ends");
        let after = drained(&root, "status");

        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        assert!(
            waiting
                .out
                .iter()
                .any(|line| line.starts_with("gate: drain process ") && line.ends_with(" job")),
            "the run's group holds the drain: {:?}",
            waiting.out
        );
        assert_eq!(waiting.out.last().map(String::as_str), Some("gate: drain state waiting"));
        assert_eq!(after.out.last().map(String::as_str), Some("gate: drain state drained"), "{:?}", after.out);
    }

    // The process scan is built for macOS only.
    #[cfg(target_os = "macos")]
    #[test]
    fn drain_status_reads_drained_after_a_run_ends() {
        let case = Case::new("drain-after-run");
        let commit = case.snapshot();
        let root = case.root();
        let now = DRAIN_NOW.to_string();
        let ended = ran(
            &["--test-box-now", &now],
            &case.request(&commit, &session_keys("s1")),
            Reading::All,
            &case.base.join("beaten"),
        );
        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        raised(&root);

        // Past the session's lease, its job's group gone with the run.
        let status = drained_at(&root, "status", DRAIN_NOW + 600);

        assert_eq!(status.status, Some(0), "{:?}", status.err);
        assert_eq!(
            status.out.last().map(String::as_str),
            Some("gate: drain state drained"),
            "{:?}",
            status.out
        );
        assert!(drain_names(&root, "sessions").is_empty(), "the dead session's record is removed");
    }

    /// An idle work folder of `case` and a leftover run folder in its own temp folder, both two days old; the
    /// variables an age prune of it runs with, the free space plenty.
    fn idle_work(case: &Case) -> (PathBuf, PathBuf, [(&'static str, String); 2]) {
        let two_days = std::time::Duration::from_hours(48);
        let work = PathBuf::from(case.work());
        fs::create_dir(&work).expect("the work folder is made");
        backdated(&work, two_days);
        let tmp = case.base.join("tmp");
        let leftover = tmp.join("gate.Ab3dE6g8");
        fs::create_dir_all(&leftover).expect("the leftover run folder is made");
        backdated(&leftover, two_days);
        let env = [
            ("GATE_BOX_TEST_TMP_ROOT", tmp.to_str().expect("the temp folder is UTF-8").to_owned()),
            ("GATE_BOX_TEST_FREE_BYTES", (1000u64 << 30).to_string()),
        ];
        (work, leftover, env)
    }

    #[test]
    fn a_prune_under_a_standing_barrier_prunes_nothing() {
        let case = Case::new("prune-drained");
        let (work, leftover, env) = idle_work(&case);
        raised(&case.root());
        let later = (DRAIN_NOW + 60).to_string();
        // A volume nearly full would have the floor pass remove the folder too.
        let env = [(env[0].0, env[0].1.as_str()), ("GATE_BOX_TEST_FREE_BYTES", "1")];

        let ended = pruned_with(&["--test-box-now", &later], &["--root", &case.root()], &env);

        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        assert_eq!(ended.out, ["gate: remote prune skipped: the builder is drained"]);
        assert!(ended.err.is_empty(), "{:?}", ended.err);
        assert!(work.exists() && leftover.exists(), "nothing is removed");
    }

    #[test]
    fn a_keyed_prune_under_a_barrier_keeps_every_key() {
        let case = Case::new("prune-keyed-drained");
        let commit = case.snapshot();
        let work = PathBuf::from(case.work());
        fs::create_dir(&work).expect("the work folder is made");
        raised(&case.root());
        let later = (DRAIN_NOW + 60).to_string();
        let never = "never-0123abcd";

        let ended = pruned_with(&["--test-box-now", &later], &["--root", &case.root(), "--keys", KEY, never], &[]);

        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        assert_eq!(
            ended.out,
            [
                format!("gate: remote kept {KEY}: the builder is drained"),
                format!("gate: remote kept {never}: the builder is drained"),
            ]
        );
        assert!(work.exists(), "the work folder is kept");
        assert_eq!(
            git(&[&case.bare_dir(), "rev-parse", &format!("refs/gate/{KEY}")]),
            commit,
            "the key's refs are kept"
        );
    }

    #[test]
    fn a_prune_after_the_lapse_sweeps() {
        let case = Case::new("prune-lapsed");
        let (work, leftover, env) = idle_work(&case);
        raised(&case.root());
        let lapsed = (DRAIN_NOW + 1800).to_string();
        let env = env.each_ref().map(|(name, value)| (*name, value.as_str()));

        let ended = pruned_with(&["--test-box-now", &lapsed], &["--root", &case.root()], &env);

        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        assert_eq!(
            ended.out.first().map(String::as_str),
            Some(format!("gate: remote pruned {KEY}: idle 48 h").as_str()),
            "{:?}",
            ended.out
        );
        assert!(!work.exists() && !leftover.exists(), "the sweep removes both");
    }

    /// Writes the root `root`'s count file, giving it `slots` builder slots.
    fn counted(root: &str, slots: u32) {
        fs::write(format!("{root}/slots.json"), json!({ "v": 1, "slots": slots }).to_string()).expect("the count file is written");
    }

    /// The JSON of slot `n`'s claim under the root `root`; `None` when it has none.
    fn claim(root: &str, n: usize) -> Option<Value> {
        let text = fs::read_to_string(format!("{root}/slots/{n}.json")).ok()?;
        Some(serde_json::from_str(&text).expect("a claim is JSON"))
    }

    /// The session slot `n`'s claim under the root `root` names.
    fn claimed_by(root: &str, n: usize) -> Option<String> {
        claim(root, n).and_then(|claim| claim["session"].as_str().map(str::to_owned))
    }

    /// The lines a call of the nonce `nonce` (`hold` for a hold) refused by a builder whose `slots` slots are all held
    /// says, a preflight's after its version line.
    fn full_lines(nonce: &str, slots: u32) -> [String; 2] {
        [
            refused_line(nonce, Refusal::Full),
            format!("gate: remote builder is full: {slots} of {slots} slots held"),
        ]
    }

    /// The key of a second work folder, never made by a case.
    const OTHER_KEY: &str = "other-0123abcd";

    /// The payload keys of a call of the session `id` for the work folder [`OTHER_KEY`] of the bare repository
    /// `<root>/other.git`, neither of them made.
    fn other_keys(case: &Case, id: &str) -> Value {
        let (root, work) = (case.root(), format!("{}/work/{OTHER_KEY}", case.root()));
        json!({
            "Session": id,
            "Pc": "pc1",
            "Repo": "other",
            "Key": OTHER_KEY,
            "Bare": format!("{root}/other.git"),
            "Work": work,
            "Lock": format!("{work}.lock"),
            "Index": format!("{work}.index"),
            "Job": format!("gate-remote-{OTHER_KEY}-0123456789ab"),
        })
    }

    /// The run of the session `id` checking out `commit`, at [`DRAIN_NOW`], read to its end.
    fn session_ran(case: &Case, commit: &str, id: &str) -> Ended {
        let now = DRAIN_NOW.to_string();
        ran(
            &["--test-box-now", &now],
            &case.request(commit, &session_keys(id)),
            Reading::All,
            &case.base.join("beaten"),
        )
    }

    #[test]
    fn a_full_builder_refuses_a_second_session_s_preflight_until_the_first_run_ends() {
        let case = Case::new("slots-full");
        let commit = case.snapshot();
        let root = case.root();
        counted(&root, 1);

        let first = preflighted(&case, &session_keys("s1"));
        let held = claim(&root, 0);
        let refused = preflighted(&case, &other_keys(&case, "s2"));
        let made_work = Path::new(&format!("{root}/work/{OTHER_KEY}")).exists();
        let made_bare = Path::new(&format!("{root}/other.git")).exists();
        let run = session_ran(&case, &commit, "s1");
        let after_run = claim(&root, 0);
        let second = preflighted(&case, &other_keys(&case, "s2"));

        assert_eq!(first.status, Some(0), "{:?}", first.err);
        let held = held.expect("the first session holds slot 0");
        assert_eq!(held["kind"], json!("session"), "{held}");
        assert_eq!(held["session"], json!("s1"), "{held}");
        assert_eq!(held["pc"], json!("pc1"), "{held}");
        assert_eq!(refused.status, Some(75), "{:?}", refused.err);
        assert_eq!(refused.out.len(), 2, "the version line, then the box slots line: {:?}", refused.out);
        assert!(refused.out[0].starts_with("gate: remote version "), "{:?}", refused.out);
        assert!(
            refused.out[1].starts_with("gate: remote box slots ") && refused.out[1].contains(r#""session":"s1""#),
            "{:?}",
            refused.out
        );
        assert_eq!(refused.err, full_lines(NONCE, 1));
        assert!(!made_work, "a refused preflight makes no work folder");
        assert!(!made_bare, "nor a bare repository");
        assert_eq!(run.status, Some(0), "the run keeps its preflight's slot: {:?}", run.err);
        assert_eq!(after_run, None, "the run's end lets its slot go");
        assert_eq!(second.status, Some(0), "{:?}", second.err);
        assert_eq!(claimed_by(&root, 0).as_deref(), Some("s2"));
    }

    #[test]
    fn a_session_s_second_preflight_keeps_its_slot_and_takes_no_second() {
        let case = Case::new("slots-kept");
        let root = case.root();
        counted(&root, 2);

        let first = preflighted(&case, &session_keys("s1"));
        let again = preflighted(&case, &session_keys("s1"));
        let after_again = claim(&root, 1);
        let other = preflighted(&case, &other_keys(&case, "s2"));

        assert_eq!(first.status, Some(0), "{:?}", first.err);
        assert_eq!(again.status, Some(0), "{:?}", again.err);
        assert_eq!(after_again, None, "the second preflight takes no slot");
        assert_eq!(other.status, Some(0), "a second session finds slot 1 free: {:?}", other.err);
        assert_eq!(claimed_by(&root, 0).as_deref(), Some("s1"));
        assert_eq!(claimed_by(&root, 1).as_deref(), Some("s2"));
    }

    #[test]
    fn a_preflight_that_fails_after_its_slot_take_leaves_no_claim() {
        let case = Case::new("slots-failed-preflight");
        let root = case.root();
        counted(&root, 1);
        // A `.git` folder holding nothing is no repository, so the preflight's first git step in it fails.
        std::os::unix::fs::DirBuilderExt::mode(fs::DirBuilder::new().recursive(true), 0o700)
            .create(format!("{}/.git", case.work()))
            .expect("an empty .git folder is made");

        let failed = preflighted(&case, &session_keys("s1"));

        assert!(failed.status.is_some_and(|status| status != 0 && status != 75), "{:?}", failed.err);
        assert!(
            failed.err.last().is_some_and(|line| line.starts_with("gate: remote git ")),
            "the git step's line: {:?}",
            failed.err
        );
        assert_eq!(claim(&root, 0), None, "the failed preflight's slot is let go");
        assert!(!failed.err.iter().any(|line| line.contains("slot release failed")), "{:?}", failed.err);
    }

    #[test]
    fn with_no_count_file_a_preflight_and_its_run_take_no_slot() {
        let case = Case::new("slots-uncounted");
        let commit = case.snapshot();
        let root = case.root();

        let preflight = preflighted(&case, &session_keys("s1"));
        let run = session_ran(&case, &commit, "s1");

        assert_eq!(preflight.status, Some(0), "{:?}", preflight.err);
        assert_eq!(run.status, Some(0), "{:?}", run.err);
        assert!(!Path::new(&format!("{root}/slots")).exists(), "no slot folder is made");
    }

    #[test]
    fn an_unparsable_count_file_is_said_and_takes_no_slot() {
        let case = Case::new("slots-unparsable");
        let root = case.root();
        fs::write(format!("{root}/slots.json"), "not a count").expect("the count file is written");

        let first = preflighted(&case, &session_keys("s1"));
        let other = preflighted(&case, &other_keys(&case, "s2"));

        for ended in [&first, &other] {
            assert_eq!(ended.status, Some(0), "{:?}", ended.err);
            assert_eq!(ended.err.len(), 1, "{:?}", ended.err);
            assert!(
                ended.err[0].starts_with("gate box: the builder's slot count cannot be read, so it takes no slot: ")
                    && ended.err[0].contains("slots.json: it is not JSON"),
                "{:?}",
                ended.err
            );
        }
        assert!(!Path::new(&format!("{root}/slots")).exists(), "no slot folder is made");
    }

    #[test]
    fn a_second_run_call_of_a_session_refused_busy_keeps_the_live_run_s_claim() {
        let case = Case::new("slots-busy");
        let commit = case.snapshot();
        let root = case.root();
        counted(&root, 1);
        let first = preflighted(&case, &session_keys("s1"));
        assert_eq!(first.status, Some(0), "{:?}", first.err);
        let (started, go) = (case.base.join("started"), case.base.join("go"));
        let mut keys = session_keys("s1");
        keys["Command"] = json!(["/bin/sh", "-c", waits_for_go(&started, &go)]);
        let request = case.request(&commit, &keys);
        let beaten = case.base.join("beaten");
        let now = DRAIN_NOW.to_string();
        let live = thread::spawn(move || ran(&["--test-box-now", &now], &request, Reading::All, &beaten));
        waited_for("the live run's command", || started.exists() || live.is_finished());
        assert!(started.exists(), "the live run's command started");

        let second = session_ran(&case, &commit, "s1");
        let while_live = claim(&root, 0);
        fs::write(&go, "").expect("the go marker is made");
        let ended = live.join().expect("the live run's reader ends");

        assert_eq!(second.status, Some(75), "{:?}", second.err);
        let tag = second.err.iter().position(|line| *line == refused_line(NONCE, Refusal::Busy));
        assert!(
            tag.is_some_and(|tag| second.err_texts().get(tag + 1) == Some(&busy_line(KEY).as_str())),
            "the busy tag, then the busy line: {:?}",
            second.err
        );
        assert_eq!(
            while_live.as_ref().and_then(|claim| claim["session"].as_str()),
            Some("s1"),
            "the live run's claim is still there: {while_live:?}"
        );
        assert_eq!(ended.status, Some(0), "{:?}", ended.err);
        assert_eq!(claim(&root, 0), None, "the live run's end lets the slot go");
    }

    /// The holder text of every hold of the suite.
    const HOLDER: &str = "github-actions repo ci/test run 7";

    /// The usual options of a hold: the suite's holder and a ceiling of 600 s.
    const HOLD_OPTIONS: [&str; 4] = ["--holder", HOLDER, "--ceiling", "600"];

    /// `gate-box box --test-box-now <DRAIN_NOW> <seams> hold --root <root> <options> -- <command>`, not started, its
    /// stdin `/dev/null` and its output piped.
    fn hold_command(root: &str, seams: &[&str], options: &[&str], command: &[&str]) -> Command {
        let now = DRAIN_NOW.to_string();
        let mut words = vec!["box", "--test-box-now", &now];
        words.extend_from_slice(seams);
        words.extend_from_slice(&["hold", "--root", root]);
        words.extend_from_slice(options);
        words.push("--");
        words.extend_from_slice(command);
        let mut hold = Command::new(env!("CARGO_BIN_EXE_gate-box"));
        hold.args(&words).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        hold
    }

    /// The hold `command` run to its end.
    fn held(mut command: Command) -> Ended {
        ended_of(&command.output().expect("gate-box runs"))
    }

    /// A command that makes `started`, then waits until `go` is made.
    fn waits_for_go(started: &Path, go: &Path) -> String {
        format!(": > '{}'; while [ ! -e '{}' ]; do sleep 0.05; done", started.display(), go.display())
    }

    /// Waits until `done`, failing as hung after 120 s, a hang guard only.
    fn waited_for(what: &str, done: impl Fn() -> bool) {
        let guard = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while !done() && std::time::Instant::now() < guard {
            thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(done(), "hung waiting for {what}");
    }

    /// Whether slot 0's claim under the root `root` is a hold's.
    fn hold_claims(root: &str) -> bool {
        claim(root, 0).is_some_and(|claim| claim["kind"] == json!("hold"))
    }

    /// An argv word unique to the case `name` of this test process, which every lasting process of its command carries.
    fn marker(name: &str) -> String {
        format!("gate-hold-mark-{}-{name}", std::process::id())
    }

    /// The script of a command run as `/bin/sh -c <script> <marker>`: a shell carrying `marker` started in the
    /// background, looping until it is killed, then `then`.
    fn marked_script(marker: &str, then: &str) -> String {
        format!("/bin/sh -c 'while :; do sleep 0.05; done' {marker} & {then}")
    }

    /// Whether a process whose command line holds `marker` runs (`pgrep -f`).
    fn marked_running(marker: &str) -> bool {
        Command::new("/usr/bin/pgrep")
            .args(["-f", marker])
            .stdout(Stdio::null())
            .status()
            .expect("pgrep runs")
            .success()
    }

    /// Kills every process whose command line holds its marker when dropped, so a case that fails leaves none behind.
    struct Marked(String);

    impl Drop for Marked {
        fn drop(&mut self) {
            // Best effort: none is left when the case passed.
            let _ = Command::new("/usr/bin/pkill").args(["-9", "-f", &self.0]).status();
        }
    }

    /// [`hold_command`] of the command `/bin/sh -c <script> <marker>`, its stdout and stderr `/dev/null`, so a process
    /// left holding them never keeps the case waiting for their end.
    fn marked_hold(root: &str, options: &[&str], script: &str, marker: &str) -> Command {
        let mut hold = hold_command(root, &[], options, &["/bin/sh", "-c", script, marker]);
        hold.stdout(Stdio::null()).stderr(Stdio::null());
        hold
    }

    #[test]
    fn a_hold_runs_its_command_holding_its_claim_and_exits_with_its_status() {
        let case = Case::new("hold-runs");
        let root = case.root();
        counted(&root, 1);
        let seen = case.base.join("seen.json");
        let script = format!("cat '{root}/slots/0.json' > '{}'; echo held; echo oops >&2; exit 3", seen.display());

        let ended = held(hold_command(&root, &[], &HOLD_OPTIONS, &["/bin/sh", "-c", &script]));

        assert_eq!(ended.status, Some(3), "{:?}", ended.err);
        assert_eq!(ended.out, ["held"]);
        assert_eq!(ended.err, ["oops"]);
        let seen: Value = serde_json::from_str(&fs::read_to_string(&seen).expect("the command read the claim")).expect("a claim is JSON");
        assert_eq!(seen["kind"], json!("hold"), "{seen}");
        assert_eq!(seen["holder"], json!(HOLDER), "{seen}");
        assert_eq!(seen["ceiling"], json!(600), "{seen}");
        assert!(seen["pid"].as_u64().is_some_and(|pid| pid > 0), "{seen}");
        assert_eq!(claim(&root, 0), None, "the hold's claim goes with it");
        assert!(!Path::new(&format!("{root}/slots/0.lock")).exists(), "and its lock");
    }

    #[test]
    fn a_hold_on_a_full_builder_refuses_at_once_and_with_a_wait_takes_the_slot_once_its_holder_ends() {
        let case = Case::new("hold-full");
        let root = case.root();
        counted(&root, 1);
        let (started, go) = (case.base.join("started"), case.base.join("go"));
        let first = hold_command(&root, &[], &HOLD_OPTIONS, &["/bin/sh", "-c", &waits_for_go(&started, &go)]);
        let first = thread::spawn(move || held(first));
        waited_for("the first hold's command", || started.exists());
        let poll = ["--test-box-hold-poll-ms", "50"];
        let long = [&HOLD_OPTIONS[..], &["--wait", "600"]].concat();

        let gave_up = held(hold_command(&root, &poll, &HOLD_OPTIONS, &["/bin/echo", "never"]));
        let mut waiting = hold_command(&root, &poll, &long, &["/bin/echo", "took"])
            .spawn()
            .expect("gate-box starts");
        let mut err = BufReader::new(waiting.stderr.take().expect("stderr is piped"));
        let mut first_line = String::new();
        err.read_line(&mut first_line).expect("the waiting hold says a line");
        fs::write(&go, "").expect("the go marker is made");
        let first = first.join().expect("the first hold's reader ends");
        let mut rest = String::new();
        err.read_to_string(&mut rest).expect("the waiting hold's stderr is read");
        let took = waiting.wait_with_output().expect("the waiting hold is waited for");

        assert_eq!(gave_up.status, Some(75), "{:?}", gave_up.err);
        assert!(gave_up.out.is_empty(), "the command never ran: {:?}", gave_up.out);
        assert_eq!(gave_up.err, full_lines("hold", 1), "no wait refuses at once");
        assert_eq!(first_line, "gate: waiting for a builder slot: 1 of 1 held\n");
        assert_eq!(first.status, Some(0), "{:?}", first.err);
        assert_eq!(took.status.code(), Some(0), "{rest}");
        assert_eq!(String::from_utf8_lossy(&took.stdout), "took\n", "{rest}");
        assert_eq!(claim(&root, 0), None, "the slot is let go");
    }

    #[test]
    fn a_hold_past_its_ceiling_ends_its_command_s_group_and_exits_124() {
        let case = Case::new("hold-ceiling");
        let root = case.root();
        counted(&root, 1);
        let mark = marker("ceiling");
        let _marked = Marked(mark.clone());
        // The background shell is of the command's group, and outlives its parent unless the group is ended. Nothing
        // here needs the shell to get anywhere before the ceiling: a shell ended sooner leaves no process all the same.
        let script = marked_script(&mark, "wait");

        let ended = held(hold_command(
            &root,
            &[],
            &["--holder", HOLDER, "--ceiling", "1"],
            &["/bin/sh", "-c", &script, &mark],
        ));

        assert_eq!(ended.status, Some(124), "{:?}", ended.err);
        assert_eq!(
            ended.err.last().map(String::as_str),
            Some("gate: hold ended at its ceiling of 1 s"),
            "{:?}",
            ended.err
        );
        waited_for("no process of the command", || !marked_running(&mark));
        assert_eq!(claim(&root, 0), None, "the slot is let go");
    }

    #[test]
    fn a_hold_sent_sigterm_ends_its_command_s_group_and_exits_143() {
        let case = Case::new("hold-sigterm");
        let root = case.root();
        counted(&root, 1);
        let mark = marker("sigterm");
        let _marked = Marked(mark.clone());
        let started = case.base.join("started");
        let script = marked_script(&mark, &format!(": > '{}'; wait", started.display()));
        let mut hold = marked_hold(&root, &HOLD_OPTIONS, &script, &mark).spawn().expect("gate-box starts");
        waited_for("the hold's command", || started.exists());
        let pid = libc::pid_t::try_from(hold.id()).expect("a pid is a pid_t");

        // SAFETY: `kill` takes two integers and touches no memory; `pid` is the hold, not yet waited for.
        let sent = unsafe { libc::kill(pid, libc::SIGTERM) };
        let ended = hold.wait().expect("the hold is waited for");

        assert_eq!(sent, 0, "the hold is sent SIGTERM");
        assert_eq!(ended.code(), Some(143), "{ended:?}");
        waited_for("no process of the command", || !marked_running(&mark));
        assert_eq!(claim(&root, 0), None, "the slot is let go");
    }

    #[test]
    fn a_hold_whose_command_s_leader_ends_ends_its_leftovers_and_exits_with_the_leader_s_status() {
        let case = Case::new("hold-leftovers");
        let root = case.root();
        counted(&root, 1);
        let mark = marker("leftovers");
        let _marked = Marked(mark.clone());

        let ended = marked_hold(&root, &HOLD_OPTIONS, &marked_script(&mark, "exit 5"), &mark)
            .status()
            .expect("the hold runs");

        assert_eq!(ended.code(), Some(5), "{ended:?}");
        waited_for("no process of the command", || !marked_running(&mark));
        assert_eq!(claim(&root, 0), None, "the slot is let go");
    }

    #[test]
    fn a_killed_hold_leaves_a_claim_the_next_taker_reclaims() {
        let case = Case::new("hold-killed");
        let root = case.root();
        counted(&root, 1);
        let (started, go) = (case.base.join("started"), case.base.join("go"));
        let mut hold = hold_command(&root, &[], &HOLD_OPTIONS, &["/bin/sh", "-c", &waits_for_go(&started, &go)])
            .spawn()
            .expect("gate-box starts");
        waited_for("the hold's command", || started.exists());
        let holding = hold_claims(&root);

        hold.kill().expect("the hold is killed");
        let killed = hold.wait().expect("the killed hold is waited for");
        let left = hold_claims(&root);
        let next = preflighted(&case, &session_keys("s1"));
        // The killed hold's command is in a group of its own, and runs on until it is let go.
        fs::write(&go, "").expect("the go marker is made");

        assert!(holding, "the hold claimed slot 0");
        assert_eq!(killed.code(), None, "{killed:?}");
        assert!(left, "the killed hold's claim is left behind");
        assert_eq!(next.status, Some(0), "the killed hold's slot is free: {:?}", next.err);
        assert_eq!(claimed_by(&root, 0).as_deref(), Some("s1"));
        assert!(!Path::new(&format!("{root}/slots/0.lock")).exists(), "the dead lock is removed");
    }

    #[test]
    fn a_hold_on_a_builder_with_no_count_file_exits_2_with_its_line() {
        let case = Case::new("hold-uncounted");
        let root = case.root();

        let ended = held(hold_command(&root, &[], &HOLD_OPTIONS, &["/bin/echo", "never"]));

        assert_eq!(ended.status, Some(2), "{:?}", ended.err);
        assert!(ended.out.is_empty(), "{:?}", ended.out);
        assert_eq!(ended.err, ["gate box: this builder has no slot count; run gate push"]);
    }

    #[test]
    fn a_holder_with_a_control_character_is_a_usage_error() {
        let case = Case::new("hold-holder");
        let root = case.root();
        counted(&root, 1);

        let ended = held(hold_command(
            &root,
            &[],
            &["--holder", "ci\u{1b}[2J", "--ceiling", "60"],
            &["/bin/echo", "never"],
        ));

        assert_eq!(ended.status, Some(2), "{:?}", ended.err);
        assert!(ended.out.is_empty(), "{:?}", ended.out);
        assert_eq!(ended.err, [r"gate box: the holder 'ci\u{1b}[2J' holds a control character"]);
        assert!(!Path::new(&format!("{root}/slots")).exists(), "no slot is taken");
    }
}
