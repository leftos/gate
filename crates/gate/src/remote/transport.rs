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

use gate_args::BoxVerb;
use gate_router::{Builder, BuilderOs};
use gate_wire::PathForm;

use crate::remote::payload::{self, BOX_EXE};

/// The pack commands a Windows builder's git is served through: pwsh reads the single-quoted path git sends through cmd.
const RECEIVE_PACK: &str = "pwsh -NoProfile -NonInteractive -Command git-receive-pack";
const UPLOAD_PACK: &str = "pwsh -NoProfile -NonInteractive -Command git-upload-pack";
/// The box a macOS builder runs, below its root.
const UNIX_BOX: &str = "/bin/gate-box";

/// How the client speaks to one builder: its alias, the operating system it runs, which sets how its paths are written
/// and joined ([`PathForm`]), its box line and its pack commands, and the root every work folder and bare repository
/// lives under. A Windows builder runs the gate of [`BOX_EXE`] and serves git through pwsh; a macOS builder runs
/// `<root>/bin/gate-box`, git's own pack commands serve it, and its paths are joined with `/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Profile {
    alias: String,
    os: BuilderOs,
    root: String,
}

impl Profile {
    /// The profile of `builder` reached over the ssh transport, whose own root is `transport_root`: the builder's root
    /// as the routing file writes it, else the transport's. A test transport lays a Windows builder out under its own
    /// folder whatever root the routing file gives, and keeps a macOS builder's own (`Profile::in_folder`).
    pub(crate) fn of(builder: &Builder, transport_root: &str) -> Profile {
        Profile::new(&builder.alias, builder.os, builder.root.as_deref().unwrap_or(transport_root))
    }

    /// The profile of `builder` reached over a test transport whose builder root is `folder`: a Windows builder lays out
    /// under `folder` whatever root the routing file gives it, so no case writes outside its own folder; a macOS builder
    /// keeps its own root, which the PC's stand-in box refuses at the preflight before any write, so its cases pin the
    /// Unix path forms.
    #[cfg(feature = "test-seams")]
    pub(crate) fn in_folder(builder: &Builder, folder: &str) -> Profile {
        let root = match builder.os {
            BuilderOs::Windows => folder,
            BuilderOs::Macos => builder.root.as_deref().unwrap_or(folder),
        };
        Profile::new(&builder.alias, builder.os, root)
    }

    /// The profile of the builder `alias` running `os` under `root`.
    pub(crate) fn new(alias: &str, os: BuilderOs, root: &str) -> Profile {
        Profile {
            alias: alias.to_owned(),
            os,
            root: root.to_owned(),
        }
    }

    /// The builder's root, as given.
    pub(crate) fn root(&self) -> &str {
        &self.root
    }

    /// The path rules the builder's box reads a payload under.
    pub(crate) fn form(&self) -> PathForm {
        match self.os {
            BuilderOs::Windows => PathForm::Windows,
            BuilderOs::Macos => PathForm::Unix,
        }
    }

    /// The separator the builder's paths are joined with.
    fn separator(&self) -> char {
        match self.os {
            BuilderOs::Windows => '\\',
            BuilderOs::Macos => '/',
        }
    }

    /// The root without its trailing separators: `\` and `/` on Windows, `/` on macOS.
    fn base(&self) -> &str {
        match self.os {
            BuilderOs::Windows => self.root.trim_end_matches(['\\', '/']),
            BuilderOs::Macos => self.root.trim_end_matches('/'),
        }
    }

    /// The work folder `key`: `{root}\work\{key}`, or `{root}/work/{key}` on macOS.
    pub(crate) fn work(&self, key: &str) -> String {
        let separator = self.separator();
        format!("{}{separator}work{separator}{key}", self.base())
    }

    /// The bare repository of `repo`: `{root}\{repo}.git`, or `{root}/{repo}.git` on macOS.
    pub(crate) fn bare(&self, repo: &str) -> String {
        format!("{}{}{repo}.git", self.base(), self.separator())
    }

    /// The folder outputs wait in: `{root}\outputs`, or `{root}/outputs` on macOS.
    pub(crate) fn output_folder(&self) -> String {
        format!("{}{}outputs", self.base(), self.separator())
    }

    /// The job of a run of the work folder `key` named with `nonce`: `Local\gate-remote-{key}-{nonce}`, or
    /// `gate-remote-{key}-{nonce}` on macOS, which has no `Local\` namespace.
    pub(crate) fn job(&self, key: &str, nonce: &str) -> String {
        match self.os {
            BuilderOs::Windows => format!(r"Local\gate-remote-{key}-{nonce}"),
            BuilderOs::Macos => format!("gate-remote-{key}-{nonce}"),
        }
    }

    /// The line ssh carries for `verb` ([`payload::line`]): the builder's gate [`BOX_EXE`], or `<root>/bin/gate-box` on
    /// macOS.
    pub(crate) fn line(&self, verb: BoxVerb) -> String {
        match self.os {
            BuilderOs::Windows => payload::line(BOX_EXE, verb),
            BuilderOs::Macos => payload::line(&format!("{}{UNIX_BOX}", self.base()), verb),
        }
    }

    /// The URL a snapshot of `repo` is pushed to over ssh: `{alias}:{bare}`, the bare repository's path with `/`
    /// separators.
    pub(crate) fn push_url(&self, repo: &str) -> String {
        format!("{}:{}", self.alias, self.bare(repo).replace('\\', "/"))
    }

    /// The receive-pack the push names: pwsh's on Windows, `None`, git's own, on macOS.
    pub(crate) fn receive_pack(&self) -> Option<String> {
        match self.os {
            BuilderOs::Windows => Some(RECEIVE_PACK.to_owned()),
            BuilderOs::Macos => None,
        }
    }

    /// The upload-pack the sync-back's fetch names: pwsh's on Windows, `None`, git's own, on macOS.
    pub(crate) fn upload_pack(&self) -> Option<String> {
        match self.os {
            BuilderOs::Windows => Some(UPLOAD_PACK.to_owned()),
            BuilderOs::Macos => None,
        }
    }
}

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

/// The option every call of `gate box`, and `gate push`'s, puts ahead of [`SSH_OPTIONS`]: no terminal asked for, so
/// standard input reaches the builder through no pty.
const REQUEST_TTY: [&str; 2] = ["-o", "RequestTTY=no"];

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
    /// The root of a builder the routing file gives none ([`Profile::of`]), which every work folder and bare repository
    /// lives under.
    fn root(&self) -> &str;
    /// The profile `builder` is laid out and spoken to by over this transport: the ssh transport's rule
    /// ([`Profile::of`]) unless the transport gives its own.
    fn profile(&self, builder: &Builder) -> Profile {
        Profile::of(builder, self.root())
    }
    /// The program that runs `line` on the builder `host`, over a connection `wire` says how to carry.
    fn call(&self, host: &str, line: &str, wire: Wire) -> Program;
    /// Where the snapshot of `repo` is pushed on the builder of `profile`; else why the repo's name cannot be sent there.
    fn push_target(&self, profile: &Profile, repo: &str) -> Result<PushTarget, String>;
    /// The command git runs on the builder of `profile` to serve the sync-back's fetch, the upload-pack counterpart of
    /// the push's receive-pack; `None` for a bare repository on this machine, where the push names none either.
    fn upload_pack(&self, profile: &Profile) -> Option<String>;
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

/// ssh's words for running `line` on `host` over a connection `wire` says how to carry: no terminal asked for, the
/// options, the compression option for a compressed one, `--`, the host and the line, each a word of its own.
pub(crate) fn ssh_words(host: &str, line: &str, wire: Wire) -> Vec<OsString> {
    let compression: &[&str] = match wire {
        Wire::Compressed => &COMPRESSION,
        Wire::Plain => &[],
    };
    REQUEST_TTY
        .iter()
        .chain(&SSH_OPTIONS)
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
        let (folder, over_ssh) = local_form(transport).ok_or_else(|| cannot("it is not local:<folder>".to_owned()))?;
        let git_ssh = match (over_ssh, stub) {
            (false, _) => None,
            (true, Some(stub)) => Some(ssh::git_ssh_command(stub)),
            (true, None) => {
                return Err(cannot(
                    "local-ssh:<folder> runs git through --test-remote-ssh, which is not given".to_owned(),
                ));
            }
        };
        let root = local_root(folder).map_err(|error| cannot(error.to_string()))?;
        let gate = std::env::current_exe().map_err(|error| cannot(format!("cannot find this gate's own exe: {error}")))?;
        Ok(Local {
            root,
            gate,
            stub: stub.map(Path::to_path_buf),
            git_ssh,
        })
    }
}

/// The folder of the local transport `transport` names and whether it is the `local-ssh:` form; `None` when it is
/// neither `local:<folder>` nor `local-ssh:<folder>`.
#[cfg(feature = "test-seams")]
fn local_form(transport: &str) -> Option<(&str, bool)> {
    match transport.split_once(':') {
        Some(("local", folder)) if !folder.is_empty() => Some((folder, false)),
        Some(("local-ssh", folder)) if !folder.is_empty() => Some((folder, true)),
        _ => None,
    }
}

/// The builder's root of a local transport's `folder`: the folder read against the working directory.
#[cfg(feature = "test-seams")]
fn local_root(folder: &str) -> std::io::Result<String> {
    std::path::absolute(folder).map(|root| root.to_string_lossy().into_owned())
}

/// The profile a routed run of `builder` reaches it by: over the local transport `transport` names, its folder read as
/// [`Local::new`] reads it ([`Profile::in_folder`]), else over the ssh transport ([`Profile::of`]). Else why the named
/// transport has none.
#[cfg(feature = "test-seams")]
pub(crate) fn builder_profile(builder: &Builder, transport: Option<&str>) -> Result<Profile, String> {
    let Some(transport) = transport else {
        return Ok(Profile::of(builder, ssh::BUILDER_ROOT));
    };
    let said = gate_router::escaped(transport);
    let (folder, _) = local_form(transport).ok_or_else(|| format!("the remote transport '{said}' is not local:<folder>"))?;
    let folder =
        local_root(folder).map_err(|error| format!("cannot read the remote transport '{said}': {}", gate_router::escaped(&error.to_string())))?;
    Ok(Profile::in_folder(builder, &folder))
}

/// The profile a routed run of `builder` reaches it by: the ssh transport's ([`Profile::of`]), in a gate built without
/// the test seams.
#[cfg(not(feature = "test-seams"))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the signature of the test-seams build, whose named transport can be unusable"
)]
pub(crate) fn builder_profile(builder: &Builder, _transport: Option<&str>) -> Result<Profile, String> {
    Ok(Profile::of(builder, ssh::BUILDER_ROOT))
}

#[cfg(feature = "test-seams")]
impl Transport for Local {
    fn root(&self) -> &str {
        &self.root
    }

    fn profile(&self, builder: &Builder) -> Profile {
        Profile::in_folder(builder, &self.root)
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

    fn push_target(&self, profile: &Profile, repo: &str) -> Result<PushTarget, String> {
        Ok(match &self.git_ssh {
            Some(ssh_command) => PushTarget {
                url: profile.push_url(repo),
                receive_pack: None,
                ssh_command: Some(ssh_command.clone()),
            },
            None => PushTarget {
                url: profile.bare(repo),
                receive_pack: None,
                ssh_command: None,
            },
        })
    }

    fn upload_pack(&self, _profile: &Profile) -> Option<String> {
        None
    }

    fn box_runs_here(&self) -> bool {
        true
    }
}

/// The words `line` carries after the builder's gate, [`BOX_EXE`] or a macOS builder's `<root>/bin/gate-box`: `box`,
/// the verb and `--stdin`.
#[cfg(feature = "test-seams")]
fn box_words(line: &str) -> Vec<OsString> {
    let unix_gate = || {
        line.split_once(' ')
            .filter(|(gate, _)| gate.starts_with('/') && gate.ends_with(UNIX_BOX))
            .map(|(_, rest)| rest)
    };
    line.strip_prefix(BOX_EXE)
        .or_else(unix_gate)
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

    use super::{Profile, Program, PushTarget, SSH_OPTIONS, Transport, Wire, sent_repo, ssh_words};
    use crate::environment::Variables;
    use crate::remote::git::judged;
    use crate::run::find_program;

    pub(super) const BUILDER_ROOT: &str = r"D:\gate-remote";
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

        fn push_target(&self, profile: &Profile, repo: &str) -> Result<PushTarget, String> {
            let repo = sent_repo(repo)?;
            Ok(PushTarget {
                url: profile.push_url(repo),
                receive_pack: profile.receive_pack(),
                ssh_command: Some(git_ssh_command(&self.program)),
            })
        }

        fn upload_pack(&self, profile: &Profile) -> Option<String> {
            profile.upload_pack()
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
    pub(crate) fn git_ssh_command(ssh: &Path) -> String {
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

    use gate_args::BoxVerb;
    use gate_router::{Builder, BuilderOs};

    use super::ssh::{Ssh, git_ssh_command};
    use super::{Local, Profile, Program, PushTarget, Transport, Wire, box_words, ssh_words};
    use crate::environment::Variables;

    fn words(texts: &[&str]) -> Vec<OsString> {
        texts.iter().map(OsString::from).collect()
    }

    /// The Windows builder `builder` under `transport`'s own root, as `host` is reached.
    fn windows(transport: &dyn Transport) -> Profile {
        Profile::new("builder", BuilderOs::Windows, transport.root())
    }

    /// The macOS builder `mac-1` under `/Users/b/gate-remote`.
    fn mac() -> Profile {
        let builder = Builder {
            alias: "mac-1".to_owned(),
            os: BuilderOs::Macos,
            root: Some("/Users/b/gate-remote".to_owned()),
            slots: 1,
            id: Some("00000000".to_owned()),
        };
        Profile::of(&builder, r"D:\gate-remote")
    }

    #[test]
    fn host_s_profile_is_the_windows_constants() {
        let host = Builder {
            alias: "builder".to_owned(),
            os: BuilderOs::Windows,
            root: None,
            slots: 1,
            id: None,
        };
        let ssh = Ssh {
            program: PathBuf::from(r"C:\Program Files\OpenSSH\ssh.exe"),
        };
        let profile = Profile::of(&host, ssh.root());
        assert_eq!(profile.root(), r"D:\gate-remote");
        assert_eq!(profile.line(BoxVerb::Run), RUN_LINE);
        assert_eq!(profile.push_url("app"), "builder:D:/gate-remote/app.git");
        assert_eq!(
            profile.receive_pack().as_deref(),
            Some("pwsh -NoProfile -NonInteractive -Command git-receive-pack")
        );
        assert_eq!(
            profile.upload_pack().as_deref(),
            Some("pwsh -NoProfile -NonInteractive -Command git-upload-pack")
        );
        assert_eq!(profile.form(), gate_wire::PathForm::Windows);
    }

    /// Under a test transport a Windows entry's own root gives way to the transport's folder, so a case's run lays out,
    /// and pushes, inside the case folder; over ssh the entry's root stands. A macOS entry keeps its root either way.
    #[test]
    fn a_windows_entry_s_root_gives_way_to_the_local_transport_s_folder() {
        let entry = Builder {
            alias: "win-2".to_owned(),
            os: BuilderOs::Windows,
            root: Some(r"E:\remote".to_owned()),
            slots: 1,
            id: Some("084c286b".to_owned()),
        };
        let local = Local::new(r"local:C:\builder", None).expect("a local transport");
        let ssh = Ssh {
            program: PathBuf::from(r"C:\Program Files\OpenSSH\ssh.exe"),
        };

        let profile = local.profile(&entry);
        let named = super::builder_profile(&entry, Some(r"local:C:\builder")).expect("the local transport is read");

        assert_eq!(profile.root(), r"C:\builder");
        assert_eq!(profile.work("app-1a2b3c4d"), r"C:\builder\work\app-1a2b3c4d");
        let pushed = local.push_target(&profile, "app").expect("the repo can be pushed");
        assert_eq!(pushed.url, r"C:\builder\app.git");
        assert_eq!(named, profile, "the routing's trees are laid out as the client's run is");
        assert_eq!(ssh.profile(&entry).root(), r"E:\remote", "over ssh the entry's root stands");
        let mac = Builder {
            alias: "mac-1".to_owned(),
            os: BuilderOs::Macos,
            root: Some("/Users/b/gate-remote".to_owned()),
            slots: 1,
            id: Some("1f0efc62".to_owned()),
        };
        assert_eq!(local.profile(&mac).root(), "/Users/b/gate-remote", "a macOS entry keeps its own root");
    }

    #[test]
    fn a_macos_builder_s_profile_runs_its_gate_box_and_git_s_own_pack_commands() {
        let profile = mac();
        assert_eq!(profile.root(), "/Users/b/gate-remote", "its own root, not the transport's");
        assert_eq!(
            profile.line(BoxVerb::Preflight),
            "/Users/b/gate-remote/bin/gate-box box preflight --stdin"
        );
        assert_eq!(
            profile.line(BoxVerb::RegisterDesktop),
            "/Users/b/gate-remote/bin/gate-box box register-desktop"
        );
        assert_eq!(profile.push_url("duel"), "mac-1:/Users/b/gate-remote/duel.git");
        assert_eq!((profile.receive_pack(), profile.upload_pack()), (None, None), "git's own");
        assert_eq!(profile.form(), gate_wire::PathForm::Unix);

        let ssh = Ssh {
            program: PathBuf::from(r"C:\Program Files\OpenSSH\ssh.exe"),
        };
        assert_eq!(
            ssh.push_target(&profile, "duel"),
            Ok(PushTarget {
                url: "mac-1:/Users/b/gate-remote/duel.git".to_owned(),
                receive_pack: None,
                ssh_command: Some(git_ssh_command(&ssh.program)),
            })
        );
        assert_eq!(ssh.upload_pack(&profile), None);
    }

    #[test]
    fn the_local_transport_runs_the_words_after_a_macos_builder_s_gate_box() {
        let line = mac().line(BoxVerb::Preflight);
        assert_eq!(box_words(&line), words(&["box", "preflight", "--stdin"]));
        let local = Local::new(r"local:C:\builder", None).expect("a local transport");
        assert_eq!(local.call("mac-1", &line, Wire::Plain).args, words(&["box", "preflight", "--stdin"]));
    }

    const RUN_LINE: &str = r#""C:\Program Files\gate\bin\gate.exe" box run --stdin"#;

    #[test]
    fn ssh_s_words_are_the_options_then_dash_dash_then_the_host_then_the_line() {
        assert_eq!(
            ssh_words("builder", RUN_LINE, Wire::Plain),
            words(&[
                "-o",
                "RequestTTY=no",
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
                RUN_LINE,
            ])
        );
    }

    #[test]
    fn every_box_call_s_ssh_words_ask_for_no_tty() {
        let preflight = r#""C:\Program Files\gate\bin\gate.exe" box preflight --stdin"#;
        for (line, wire) in [(RUN_LINE, Wire::Compressed), (preflight, Wire::Plain)] {
            let said = ssh_words("builder", line, wire);
            let tty = said.iter().position(|word| word == "RequestTTY=no").expect("no terminal is asked for");
            let dashes = said.iter().position(|word| word == "--").expect("the options end with --");
            assert!(
                tty > 0 && said.get(tty - 1).is_some_and(|word| word == "-o"),
                "an option of its own: {said:?}"
            );
            assert!(tty < dashes, "an option, before the host: {said:?}");
        }
    }

    #[test]
    fn the_run_call_s_ssh_words_ask_for_compression_and_others_do_not() {
        let line = RUN_LINE;
        let compressed = ssh_words("builder", line, Wire::Compressed);
        let plain = ssh_words("builder", line, Wire::Plain);
        assert_eq!(compressed.get(..14), plain.get(..14), "every call's own options come first");
        assert_eq!(
            compressed.get(14..),
            Some(words(&["-o", "Compression=yes", "--", "builder", line]).as_slice())
        );
        assert_eq!(
            plain.get(14..),
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
            words(&["box", "run", "--stdin"]),
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
            ssh.push_target(&windows(&ssh), "app"),
            Ok(PushTarget {
                url: "builder:D:/gate-remote/app.git".to_owned(),
                receive_pack: Some("pwsh -NoProfile -NonInteractive -Command git-receive-pack".to_owned()),
                ssh_command: Some(git_ssh_command(&ssh.program)),
            })
        );
        assert_eq!(
            ssh.upload_pack(&windows(&ssh)),
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
        let line = r#""C:\Program Files\gate\bin\gate.exe" box preflight --stdin"#;
        assert_eq!(box_words(line), words(&["box", "preflight", "--stdin"]));
        let local = Local::new(r"local:C:\builder", None).expect("a local transport");
        assert_eq!(local.root(), r"C:\builder");
        assert_eq!(local.call("builder", line, Wire::Plain).args, words(&["box", "preflight", "--stdin"]));
        assert_eq!(
            local.push_target(&windows(&local), "app").expect("a push target").url,
            r"C:\builder\app.git"
        );
        assert_eq!(local.upload_pack(&windows(&local)), None, "a bare repository here names no pack command");
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
            local.push_target(&windows(&local), "app"),
            Ok(PushTarget {
                url: "builder:C:/builder/app.git".to_owned(),
                receive_pack: None,
                ssh_command: Some(git_ssh_command(&stub)),
            })
        );
        assert_eq!(local.upload_pack(&windows(&local)), None, "git runs its own service names");
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
            ssh.push_target(&windows(&ssh), "app'x"),
            Err("cannot push to the repo 'app'x': a repo name is letters, digits, '_' and '-'".to_owned())
        );
        assert!(ssh.push_target(&windows(&ssh), "").is_err(), "an empty name is refused too");
        assert!(ssh.push_target(&windows(&ssh), "App_1-b").is_ok(), "a name gate-router makes is sent");
    }

    #[test]
    fn ssh_is_found_as_ssh_exe_past_an_earlier_ssh_cmd() {
        let root = crate::test_support::temp_dir().join(format!("gate-ssh-find-{}", std::process::id()));
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
