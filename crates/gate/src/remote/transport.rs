//! How the client reaches the builder: the program and words of one call of `gate box` there, and where a snapshot is
//! pushed. The ssh transport runs OpenSSH, and git over it, as the PowerShell client does: every option a word of its
//! own and `--` before the host. The local transport of the test seams has its builder root in a folder here, runs
//! this gate's own `box` with the words the line carries after the builder's gate, or a stand-in for ssh with ssh's own
//! words, and pushes to the bare repository's own path; its `local-ssh:` form pushes and fetches as the ssh transport
//! does, to `<host>:<path>` with the stand-in as git's ssh. Either form gives one folder every builder is laid out under,
//! or a map, `local:<alias>=<folder>|…`, giving each builder its own.
//!
//! A gate built without the `test-seams` feature has the ssh transport alone.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use gate_args::BoxVerb;
use gate_router::{Builder, BuilderOs};
use gate_wire::PathForm;

use crate::remote::payload::{self, BOX_EXE};

/// The pack commands a Windows builder's git is served through: pwsh reads the single-quoted path git sends through cmd.
/// A push runs with `receive.autogc` off, so no gc it starts detaches past a drain; the scheduled age prune collects the
/// bare repositories instead.
const RECEIVE_PACK: &str = "pwsh -NoProfile -NonInteractive -Command $env:GIT_CONFIG_COUNT='1'; $env:GIT_CONFIG_KEY_0='receive.autogc'; \
                            $env:GIT_CONFIG_VALUE_0='false'; git-receive-pack";
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
/// command too, the stand-in's, which git runs for the push and the sync-back's calls. Either form may give a map of
/// `{alias}={folder}` entries a `|` between ([`folders_of`]), each builder laid out under its own.
#[cfg(feature = "test-seams")]
pub(crate) struct Local {
    folders: Folders,
    gate: PathBuf,
    stub: Option<PathBuf>,
    git_ssh: Option<String>,
}

/// The folders a local transport names: one every builder is laid out under, or each builder's own by its alias.
#[cfg(feature = "test-seams")]
enum Folders {
    /// `local:<folder>`: every builder under the one folder.
    One(String),
    /// `local:<alias>=<folder>|…`: the alias as the map spells it, and the folder read against the working directory.
    Map(Vec<(String, String)>),
}

#[cfg(feature = "test-seams")]
impl Folders {
    /// The folder [`Transport::root`] gives: the plain form's one folder, which every builder of that form is laid out
    /// under. A map's builders are laid out under their own entries ([`folder_for`]), never under this.
    fn one(&self) -> &str {
        match self {
            Folders::One(folder) => folder,
            Folders::Map(entries) => entries.first().map_or("", |(_, folder)| folder.as_str()),
        }
    }
}

#[cfg(feature = "test-seams")]
impl Local {
    /// The local transport `transport` names, `local:<folder>` or `local-ssh:<folder>` — or either with a map of
    /// `{alias}={folder}` entries — with each folder read against the working directory, its calls made through `stub`
    /// when one is given, which `local-ssh:` needs; else the line saying why it cannot be used.
    pub(crate) fn new(transport: &str, stub: Option<&Path>) -> Result<Local, String> {
        Local::new_beside(transport, stub, std::env::var_os("LOCALAPPDATA").as_deref())
    }

    /// [`Local::new`] with the folder holding the routing file a map form is checked against given as `local_app_data`.
    fn new_beside(transport: &str, stub: Option<&Path>, local_app_data: Option<&std::ffi::OsStr>) -> Result<Local, String> {
        let cannot = |why: String| format!("gate: cannot use the remote transport '{transport}': {why}");
        let (part, over_ssh) = local_form(transport).ok_or_else(|| cannot("it is not local:<folder>".to_owned()))?;
        let folders = folders_of(part)
            .map_err(|error| cannot(error.to_string()))?
            .ok_or_else(|| cannot("it is not local:<folder>".to_owned()))?;
        let git_ssh = match (over_ssh, stub) {
            (false, _) => None,
            (true, Some(stub)) => Some(ssh::git_ssh_command(stub)),
            (true, None) => {
                return Err(cannot(
                    "local-ssh:<folder> runs git through --test-remote-ssh, which is not given".to_owned(),
                ));
            }
        };
        check_map(&folders, local_app_data)?;
        let gate = std::env::current_exe().map_err(|error| cannot(format!("cannot find this gate's own exe: {error}")))?;
        Ok(Local {
            folders,
            gate,
            stub: stub.map(Path::to_path_buf),
            git_ssh,
        })
    }
}

/// What the local transport `transport` names after its form — one folder, or a map of them ([`folders_of`]) — and
/// whether it is the `local-ssh:` form; `None` when it is neither `local:<folder>` nor `local-ssh:<folder>`.
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

/// The folders the part of a local transport's name after its form gives: `{folder}` for every builder, or
/// `{alias}={folder}` entries a `|` between for each builder's own, each folder read against the working directory;
/// `None` for a part that is neither form, which the caller refuses as not `local:<folder>`.
#[cfg(feature = "test-seams")]
fn folders_of(part: &str) -> std::io::Result<Option<Folders>> {
    if !part.contains('|') && map_alias(part).is_none() {
        return local_root(part).map(|folder| Some(Folders::One(folder)));
    }
    let mut entries: Vec<(String, String)> = Vec::new();
    for entry in part.split('|') {
        let Some((alias, folder)) = map_entry(entry) else {
            return Ok(None);
        };
        if entries.iter().any(|(named, _)| named.eq_ignore_ascii_case(alias)) {
            return Ok(None);
        }
        entries.push((alias.to_owned(), local_root(folder)?));
    }
    Ok(Some(Folders::Map(entries)))
}

/// The map entry `{alias}={folder}`, split at its first `=`: the alias, one a routing file may name a builder by
/// ([`map_alias`]), and the folder it names, which is not empty; `None` for a part that is no entry, such as a drive
/// path, whose `C:\` comes before its `=`.
#[cfg(feature = "test-seams")]
fn map_entry(entry: &str) -> Option<(&str, &str)> {
    let (alias, folder) = entry.split_once('=')?;
    (!folder.is_empty() && gate_router::is_usable_host(alias)).then_some((alias, folder))
}

/// The alias a map part starts with and an `=` after, whether or not a folder follows: a routing file's builder alias,
/// so a drive path's `C:` before its `=` names none, and a part shaped `{alias}=` is a map entry whose folder is
/// missing rather than a plain folder serving every builder.
#[cfg(feature = "test-seams")]
fn map_alias(part: &str) -> Option<&str> {
    let (alias, _) = part.split_once('=')?;
    gate_router::is_usable_host(alias).then_some(alias)
}

/// The folder the builder `alias` is laid out under: the plain form's one folder, else the map's entry for the alias,
/// matched in any ASCII letter case; else the line naming the builder the map leaves out.
#[cfg(feature = "test-seams")]
fn folder_for<'f>(folders: &'f Folders, alias: &str) -> Result<&'f str, String> {
    match folders {
        Folders::One(folder) => Ok(folder),
        Folders::Map(entries) => entries
            .iter()
            .find(|(named, _)| named.eq_ignore_ascii_case(alias))
            .map(|(_, folder)| folder.as_str())
            .ok_or_else(|| format!("gate: --test-remote-transport names no folder for builder {alias}")),
    }
}

/// Refuses the map `folders` holds when the routing file under `local_app_data` names a builder it leaves out: the run
/// cannot say where that builder's work folder would lie, and no folder stands in for it. A routing file that is not
/// there or cannot be read names no builder, so the transport leaves its own report to the routing.
#[cfg(feature = "test-seams")]
fn check_map(folders: &Folders, local_app_data: Option<&std::ffi::OsStr>) -> Result<(), String> {
    if !matches!(folders, Folders::Map(_)) {
        return Ok(());
    }
    let Some(path) = crate::route::remote_json_path(local_app_data) else {
        return Ok(());
    };
    let Ok(Some(config)) = crate::route::read_remote_json(&path) else {
        return Ok(());
    };
    // `host`, each `hosts` entry (which `gate check --builder` resolves to a builder) and each `builders` entry: every
    // builder a routed run or a check can lay out. A file whose `hosts` holds no list names `host` all the same.
    let hosts = config
        .builders()
        .ok()
        .into_iter()
        .flat_map(|builders| builders.iter().map(String::as_str));
    let named = std::iter::once(config.host())
        .chain(hosts)
        .chain(config.builder_entries().map(|builder| builder.alias.as_str()));
    for alias in named {
        folder_for(folders, alias)?;
    }
    Ok(())
}

/// The profile a routed run of `builder` reaches it by: over the local transport `transport` names, its folder read as
/// [`Local::new`] reads it ([`Profile::in_folder`]), else over the ssh transport ([`Profile::of`]). Else why the named
/// transport has none, a map leaving the builder out among them.
#[cfg(feature = "test-seams")]
pub(crate) fn builder_profile(builder: &Builder, transport: Option<&str>) -> Result<Profile, String> {
    let Some(transport) = transport else {
        return Ok(Profile::of(builder, ssh::BUILDER_ROOT));
    };
    let said = gate_router::escaped(transport);
    let not_form = || format!("the remote transport '{said}' is not local:<folder>");
    let (part, _) = local_form(transport).ok_or_else(not_form)?;
    let folders = folders_of(part)
        .map_err(|error| format!("cannot read the remote transport '{said}': {}", gate_router::escaped(&error.to_string())))?
        .ok_or_else(not_form)?;
    Ok(Profile::in_folder(builder, folder_for(&folders, &builder.alias)?))
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
        self.folders.one()
    }

    #[expect(
        clippy::expect_used,
        reason = "a map is checked against the routing file when the transport is chosen, so no routed builder is missing"
    )]
    fn profile(&self, builder: &Builder) -> Profile {
        let folder = folder_for(&self.folders, &builder.alias).expect("the map was checked against remote.json when the transport was chosen");
        Profile::in_folder(builder, folder)
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
            data: None,
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
            data: None,
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
            Some(
                "pwsh -NoProfile -NonInteractive -Command $env:GIT_CONFIG_COUNT='1'; $env:GIT_CONFIG_KEY_0='receive.autogc'; \
                 $env:GIT_CONFIG_VALUE_0='false'; git-receive-pack"
            )
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
            data: None,
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
            data: None,
        };
        assert_eq!(local.profile(&mac).root(), "/Users/b/gate-remote", "a macOS entry keeps its own root");
    }

    /// The Windows builder `alias` of a routing file entry with no root of its own.
    fn entry(alias: &str) -> Builder {
        Builder {
            alias: alias.to_owned(),
            os: BuilderOs::Windows,
            root: None,
            slots: 1,
            id: None,
            data: None,
        }
    }

    /// The local transport `transport` names, checked against no routing file: [`Local::new_beside`] with none.
    fn local(transport: &str, stub: Option<&std::path::Path>) -> Local {
        Local::new_beside(transport, stub, None).expect("a local transport")
    }

    /// Each map entry lays its builder out under its own folder, and the routing's profile agrees, in both forms.
    #[test]
    fn a_transport_map_gives_each_builder_its_own_folder() {
        let stub = PathBuf::from(r"C:\suite\fake-ssh.exe");
        for transport in [r"local:builder-a=C:\one|builder-b=C:\two", r"local-ssh:builder-a=C:\one|builder-b=C:\two"] {
            let stubbed = transport.starts_with("local-ssh:").then_some(stub.as_path());
            let local = local(transport, stubbed);
            for (alias, folder) in [("builder-a", r"C:\one"), ("builder-b", r"C:\two")] {
                assert_eq!(local.profile(&entry(alias)).root(), folder, "{transport}");
                let named = super::builder_profile(&entry(alias), Some(transport)).expect("the map is read");
                assert_eq!(
                    named,
                    local.profile(&entry(alias)),
                    "the routing's trees are laid out as the client's run is"
                );
            }
        }
    }

    /// The plain form, one folder, still serves every builder as it did.
    #[test]
    fn a_single_folder_transport_keeps_serving_every_builder() {
        let stub = PathBuf::from(r"C:\suite\fake-ssh.exe");
        for transport in [r"local:C:\builder", r"local-ssh:C:\builder"] {
            let stubbed = transport.starts_with("local-ssh:").then_some(stub.as_path());
            let local = local(transport, stubbed);
            for alias in ["builder", "builder-a"] {
                assert_eq!(local.profile(&entry(alias)).root(), r"C:\builder", "{transport}");
                let named = super::builder_profile(&entry(alias), Some(transport)).expect("the folder is read");
                assert_eq!(named.root(), r"C:\builder", "{transport}");
            }
        }
    }

    /// A map that leaves the builder out names it in the line that refuses it, never a default root.
    #[test]
    fn a_transport_map_without_the_builder_fails_with_its_alias() {
        assert_eq!(
            super::builder_profile(&entry("builder-b"), Some(r"local:builder-a=C:\one")).err(),
            Some("gate: --test-remote-transport names no folder for builder builder-b".to_owned())
        );
    }

    /// A folder holding `=` keeps it: an entry splits at its first `=` and no other.
    #[test]
    fn a_map_entry_splits_at_its_first_equals_sign() {
        let local = local(r"local:builder-a=C:\one=two|builder-b=C:\three", None);
        assert_eq!(local.profile(&entry("builder-a")).root(), r"C:\one=two");
        assert_eq!(local.profile(&entry("builder-b")).root(), r"C:\three");
    }

    /// A builder alias gate-router accepts — one with a dot, or a `user@host` — is a map entry, not a plain folder,
    /// whether the map holds one entry or several.
    #[test]
    fn a_map_entry_takes_every_alias_gate_router_accepts() {
        let both = local(r"local:mac-mini.lan=C:\m|user@host=C:\h", None);
        assert_eq!(both.profile(&entry("mac-mini.lan")).root(), r"C:\m");
        assert_eq!(both.profile(&entry("user@host")).root(), r"C:\h");
        let alone = local(r"local:mac-mini.lan=C:\m", None);
        assert_eq!(alone.profile(&entry("mac-mini.lan")).root(), r"C:\m", "a single entry is a map too");
    }

    /// An alias with no folder after its `=` is a malformed map, refused, never a plain folder serving every builder.
    #[test]
    fn a_map_entry_with_no_folder_is_refused() {
        assert_eq!(
            Local::new_beside(r"local:builder-a=", None, None).err(),
            Some(r"gate: cannot use the remote transport 'local:builder-a=': it is not local:<folder>".to_owned())
        );
    }

    /// A drive path's `C:` comes before its `=`, so a folder holding one stays the plain form, one for every builder.
    #[test]
    fn a_drive_path_holding_an_equals_sign_stays_one_folder() {
        let local = local(r"local:C:\a=b", None);
        for alias in ["builder", "builder-a"] {
            assert_eq!(local.profile(&entry(alias)).root(), r"C:\a=b");
        }
    }

    /// Two entries naming one alias in different letter cases are refused: the map would take the first silently.
    #[test]
    fn a_map_naming_an_alias_twice_is_refused() {
        let transport = r"local:builder-a=C:\one|Builder-A=C:\two";
        assert_eq!(
            Local::new_beside(transport, None, None).err(),
            Some(format!("gate: cannot use the remote transport '{transport}': it is not local:<folder>"))
        );
    }

    /// The `hosts` entries a `gate check --builder` resolves are checked too, not the `builders` entries alone.
    #[test]
    fn a_map_the_routing_file_leaves_out_a_hosts_entry_for_fails_before_the_run() {
        let root = crate::test_support::temp_dir().join(format!("gate-map-hosts-{}", std::process::id()));
        let folder = root.join("gate");
        fs::create_dir_all(&folder).expect("the routing folder can be made");
        fs::write(
            folder.join("remote.json"),
            r#"{"host": "builder", "hosts": ["hosts-a"], "repos": {"app": ["*tool*"]}}"#,
        )
        .expect("the routing file can be written");
        let map = super::Folders::Map(vec![("builder".to_owned(), r"C:\two".to_owned())]);

        assert_eq!(
            super::check_map(&map, Some(root.as_os_str())),
            Err("gate: --test-remote-transport names no folder for builder hosts-a".to_owned())
        );
        fs::remove_dir_all(&root).expect("the folder can be removed");
    }

    /// A map is checked against the routing file's builders before the run starts: one it leaves out fails with the
    /// alias the file spells, an extra map entry passes, and a routing file that is not there passes too.
    #[test]
    fn a_map_the_routing_file_leaves_out_a_builder_for_fails_before_the_run() {
        let root = crate::test_support::temp_dir().join(format!("gate-map-check-{}", std::process::id()));
        let folder = root.join("gate");
        fs::create_dir_all(&folder).expect("the routing folder can be made");
        let map = || {
            super::Folders::Map(vec![
                ("builder-a".to_owned(), r"C:\one".to_owned()),
                ("builder".to_owned(), r"C:\two".to_owned()),
            ])
        };
        let routing = |host: &str| {
            format!(
                concat!(
                    r#"{{"host": "{}", "repos": {{"a": ["*x*"]}}, "#,
                    r#""builders": [{{"host": "builder-a", "os": "windows", "repos": {{"a": ["*x*"]}}}}]}}"#
                ),
                host
            )
        };

        assert_eq!(
            super::check_map(&map(), Some(root.as_os_str())),
            Ok(()),
            "no routing file names no builder"
        );
        fs::write(folder.join("remote.json"), routing("host-1")).expect("the routing file can be written");
        assert_eq!(
            super::check_map(&map(), Some(root.as_os_str())),
            Err("gate: --test-remote-transport names no folder for builder host-1".to_owned())
        );
        fs::write(folder.join("remote.json"), routing("builder")).expect("the routing file can be written");
        assert_eq!(
            super::check_map(&map(), Some(root.as_os_str())),
            Ok(()),
            "an extra map entry names no builder of the file"
        );
        fs::remove_dir_all(&root).expect("the folder can be removed");
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
                receive_pack: Some(
                    "pwsh -NoProfile -NonInteractive -Command $env:GIT_CONFIG_COUNT='1'; $env:GIT_CONFIG_KEY_0='receive.autogc'; \
                     $env:GIT_CONFIG_VALUE_0='false'; git-receive-pack"
                        .to_owned()
                ),
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
