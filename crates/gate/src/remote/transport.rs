//! How the client reaches the builder: the program and words of one call of `gate box` there, and where a snapshot is
//! pushed. The ssh transport runs OpenSSH, and git over it, as the PowerShell client does: every option a word of its
//! own and `--` before the host. The local transport of the test seams has its builder root in a folder here, runs
//! this gate's own `box` with the words the line carries after the builder's gate, or a stand-in for ssh with ssh's own
//! words, and pushes to the bare repository's own path; its `local-ssh:` form pushes and fetches as the ssh transport
//! does, to `<host>:<path>` with the stand-in as git's ssh.
//!
//! A gate built without the `test-seams` feature has the ssh transport alone.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[cfg(feature = "test-seams")]
use crate::remote::payload::{self, BOX_EXE};

/// The options of every ssh call, git's own included: no prompt, the timeouts, and nothing forwarded to the builder.
pub(crate) const SSH_OPTIONS: [&str; 12] = [
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=4",
    "-o",
    "ForwardAgent=no",
    "-o",
    "ClearAllForwardings=yes",
];

/// The options a call adds after [`SSH_OPTIONS`] for a connection ssh compresses.
const COMPRESSION: [&str; 2] = ["-o", "Compression=yes"];

/// How a call's ssh connection carries its streams: compressed, as the run call's, which carries the command's output,
/// or plain, as every other call's and git's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Wire {
    Compressed,
    #[default]
    Plain,
}

/// A program and the words it is started with.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Program {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

/// Where a snapshot is pushed: the URL, the receive-pack the builder runs, and the ssh command git runs; the latter two
/// `None` for a bare repository on this machine.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PushTarget {
    pub url: String,
    pub receive_pack: Option<String>,
    pub ssh_command: Option<String>,
}

/// The way to the builder.
pub(crate) trait Transport {
    /// The builder's root, which every work folder and bare repository lives under.
    fn root(&self) -> &str;
    /// The program that runs `line` on the builder `host`, over a connection `wire` says how to carry.
    fn call(&self, host: &str, line: &str, wire: Wire) -> Program;
    /// Where the snapshot of `repo` is pushed on the builder `host`; else why the repo's name cannot be sent there.
    fn push_target(&self, host: &str, repo: &str) -> Result<PushTarget, String>;
    /// The command git runs on the builder to serve the sync-back's fetch, the upload-pack counterpart of the push's
    /// receive-pack; `None` for a bare repository on this machine, where the push names none either.
    fn upload_pack(&self) -> Option<String>;
    /// Whether `gate box` runs on this machine, as a call's child: then a call's job must let the box start its
    /// command outside it, and else it lets nothing leave.
    fn box_runs_here(&self) -> bool;
    /// Checks the program the transport runs against the run's `worktree` before its first call, and from then on runs
    /// the file it was judged as; else why it is refused. A transport whose program the test seams give checks nothing.
    fn judge(&mut self, _worktree: &Path) -> Result<(), String> {
        Ok(())
    }
}

/// `repo` when it is a name gate-router makes, `[A-Za-z0-9_-]+`, so the URL built from it carries no quote or other
/// character cmd.exe or pwsh on the builder would read; else why it is refused.
fn sent_repo(repo: &str) -> Result<&str, String> {
    let named = !repo.is_empty() && repo.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    if named {
        Ok(repo)
    } else {
        Err(format!("cannot push to the repo '{repo}': a repo name is letters, digits, '_' and '-'"))
    }
}

/// ssh's words for running `line` on `host` over a connection `wire` says how to carry: the options, the compression
/// option for a compressed one, `--`, the host and the line, each a word of its own.
pub(crate) fn ssh_words(host: &str, line: &str, wire: Wire) -> Vec<OsString> {
    let compression: &[&str] = match wire {
        Wire::Compressed => &COMPRESSION,
        Wire::Plain => &[],
    };
    SSH_OPTIONS
        .iter()
        .chain(compression)
        .copied()
        .chain(["--", host, line])
        .map(OsString::from)
        .collect()
}

/// The local transport of `--test-remote-transport local:<folder>`: the builder's root `<folder>`, every call this
/// gate's own exe, or the stand-in `stub` of `--test-remote-ssh` given ssh's words. Of `local-ssh:<folder>`, git's ssh
/// command too, the stand-in's, which git runs for the push and the sync-back's calls.
#[cfg(feature = "test-seams")]
pub(crate) struct Local {
    root: String,
    gate: PathBuf,
    stub: Option<PathBuf>,
    git_ssh: Option<String>,
}

#[cfg(feature = "test-seams")]
impl Local {
    /// The local transport `transport` names, `local:<folder>` or `local-ssh:<folder>` with the folder read against the
    /// working directory, its calls made through `stub` when one is given, which `local-ssh:` needs; else the line
    /// saying why it cannot be used.
    pub(crate) fn new(transport: &str, stub: Option<&Path>) -> Result<Local, String> {
        let cannot = |why: String| format!("gate: cannot use the remote transport '{transport}': {why}");
        let (folder, over_ssh) = match transport.split_once(':') {
            Some(("local", folder)) if !folder.is_empty() => (folder, false),
            Some(("local-ssh", folder)) if !folder.is_empty() => (folder, true),
            _ => return Err(cannot("it is not local:<folder>".to_owned())),
        };
        let git_ssh = match (over_ssh, stub) {
            (false, _) => None,
            (true, Some(stub)) => Some(ssh::git_ssh_command(stub)),
            (true, None) => {
                return Err(cannot(
                    "local-ssh:<folder> runs git through --test-remote-ssh, which is not given".to_owned(),
                ));
            }
        };
        let root = std::path::absolute(folder).map_err(|error| cannot(error.to_string()))?;
        let gate = std::env::current_exe().map_err(|error| cannot(format!("cannot find this gate's own exe: {error}")))?;
        Ok(Local {
            root: root.to_string_lossy().into_owned(),
            gate,
            stub: stub.map(Path::to_path_buf),
            git_ssh,
        })
    }
}

#[cfg(feature = "test-seams")]
impl Transport for Local {
    fn root(&self) -> &str {
        &self.root
    }

    /// The stand-in is given ssh's words, `wire`'s among them; this gate's own `box` runs with no connection to carry.
    fn call(&self, host: &str, line: &str, wire: Wire) -> Program {
        match &self.stub {
            Some(stub) => Program {
                program: stub.clone(),
                args: ssh_words(host, line, wire),
            },
            None => Program {
                program: self.gate.clone(),
                args: box_words(line),
            },
        }
    }

    fn push_target(&self, host: &str, repo: &str) -> Result<PushTarget, String> {
        let bare = payload::bare(&self.root, repo);
        Ok(match &self.git_ssh {
            Some(ssh_command) => PushTarget {
                url: format!("{host}:{}", bare.replace('\\', "/")),
                receive_pack: None,
                ssh_command: Some(ssh_command.clone()),
            },
            None => PushTarget {
                url: bare,
                receive_pack: None,
                ssh_command: None,
            },
        })
    }

    fn upload_pack(&self) -> Option<String> {
        None
    }

    fn box_runs_here(&self) -> bool {
        true
    }
}

/// The words `line` carries after the builder's gate: `box`, the verb and the payload word.
#[cfg(feature = "test-seams")]
fn box_words(line: &str) -> Vec<OsString> {
    line.strip_prefix(BOX_EXE)
        .unwrap_or(line)
        .split_whitespace()
        .map(OsString::from)
        .collect()
}

/// The ssh transport: OpenSSH found along the gate's own `PATH`, the builder's root `D:\gate-remote`, and git pushing
/// over ssh to a receive-pack started through pwsh, which reads the single-quoted path git sends through cmd.
/// `gate push` runs its calls with ssh's own path; a routed run that no test seam gives a client uses the ssh
/// transport, and judges the ssh it found against the run's worktree before its first call.
pub(crate) mod ssh {
    use std::path::{Path, PathBuf};

    use super::{Program, PushTarget, SSH_OPTIONS, Transport, Wire, sent_repo, ssh_words};
    use crate::environment::Variables;
    use crate::remote::git::judged;
    use crate::run::find_program;

    const BUILDER_ROOT: &str = r"D:\gate-remote";
    const RECEIVE_PACK: &str = "pwsh -NoProfile -NonInteractive -Command git-receive-pack";
    const UPLOAD_PACK: &str = "pwsh -NoProfile -NonInteractive -Command git-upload-pack";
    /// The one name ssh is looked for by: no `ssh.cmd` or `ssh.bat` shim earlier on `PATH` runs in its place.
    const SSH_EXE: &str = "ssh.exe";

    pub(crate) struct Ssh {
        pub(crate) program: PathBuf,
    }

    impl Ssh {
        /// `ssh.exe` found along the gate's own `PATH`, read against the working directory; else why it cannot be
        /// found.
        pub(crate) fn find() -> Result<Ssh, String> {
            let folder = std::env::current_dir().map_err(|error| format!("cannot read the working directory: {error}"))?;
            Ssh::find_in(&Variables::from_process(), &folder)
        }

        /// `ssh.exe` found along the `PATH` of `variables`, read against `folder`; else why it cannot be found.
        pub(super) fn find_in(variables: &Variables, folder: &Path) -> Result<Ssh, String> {
            find_program(SSH_EXE, variables, folder)
                .map(|program| Ssh { program })
                .map_err(|why| format!("cannot run ssh: {why}"))
        }
    }

    impl Transport for Ssh {
        fn root(&self) -> &str {
            BUILDER_ROOT
        }

        fn call(&self, host: &str, line: &str, wire: Wire) -> Program {
            Program {
                program: self.program.clone(),
                args: ssh_words(host, line, wire),
            }
        }

        fn push_target(&self, host: &str, repo: &str) -> Result<PushTarget, String> {
            let repo = sent_repo(repo)?;
            Ok(PushTarget {
                url: format!("{host}:{}/{repo}.git", BUILDER_ROOT.replace('\\', "/")),
                receive_pack: Some(RECEIVE_PACK.to_owned()),
                ssh_command: Some(git_ssh_command(&self.program)),
            })
        }

        fn upload_pack(&self) -> Option<String> {
            Some(UPLOAD_PACK.to_owned())
        }

        fn box_runs_here(&self) -> bool {
            false
        }

        /// Follows the ssh found through links and refuses it when that leads inside `worktree`; else every call, and
        /// git's ssh command, runs the file it leads to.
        fn judge(&mut self, worktree: &Path) -> Result<(), String> {
            self.program = judged(&self.program, worktree).map_err(|why| format!("cannot run ssh: {why}"))?;
            Ok(())
        }
    }

    /// The ssh command git runs: ssh's path with `/` separators, single-quoted for the shell git starts it through, and
    /// the options.
    pub(super) fn git_ssh_command(ssh: &Path) -> String {
        let path = ssh.to_string_lossy().replace('\\', "/");
        let quoted = format!("'{}'", path.replace('\'', r"'\''"));
        [quoted.as_str()].into_iter().chain(SSH_OPTIONS).collect::<Vec<_>>().join(" ")
    }
}

#[cfg(all(test, feature = "test-seams"))]
#[expect(clippy::expect_used, reason = "a test whose transport cannot be made must fail loudly")]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::path::PathBuf;

    use super::ssh::{Ssh, git_ssh_command};
    use super::{Local, Program, PushTarget, Transport, Wire, box_words, ssh_words};
    use crate::environment::Variables;

    fn words(texts: &[&str]) -> Vec<OsString> {
        texts.iter().map(OsString::from).collect()
    }

    #[test]
    fn ssh_s_words_are_the_options_then_dash_dash_then_the_host_then_the_line() {
        assert_eq!(
            ssh_words("builder", r#""%LOCALAPPDATA%\gate\bin\gate.exe" box run e30="#, Wire::Plain),
            words(&[
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=4",
                "-o",
                "ForwardAgent=no",
                "-o",
                "ClearAllForwardings=yes",
                "--",
                "builder",
                r#""%LOCALAPPDATA%\gate\bin\gate.exe" box run e30="#,
            ])
        );
    }

    #[test]
    fn the_run_call_s_ssh_words_ask_for_compression_and_others_do_not() {
        let line = r#""%LOCALAPPDATA%\gate\bin\gate.exe" box run e30="#;
        let compressed = ssh_words("builder", line, Wire::Compressed);
        let plain = ssh_words("builder", line, Wire::Plain);
        assert_eq!(compressed.get(..12), plain.get(..12), "every call's own options come first");
        assert_eq!(
            compressed.get(12..),
            Some(words(&["-o", "Compression=yes", "--", "builder", line]).as_slice())
        );
        assert_eq!(
            plain.get(12..),
            Some(words(&["--", "builder", line]).as_slice()),
            "a plain call asks for nothing more"
        );
        assert!(!plain.iter().any(|word| word.to_string_lossy().contains("Compression")), "{plain:?}");

        let ssh = Ssh {
            program: PathBuf::from(r"C:\Program Files\OpenSSH\ssh.exe"),
        };
        assert_eq!(ssh.call("builder", line, Wire::Compressed).args, compressed);
        assert_eq!(ssh.call("builder", line, Wire::Plain).args, plain);
        let git = git_ssh_command(&ssh.program);
        assert!(!git.contains("Compression"), "git's ssh is never compressed: {git}");

        let stubbed = Local::new(r"local:C:\builder", Some(PathBuf::from("fake-ssh.exe").as_path())).expect("a local transport");
        assert_eq!(
            stubbed.call("builder", line, Wire::Compressed).args,
            compressed,
            "the stand-in is given ssh's words"
        );
        let local = Local::new(r"local:C:\builder", None).expect("a local transport");
        assert_eq!(
            local.call("builder", line, Wire::Compressed).args,
            words(&["box", "run", "e30="]),
            "the local box is run with no ssh between"
        );
    }

    #[test]
    fn the_ssh_transport_calls_ssh_and_pushes_through_pwsh_s_receive_pack() {
        let ssh = Ssh {
            program: PathBuf::from(r"C:\Program Files\Open'SSH\ssh.exe"),
        };
        assert_eq!(ssh.root(), r"D:\gate-remote");
        assert!(!ssh.box_runs_here(), "the builder's box runs there, so a call's job lets nothing leave");
        let local = Local::new(r"local:C:\builder", None).expect("a local transport");
        assert!(
            local.box_runs_here(),
            "the local box runs here, and starts its command outside the call's job"
        );
        assert_eq!(
            ssh.call("builder", "line", Wire::Plain),
            Program {
                program: PathBuf::from(r"C:\Program Files\Open'SSH\ssh.exe"),
                args: ssh_words("builder", "line", Wire::Plain),
            }
        );
        assert_eq!(
            ssh.push_target("builder", "app"),
            Ok(PushTarget {
                url: "builder:D:/gate-remote/app.git".to_owned(),
                receive_pack: Some("pwsh -NoProfile -NonInteractive -Command git-receive-pack".to_owned()),
                ssh_command: Some(git_ssh_command(&ssh.program)),
            })
        );
        assert_eq!(
            ssh.upload_pack(),
            Some("pwsh -NoProfile -NonInteractive -Command git-upload-pack".to_owned()),
            "the fetch's pack command is the push's own with git's other verb"
        );
        assert_eq!(
            git_ssh_command(&ssh.program),
            concat!(
                r"'C:/Program Files/Open'\''SSH/ssh.exe' -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=15",
                " -o ServerAliveCountMax=4 -o ForwardAgent=no -o ClearAllForwardings=yes"
            )
        );
    }

    #[test]
    fn the_local_transport_runs_the_words_after_the_builder_s_gate() {
        let line = r#""%LOCALAPPDATA%\gate\bin\gate.exe" box preflight e30="#;
        assert_eq!(box_words(line), words(&["box", "preflight", "e30="]));
        let local = Local::new(r"local:C:\builder", None).expect("a local transport");
        assert_eq!(local.root(), r"C:\builder");
        assert_eq!(local.call("builder", line, Wire::Plain).args, words(&["box", "preflight", "e30="]));
        assert_eq!(local.push_target("builder", "app").expect("a push target").url, r"C:\builder\app.git");
        assert_eq!(local.upload_pack(), None, "a bare repository here names no pack command");
        let stubbed = Local::new(r"local:C:\builder", Some(PathBuf::from("fake-ssh.exe").as_path())).expect("a local transport");
        assert_eq!(
            stubbed.call("builder", line, Wire::Plain),
            Program {
                program: PathBuf::from("fake-ssh.exe"),
                args: ssh_words("builder", line, Wire::Plain),
            }
        );
        assert_eq!(
            Local::new("remote:x", None).err(),
            Some("gate: cannot use the remote transport 'remote:x': it is not local:<folder>".to_owned())
        );
    }

    #[test]
    fn the_local_ssh_transport_pushes_and_fetches_through_the_stand_in_as_git_s_ssh() {
        let stub = PathBuf::from(r"C:\suite\fake-ssh.exe");
        let local = Local::new(r"local-ssh:C:\builder", Some(stub.as_path())).expect("a local-ssh transport");
        assert_eq!(local.root(), r"C:\builder");
        assert_eq!(
            local.push_target("builder", "app"),
            Ok(PushTarget {
                url: "builder:C:/builder/app.git".to_owned(),
                receive_pack: None,
                ssh_command: Some(git_ssh_command(&stub)),
            })
        );
        assert_eq!(local.upload_pack(), None, "git runs its own service names");
        assert_eq!(
            Local::new(r"local-ssh:C:\builder", None).err(),
            Some(
                r"gate: cannot use the remote transport 'local-ssh:C:\builder': local-ssh:<folder> runs git through --test-remote-ssh, which is not given"
                    .to_owned()
            )
        );
        assert_eq!(
            Local::new("local:", None).err(),
            Some("gate: cannot use the remote transport 'local:': it is not local:<folder>".to_owned())
        );
    }

    #[test]
    fn a_repo_name_holding_a_quote_is_never_pushed_to() {
        let ssh = Ssh {
            program: PathBuf::from(r"C:\Program Files\OpenSSH\ssh.exe"),
        };

        assert_eq!(
            ssh.push_target("builder", "app'x"),
            Err("cannot push to the repo 'app'x': a repo name is letters, digits, '_' and '-'".to_owned())
        );
        assert!(ssh.push_target("builder", "").is_err(), "an empty name is refused too");
        assert!(ssh.push_target("builder", "App_1-b").is_ok(), "a name gate-router makes is sent");
    }

    #[test]
    fn ssh_is_found_as_ssh_exe_past_an_earlier_ssh_cmd() {
        let root = std::env::temp_dir().join(format!("gate-ssh-find-{}", std::process::id()));
        let (shim, real) = (root.join("shim"), root.join("real"));
        for folder in [&shim, &real] {
            fs::create_dir_all(folder).expect("a PATH folder can be made");
        }
        fs::write(shim.join("ssh.cmd"), "").expect("the shim can be written");
        fs::write(real.join("ssh.exe"), "").expect("the stand-in exe can be written");
        let path = std::env::join_paths([&shim, &real]).expect("the folders join into a PATH");
        let variables = Variables::from_process().with("PATH", &path.to_string_lossy());

        let found = Ssh::find_in(&variables, &root).expect("ssh.exe is found");

        assert_eq!(found.program, real.join("ssh.exe"), "the .cmd earlier on PATH is passed over");
        fs::remove_dir_all(&root).expect("the folders can be removed");
    }
}
