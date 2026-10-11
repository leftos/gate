//! How the client reaches the builder: the program and words of one call of `gate box` there, and where a snapshot is
//! pushed. The ssh transport runs OpenSSH, and git over it, as the PowerShell client does: every option a word of its
//! own and `--` before the host. The local transport of the test seams has its builder root in a folder here, runs
//! this gate's own `box` with the words the line carries after the builder's gate, or a stand-in for ssh with ssh's own
//! words, and pushes to the bare repository's own path; its `local-ssh:` form pushes and fetches as the ssh transport
//! does, to `<host>:<path>` with the stand-in as git's ssh.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

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
    /// The program that runs `line` on the builder `host`.
    fn call(&self, host: &str, line: &str) -> Program;
    /// Where the snapshot of `repo` is pushed on the builder `host`.
    fn push_target(&self, host: &str, repo: &str) -> PushTarget;
    /// The command git runs on the builder to serve the sync-back's fetch, the upload-pack counterpart of the push's
    /// receive-pack; `None` for a bare repository on this machine, where the push names none either.
    fn upload_pack(&self) -> Option<String>;
    /// Whether `gate box` runs on this machine, as a call's child: then a call's job must let the box start its
    /// command outside it, and else it lets nothing leave.
    fn box_runs_here(&self) -> bool;
}

/// ssh's words for running `line` on `host`: the options, `--`, the host and the line, each a word of its own.
pub(crate) fn ssh_words(host: &str, line: &str) -> Vec<OsString> {
    SSH_OPTIONS.iter().copied().chain(["--", host, line]).map(OsString::from).collect()
}

/// The local transport of `--test-remote-transport local:<folder>`: the builder's root `<folder>`, every call this
/// gate's own exe, or the stand-in `stub` of `--test-remote-ssh` given ssh's words. Of `local-ssh:<folder>`, git's ssh
/// command too, the stand-in's, which git runs for the push and the sync-back's calls.
pub(crate) struct Local {
    root: String,
    gate: PathBuf,
    stub: Option<PathBuf>,
    git_ssh: Option<String>,
}

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

impl Transport for Local {
    fn root(&self) -> &str {
        &self.root
    }

    fn call(&self, host: &str, line: &str) -> Program {
        match &self.stub {
            Some(stub) => Program {
                program: stub.clone(),
                args: ssh_words(host, line),
            },
            None => Program {
                program: self.gate.clone(),
                args: box_words(line),
            },
        }
    }

    fn push_target(&self, host: &str, repo: &str) -> PushTarget {
        let bare = payload::bare(&self.root, repo);
        match &self.git_ssh {
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
        }
    }

    fn upload_pack(&self) -> Option<String> {
        None
    }

    fn box_runs_here(&self) -> bool {
        true
    }
}

/// The words `line` carries after the builder's gate: `box`, the verb and the payload word.
fn box_words(line: &str) -> Vec<OsString> {
    line.strip_prefix(BOX_EXE)
        .unwrap_or(line)
        .split_whitespace()
        .map(OsString::from)
        .collect()
}

/// The ssh transport: OpenSSH found along the gate's own `PATH`, the builder's root `D:\gate-remote`, and git pushing
/// over ssh to a receive-pack started through pwsh, which reads the single-quoted path git sends through cmd.
#[cfg_attr(not(test), expect(dead_code, reason = "no seam selects the ssh transport; only its tests build it"))]
mod ssh {
    use std::path::{Path, PathBuf};

    use super::{Program, PushTarget, SSH_OPTIONS, Transport, ssh_words};
    use crate::environment::Variables;
    use crate::run::find_program;

    const BUILDER_ROOT: &str = r"D:\gate-remote";
    const RECEIVE_PACK: &str = "pwsh -NoProfile -NonInteractive -Command git-receive-pack";
    const UPLOAD_PACK: &str = "pwsh -NoProfile -NonInteractive -Command git-upload-pack";

    pub(crate) struct Ssh {
        pub(super) program: PathBuf,
    }

    impl Ssh {
        /// ssh found along the gate's own `PATH`, read against the working directory; else why it cannot be found.
        #[expect(dead_code, reason = "no seam selects the ssh transport, and its tests give ssh's path themselves")]
        pub(crate) fn find() -> Result<Ssh, String> {
            let folder = std::env::current_dir().map_err(|error| format!("cannot read the working directory: {error}"))?;
            find_program("ssh", &Variables::from_process(), &folder)
                .map(|program| Ssh { program })
                .map_err(|why| format!("cannot run ssh: {why}"))
        }
    }

    impl Transport for Ssh {
        fn root(&self) -> &str {
            BUILDER_ROOT
        }

        fn call(&self, host: &str, line: &str) -> Program {
            Program {
                program: self.program.clone(),
                args: ssh_words(host, line),
            }
        }

        fn push_target(&self, host: &str, repo: &str) -> PushTarget {
            PushTarget {
                url: format!("{host}:{}/{repo}.git", BUILDER_ROOT.replace('\\', "/")),
                receive_pack: Some(RECEIVE_PACK.to_owned()),
                ssh_command: Some(git_ssh_command(&self.program)),
            }
        }

        fn upload_pack(&self) -> Option<String> {
            Some(UPLOAD_PACK.to_owned())
        }

        fn box_runs_here(&self) -> bool {
            false
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

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose transport cannot be made must fail loudly")]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use super::ssh::{Ssh, git_ssh_command};
    use super::{Local, Program, PushTarget, Transport, box_words, ssh_words};

    fn words(texts: &[&str]) -> Vec<OsString> {
        texts.iter().map(OsString::from).collect()
    }

    #[test]
    fn ssh_s_words_are_the_options_then_dash_dash_then_the_host_then_the_line() {
        assert_eq!(
            ssh_words("builder", r#""%LOCALAPPDATA%\gate\bin\gate.exe" box run e30="#),
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
            ssh.call("builder", "line"),
            Program {
                program: PathBuf::from(r"C:\Program Files\Open'SSH\ssh.exe"),
                args: ssh_words("builder", "line"),
            }
        );
        assert_eq!(
            ssh.push_target("builder", "app"),
            PushTarget {
                url: "builder:D:/gate-remote/app.git".to_owned(),
                receive_pack: Some("pwsh -NoProfile -NonInteractive -Command git-receive-pack".to_owned()),
                ssh_command: Some(git_ssh_command(&ssh.program)),
            }
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
        assert_eq!(local.call("builder", line).args, words(&["box", "preflight", "e30="]));
        assert_eq!(local.push_target("builder", "app").url, r"C:\builder\app.git");
        assert_eq!(local.upload_pack(), None, "a bare repository here names no pack command");
        let stubbed = Local::new(r"local:C:\builder", Some(PathBuf::from("fake-ssh.exe").as_path())).expect("a local transport");
        assert_eq!(
            stubbed.call("builder", line),
            Program {
                program: PathBuf::from("fake-ssh.exe"),
                args: ssh_words("builder", line),
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
            PushTarget {
                url: "builder:C:/builder/app.git".to_owned(),
                receive_pack: None,
                ssh_command: Some(git_ssh_command(&stub)),
            }
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
}
