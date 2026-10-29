//! `gate-box box run`: the request read and checked as the Windows box reads it, what this builder does not build
//! refused so the PC runs the command itself, the work root checked, the work lock taken, the subfolder placed, the work
//! folder laid out, the inputs the PC sent checked and the payload's snapshot checked out; then the command run, the tree it left synced back
//! and, after a passing command, its outputs packed for the PC to fetch ([`run`]). Each line is said as the Windows box says it, byte for
//! byte.

use std::ffi::{CStr, OsStr};
use std::fs;
use std::io::{self, BufRead};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use gate_args::BoxSeams;
use gate_inputs::failure::Caps;
use gate_inputs::manifest::{Listing, Manifest};
use gate_wire::drain::refused_lines;
use gate_wire::{
    CROSS_STEP_START, CrossBuild, LAYOUT_CHANGING, PairSibling, PathForm, Payload, Refusal, bare_stem, busy_line, checkout_done_line,
    checkout_start_line, escaped, layout_line, layout_text, names_another_layout, refused_line, trees,
};

use gate_watchdog::Pump;

use crate::back::{self, BackFailed};
use crate::beneath;
use crate::command::{self, CrossEnd, Launch, Sampler, command_record, run_command};
use crate::console::{Console, Forwarding, GAT, LineSink, Output, Stamp, Stream};
use crate::cross;
use crate::drain::{self, Unpassed};
use crate::files::{self, HeldLock, Locked};
use crate::git::{
    self, AlternatesFailure, CLOSED_STATUS, Failed, Git, GitClass, RunGit, Steps, Tree, failed_for, for_repo, line_ends_set, origin_set,
};
use crate::heartbeat::{self, Periods};
use crate::inputs::{self, Unmatched};
use crate::lines::{
    Along, BACK_COMMITS_FAILED, CommandEnd, FAILED_STATUS, FolderFlaw, Godot, Login, OwnerFlaw, Translation, USAGE_STATUS, WRITABLE_BY_OTHERS,
    back_commits_line, command_status, login_path_line, owner_flaw, recorded_group, rosetta_lines, signal_guard, translation, work_layout,
};
use crate::outputs::{self, Asked};
use crate::packages;
use crate::preflight::{checked_root, layout_record};
use crate::probe;
use crate::process::{self, GroupRecord};
use crate::slots::{self, Untaken};
use crate::stop::{Record, own_group, record_current, records_folder, unusable_refusal};
use crate::tools;

/// The status of a run refused: another run holds the work folder, it could not be laid out, or it asks for what this
/// builder does not build. Said with a refusal tag and before any start marker, so the PC runs the command itself.
pub const REFUSED_STATUS: u8 = 75;

/// What a run asks for that this builder does not build, refused with the tag of its refusal.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Unbuilt {
    pub(crate) refusal: Refusal,
    /// What asks for it: the payload key.
    pub(crate) key: &'static str,
}

impl Unbuilt {
    /// The line said after the refusal's tag: the layout line naming the repo of the bare repository `bare` for a
    /// `layout` refusal ([`layout_line`]), else `gate: remote <key> is not built on this builder`.
    pub(crate) fn line(self, bare: &str) -> String {
        let why = format!("{} is not built on this builder", self.key);
        if self.refusal == Refusal::Layout {
            layout_line(repo_name(bare), &escaped(&why))
        } else {
            format!("gate: remote {why}")
        }
    }
}

/// One repo of a run, made once as the payload is read: its name on a pair run, its tree, the bare repository its
/// snapshot is fetched from and its back ref lives in, and the commit checked out.
pub struct Repo<'p> {
    /// The repo's name on a pair run, the leaf's or a sibling's; `None` on a run of one repo.
    pub name: Option<String>,
    /// The repo's tree: the work folder itself on a run of one repo, `<work>/<name>` on a pair run.
    pub tree: String,
    /// The repo's bare repository, `<root>/<name>.git`.
    pub bare: String,
    /// The commit checked out, 40 lower-case hex digits.
    pub snapshot: &'p str,
}

impl Repo<'_> {
    /// The words naming this repo's tree to git ([`Tree`]), its name carried into its layout steps' names.
    pub(crate) fn git_tree(&self) -> Tree {
        Tree::named(&self.tree, self.name.as_deref())
    }

    /// The checkout step `step` as its lines name it: followed by the repo's name on a pair run, as the Windows box's
    /// `named_step` says it; `step` itself on a run of one repo.
    pub(crate) fn step(&self, step: &str) -> String {
        match &self.name {
            Some(name) => format!("{step} {name}"),
            None => step.to_owned(),
        }
    }
}

/// The main repo and the siblings of a run in the work folder `work`, as the Windows box reads them: with no `Pair`, the
/// one repo of `bare` checked out at `snapshot` in `work` itself; with one, the main repo in `<work>/<leaf>` and each
/// sibling in `<work>/<name>` with its own bare repository and its commit from `SiblingSnapshots` ([`gate_wire::trees`]).
/// Else the usage line.
fn repos<'p>(payload: &'p Payload, work: &str, bare: &str, snapshot: &'p str) -> Result<(Repo<'p>, Vec<Repo<'p>>), String> {
    let Some(pair) = payload.pair()? else {
        let (tree, _) = trees(work, None, PathForm::Unix);
        let main = Repo {
            name: None,
            tree,
            bare: bare.to_owned(),
            snapshot,
        };
        return Ok((main, Vec::new()));
    };
    let commits = payload.sibling_snapshots(&pair)?;
    let names: Vec<&str> = pair.siblings().iter().map(PairSibling::name).collect();
    let (main_tree, sibling_trees) = trees(work, Some((pair.leaf(), &names)), PathForm::Unix);
    let main = Repo {
        name: Some(pair.leaf().to_owned()),
        tree: main_tree,
        bare: bare.to_owned(),
        snapshot,
    };
    let siblings = pair
        .siblings()
        .iter()
        .zip(commits)
        .zip(sibling_trees)
        .map(|((sibling, commit), tree)| Repo {
            name: Some(sibling.name().to_owned()),
            tree,
            bare: sibling.bare().to_owned(),
            snapshot: commit,
        })
        .collect();
    Ok((main, siblings))
}

/// What a run reads of its payload.
pub struct Request<'p> {
    /// The main repo, checked out in the work folder, or in `<work>/<leaf>` on a pair run.
    pub main: Repo<'p>,
    /// Each sibling of a pair run, in the payload's order, checked out in `<work>/<name>`; none on a run of one repo.
    pub siblings: Vec<Repo<'p>>,
    /// The work folder's key.
    pub key: &'p str,
    /// The work folder, `<root>/work/<key>`: the main repo's tree on a run of one repo, and the folder holding each
    /// repo's tree on a pair run. The lock, the index and the run's records sit beside it.
    pub work: &'p str,
    /// The work root, which git never searches up into.
    pub root: String,
    /// The folder holding the work folders, `<root>/work`, which git runs in.
    pub parent: &'p Path,
    /// The run's nonce, which its refusal tags and its start marker name.
    pub nonce: &'p str,
    /// The run's ceiling in whole seconds of load-adjusted time, `None` for none.
    pub timeout: Option<u64>,
    /// The PC's name for the builder, empty when the payload gives none.
    pub host: &'p str,
    /// The work lock, `<work>.lock`.
    pub lock: &'p str,
    /// The index the back commit is made with, `<work>.index`.
    pub index: &'p str,
    /// The run's job name.
    pub job: &'p str,
    /// The command's words, its program first.
    pub command: Vec<String>,
    /// The folder the command runs in, relative to the work folder; empty for the work folder itself.
    pub subfolder: &'p str,
    /// The back commit's author and committer variables.
    pub identity: Vec<(String, String)>,
    /// The caller's variables for the command.
    pub env: Vec<(String, String)>,
    /// The Godot version the run chose, `GodotVersion`; `None` for none.
    pub godot: Option<&'p str>,
    /// The digest of the inputs manifest the PC sent before the run, `InputsDigest`; `None` for none.
    pub inputs_digest: Option<&'p str>,
    /// The outputs the run brings back after a passing command, and the test reports among them after a failed one,
    /// `Outputs` and `OutputsFull`; `None` without globs.
    pub outputs: Option<Asked<'p>>,
    /// The cross build run after a passing command, before its outputs are brought back, `CrossBuild`; `None` for none.
    pub cross: Option<CrossBuild>,
    /// The first thing the payload asks for that this builder does not build, in the order the payload is read.
    unbuilt: Option<Unbuilt>,
}

impl<'p> Request<'p> {
    /// Reads `payload` in the order the Windows box reads it, each key checked as it does, the outputs asked for
    /// ([`Asked::new`]) read once the output folder is; then notes the first thing asking for what this builder does not
    /// build: `Desktop`, a warm-up's `WarmCommands`, `EngineEnv`, `UnrealVersion`, `SentDigest` or `LfsDigest`; then,
    /// last, the repos ([`repos`]), a sibling pair as `gate-wire` checks it. A work root holding a `:` is refused just
    /// after the work folder is read ([`work_layout`]).
    ///
    /// # Errors
    ///
    /// The usage line of the first key that cannot be read.
    pub fn read(payload: &'p Payload) -> Result<Request<'p>, String> {
        let snapshot = payload.snapshot()?;
        let (key, work) = payload.work_folder()?;
        let (root, parent) = work_layout(work)?;
        let (globs, full) = (payload.output_globs()?, payload.outputs_full()?);
        let failure = payload.failure_output_globs()?;
        let cross = payload.cross_build()?;
        let nonce = payload.nonce()?;
        let outputs = Asked::new((&globs, &failure), full, key, nonce, payload.output_folder(work)?)?
            .map(|asked| asked.crossed_for(cross.as_ref().map(|cross| cross.rid.as_str())));
        let (timeout, host) = (payload.timeout()?, payload.optional_text("Host")?);
        let desktop = payload.desktop()?;
        if desktop && payload.desktop_wait()?.is_none() {
            return Err("gate: box payload lacks DesktopWait".to_owned());
        }
        let bare = payload.path("Bare")?;
        let lock = payload.beside_work("Lock", work, "lock")?;
        let index = payload.beside_work("Index", work, "index")?;
        let job = payload.job(key)?;
        let commands = payload.commands()?;
        let warm = commands.warm_hash.is_some();
        if desktop && warm {
            return Err("gate: box payload asks a warm-up for the desktop".to_owned());
        }
        let subfolder = payload.optional_text("Subfolder")?;
        let identity = payload.identity()?;
        let env = payload.caller_env()?;
        let godot = payload.godot_version()?;
        let engine_env = !payload.engine_env()?.is_empty();
        let unreal = payload.unreal_version()?.is_some();
        let inputs_digest = payload.inputs_digest()?;
        let sent = payload.sent_digest()?.is_some();
        let lfs = payload.lfs_digest()?.is_some();
        // Last, so a payload a run of one repo refuses is refused with the same line whatever its Pair says.
        let (main, siblings) = repos(payload, work, bare, snapshot)?;
        let unbuilt = [
            (desktop, Refusal::Desktop, "Desktop"),
            (warm, Refusal::Layout, "WarmCommands"),
            (engine_env, Refusal::Layout, "EngineEnv"),
            (unreal, Refusal::Layout, "UnrealVersion"),
            (sent, Refusal::Sent, "SentDigest"),
            (lfs, Refusal::Lfs, "LfsDigest"),
        ]
        .into_iter()
        .find_map(|(asked, refusal, key)| asked.then_some(Unbuilt { refusal, key }));
        Ok(Request {
            main,
            siblings,
            key,
            work,
            root,
            parent,
            nonce,
            timeout,
            host,
            lock,
            index,
            job,
            command: commands.words.into_iter().next().unwrap_or_default(),
            subfolder,
            identity,
            env,
            godot,
            inputs_digest,
            outputs,
            cross,
            unbuilt,
        })
    }

    /// The main repo's tree, which the command's subfolder is placed in.
    #[must_use]
    pub fn tree(&self) -> &str {
        &self.main.tree
    }

    /// Every repo of the run, the main one first.
    pub fn repos(&self) -> impl Iterator<Item = &Repo<'_>> {
        self.laying().repos()
    }

    /// What the work folder's layout reads of the run.
    pub(crate) fn laying(&self) -> Laying<'_> {
        Laying {
            work: self.work,
            parent: self.parent,
            key: self.key,
            main: &self.main,
            siblings: &self.siblings,
        }
    }
}

/// What the work folder's layout reads of a run, so a step that lays the folder out without a run's request lays it
/// out as a run does.
pub(crate) struct Laying<'a> {
    /// The work folder, `<root>/work/<key>`.
    pub(crate) work: &'a str,
    /// The folder holding the work folders, `<root>/work`.
    pub(crate) parent: &'a Path,
    /// The work folder's key.
    pub(crate) key: &'a str,
    /// The main repo, checked out in the work folder, or in `<work>/<leaf>` on a pair run.
    pub(crate) main: &'a Repo<'a>,
    /// Each sibling of a pair run, in the payload's order; none on a run of one repo.
    pub(crate) siblings: &'a [Repo<'a>],
}

impl<'a> Laying<'a> {
    /// Every repo laid out, the main one first.
    pub(crate) fn repos(&self) -> impl Iterator<Item = &'a Repo<'a>> + use<'a> {
        std::iter::once(self.main).chain(self.siblings)
    }

    /// The layout the work folder is laid out in, as its record says it ([`layout_text`]).
    fn layout(&self) -> String {
        let siblings = self.siblings.iter().filter_map(|repo| repo.name.as_deref()).collect();
        layout_text(self.main.name.as_deref(), siblings)
    }
}

/// A run ready for its command: the work lock held until this is dropped, the snapshot checked out, and the folder
/// the command runs in.
pub struct Ready<'p> {
    /// What the run read of its payload.
    pub request: Request<'p>,
    /// The session's builder slot, let go once the run is done ([`slots::Release`]); `None` when it holds none.
    /// Declared before [`Ready::lock`] and [`Ready::admitted`], so it is dropped first, an unwind's drop included: the
    /// slot goes while the work lock and the session are still held, so no call of the session waiting on the lock
    /// takes it while this run's claim is still to go.
    pub(crate) slot: Option<slots::Release>,
    /// The work lock, held.
    pub lock: HeldLock,
    /// The folder the command runs in: the work folder, or its subfolder.
    pub place: PathBuf,
    /// The heartbeat, beating since before the checkout; `None` when its thread could not start, which ends the
    /// heartbeats, never the run. Stopped at the run's end with [`heartbeat::stop`]; dropped, it is only told to stop.
    pub pump: Option<Pump>,
    /// How often the run's watches look.
    pub periods: Periods,
    /// The login shell's `PATH` the git steps ran along and the command runs along; `None` for the box's own.
    pub login_path: Option<String>,
    /// The login shell's `GATE_GODOT_ROOT`, the folder of the builder's Godot installs; `None` when it is unset or
    /// empty, or the login shell could not be read, so Godot is missing.
    pub godot_root: Option<String>,
    /// The run's own temp folder, the command's `TMPDIR`, removed at the run's end ([`temp_removed`]); `None` when the
    /// caller's `Env` sets `TMPDIR`.
    pub temp: Option<RunTemp>,
    /// The PC's outputs manifest and the listing taken before the command, for a run with output globs; `None`
    /// without.
    pub outputs: Option<RunOutputs>,
    /// The run's hold of its drain session, its end stamped once the run is done ([`drain::passed`]).
    #[expect(dead_code, reason = "a guard, held for its drop, which stamps the session's end")]
    pub(crate) admitted: drain::Admitted,
}

/// A run's outputs as they stand before its command, as the Windows box's `RunOutputs` holds them: the PC's manifest
/// or why it could not be read, said only once the command has passed, and the listing of the files under the globs
/// taken before the command, under `OutputsFull` too, `None` when it could not be taken.
pub struct RunOutputs {
    /// The PC's manifest, or why it could not be read.
    pub manifest: Result<Manifest, String>,
    /// The listing taken before the command; `None` until then and when it could not be taken.
    pub listing: Option<Listing>,
}

impl RunOutputs {
    /// Reads the PC's manifest from `input`, up to its first newline ([`outputs::read_manifest`]), then removes the
    /// output folder's old archives ([`outputs::prune`]), saying each line through `output`.
    fn read<S: LineSink>(asked: &Asked<'_>, input: &mut dyn BufRead, output: &Output<S>) -> RunOutputs {
        let manifest = outputs::read_manifest(input);
        for line in outputs::prune(asked, SystemTime::now()) {
            noted(output, GAT, &line);
        }
        RunOutputs { manifest, listing: None }
    }

    /// Takes the listing of the files under `asked`'s globs in `work` ([`outputs::listing`]), saying through `output`
    /// that it could not be taken.
    fn list<S: LineSink>(&mut self, asked: &Asked<'_>, work: &Path, output: &Output<S>) {
        let (listing, line) = outputs::listing(asked, work);
        if let Some(line) = line {
            noted(output, GAT, &line);
        }
        self.listing = listing;
    }
}

/// The template `mkdtemp(3)` names each run's temp folder from: short, so the sockets .NET makes under it fit.
const TEMP_TEMPLATE: &CStr = c"/tmp/gate.XXXXXXXX";
/// How long a run's `TMPDIR` may be, in bytes, so the pipes .NET names `CoreFxPipe_<name>` under it still fit a
/// socket path's 104.
const TEMP_LIMIT: usize = 37;
const _: () = assert!(
    TEMP_TEMPLATE.count_bytes() < TEMP_LIMIT,
    "the TMPDIR, with its trailing /, fits TEMP_LIMIT"
);

/// A run's own temp folder, made by `mkdtemp(3)` ([`temp_made`]) and removed at the run's end ([`temp_removed`]).
#[derive(Debug)]
pub struct RunTemp {
    /// The folder, as `mkdtemp` named it.
    path: PathBuf,
    /// The folder with a trailing `/`: the run's `TMPDIR`.
    tmpdir: String,
    /// The folder's device and inode as `mkdtemp` made it, so the removal leaves a folder of that name it did not make.
    identity: (u64, u64),
}

/// The template a test's thread makes its runs' temp folders from, in place of [`TEMP_TEMPLATE`].
#[cfg(test)]
pub(crate) mod test_template {
    use std::cell::RefCell;
    use std::ffi::{CStr, CString};

    thread_local! {
        static TEMPLATE: RefCell<Option<CString>> = const { RefCell::new(None) };
    }

    /// Makes every temp folder this thread's runs make from now on from `template`.
    pub fn set(template: &CStr) {
        TEMPLATE.with(|cell| *cell.borrow_mut() = Some(template.to_owned()));
    }

    /// The template this thread set, if any.
    pub fn get() -> Option<CString> {
        TEMPLATE.with(|cell| cell.borrow().clone())
    }
}

/// No test template outside the tests.
#[cfg(not(test))]
mod test_template {
    use std::ffi::CString;

    /// None.
    pub fn get() -> Option<CString> {
        None
    }
}

impl RunTemp {
    /// The folder, as `mkdtemp` named it.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The folder with a trailing `/`: the run's `TMPDIR`.
    #[must_use]
    pub fn tmpdir(&self) -> &str {
        &self.tmpdir
    }
}

/// A new temp folder for the run, made by `mkdtemp(3)` from [`TEMP_TEMPLATE`]: at mode 0700, owned by the builder
/// user, in the builder user's own group, and never at a name that is already there, so no other account can plant it
/// or a link in its place. The group is the builder user's own, else the kernel drops `S_ISGID` from a file made in the
/// folder and made setgid there.
fn temp_made() -> io::Result<RunTemp> {
    let template = test_template::get().unwrap_or_else(|| TEMP_TEMPLATE.to_owned());
    let mut template = template.into_bytes_with_nul();
    // SAFETY: `template` is a NUL-terminated buffer that lives across the call, which only writes its X's in place.
    let made = unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) };
    if made.is_null() {
        return Err(io::Error::last_os_error());
    }
    template.pop();
    // `mkdtemp` fills the X's from letters and digits only, so the name stays ASCII.
    let path = String::from_utf8_lossy(&template).into_owned();
    let tmpdir = format!("{path}/");
    let path = PathBuf::from(path);
    let made = std::os::unix::fs::lchown(&path, None, Some(files::effective_gid()))
        .and_then(|()| fs::symlink_metadata(&path))
        .inspect_err(|_| {
            // The run is refused with this call's or this look's error; the folder just made is empty, and
            // `remove_dir` takes no link.
            let _ = fs::remove_dir(&path);
        })?;
    Ok(RunTemp {
        path,
        tmpdir,
        identity: (made.dev(), made.ino()),
    })
}

/// The temp folder of the run `request`, saying its lines through `output`: `None` when the caller's `Env` sets
/// `TMPDIR`, which the command then keeps as given, else a new one ([`temp_made`]).
///
/// # Errors
///
/// [`REFUSED_STATUS`] when the folder cannot be made, refused `layout` with the line saying why, so the PC runs the
/// command itself.
fn temp_for<S: LineSink>(request: &Request<'_>, output: &Output<S>) -> Result<Option<RunTemp>, Unready> {
    if request.env.iter().any(|(name, _)| name == "TMPDIR") {
        return Ok(None);
    }
    temp_made().map(Some).map_err(|error| {
        let line = format!("gate: remote could not make the run's temp folder in /tmp: {error}");
        refused(output, request, Refusal::Layout, &line);
        Unready::Refused
    })
}

/// Removes the run's temp folder `temp` and everything in it, following no link at any depth: a link is removed as
/// itself ([`fs::remove_dir_all`]). A folder already gone counts as removed; one at its name that is not the folder
/// `mkdtemp` made (another device or inode) is left and says `gate: remote did not remove <path>: it is no longer the
/// run's temp folder`; one that cannot be removed says `gate: remote could not remove the run's temp folder <path>:
/// <why>`. Each through `output`, changing nothing else.
fn temp_removed<S: LineSink>(temp: RunTemp, output: &Output<S>) {
    let RunTemp { path, identity, .. } = temp;
    let removed = match fs::symlink_metadata(&path) {
        Ok(found) if (found.dev(), found.ino()) != identity => {
            let line = format!("gate: remote did not remove {}: it is no longer the run's temp folder", path.display());
            // Said; the run's status stands.
            let _ = output.say(Stream::Err, GAT, &line);
            return;
        }
        Ok(_) => fs::remove_dir_all(&path),
        Err(error) => Err(error),
    };
    match removed {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            let line = format!("gate: remote could not remove the run's temp folder {}: {error}", path.display());
            // Said; the run's status stands.
            let _ = output.say(Stream::Err, GAT, &line);
        }
    }
}

/// Where a run reads the login shell's `PATH` and `GATE_GODOT_ROOT` from: [`process::login_shell`], or a test's own.
pub type LoginShell = fn() -> Result<Login, String>;

/// Readies the run of `payload` for its command, saying its lines through `output`: the payload read ([`Request::read`]),
/// each failure one line and [`USAGE_STATUS`]; what this builder does not build refused with its tag and a line said
/// together, [`REFUSED_STATUS`]; a box running under Rosetta refused `install` the same way
/// ([`crate::lines::rosetta_lines`]), and one whose `sysctl.proc_translated` cannot be read said and taken as native; the
/// work root, `<root>/work` and `Bare` checked as the preflight checks them
/// ([`root_checked`]); the work lock taken, a held one refused `busy` with the busy line, one that cannot be opened
/// failing with [`FAILED_STATUS`]; an earlier run's group record whose group still runs refused `busy`, and one whose
/// group is gone removed ([`earlier_group_cleared`]); the lock files a git step cut short left removed ([`swept`]), else
/// refused `layout`;
/// the subfolder placed inside the work folder, else refused with [`USAGE_STATUS`] once the earlier run's back ref is
/// deleted; the work folder's files checked ([`layout_checked`]) and it laid out ([`laid_out`]), refused `layout` when
/// it cannot be; the snapshot fetched and checked out ([`checked_out`]); and the subfolder found in it, a folder inside
/// the work folder once its links are followed, else [`USAGE_STATUS`]; then the run's temp folder made ([`temp_for`]),
/// one that cannot be made refused `layout`; then the repo's dotnet tools restored when a
/// manifest changed ([`tools::update`]), then the packages of a pnpm, bun or npm command restored when its lockfile's
/// folder changed ([`packages::update`]), each with the folder as its `TMPDIR`, a restore that fails refused `tools`
/// once the folder is removed ([`temp_removed`]). Each git step's process group is recorded in
/// `<work>.pgid` under the run's job while it runs. The heartbeat starts once the subfolder is placed, before the layout
/// and the checkout, beating every `periods.heartbeat` ([`heartbeat::start`]). The login shell's `PATH` is read from
/// `login` just before the first git step, after every refusal said before it ([`login_read`]). No start marker
/// is said. The heartbeat starts with `sources`' `start`, and one that cannot start is said and the run goes on
/// without it; a run with output globs reads the PC's outputs manifest from its `input` ([`locked`]).
///
/// # Errors
///
/// The status the run ends on, its lines said.
fn prepared<'p, S: LineSink + 'static>(
    payload: &'p Payload,
    output: &Arc<Output<S>>,
    periods: Periods,
    sources: Sources<'_>,
) -> Result<Ready<'p>, u8> {
    let request = Request::read(payload).map_err(|line| said(output, &[&line], USAGE_STATUS))?;
    if let Some(unbuilt) = request.unbuilt {
        return Err(refused(output, &request, unbuilt.refusal, &unbuilt.line(&request.main.bare)));
    }
    match translation(crate::command::proc_translated()) {
        Translation::Native => {}
        Translation::Unread(line) => {
            // A note: the run goes on, and a client gone finds out at the next line.
            let _ = output.say(Stream::Err, GAT, &line);
        }
        Translation::Rosetta => {
            let [tag, line] = rosetta_lines(request.nonce);
            return Err(said_as(output, &[(Stamp::Bare, &tag), (GAT, &line)], REFUSED_STATUS));
        }
    }
    // A usage error says no back commits line; a root or a bare repository the box cannot use, as a lock it cannot
    // open, is a pair run's back commits never made, as the Windows box says it.
    root_checked(&request).map_err(|(line, status)| match status {
        FAILED_STATUS => back_commits_failed(output, &request, said(output, &[&line], status)),
        _ => said(output, &[&line], status),
    })?;
    // Held until the run ends, its command and back commit done, so a drain counts the run as going.
    let admitted = admitted(payload, &request, &sources.drain.seams(), output)?;
    // A session whose slot lapsed and was taken takes a free one, else the run is refused as its preflight would be.
    let claimed = slot_taken(payload, &request, &sources.drain.seams(), output)?;
    let lock = match files::take_lock(Path::new(request.lock)) {
        Ok(Locked::Held(lock)) => lock,
        Ok(Locked::Busy) => return Err(refused(output, &request, Refusal::Busy, &busy_line(request.key))),
        Err(error) => {
            return Err(match not_laid_out(output, &request, lock_untaken(&error, request.lock)) {
                Unready::Refused => REFUSED_STATUS,
                Unready::Ended(status) => back_commits_failed(output, &request, status),
            });
        }
    };
    // Made once the lock is held and only when the session holds a claim, so a second call of the session refused busy
    // leaves the live run's slot alone, and dropped at every later end of the run, before its admission and so while
    // its session is held: the session's builder slot is let go.
    let slot = claimed.then(|| slots::Release::at_end(Path::new(&request.root), payload));
    match locked(&request, &lock, output, periods.heartbeat, sources) {
        Ok(Placed {
            place,
            pump,
            login_path,
            godot_root,
            temp,
            outputs,
        }) => Ok(Ready {
            request,
            slot,
            lock,
            place,
            pump,
            periods,
            login_path,
            godot_root,
            temp,
            outputs,
            admitted,
        }),
        // A refusal sends the PC to run the command itself; any other end, whatever its status, is a pair run's back
        // commits never made.
        Err(Unready::Refused) => Err(REFUSED_STATUS),
        Err(Unready::Ended(status)) => Err(back_commits_failed(output, &request, status)),
    }
}

/// The login shell's `PATH` and `GATE_GODOT_ROOT` read from `login`, the `PATH`'s empty and relative folders left out;
/// `None`, with the box's own `PATH` kept, no Godot root and [`login_path_line`] said, when it cannot be read or its
/// `PATH` holds no absolute folder.
fn login_read<S: LineSink>(output: &Output<S>, login: LoginShell) -> Option<Login> {
    let read = login().and_then(|Login { path, godot_root }| {
        let folders: Vec<&str> = path.split(':').filter(|folder| folder.starts_with('/')).collect();
        if folders.is_empty() {
            Err("said a PATH with no absolute folder".to_owned())
        } else {
            Ok(Login {
                path: folders.join(":"),
                godot_root,
            })
        }
    });
    read.inspect_err(|why| {
        // A note: the run goes on, and a client gone finds out at the next line.
        let _ = output.say(Stream::Err, GAT, &login_path_line(why));
    })
    .ok()
}

/// The run's git for `request`, holding the work lock `lock`, found along `login_path`, its lines said through
/// `output` and each step's group recorded in `<work>.pgid` under the run's job: the checkout's and the back commit's
/// alike, so both run with the same pins.
pub(crate) fn run_git<'a, S: LineSink + 'static>(
    request: &'a Request<'_>,
    lock: &'a HeldLock,
    login_path: Option<&'a str>,
    output: &'a Arc<Output<S>>,
) -> RunGit<'a, S> {
    let record = GroupRecord::new(PathBuf::from(group_record(request.work)), request.job, lock);
    RunGit::new(
        Git::of(&request.root, request.parent).along(login_path),
        request.lock,
        output,
        Some(record),
        GAT,
    )
}

/// How a run's watches look and where its ceiling reads the machine's load from: the defaults, or the test seams'.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Watches {
    /// How often the heartbeat looks and the ceiling samples.
    pub periods: Periods,
    /// Where the ceiling reads the machine's load from.
    pub sampler: Sampler,
    /// The bytes the output folder's archives are trimmed to after a pack ([`outputs::trim`]).
    pub budget: u64,
    /// The drain's clock the run is admitted by.
    pub drain: DrainSeams,
}

/// What the drain's clock reads of the test seams: `--test-box-now`, `--test-box-drain-lease-ms` and
/// `--test-box-drain-lapse-ms`, each `None` for its default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrainSeams {
    /// The Unix second the drain judges at.
    pub now: Option<u64>,
    /// The lease of a ticket and a session, in milliseconds.
    pub lease_ms: Option<u64>,
    /// The lapse of a barrier, in milliseconds.
    pub lapse_ms: Option<u64>,
}

impl DrainSeams {
    /// The drain's seams of `seams`.
    #[must_use]
    pub fn of(seams: &BoxSeams) -> DrainSeams {
        DrainSeams {
            now: seams.now,
            lease_ms: seams.drain_lease_ms,
            lapse_ms: seams.drain_lapse_ms,
        }
    }

    /// The seams the drain reads, every other one its default.
    fn seams(self) -> BoxSeams {
        BoxSeams {
            now: self.now,
            drain_lease_ms: self.lease_ms,
            drain_lapse_ms: self.lapse_ms,
            ..BoxSeams::default()
        }
    }
}

impl Watches {
    /// The watches `seams` ask for: `--test-heartbeat-ms` and `--test-box-sample-ms` over [`Periods::default`];
    /// `--test-box-sampler-fail`, else `--test-box-free-percent`, over [`Sampler::Machine`], and
    /// `--test-outputs-budget-bytes` over [`outputs::OUTPUTS_BUDGET`] ([`outputs::budget`]), and the drain's clock
    /// ([`DrainSeams::of`]), as the Windows box reads them. All defaults in a box built without the `test-seams` feature.
    #[must_use]
    pub fn of(seams: &BoxSeams) -> Watches {
        let defaults = Periods::default();
        let periods = Periods {
            heartbeat: seams.heartbeat_ms.map_or(defaults.heartbeat, Duration::from_millis),
            sample: seams.sample_ms.map_or(defaults.sample, Duration::from_millis),
        };
        let sampler = match (seams.sampler_fail, seams.free_percent) {
            (true, _) => Sampler::Failing,
            (false, Some(percent)) => Sampler::FreePercent(percent),
            (false, None) => Sampler::Machine,
        };
        Watches {
            periods,
            sampler,
            budget: outputs::budget(seams),
            drain: DrainSeams::of(seams),
        }
    }
}

/// Runs `payload`'s run, saying its lines through `output`, watched as `watches` says; the status the box exits with.
///
/// The login shell's `PATH` is read once, by [`prepared`] ([`process::login_shell`]); the git steps and the command run
/// along it ([`run_along`]), and a Godot run finds its install under the login shell's `GATE_GODOT_ROOT`.
pub fn run<S: LineSink + 'static>(payload: &Payload, output: &Arc<Output<S>>, watches: Watches) -> u8 {
    run_along(payload, output, watches, process::login_shell)
}

/// [`run`], the login shell's `PATH` read from `login` and the PC's outputs manifest from stdin ([`run_reading`]).
fn run_along<S: LineSink + 'static>(payload: &Payload, output: &Arc<Output<S>>, watches: Watches, login: LoginShell) -> u8 {
    run_reading(payload, output, watches, login, &mut io::stdin().lock())
}

/// [`run`], the login shell's `PATH` read from `login` and the PC's outputs manifest from `input`, in the Windows box's
/// order: the run readied ([`prepared`]), the heartbeat beating from the subfolder's check; the command run
/// ([`run_command`]); the back commit made and pushed when the command reached an end of its own, then, after a
/// passing command, its outputs brought back ([`synced_back`]); then the run's temp folder removed
/// ([`temp_removed`]), the heartbeat stopped ([`heartbeat::stop`]), and the work lock let go.
fn run_reading<S: LineSink + 'static>(
    payload: &Payload,
    output: &Arc<Output<S>>,
    watches: Watches,
    login: LoginShell,
    input: &mut dyn BufRead,
) -> u8 {
    let sources = Sources {
        login,
        start: heartbeat::start,
        input,
        drain: watches.drain,
    };
    let ready = match prepared(payload, output, watches.periods, sources) {
        Ok(ready) => ready,
        Err(status) => return status,
    };
    let status = commanded(&ready, output, watches);
    let Ready { pump, lock, temp, slot, .. } = ready;
    if let Some(temp) = temp {
        temp_removed(temp, output);
    }
    if let Some(pump) = pump {
        heartbeat::stop(pump);
    }
    // The slot goes while the work lock is still held, as the Windows box lets it go: a second call of the session
    // that takes the lock next keeps a claim no end of this run removes.
    drop(slot);
    drop(lock);
    status
}

/// The status of the run `ready` once its command has run, sampled from `watches`' sampler: the command's own
/// ([`command_status`]), the back commit made only after an end of its own ([`of_its_own`]) and the outputs trimmed to
/// `watches`' budget. A run with a chosen Godot version runs with the install of it under the login shell's
/// `GATE_GODOT_ROOT` ([`probe::installed_godot`]), or with Godot missing when there is no root or no such install.
fn commanded<S: LineSink + 'static>(ready: &Ready<'_>, output: &Arc<Output<S>>, watches: Watches) -> u8 {
    let request = &ready.request;
    let login_path = ready.login_path.as_deref();
    let installed;
    let godot = match request.godot {
        None => Godot::Unchosen,
        Some(version) => {
            installed = ready
                .godot_root
                .as_deref()
                .and_then(|root| probe::installed_godot(root, version))
                .map(|program| program.to_string_lossy().into_owned());
            installed.as_deref().map_or(Godot::Missing, Godot::Installed)
        }
    };
    let launch = Launch {
        words: &request.command,
        place: &ready.place,
        work: request.work,
        job: request.job,
        nonce: request.nonce,
        env: &request.env,
        login_path,
        tmp: ready.temp.as_ref().map(RunTemp::tmpdir),
        godot,
        lock: &ready.lock,
        timeout: request.timeout,
        host: request.host,
        sample: ready.periods.sample,
        sampler: watches.sampler,
        pump: ready.pump.as_ref(),
    };
    let end = run_command(&launch, output);
    let status = command_status(end);
    if !of_its_own(end) {
        return back_commits_failed(output, request, status);
    }
    synced_back(ready, output, status, watches.budget, &launch)
}

/// Whether the command that ended as `end` reached an end of its own, an exit or a signal the box did not send, which
/// alone is followed by a back commit: one the box killed for its ceiling or backstop, whose client is gone, that never
/// started or that the box could not see to its end makes none, as the PC applies nothing of it. A `box stop`'s
/// `SIGTERM` reads as any other signal.
fn of_its_own(end: CommandEnd) -> bool {
    matches!(end, CommandEnd::Exited(_) | CommandEnd::Signalled(_))
}

/// The status of the run `ready`, whose command ended with `status`, once its back commits were made and pushed
/// ([`back::back_commit`]): `status`, a pair run saying each repo's commit on one line ([`back_commits_line`]); a back
/// commit that failed says, on a pair run, [`BACK_COMMITS_FAILED`], then `gate: remote sync-back failed: <why>`, and
/// gives [`FAILED_STATUS`] for a command that exited 0, else `status`; a client gone gives the closed status. Once the
/// back commits are pushed, a command that exited 0 with output globs has its cross build run, when the payload asks
/// for one, as `launch` started the command ([`cross_built`]), then its outputs brought back ([`brought_back`]), which
/// gives its status; a failed command has only its test reports and failure outputs brought back
/// ([`Asked::failed_run`] under [`Caps::BUILT_IN`]), with no cross
/// build, and ends on its own status whatever the pack gives, and one with no glob naming a `TestResults` folder, a
/// failed sync-back or a cross build that did not pass packs nothing.
fn synced_back<S: LineSink + 'static>(ready: &Ready<'_>, output: &Arc<Output<S>>, status: u8, budget: u64, launch: &Launch<'_>) -> u8 {
    let request = &ready.request;
    let commits = match back::back_commit(request, &ready.lock, ready.login_path.as_deref(), output) {
        Ok(commits) => commits,
        Err(BackFailed::Gone(failed)) => return failure(output, &failed),
        Err(BackFailed::Step(why)) => {
            let failed = if status == 0 { FAILED_STATUS } else { status };
            let line = format!("gate: remote sync-back failed: {why}");
            return if request.siblings.is_empty() {
                said(output, &[&line], failed)
            } else {
                said(output, &[BACK_COMMITS_FAILED, &line], failed)
            };
        }
    };
    if !request.siblings.is_empty() {
        let named: Vec<(&str, Option<&str>)> = request
            .repos()
            .zip(&commits)
            .map(|(repo, commit)| (repo.name.as_deref().unwrap_or_default(), commit.as_deref()))
            .collect();
        said(output, &[&back_commits_line(&named)], status);
    }
    let main = commits.first().and_then(Option::as_deref);
    match (&request.outputs, &ready.outputs) {
        (Some(asked), Some(read)) if status == 0 => {
            if let Some(ended) = request.cross.as_ref().and_then(|cross| cross_built(launch, cross, output)) {
                return ended;
            }
            brought_back(ready, (asked, read), main, output, budget)
        }
        (Some(asked), Some(read)) => {
            if let Some(reports) = asked.failed_run(Caps::BUILT_IN) {
                // A failed command ends on its own status, so the pack's own is only said.
                let _ = brought_back(ready, (&reports, read), main, output, budget);
            }
            status
        }
        _ => status,
    }
}

/// Runs the cross build `build` as `launch` started the command ([`command::run_cross`]): `None` once it passed, the
/// line saying how long it took said; else the status the run ends on, its outputs not brought back. A cross build that
/// exited by a non-zero code says it failed and [`outputs::not_brought_back`]'s line, and leaves the passing command's
/// 0, a build that trapped a `box stop`'s signal and exited by code among them; one a signal ended, that could not
/// start or that the box could not see to its end says the same lines and gives [`FAILED_STATUS`]; one the box killed gives
/// [`KILLED_STATUS`](crate::lines::KILLED_STATUS) after its kill line and tag, as the box's kill of the command does;
/// a client gone before or during it gives the closed status.
fn cross_built<S: LineSink + 'static>(launch: &Launch<'_>, build: &CrossBuild, output: &Arc<Output<S>>) -> Option<u8> {
    if output.client_gone() {
        return Some(CLOSED_STATUS);
    }
    noted(output, GAT, &cross_start_line(&build.rid, &build.words));
    let (cross_status, status) = match command::run_cross(launch, &build.words, output) {
        CrossEnd::Passed(took) => {
            noted(output, GAT, &cross::passed_line(&build.rid, took.as_secs_f64()));
            return None;
        }
        CrossEnd::Failed(cross_status) => (cross_status, 0),
        CrossEnd::Ended(cross_status) => (cross_status, FAILED_STATUS),
        CrossEnd::Killed => return Some(crate::lines::KILLED_STATUS),
        CrossEnd::Gone => return Some(CLOSED_STATUS),
    };
    let not_brought = outputs::not_brought_back(cross::FAILED_REASON);
    Some(said(output, &[&cross::failed_line(&build.rid, cross_status), &not_brought], status))
}

/// The line the box says before it starts a cross build for `rid` with `words`, each word as it is run: the PC's
/// failure scan counts no `out` or `err` line after it ([`gate_wire::is_cross_step_start`]).
fn cross_start_line(rid: &str, words: &[String]) -> String {
    format!("{CROSS_STEP_START}{rid}: {}", words.join(" "))
}

/// The status of the run `ready` once its outputs `asked`, a passing command's or a failed one's test reports, read
/// before the command as `read`, are brought back from the main repo's tree alone, as the Windows box's `RunOutputs`
/// brings them: the paths the main repo's back commit `main` changes read first ([`back::back_paths`]), none without
/// one, and left out; then the pack ([`outputs::bring_back`]), its skipped line, when there is one, and its
/// `gate-remote-outputs:` line said; then, when it wrote an archive, the output folder trimmed to `budget` bytes
/// ([`outputs::trim`]), each line said, the archive held under its pack's lock until the trim is done. 0, or
/// [`FAILED_STATUS`] with [`outputs::not_brought_back`]'s line when the outputs are not brought back, but 0 when a cross
/// build's set is refused for a program the PC cannot run, which leaves the passing command's status; a client gone
/// while the paths are read, or found gone before the pack or before the trim, gives the closed status, nothing more
/// packed or trimmed.
fn brought_back<S: LineSink + 'static>(
    ready: &Ready<'_>,
    (asked, read): (&Asked<'_>, &RunOutputs),
    main: Option<&str>,
    output: &Arc<Output<S>>,
    budget: u64,
) -> u8 {
    let request = &ready.request;
    // Read after the push and only for outputs, so failing to read them fails the outputs, never the sync-back.
    let skip = match main.map(|commit| back::back_paths(request, &ready.lock, ready.login_path.as_deref(), output, commit)) {
        None => Ok(Vec::new()),
        Some(Ok(paths)) => Ok(paths),
        Some(Err(BackFailed::Gone(failed))) => return failure(output, &failed),
        Some(Err(BackFailed::Step(why))) => Err(why),
    };
    let work = Path::new(request.tree());
    if output.client_gone() {
        return CLOSED_STATUS;
    }
    let brought = match skip.and_then(|skip| outputs::bring_back(asked, &read.manifest, read.listing.as_ref(), work, &skip)) {
        Ok(brought) => brought,
        Err(reason) => return said(output, &[&outputs::not_brought_back(&reason)], FAILED_STATUS),
    };
    // A cross build that built a program the PC cannot run brings nothing back, and leaves the passing command's
    // status as it was.
    if let Some(refused) = &brought.refused {
        return said(output, &[refused, &outputs::not_brought_back(cross::REFUSED_REASON)], 0);
    }
    if let Some(skipped) = &brought.skipped {
        noted(output, GAT, skipped);
    }
    for left in &brought.left {
        noted(output, GAT, left);
    }
    if let Some(size) = &brought.size {
        noted(output, Stamp::Bare, size);
    }
    noted(output, Stamp::Bare, &brought.line);
    if !brought.wrote_archive() {
        return 0;
    }
    // A PC gone takes no archive, so none is trimmed for it: the closed status, as the run's other steps end on.
    if output.client_gone() {
        return CLOSED_STATUS;
    }
    let trimmed = outputs::trim(asked, budget);
    // Held to here, so no other run's trim removes the archive before this one's is done.
    drop(brought);
    for line in &trimmed.lines {
        noted(output, GAT, line);
    }
    match &trimmed.reason {
        Some(reason) => said(output, &[&outputs::not_brought_back(reason)], FAILED_STATUS),
        None => 0,
    }
}

/// Says `line` on stderr as `stamp` says, unless the client is gone: a line the run goes on past.
fn noted<S: LineSink>(output: &Output<S>, stamp: Stamp, line: &str) {
    said_as(output, &[(stamp, line)], 0);
}

/// How a run starts its heartbeat: [`heartbeat::start`], or a test's own.
type StartBeat = fn(Arc<dyn Forwarding>, Duration) -> io::Result<Pump>;

/// Where a run reads what its payload does not hold: the login shell's `PATH`, how its heartbeat starts, the input
/// the PC's outputs manifest follows the request line on, and the drain's clock the run is admitted by.
struct Sources<'i> {
    login: LoginShell,
    start: StartBeat,
    input: &'i mut dyn BufRead,
    drain: DrainSeams,
}

/// [`prepared`] with no input past the request line and the heartbeat started with [`heartbeat::start`]: a test's
/// way to ready a run that asks for no outputs.
#[cfg(test)]
fn prepare<'p, S: LineSink + 'static>(payload: &'p Payload, output: &Arc<Output<S>>, periods: Periods, login: LoginShell) -> Result<Ready<'p>, u8> {
    let sources = Sources {
        login,
        start: heartbeat::start,
        input: &mut io::empty(),
        drain: DrainSeams::default(),
    };
    prepared(payload, output, periods, sources)
}

/// How a run ends before its command, its lines said.
pub(crate) enum Unready {
    /// Refused, with [`REFUSED_STATUS`]: the PC runs the command itself.
    Refused,
    /// Ended with this status, a git step's own 75 among them.
    Ended(u8),
}

/// [`prepared`] once the work lock `lock` of `request` is held: the place the command runs in, the heartbeat's pump,
/// beating every `heartbeat` once started with `sources`' `start`, the login shell's `PATH` and `GATE_GODOT_ROOT` read
/// from its `login`, and the run's temp
/// folder ([`temp_for`]), made after the checks and before the tool restore and removed when the restore ends the run;
/// else how the run ends, refused or with a status, its lines said. A run with output globs reads the PC's manifest
/// from `sources`' `input` once the heartbeat has started and before the layout and the checkout, without waiting past
/// its newline, and prunes the old archives ([`RunOutputs::read`]); after the tool restore it lists the files under
/// the globs, under `OutputsFull` too ([`RunOutputs::list`]). A run without globs reads nothing past its request line.
fn locked<S: LineSink + 'static>(
    request: &Request<'_>,
    lock: &HeldLock,
    output: &Arc<Output<S>>,
    heartbeat: Duration,
    Sources { login, start, input, .. }: Sources<'_>,
) -> Result<Placed, Unready> {
    let laying = request.laying();
    // Before the sweep, so a git an earlier run left running keeps its lock files.
    earlier_group_cleared(&laying).map_err(|not| not_laid_out(output, request, not))?;
    // Before the run's first git step, so no lock file an earlier run's ended git left fails it.
    swept(&laying).map_err(|not| not_laid_out(output, request, not))?;
    // Refused before the layout, so a payload naming a folder outside the work folder changes nothing there; the
    // earlier run's back ref goes, as the checkout's would, so the client finds none to fetch.
    let Some(place) = place_of(request.tree(), request.subfolder) else {
        said(output, &[&not_inside(request.subfolder)], USAGE_STATUS);
        let login_path = login_read(output, login).map(|login| login.path);
        let git = run_git(request, lock, login_path.as_deref(), output);
        back_refs_cleared(&git, &laying).map_err(|failed| Unready::Ended(failure(output, &failed)))?;
        return Err(Unready::Ended(USAGE_STATUS));
    };
    // A heartbeat that cannot start ends the heartbeats, never the run. On an error below the pump is dropped, which
    // tells it to stop, and the box exits right after.
    let pump = start(Arc::clone(output) as Arc<dyn Forwarding>, heartbeat)
        .inspect_err(|error| {
            // A note: the run goes on, and a client gone finds out at the next line.
            let _ = output.say(
                Stream::Err,
                GAT,
                &format!("gate: remote heartbeat stopped: cannot start the heartbeat: {error}"),
            );
        })
        .ok();
    // Read while the heartbeat beats and before the layout and the checkout, so the client can take its time hashing
    // the PC's files.
    let mut outputs = request.outputs.as_ref().map(|asked| RunOutputs::read(asked, input, output));
    let fresh = layout_checked(&laying).map_err(|not| not_laid_out(output, request, not))?;
    let (login_path, godot_root) = login_read(output, login).map_or((None, None), |login| (Some(login.path), login.godot_root));
    let git = run_git(request, lock, login_path.as_deref(), output);
    laid_out(&laying, &git, &fresh).map_err(|not| not_laid_out(output, request, not))?;
    inputs_matched(request, lock, output)?;
    // A note for the client's checkout lines: the run goes on, and a client gone finds out at the next line.
    let _ = output.say(Stream::Err, Stamp::Bare, &checkout_start_line(request.nonce));
    let checking_out = Instant::now();
    checked_out(request, &git).map_err(|failed| Unready::Ended(failure(output, &failed)))?;
    // A note as the start line is: the run goes on, and a client gone finds out at the next line.
    let _ = output.say(
        Stream::Err,
        Stamp::Bare,
        &checkout_done_line(request.nonce, checking_out.elapsed().as_secs_f64()),
    );
    if !place.is_dir() {
        let line = format!("gate: remote subfolder {} does not exist in the snapshot", escaped(request.subfolder));
        return Err(Unready::Ended(said(output, &[&line], USAGE_STATUS)));
    }
    // A link the snapshot holds may lead the place out of the work folder, which only the checkout shows.
    if !inside_work(request.tree(), &place) {
        return Err(Unready::Ended(said(output, &[&not_inside(request.subfolder)], USAGE_STATUS)));
    }
    let temp = temp_for(request, output)?;
    let along = Along {
        path: login_path.as_deref(),
        tmp: temp.as_ref().map(RunTemp::tmpdir),
        godot: Godot::Unchosen,
    };
    let restored = tools::update(request, &place, lock, along, output).and_then(|()| packages::update(request, &place, lock, along, &git, output));
    if let Err(unready) = restored {
        if let Some(temp) = temp {
            temp_removed(temp, output);
        }
        return Err(unready);
    }
    if let (Some(asked), Some(outputs)) = (&request.outputs, outputs.as_mut()) {
        outputs.list(asked, Path::new(request.tree()), output);
    }
    Ok(Placed {
        place,
        pump,
        login_path,
        godot_root,
        temp,
        outputs,
    })
}

/// What [`locked`] readies for the command: its place, the heartbeat's pump, the login shell's `PATH` and
/// `GATE_GODOT_ROOT`, the run's temp folder, and its outputs as they stand before the command.
struct Placed {
    place: PathBuf,
    pump: Option<Pump>,
    login_path: Option<String>,
    godot_root: Option<String>,
    temp: Option<RunTemp>,
    outputs: Option<RunOutputs>,
}

/// On a pair run of `request`, says [`BACK_COMMITS_FAILED`] after the lines of a run that ends with `status` without
/// its back commits pushed, as the Windows box's `back_commits_failed` does, so the PC applies nothing and a back
/// commits line the command printed itself is no longer the last; `status`. A run of one repo says nothing more.
fn back_commits_failed<S: LineSink>(output: &Output<S>, request: &Request<'_>, status: u8) -> u8 {
    if request.siblings.is_empty() {
        return status;
    }
    said(output, &[BACK_COMMITS_FAILED], status)
}

/// Says `lines` on stderr together, stamped `gat`, unless the client is gone; `status`.
fn said<S: LineSink>(output: &Output<S>, lines: &[&str], status: u8) -> u8 {
    let lines: Vec<(Stamp, &str)> = lines.iter().map(|line| (GAT, *line)).collect();
    said_as(output, &lines, status)
}

/// Says `lines` on stderr together, each as its stamp says, unless the client is gone; `status`.
fn said_as<S: LineSink>(output: &Output<S>, lines: &[(Stamp, &str)], status: u8) -> u8 {
    // A client that has gone has no one left to tell, and the status still ends the ssh session; one still connected
    // but no longer reading would hold the write, and the output lock, for good.
    if output.client_gone() {
        return status;
    }
    let lines: Vec<(Stream, Stamp, &str)> = lines.iter().map(|(stamp, line)| (Stream::Err, *stamp, *line)).collect();
    let _ = output.say_together(&lines);
    status
}

/// Says the lines of `failed`; its status.
fn failure<S: LineSink>(output: &Output<S>, failed: &Failed) -> u8 {
    let lines: Vec<&str> = failed.lines.iter().map(String::as_str).collect();
    said(output, &lines, failed.status)
}

/// Checks the run's `InputsDigest`, when it has one, against the work folder's inputs record and the copies it holds,
/// and rewrites the tree's exclude file from it ([`inputs::unmatched`]), once the work folder is laid out and before
/// the checkout: a record missing, unreadable or of another digest, a copy changed or gone, or an exclude file that
/// cannot be written refuses the run `inputs`, its tag and line said together, so the PC runs the command itself. The
/// exclude file is written only while the work lock `lock` is shown still held; else the run fails with status 1, its
/// line said, as its other record writes do. A folder moved aside from the exclude file's name is said and the run
/// goes on.
fn inputs_matched<S: LineSink>(request: &Request<'_>, lock: &HeldLock, output: &Output<S>) -> Result<(), Unready> {
    let Some(sent) = request.inputs_digest else {
        return Ok(());
    };
    match inputs::unmatched(request.work, sent, lock) {
        Ok(moved_aside) => {
            if let Some(line) = moved_aside {
                // A note: the run goes on, and a client gone finds out at the next line.
                let _ = output.say(Stream::Err, GAT, &line);
            }
            Ok(())
        }
        Err(Unmatched::Inputs(why)) => {
            let line = format!(
                "gate: remote inputs of work folder {} are not the ones sent: {}",
                request.key,
                escaped(&why)
            );
            refused(output, request, Refusal::Inputs, &line);
            Err(Unready::Refused)
        }
        Err(Unmatched::LockLost(why)) => {
            let line = format!("gate: remote could not write {}/.git/info/exclude: {why}", request.work);
            Err(Unready::Ended(said(output, &[&line], FAILED_STATUS)))
        }
    }
}

/// The run of `request`, from `payload`, passed through the drain ([`drain::passed`]) as the session `payload` names,
/// with its command's words joined by spaces and its job, the drain's clock as `seams` set it: the guard, held until the
/// run ends. A line the drain said, a drain moved aside, is said stamped `gat`. Else the status the run ends on, its
/// lines said: a refusal's `drain` tag, place and barrier lines, bare, and [`REFUSED_STATUS`]; a drain that could not
/// be read or written fails the run as a lock that could not be taken does, its back commits never made.
fn admitted<S: LineSink>(payload: &Payload, request: &Request<'_>, seams: &BoxSeams, output: &Output<S>) -> Result<drain::Admitted, u8> {
    let command = request.command.join(" ");
    let mut console = Console::captured();
    let passed = drain::passed(&request.root, payload, Some((&command, request.job)), seams, &mut console);
    for line in &console.err {
        // A note: the run goes on, and a client gone finds out at the next line.
        let _ = output.say(Stream::Err, GAT, line);
    }
    passed.map_err(|unpassed| match unpassed {
        Unpassed::Usage(line) => said(output, &[&line], USAGE_STATUS),
        Unpassed::Failed(why) => back_commits_failed(output, request, said(output, &[&drain::failed_line(&why)], FAILED_STATUS)),
        Unpassed::Refused(refused) => {
            let tag = refused_line(request.nonce, Refusal::Drain);
            let [place, barrier] = refused_lines(refused);
            said_as(
                output,
                &[(Stamp::Bare, &tag), (Stamp::Bare, &place), (Stamp::Bare, &barrier)],
                REFUSED_STATUS,
            )
        }
    })
}

/// Holds the builder slot of the run of `request`, from `payload`, admitted as the session `payload` names
/// ([`slots::run_taken`]), the drain's clock as `seams` set it: whether the session holds a claim. A line it said, a
/// count file that cannot be read or a drain moved aside, is said stamped `gat`. Else the status the run ends on, its
/// lines said: with every slot held the `full` tag and the full line, bare, and [`REFUSED_STATUS`]; a slot pool that
/// could not be read or written fails the run as a drain that could not be, its back commits never made.
fn slot_taken<S: LineSink>(payload: &Payload, request: &Request<'_>, seams: &BoxSeams, output: &Output<S>) -> Result<bool, u8> {
    let command = request.command.join(" ");
    let mut console = Console::captured();
    let taken = slots::run_taken(&request.root, payload, (&command, request.job), seams, &mut console);
    for line in &console.err {
        // A note: the run goes on, and a client gone finds out at the next line.
        let _ = output.say(Stream::Err, GAT, line);
    }
    taken.map_err(|untaken| match untaken {
        Untaken::Full { held, count } => {
            let [tag, full] = slots::full_lines(request.nonce, held, count);
            said_as(output, &[(Stamp::Bare, &tag), (Stamp::Bare, &full)], REFUSED_STATUS)
        }
        Untaken::Failed(why) => back_commits_failed(output, request, said(output, &[&slots::failed_line(&why)], FAILED_STATUS)),
    })
}

/// Refuses the run of `request` for `refusal`: its tag, then `line`, said together; [`REFUSED_STATUS`].
fn refused<S: LineSink>(output: &Output<S>, request: &Request<'_>, refusal: Refusal, line: &str) -> u8 {
    said_as(
        output,
        &[(Stamp::Bare, &refused_line(request.nonce, refusal)), (GAT, line)],
        REFUSED_STATUS,
    )
}

/// Ends the run of `request` whose work folder could not be laid out as `not` says: refused `layout` with the layout
/// line naming the repo and why, or failed with a step's lines; the status.
fn not_laid_out<S: LineSink>(output: &Output<S>, request: &Request<'_>, not: NotLaidOut) -> Unready {
    match not {
        NotLaidOut::Busy => {
            refused(output, request, Refusal::Busy, &busy_line(request.key));
            Unready::Refused
        }
        NotLaidOut::Refused(why) => {
            let line = layout_line(repo_name(&request.main.bare), &escaped(&why));
            refused(output, request, Refusal::Layout, &line);
            Unready::Refused
        }
        NotLaidOut::Failed(failed) => Unready::Ended(failure(output, &failed)),
    }
}

/// Why the work lock `lock` could not be taken, for `error`: a lock file another user owns ([`files::lock_not_owned`])
/// refused `layout`, so the PC runs the command itself; any other failure the line saying it, [`FAILED_STATUS`].
pub(crate) fn lock_untaken(error: &io::Error, lock: &str) -> NotLaidOut {
    let not_owned = files::lock_not_owned(Path::new(lock));
    if error.kind() == io::ErrorKind::PermissionDenied && error.to_string() == not_owned {
        NotLaidOut::Refused(not_owned)
    } else {
        NotLaidOut::Failed(Failed::line(format!("gate: remote could not open the work folder lock {lock}: {error}")))
    }
}

/// `gate: remote subfolder <sub> is not a folder inside the work folder`, as the Windows box says it.
fn not_inside(sub: &str) -> String {
    format!("gate: remote subfolder {} is not a folder inside the work folder", escaped(sub))
}

/// Checks, making nothing and before the lock, what the run's lock and git steps lie in, as the preflight checks it:
/// `Bare` at `<root>/<name>.git` unless the run is a pair's, whose bare repositories `gate-wire` checked as it read the
/// pair ([`checked_root`]); the work root trusted and `<root>/work` the box user's own folder when they are there
/// ([`records_folder`]); and every repo's bare repository the box user's own folder when it is there
/// ([`files::unusable_folder`]). Else the line and the status.
fn root_checked(request: &Request<'_>) -> Result<(), (String, u8)> {
    checked_root(request.work, &request.main.bare, !request.siblings.is_empty()).map_err(|line| (line, USAGE_STATUS))?;
    records_folder(&request.root, request.parent)?;
    request
        .repos()
        .find_map(|repo| files::unusable_folder(Path::new(&repo.bare), false))
        .map_or(Ok(()), |flaw| Err(unusable_refusal(flaw, false)))
}

/// The name routing gives the repo of the bare repository `bare`: the stem of `<name>.git` ([`bare_stem`]), or the
/// bare repository's whole file name when it is not of that form.
pub(crate) fn repo_name(bare: &str) -> &str {
    let (folder, file) = bare.rsplit_once('/').unwrap_or(("", bare));
    bare_stem(bare, folder, PathForm::Unix).unwrap_or(file)
}

/// The folder the command runs in: `work` itself for an empty `sub`, else `sub` below it, its `.` and `..` folded
/// without reading the file system; `None` when `sub` is rooted or climbs out of `work`.
fn place_of(work: &str, sub: &str) -> Option<PathBuf> {
    if sub.is_empty() {
        return Some(PathBuf::from(work));
    }
    if sub.starts_with('/') {
        return None;
    }
    let joined = Path::new(work).join(sub);
    let folded = folded_names(&joined);
    if !folded.starts_with(&folded_names(Path::new(work))) {
        return None;
    }
    Some(Path::new("/").join(folded.iter().collect::<PathBuf>()))
}

/// The names of `path`, each `..` dropping the name before it, `.` and the root left out.
fn folded_names(path: &Path) -> Vec<&OsStr> {
    let mut folded = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => folded.push(name),
            Component::ParentDir => {
                folded.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    folded
}

/// Whether `place`, every link on its way followed, lies in the work folder `work`, its links followed too; a path
/// that cannot be followed lies nowhere.
fn inside_work(work: &str, place: &Path) -> bool {
    match (fs::canonicalize(work), fs::canonicalize(place)) {
        (Ok(work), Ok(place)) => place.starts_with(work),
        _ => false,
    }
}

/// Why the work folder could not be laid out.
pub(crate) enum NotLaidOut {
    /// An earlier run's recorded process group still runs in it: the run is refused `busy`.
    Busy,
    /// A file-system step failed, or a path is not what the run uses, for this reason: the run is refused `layout`.
    Refused(String),
    /// A git step failed, or the work folder is not one the box uses: the lines and the status.
    Failed(Failed),
}

impl From<Failed> for NotLaidOut {
    fn from(failed: Failed) -> NotLaidOut {
        NotLaidOut::Failed(failed)
    }
}

/// The group record of the work folder `work`, `<work>.pgid`, which `box stop` reads.
pub(crate) fn group_record(work: &str) -> String {
    format!("{work}.pgid")
}

/// Looks at each group record an earlier run of `laying`'s work folder left, its git step's `<work>.pgid`, its tool
/// restore's `<work>.tools.pgid` and its package restore's `<work>.packages.pgid` ([`earlier_record_cleared`]), a box
/// that ended without removing it:
/// whatever job it names, a group that is still the one its run made ([`record_current`]) refuses the run
/// [`NotLaidOut::Busy`], the group never signalled and its git's lock files never swept; a record naming no group a
/// run makes (malformed, init, or the box's own), or a group that is no longer its run's (no process left, one the box
/// may not signal, or its id another's since), is removed under the work lock and the run goes on. A record that
/// cannot be read, a link or a folder at its name, refuses the run `layout`.
pub(crate) fn earlier_group_cleared(laying: &Laying<'_>) -> Result<(), NotLaidOut> {
    let records = [
        group_record(laying.work),
        tools::tools_group_record(laying.work),
        packages::packages_group_record(laying.work),
    ];
    for record in records {
        earlier_record_cleared(&record)?;
    }
    Ok(())
}

/// [`earlier_group_cleared`] for the one group record `record`: a git step's `<work>.pgid`, a tool restore's
/// `<work>.tools.pgid` or a package restore's `<work>.packages.pgid`.
fn earlier_record_cleared(record: &str) -> Result<(), NotLaidOut> {
    let (bytes, written) = match files::read_plain_dated(Path::new(record), files::RECORD_LIMIT) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(NotLaidOut::Refused(format!("cannot read {record}: {error}"))),
        Ok(read) => read,
    };
    if let Some(recorded) = recorded_group(&bytes)
        && signal_guard(recorded.group, own_group()).is_ok()
        && record_current(recorded, written) == Record::Current
    {
        return Err(NotLaidOut::Busy);
    }
    removed(record)
}

/// Removes the lock files a git step the box ended can leave where the work lock covers them, before the run's first
/// git step, as the Windows box sweeps them: the copied index's `<work>.index.lock`; each repo's tree's
/// `.git/index.lock`, `.git/HEAD.lock` and `.git/config.lock`; and in each repo's bare repository the run's own back
/// ref's `refs/gate-back/<key>.lock`, nothing else there touched. A folder on the way (the work folder included) that is
/// a link, not a folder or missing has none of its locks looked at; a link, a folder or anything but a file at a lock's
/// name refuses the run `layout`.
pub(crate) fn swept(laying: &Laying<'_>) -> Result<(), NotLaidOut> {
    if files::plain_folder(laying.parent) {
        lock_removed(Path::new(&format!("{}.index.lock", laying.work)))?;
    }
    let work = Path::new(laying.work);
    for repo in laying.repos() {
        let tree = Path::new(&repo.tree);
        let dot_git = tree.join(".git");
        if [work, tree, dot_git.as_path()].into_iter().all(files::plain_folder) {
            for lock in ["index.lock", "HEAD.lock", "config.lock"] {
                lock_removed(&dot_git.join(lock))?;
            }
        }
        let bare = Path::new(&repo.bare);
        let refs = bare.join("refs");
        let gate_back = refs.join("gate-back");
        if [bare, &refs, &gate_back].into_iter().all(files::plain_folder) {
            lock_removed(&gate_back.join(format!("{}.lock", laying.key)))?;
        }
    }
    Ok(())
}

/// Removes git's lock file `path`, a plain file; a missing one is fine. Else why not.
fn lock_removed(path: &Path) -> Result<(), NotLaidOut> {
    let shown = path.display();
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(NotLaidOut::Refused(format!("cannot open {shown}: {error}"))),
        Ok(metadata) if metadata.is_file() => fs::remove_file(path).map_err(|error| NotLaidOut::Refused(format!("cannot remove {shown}: {error}"))),
        Ok(metadata) if metadata.is_dir() => Err(NotLaidOut::Refused(format!("{shown} is a folder where git's lock file goes"))),
        Ok(_) => Err(NotLaidOut::Refused(format!("{shown} is a link or not a file where git's lock file goes"))),
    }
}

/// What [`layout_checked`] found for [`laid_out`].
pub(crate) struct Checked {
    /// For each repo of the run, the main one first, whether its tree is made anew.
    fresh: Vec<bool>,
    /// Whether the work folder was cleared for another layout ([`work_relaid`]), so the run's layout is recorded once
    /// its trees are laid out.
    relaid: bool,
}

/// Checks the work folder of `laying` before its layout, under the work lock: the interrupted-command record
/// `<work>.command` refused unless it is missing or a file itself, not followed, as the run could neither write nor
/// remove anything else there; the work folder failed unless it is missing or the box user's own folder
/// ([`files::unusable_folder`]); the work folder cleared when its layout record names another layout than the run's
/// ([`work_relaid`]); when the interrupted-command record is still there, the warm record `<work>.warm`, the tools
/// record `<work>.tools` and the packages record `<work>.packages` removed, so none trusts a tree a command was cut
/// short in, the checkout remaking the tree;
/// then for each repo's tree, a pair's tree itself refused unless it is missing or a folder itself, `.git`,
/// `.git/objects` and `.git/objects/info` refused unless each is missing or a folder itself, and `.git/config` and
/// `.git/HEAD` unless each is missing or a file itself, none followed, and each there the box user's own and writable by
/// no group or other ([`plain_or_missing`]); a tree with anything at `.git/commondir` refused; and, when a tree has no `.git/HEAD` or no
/// `.git/objects`, which [`laid_out`] makes anew, the warm record removed, and the ran record with whatever is at its
/// name ([`command::ran_removed`]). No git step runs.
pub(crate) fn layout_checked(laying: &Laying<'_>) -> Result<Checked, NotLaidOut> {
    let work = laying.work;
    let command = command_record(work);
    plain_or_missing(Path::new(&command), false)?;
    match files::unusable_folder(Path::new(work), false) {
        None => {}
        Some(FolderFlaw::Failed(line)) => return Err(NotLaidOut::Failed(Failed::line(line))),
        Some(FolderFlaw::Open { why, .. }) => return Err(NotLaidOut::Refused(why)),
    }
    let relaid = work_relaid(laying)?;
    if files::exists(Path::new(&command)) {
        for suffix in [".warm", ".tools", ".packages"] {
            removed(&format!("{work}{suffix}"))?;
        }
    }
    let fresh = laying
        .repos()
        .map(|repo| tree_checked(Path::new(&repo.tree), repo.name.is_some()))
        .collect::<Result<Vec<bool>, NotLaidOut>>()?;
    if fresh.contains(&true) {
        removed(&format!("{work}.warm"))?;
        ran_cleared(work)?;
    }
    Ok(Checked { fresh, relaid })
}

/// [`layout_checked`] for the tree `tree`, a pair's when `paired`: whether it is made anew. No git step runs, so the
/// preflight judges a tree by it before readying it.
pub(crate) fn tree_checked(tree: &Path, paired: bool) -> Result<bool, NotLaidOut> {
    // A pair's tree sits in the work folder; a run of one repo's is the work folder, checked before.
    if paired {
        plain_or_missing(tree, true)?;
    }
    let dot_git = tree.join(".git");
    let objects = dot_git.join("objects");
    let info = objects.join("info");
    for folder in [&dot_git, &objects, &info] {
        plain_or_missing(folder, true)?;
    }
    for file in [dot_git.join("config"), dot_git.join("HEAD")] {
        plain_or_missing(&file, false)?;
    }
    let commondir = dot_git.join("commondir");
    if files::exists(&commondir) {
        return Err(NotLaidOut::Refused(format!("{} names another repository's folders", commondir.display())));
    }
    Ok(!files::exists(&dot_git.join("HEAD")) || !files::exists(&objects))
}

/// Clears the work folder of `laying` when its layout record `<work>.layout` names another layout than the run's
/// ([`names_another_layout`]), a pair's first run having none, as the Windows box clears it: the warm record and the
/// ran record ([`command::ran_record`]) removed, so the folder is new again until a command starts in it;
/// the inputs record `<work>.inputs` removed, whether or not the work folder is there, so no record names copies the
/// clear takes away; the layout record saying [`LAYOUT_CHANGING`], so a clear cut short still finds another layout
/// and clears again; every child of the work folder removed as itself, a link never followed ([`work_cleared`]); then
/// the interrupted-command record removed, since what it guarded is gone. Whether the folder was cleared.
fn work_relaid(laying: &Laying<'_>) -> Result<bool, NotLaidOut> {
    let work = laying.work;
    if !names_another_layout(&layout_record(work), &laying.layout()) {
        return Ok(false);
    }
    removed(&format!("{work}.warm"))?;
    ran_cleared(work)?;
    removed(&format!("{work}.inputs"))?;
    layout_written(work, LAYOUT_CHANGING)?;
    if files::exists(Path::new(work)) {
        work_cleared(Path::new(work))?;
    }
    removed(&command_record(work))?;
    Ok(true)
}

/// Removes every child of the work folder `work`, each as itself, beneath the folder's own descriptor: the folder
/// reached beneath its judged root and `<root>/work`, a link at any of them never followed
/// ([`beneath::work_beneath_root`]), listed from that descriptor, and each child removed beneath it, a folder with all
/// it holds and a link never followed ([`beneath::Folder::remove_child_tree`]). Else why one could not be removed or the folder
/// not opened or listed, a link at `work` or an untrusted root included.
fn work_cleared(work: &Path) -> Result<(), NotLaidOut> {
    let unlisted = |error: io::Error| NotLaidOut::Refused(format!("cannot list {}: {error}", work.display()));
    let folder = beneath::work_beneath_root(work).map_err(unlisted)?;
    for name in folder.entries().map_err(unlisted)? {
        folder
            .remove_child_tree(&name)
            .map_err(|error| NotLaidOut::Refused(format!("cannot remove {}: {error}", work.join(&name).display())))?;
    }
    Ok(())
}

/// Writes `layout` as the whole of the layout record of the work folder `work`, `<work>.layout`
/// ([`files::replace_plain`]); else why it could not be, refusing the run `layout`.
fn layout_written(work: &str, layout: &str) -> Result<(), NotLaidOut> {
    let record = format!("{work}.layout");
    files::replace_plain(Path::new(&record), layout.as_bytes()).map_err(|failure| NotLaidOut::Refused(format!("cannot write {record}: {failure}")))
}

/// Lays out the work folder of `laying`, its files checked as `checked` says ([`layout_checked`]): on a pair run every
/// repo's back ref deleted first ([`back_refs_cleared`]), so no repo keeps an earlier run's back commit whatever step
/// of this run fails after; then its trees laid out and its layout recorded ([`trees_laid_out`]).
fn laid_out<S: LineSink>(laying: &Laying<'_>, git: &RunGit<'_, S>, checked: &Checked) -> Result<(), NotLaidOut> {
    if !laying.siblings.is_empty() {
        back_refs_cleared(git, laying)?;
    }
    trees_laid_out(laying, git, checked)
}

/// The layout of a run of one repo, and of a pair's once [`laid_out`] has cleared its back refs: each repo's tree laid
/// out ([`tree_laid_out`]); then, after a clear, the layout recorded, last, so a layout cut short leaves
/// [`LAYOUT_CHANGING`] and the next run clears again.
pub(crate) fn trees_laid_out(laying: &Laying<'_>, git: &impl Steps, checked: &Checked) -> Result<(), NotLaidOut> {
    for (repo, fresh) in laying.repos().zip(&checked.fresh) {
        tree_laid_out(git, repo, *fresh)?;
    }
    if checked.relaid {
        layout_written(laying.work, &laying.layout())?;
    }
    Ok(())
}

/// Lays out the tree of `repo`: a `fresh` tree made with `git init`; then its `origin`, its line-ending config and its
/// alternates file naming its own bare repository's objects, written whole ([`git::alternates_written`]). Each step and
/// its failure name the repo on a pair run, as the Windows box's layout steps do ([`for_repo`], [`failed_for`]).
fn tree_laid_out(git: &impl Steps, repo: &Repo<'_>, fresh: bool) -> Result<(), NotLaidOut> {
    let name = repo.name.as_deref();
    if fresh {
        let step = "git init";
        git.step(&for_repo(step, name), &["init", "-q", &repo.tree])
            .map_err(|failed| failed_for(failed, step, name))?;
    }
    let info = Path::new(&repo.tree).join(".git").join("objects").join("info");
    let tree = repo.git_tree();
    origin_set(git, &tree, &repo.bare)?;
    line_ends_set(git, &tree)?;
    git::alternates_written(&info, &repo.bare).map_err(|failure| {
        NotLaidOut::Refused(match failure {
            AlternatesFailure::Folder { path, error } => format!("cannot create {}: {error}", path.display()),
            AlternatesFailure::Write { path, failure } => format!("cannot write {}: {failure}", path.display()),
        })
    })
}

/// Removes the file `path`; a missing one is fine. Else why not, refusing the run `layout`.
fn removed(path: &str) -> Result<(), NotLaidOut> {
    files::remove_if_there(Path::new(path)).map_err(|error| NotLaidOut::Refused(format!("cannot remove {path}: {error}")))
}

/// Removes whatever is at the ran record of the work folder `work` as itself ([`command::ran_removed`]), a folder
/// there included; else why it could not be, refusing the run `layout`.
fn ran_cleared(work: &str) -> Result<(), NotLaidOut> {
    command::ran_removed(work).map_err(|error| NotLaidOut::Refused(format!("cannot remove {}: {error}", command::ran_record(work))))
}

/// Whether `path` is missing or, looked at without following a link, a folder itself (`folder`) or a file itself, the
/// box user's own and writable by no group or other.
fn plain_or_missing(path: &Path, folder: bool) -> Result<(), NotLaidOut> {
    let shown = path.display();
    let metadata = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(NotLaidOut::Refused(format!("cannot read {shown}: {error}"))),
        Ok(metadata) => metadata,
    };
    let euid = files::effective_uid();
    if folder && !metadata.is_dir() {
        Err(NotLaidOut::Refused(format!("{shown} is a link or not a folder")))
    } else if !folder && !metadata.is_file() {
        Err(NotLaidOut::Refused(format!("{shown} is a link or not a file")))
    } else {
        match owner_flaw(metadata.uid(), metadata.mode(), euid, WRITABLE_BY_OTHERS) {
            Some(OwnerFlaw::Owner) => Err(NotLaidOut::Refused(format!("{shown} is not owned by {}", files::user_name(euid)))),
            Some(OwnerFlaw::Open) => Err(NotLaidOut::Refused(format!("{shown} is writable by others"))),
            None => Ok(()),
        }
    }
}

/// Each repo of `request`'s snapshot fetched and checked out, the main one first ([`repo_checked_out`]); then, on a run
/// of one repo, the earlier run's back ref deleted ([`back_ref_cleared`]). A pair run deleted every repo's as it laid
/// the work folder out ([`laid_out`]).
fn checked_out<S: LineSink>(request: &Request<'_>, git: &RunGit<'_, S>) -> Result<(), Failed> {
    for repo in request.repos() {
        repo_checked_out(git, request.key, repo)?;
    }
    if request.siblings.is_empty() {
        back_ref_cleared(git, request.key, &request.main)?;
    }
    Ok(())
}

/// The snapshot of `repo` fetched from its bare repository at `refs/gate/<key>`, its commit found, and checked out by
/// its commit over whatever the last run left, untracked files removed and ignored ones (build output) kept. The commit,
/// never the ref's tip, is what is checked out, so a later push of the key changes nothing this run runs. Each step is
/// named for the repo on a pair run ([`Repo::step`]).
fn repo_checked_out<S: LineSink>(git: &RunGit<'_, S>, key: &str, repo: &Repo<'_>) -> Result<(), Failed> {
    let tree = repo.git_tree();
    let refname = format!("refs/gate/{key}");
    git.step_as(
        &repo.step("fetch"),
        &tree.args(&[
            "fetch",
            "-q",
            "--no-recurse-submodules",
            "--upload-pack=git-upload-pack",
            &repo.bare,
            &refname,
        ]),
        GitClass::Long,
    )?;
    let commit = format!("{}^{{commit}}", repo.snapshot);
    if !git
        .quiet(&repo.step("cat-file"), &tree.args(&["cat-file", "-e", &commit]), GitClass::Short)?
        .status
        .success()
    {
        return Err(Failed::line(missing_snapshot(repo)));
    }
    git.tree_step(
        &repo.step("checkout"),
        &tree.args(&["checkout", "-q", "--force", "--detach", repo.snapshot]),
        GitClass::Long,
    )?;
    git.tree_step(&repo.step("clean"), &tree.args(&["clean", "-fdq"]), GitClass::Long)
        .map(drop)
}

/// The line of a snapshot of `repo` its repository lacks after the fetch, as the Windows box says it.
fn missing_snapshot(repo: &Repo<'_>) -> String {
    let sha = repo.snapshot;
    match &repo.name {
        None => format!("gate: remote snapshot {sha} is not in the work folder's repository after the fetch"),
        Some(name) => format!("gate: remote snapshot {sha} is not in the repository of {name} after the fetch"),
    }
}

/// Deletes every repo's `refs/gate-back/<key>` in its bare repository ([`back_ref_cleared`]), the main one first.
fn back_refs_cleared<S: LineSink>(git: &RunGit<'_, S>, laying: &Laying<'_>) -> Result<(), Failed> {
    laying.repos().try_for_each(|repo| back_ref_cleared(git, laying.key, repo))
}

/// Deletes `refs/gate-back/<key>` in the bare repository of `repo`, an earlier run's back commit, so the client finds
/// none to fetch after a run that makes none.
pub(crate) fn back_ref_cleared<S: LineSink>(git: &RunGit<'_, S>, key: &str, repo: &Repo<'_>) -> Result<(), Failed> {
    let bare = format!("--git-dir={}", repo.bare);
    let back = format!("refs/gate-back/{key}");
    git.bare_step(&repo.step("back ref reset"), &[&bare, "update-ref", "-d", &back], GitClass::Short)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test whose temp folder, repository or payload cannot be made must fail loudly"
)]
mod tests {
    use std::fs;
    use std::io::{BufRead, Cursor, Read};
    use std::os::unix::fs::{MetadataExt, symlink};
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, PoisonError, mpsc};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime};

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use gate_inputs::hash::sha256_of;
    use gate_inputs::manifest::Manifest;
    use gate_wire::{
        PathForm, Payload, Refusal, busy_line, checkout_prefix, checkout_start_line, escaped, layout_line, refused_line, started_line, tools_line,
    };
    use serde_json::{Value, json};

    use gate_watchdog::Pump;

    use super::{
        DrainSeams, NotLaidOut, REFUSED_STATUS, Ready, Request, Sources, Unready, Watches, inputs_matched, login_read, place_of, prepare, prepared,
        run_along, run_reading, temp_made, test_template, work_cleared,
    };
    #[cfg(target_os = "macos")]
    use crate::beneath::ANOTHER_VOLUME;
    use crate::command::{Launch, Sampler, run_command};
    use crate::console::{Console, Forwarding};
    use crate::console::{LineSink, Output, RecordingSink, Stream};
    use crate::files::{self, Locked, effective_uid, take_lock};
    use crate::git::test_program::{self, real_git};
    use crate::git::{CLOSED_STATUS, GitClass, test_limit};
    use crate::heartbeat::{self, Periods};
    use crate::lines::{Godot, Login, command_status, job_record_text, login_path_line, recorded_group};
    use crate::outputs::OUTPUTS_BUDGET;
    use crate::outputs::tests::read_archive;
    use crate::pid_info::start_of;
    use crate::probe::tests::fake_godot;
    use crate::stop::stop;

    const KEY: &str = "repo-0123abcd";
    const NONCE: &str = "0123456789abcdef0123456789abcdef";
    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// Runs git with `args` to set a case up or look at it; its standard output, trimmed, and whether it exited 0.
    #[expect(
        clippy::disallowed_methods,
        reason = "the spawn lock is the Windows gate's; this sets a case's repository up"
    )]
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

    /// A work root in a temp folder of the case `name`, private to this process and removed when the case ends,
    /// with its `<root>/work` and an empty bare repository.
    struct Case {
        base: PathBuf,
    }

    impl Case {
        fn new(name: &str) -> Case {
            Case::rooted(name, "gate-remote")
        }

        /// A case whose work root is named `leaf`.
        fn rooted(name: &str, leaf: &str) -> Case {
            let base = std::env::temp_dir().join(format!("gate-box-unix-run-{}-{name}", std::process::id()));
            if base.exists() {
                fs::remove_dir_all(&base).expect("a stale case folder is removed");
            }
            let case = Case { base: base.join(leaf) };
            // Mode 0700 as the box demands of a root and its `<root>/work`, whatever the umask.
            std::os::unix::fs::DirBuilderExt::mode(fs::DirBuilder::new().recursive(true), 0o700)
                .create(format!("{}/work", case.root()))
                .expect("the work folders' parent is made");
            git(&["init", "--bare", "-q", &case.bare()]);
            case
        }

        fn root(&self) -> String {
            self.base.to_str().expect("the temp folder is UTF-8").to_owned()
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

        fn source(&self) -> PathBuf {
            self.base.with_file_name("source")
        }

        /// The run payload of this case's work folder checking out `snapshot`, with `extra` keys set over it.
        fn json(&self, snapshot: &str, extra: &Value) -> Value {
            let (root, work) = (self.root(), self.work());
            let mut json = json!({
                "Key": KEY,
                "Bare": self.bare(),
                "Work": work,
                "Lock": format!("{work}.lock"),
                "Index": format!("{work}.index"),
                "OutputFolder": format!("{root}/outputs"),
                "Job": format!("gate-remote-{KEY}-0123456789ab"),
                "Nonce": NONCE,
                "Snapshot": snapshot,
                "Command": ["true"],
            });
            if let (Some(object), Some(extra)) = (json.as_object_mut(), extra.as_object()) {
                object.extend(extra.clone());
            }
            json
        }

        /// A commit holding `files`, written over the case's source tree, pushed to the bare repository as
        /// `refs/gate/<key>`; its sha.
        fn snapshot(&self, files: &[(&str, &str)]) -> String {
            snapshot_pushed(&self.source(), &self.bare(), files)
        }

        /// Points the earlier run's back ref at `commit`.
        fn back_ref_set(&self, commit: &str) {
            git(&[
                &format!("--git-dir={}", self.bare()),
                "update-ref",
                &format!("refs/gate-back/{KEY}"),
                commit,
            ]);
        }

        fn has_back_ref(&self) -> bool {
            let back = format!("refs/gate-back/{KEY}");
            git_said(&[&format!("--git-dir={}", self.bare()), "rev-parse", "--verify", "-q", &back]).1
        }

        /// The commit the work folder's tree has checked out.
        fn head(&self) -> String {
            git(&["-C", &self.work(), "rev-parse", "HEAD"])
        }

        /// Runs a first run of `commit`, which lays the tree out; fails the case unless it is ready.
        fn laid_out(&self, commit: &str) {
            let (status, said) = ran(&self.json(commit, &json!({})));
            assert_eq!(status, 0, "{said:?}");
        }
    }

    impl Drop for Case {
        fn drop(&mut self) {
            // Best effort: a folder left behind is in the temp folder and named for this process.
            if let Some(base) = self.base.parent() {
                let _ = fs::remove_dir_all(base);
            }
        }
    }

    /// A commit holding `files`, written over the source tree `source`, pushed to the bare repository `bare` as
    /// `refs/gate/<key>`; its sha.
    fn snapshot_pushed(source: &Path, bare: &str, files: &[(&str, &str)]) -> String {
        let source = source.to_str().expect("the temp folder is UTF-8");
        if !Path::new(source).exists() {
            git(&["init", "-q", source]);
        }
        for (name, text) in files {
            let path = Path::new(source).join(name);
            fs::create_dir_all(path.parent().expect("a file has a folder")).expect("the file's folder is made");
            fs::write(path, text).expect("the file is written");
        }
        git(&["-C", source, "add", "-A"]);
        let identity = ["-c", "user.name=gate", "-c", "user.email=gate@example.com", "-c", "commit.gpgsign=false"];
        let mut commit = vec!["-C", source];
        commit.extend(identity);
        commit.extend(["commit", "-q", "--allow-empty", "-m", "snapshot"]);
        git(&commit);
        let refspec = format!("HEAD:refs/gate/{KEY}");
        git(&["-C", source, "push", "-q", "--force", bare, &refspec]);
        git(&["-C", source, "rev-parse", "HEAD"])
    }

    fn decoded(json: &Value) -> Payload {
        Payload::decode(&STANDARD.encode(json.to_string()), PathForm::Unix).expect("the payload decodes")
    }

    /// Readies the run of `json`; the status it ended on, 0 for a run ready for its command (its lock let go), and the
    /// lines it said, each checked to be on stderr.
    fn ran(json: &Value) -> (u8, Vec<String>) {
        let payload = decoded(json);
        let output = Arc::new(Output::new(RecordingSink::default()));
        let status = match prepare(&payload, &output, quiet_periods(), own_path) {
            Ok(mut ready) => {
                tmp_cleared(&mut ready);
                0
            }
            Err(status) => status,
        };
        (status, stderr_of(&output))
    }

    /// Removes the temp folder `ready` made, failing the case unless it goes, so a case that keeps a readied run
    /// leaves nothing in `/tmp`.
    fn tmp_cleared(ready: &mut Ready<'_>) {
        if let Some(temp) = ready.temp.take() {
            fs::remove_dir_all(temp.path()).expect("the run's temp folder is removed");
        }
    }

    /// Periods whose heartbeat never beats within a case, so a case's lines are exactly the run's own.
    fn quiet_periods() -> Periods {
        Periods {
            heartbeat: Duration::from_secs(3600),
            ..Periods::default()
        }
    }

    /// A sink that keeps every line and its stream, and makes `marker` once it is given a heartbeat.
    struct MarkingSink {
        lines: Mutex<Vec<(Stream, String)>>,
        marker: PathBuf,
    }

    impl MarkingSink {
        /// The lines written so far with their streams, unstamped, the process lines and their note left out, as
        /// [`RecordingSink::said`] gives them.
        fn said(&self) -> Vec<(Stream, String)> {
            self.lines
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .map(|(stream, line)| (*stream, gate_wire::stamp::unstamped(line).text.to_owned()))
                .filter(|(_, text)| !gate_wire::stamp::is_process_line(text) && !text.starts_with("process lines: "))
                .collect()
        }
    }

    impl LineSink for MarkingSink {
        fn write_line(&self, stream: Stream, line: &str) -> std::io::Result<()> {
            if line.starts_with("gate-remote-heartbeat:") && !self.marker.exists() {
                fs::write(&self.marker, "")?;
            }
            self.lines.lock().unwrap_or_else(PoisonError::into_inner).push((stream, line.to_owned()));
            Ok(())
        }
    }

    /// A heartbeat whose thread cannot start.
    fn no_beat(_output: Arc<dyn Forwarding>, _period: Duration) -> std::io::Result<Pump> {
        Err(std::io::Error::other("no thread"))
    }

    #[test]
    fn a_heartbeat_that_cannot_start_is_said_and_the_run_goes_on() {
        let case = Case::new("no-beat");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let payload = decoded(&case.json(&commit, &json!({})));
        let output = Arc::new(Output::new(RecordingSink::default()));
        let sources = Sources {
            login: own_path,
            start: no_beat,
            input: &mut Cursor::new(Vec::new()),
            drain: DrainSeams::default(),
        };
        let mut ready = prepared(&payload, &output, quiet_periods(), sources);
        if let Ok(ready) = ready.as_mut() {
            tmp_cleared(ready);
        }
        assert!(ready.as_ref().is_ok_and(|ready| ready.pump.is_none()), "{:?}", output.sink().said());
        let said = stderr_of(&output);
        assert_eq!(
            said.first().map(String::as_str),
            Some("gate: remote heartbeat stopped: cannot start the heartbeat: no thread"),
            "{said:?}"
        );
        assert_eq!(after_checkout(said.get(1..).unwrap_or_default()), Vec::<String>::new());
        assert_eq!(case.head(), commit, "the run went on to its checkout");
    }

    #[test]
    fn the_pump_beats_before_the_checkout() {
        let case = Case::new("beats");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let marker = case.base.with_file_name("beaten");
        let shims = case.base.with_file_name("shims");
        fs::create_dir_all(&shims).expect("the shim folder is made");
        // The fetch, the checkout's first step, waits for the first heartbeat, within a hang guard.
        let shim = shims.join("git");
        let script = format!(
            "#!/bin/sh\ncase \" $* \" in\n*\" fetch \"*)\n  i=0\n  while [ ! -e '{marker}' ]; do\n    [ $i -ge 1200 ] && exit 97\n    \
             sleep 0.1; i=$((i+1))\n  done;;\nesac\nexec '{git}' \"$@\"\n",
            marker = marker.display(),
            git = real_git().display()
        );
        fs::write(&shim, script).expect("the shim is written");
        fs::set_permissions(&shim, std::os::unix::fs::PermissionsExt::from_mode(0o700)).expect("the shim is made runnable");
        test_program::set(&shim);
        let payload = decoded(&case.json(&commit, &json!({})));
        let output = Arc::new(Output::new(MarkingSink {
            lines: Mutex::new(Vec::new()),
            marker: marker.clone(),
        }));
        let periods = Periods {
            heartbeat: Duration::from_millis(1),
            ..Periods::default()
        };
        let ready = prepare(&payload, &output, periods, own_path);
        let lines = || output.sink().said();
        assert!(ready.is_ok(), "the run is ready: {:?}", lines());
        let mut ready = ready.expect("the run is ready");
        tmp_cleared(&mut ready);
        let launch = Launch {
            words: &ready.request.command,
            place: &ready.place,
            work: ready.request.work,
            job: ready.request.job,
            nonce: ready.request.nonce,
            env: &ready.request.env,
            login_path: None,
            tmp: None,
            godot: Godot::Unchosen,
            lock: &ready.lock,
            timeout: None,
            host: "",
            sample: periods.sample,
            sampler: Sampler::Machine,
            pump: ready.pump.as_ref(),
        };
        let status = command_status(run_command(&launch, &output));
        let Ready { pump, .. } = ready;
        if let Some(pump) = pump {
            heartbeat::stop(pump);
        }
        assert_eq!(status, 0, "{:?}", lines());
        let said = lines();
        let beat = said
            .iter()
            .position(|(stream, line)| *stream == Stream::Err && line.starts_with("gate-remote-heartbeat:"));
        let started = said
            .iter()
            .position(|(stream, line)| *stream == Stream::Err && *line == started_line(NONCE));
        assert!(
            beat.is_some() && started.is_some() && beat < started,
            "a beat before the start marker: {said:?}"
        );
    }

    /// Watches that never beat within a case and sample the whole machine free every 20 ms, so a ceiling runs with
    /// wall time however loaded the machine is.
    fn free_watches() -> Watches {
        Watches {
            periods: Periods {
                sample: Duration::from_millis(20),
                ..quiet_periods()
            },
            sampler: Sampler::FreePercent(100),
            budget: OUTPUTS_BUDGET,
            drain: DrainSeams::default(),
        }
    }

    /// The identity a case's payload gives the back commit.
    fn identity() -> Value {
        json!({
            "GIT_AUTHOR_NAME": "Box Author",
            "GIT_AUTHOR_EMAIL": "author@example.com",
            "GIT_COMMITTER_NAME": "Box Committer",
            "GIT_COMMITTER_EMAIL": "committer@example.com",
        })
    }

    /// Runs the whole run of `json` on the box's own `PATH`; the status and every line said, with its stream.
    fn whole_run(json: &Value) -> (u8, Vec<(Stream, String)>) {
        let payload = decoded(json);
        let output = Arc::new(Output::new(RecordingSink::default()));
        let status = run_along(&payload, &output, free_watches(), own_path);
        (status, output.sink().said())
    }

    impl Case {
        /// The back ref's commit, `None` when there is none.
        fn back_commit(&self) -> Option<String> {
            let back = format!("refs/gate-back/{KEY}");
            let (said, found) = git_said(&[&format!("--git-dir={}", self.bare()), "rev-parse", "--verify", "-q", &back]);
            found.then_some(said)
        }

        /// The words of `format` shown for `commit` in the bare repository.
        fn shown(&self, format: &str, commit: &str) -> String {
            git(&[&format!("--git-dir={}", self.bare()), "show", "-s", &format!("--format={format}"), commit])
        }

        /// The run payload of `snapshot` running `command`, with the case's identity.
        fn commanded(&self, snapshot: &str, command: &[&str]) -> Value {
            self.json(snapshot, &json!({"Command": command, "Identity": identity()}))
        }
    }

    #[test]
    fn a_run_says_the_checkout_start_and_done_lines_before_its_start_marker() {
        let case = Case::new("checkout-lines");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let (status, said) = whole_run(&case.commanded(&commit, &["/bin/sh", "-c", "true"]));
        assert_eq!(status, 0, "{said:?}");
        let err: Vec<String> = said
            .iter()
            .filter(|(stream, _)| *stream == Stream::Err)
            .map(|(_, line)| line.clone())
            .collect();
        let start = checkout_start_line(NONCE);
        let started = err.iter().position(|line| *line == start).expect("the checkout's start line is said");
        let after = after_checkout(err.get(started..).unwrap_or_default());
        assert!(
            after.contains(&started_line(NONCE)),
            "the start line, then the done line, then the start marker: {said:?}"
        );
    }

    #[test]
    fn a_command_that_changes_the_tree_pushes_a_back_commit_on_head_with_the_payload_s_identity() {
        let case = Case::new("back-changed");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        case.back_ref_set(&commit);
        let (status, said) = whole_run(&case.commanded(&commit, &["/bin/sh", "-c", "echo new > new.txt; echo b > a.txt"]));
        assert_eq!(status, 0, "{said:?}");
        assert_eq!(
            back_commits_said(said.iter().map(|(_, line)| line.as_str())),
            0,
            "a run of one repo: {said:?}"
        );
        let back = case.back_commit().expect("the back ref is pushed");
        assert_ne!(back, commit, "a new commit, not the earlier run's ref");
        assert_eq!(case.shown("%P", &back), commit, "its parent is the snapshot");
        assert_eq!(
            case.shown("%an <%ae>|%cn <%ce>|%s", &back),
            "Box Author <author@example.com>|Box Committer <committer@example.com>|gate-remote back"
        );
        let bare = format!("--git-dir={}", case.bare());
        assert_eq!(git(&[&bare, "show", &format!("{back}:new.txt")]), "new");
        assert_eq!(git(&[&bare, "show", &format!("{back}:a.txt")]), "b");
        assert!(!Path::new(&format!("{}.index", case.work())).exists(), "the copied index is removed");
        assert!(matches!(take_lock(Path::new(&case.lock())), Ok(Locked::Held(_))), "the lock is let go");
    }

    #[test]
    fn the_fetch_and_the_back_push_recurse_into_no_submodule() {
        let case = Case::new("no-submodules");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let words = case.base.with_file_name("git-words");
        let shim = case.base.with_file_name("git-shim-words");
        let script = format!(
            "#!/bin/sh\nprintf ' %s \\n' \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
            words.display(),
            real_git().display()
        );
        fs::write(&shim, script).expect("the shim is written");
        fs::set_permissions(&shim, std::os::unix::fs::PermissionsExt::from_mode(0o700)).expect("the shim is made runnable");
        test_program::set(&shim);

        let (status, said) = whole_run(&case.commanded(&commit, &["/bin/sh", "-c", "echo new > new.txt"]));

        assert_eq!(status, 0, "{said:?}");
        assert!(case.back_commit().is_some(), "the back ref is pushed");
        let words = fs::read_to_string(&words).expect("the shim's words are read");
        for step in [
            " fetch -q --no-recurse-submodules --upload-pack=git-upload-pack ",
            " push -q --no-recurse-submodules --receive-pack=git-receive-pack ",
        ] {
            assert!(words.lines().any(|line| line.contains(step)), "{step}: {words}");
        }
    }

    #[test]
    fn a_same_size_rewrite_in_the_second_the_index_was_written_is_in_the_back_commit() {
        let case = Case::new("back-racy");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        // The command rewrites a.txt at its size and leaves it, and the tree's index, in one second the index's entry
        // for a.txt also names, as a fast command leaves them within its checkout's second: the stat data alone cannot
        // tell the rewrite, and only the index's own mtime makes git look at the content. git compares ctime only to
        // the second, so it is left out of the comparison here rather than raced.
        let script = "git config core.trustctime false && touch -t 200101010000 a.txt && git update-index -q --refresh \
                      && echo b > a.txt && touch -t 200101010000 a.txt .git/index";
        let (status, said) = whole_run(&case.commanded(&commit, &["/bin/sh", "-c", script]));
        assert_eq!(status, 0, "{said:?}");
        let back = case.back_commit().expect("the rewritten a.txt makes a back commit");
        assert_eq!(git(&[&format!("--git-dir={}", case.bare()), "show", &format!("{back}:a.txt")]), "b");
    }

    #[test]
    fn a_command_that_changes_nothing_makes_no_commit_and_leaves_no_back_ref() {
        let case = Case::new("back-unchanged");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        case.back_ref_set(&commit);
        let (status, said) = whole_run(&case.commanded(&commit, &["/bin/sh", "-c", "cat a.txt"]));
        assert_eq!(status, 0, "{said:?}");
        assert!(!case.has_back_ref(), "no back ref: {said:?}");
    }

    #[test]
    fn a_stale_copied_index_is_replaced() {
        let case = Case::new("back-stale-index");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let index = format!("{}.index", case.work());
        fs::create_dir_all(Path::new(&case.work()).parent().expect("the work folder has a parent")).expect("the parent is made");
        fs::write(&index, "not an index").expect("a stale index is left");
        let (status, said) = whole_run(&case.commanded(&commit, &["/bin/sh", "-c", "echo c > c.txt"]));
        assert_eq!(status, 0, "{said:?}");
        let back = case.back_commit().expect("the back ref is pushed");
        assert_eq!(
            git(&[&format!("--git-dir={}", case.bare()), "show", &format!("{back}:a.txt")]),
            "a",
            "the tree kept a.txt"
        );
        assert!(!Path::new(&index).exists(), "the copy is removed");
    }

    #[test]
    fn a_push_that_fails_says_the_sync_back_line_and_fails_a_command_that_exited_0() {
        let case = Case::new("back-push-fails");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        // The command puts a folder holding a file at the back ref's name once the checkout reset the ref, which fails
        // the push's ref update; git would remove an empty one.
        let in_the_way = format!("{}/refs/gate-back/{KEY}", case.bare());
        let script = format!("echo x > x.txt; mkdir -p '{in_the_way}'; echo x > '{in_the_way}/in-the-way'");
        let payload = decoded(&case.commanded(&commit, &["/bin/sh", "-c", &script]));
        let output = Arc::new(Output::new(RecordingSink::default()));
        let status = run_along(&payload, &output, free_watches(), own_path);
        let err: Vec<String> = output
            .sink()
            .said()
            .into_iter()
            .filter(|(stream, _)| *stream == Stream::Err)
            .map(|(_, line)| line)
            .collect();
        assert_eq!(status, 1, "{err:?}");
        assert!(
            err.last()
                .is_some_and(|line| line.starts_with("gate: remote sync-back failed: push failed")),
            "{err:?}"
        );
    }

    #[test]
    fn a_command_the_box_killed_makes_no_back_commit() {
        let case = Case::new("back-killed");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        case.back_ref_set(&commit);
        let json = case.json(
            &commit,
            &json!({"Command": ["/bin/sh", "-c", "echo x > x.txt; sleep 600"], "Identity": identity(), "Timeout": 1}),
        );
        let (status, said) = whole_run(&json);
        assert_eq!(status, 124, "{said:?}");
        assert!(!case.has_back_ref(), "no back ref, the earlier one deleted: {said:?}");
        assert_eq!(
            back_commits_said(said.iter().map(|(_, line)| line.as_str())),
            0,
            "a run of one repo: {said:?}"
        );
    }

    /// The box's own `PATH` as the login shell's, and no Godot root, so a case starts no login shell.
    fn own_path() -> Result<Login, String> {
        let path = std::env::var("PATH").map_err(|error| error.to_string())?;
        Ok(Login { path, godot_root: None })
    }

    /// Whether [`counted_login`] was called.
    static LOGIN_READ: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// A login shell's `PATH` that cannot be read, noting that it was asked for.
    fn counted_login() -> Result<Login, String> {
        LOGIN_READ.store(true, std::sync::atomic::Ordering::SeqCst);
        Err("the case's login shell".to_owned())
    }

    /// Runs every git step of this thread through a shim that runs `after_add` once git's `add -A` on the copied index
    /// has run, `$GIT_INDEX_FILE` naming the copy; every other step is the real git's.
    fn shim_after_add(case: &Case, after_add: &str) {
        let shim = case.base.with_file_name("git-shim");
        let script = format!(
            "#!/bin/sh\n'{git}' \"$@\"\nstatus=$?\ncase \" $* \" in\n\
             *\" add -A \"*) [ -n \"$GIT_INDEX_FILE\" ] && {{ {after_add}; }};;\nesac\nexit $status\n",
            git = real_git().display()
        );
        fs::write(&shim, script).expect("the shim is written");
        fs::set_permissions(&shim, std::os::unix::fs::PermissionsExt::from_mode(0o700)).expect("the shim is made runnable");
        test_program::set(&shim);
    }

    /// A login shell whose `GATE_GODOT_ROOT` is set but whose `PATH` holds no absolute folder.
    #[expect(clippy::unnecessary_wraps, reason = "a run reads its login PATH through a `LoginShell`, which may fail")]
    fn relative_login() -> Result<Login, String> {
        Ok(Login {
            path: "bin:".to_owned(),
            godot_root: Some("/Users/b/godot".to_owned()),
        })
    }

    #[test]
    fn a_login_shell_that_cannot_be_read_keeps_the_box_s_path_and_gives_no_godot_root() {
        let output = Output::new(RecordingSink::default());
        let failed = || Err("the case's login shell".to_owned());
        assert_eq!(login_read(&output, failed), None, "the box's own PATH and no root");
        assert_eq!(
            login_read(&output, relative_login),
            None,
            "a PATH that cannot be used drops the root with it"
        );
        assert_eq!(
            err_of(&output),
            [
                login_path_line("the case's login shell"),
                login_path_line("said a PATH with no absolute folder")
            ]
        );
        let both = || {
            Ok(Login {
                path: "/usr/bin::bin".to_owned(),
                godot_root: Some("/Users/b/godot".to_owned()),
            })
        };
        assert_eq!(
            login_read(&output, both),
            Some(Login {
                path: "/usr/bin".to_owned(),
                godot_root: Some("/Users/b/godot".to_owned()),
            })
        );
    }

    thread_local! {
        /// What [`godot_login`] gives this thread's runs as the login shell's.
        static GODOT_LOGIN: std::cell::RefCell<Option<Login>> = const { std::cell::RefCell::new(None) };
    }

    /// The login shell this thread's case set ([`godot_login_set`]), else the box's own `PATH` and no Godot root.
    fn godot_login() -> Result<Login, String> {
        GODOT_LOGIN.with(|login| login.borrow().clone()).map_or_else(own_path, Ok)
    }

    /// Makes this thread's runs read `front` before the box's own `PATH`, and `root` as the Godot root.
    fn godot_login_set(front: Option<&Path>, root: Option<&Path>) {
        let own = std::env::var("PATH").expect("PATH is set");
        let login = Login {
            path: front.map_or_else(|| own.clone(), |front| format!("{}:{own}", front.display())),
            godot_root: root.map(|root| root.display().to_string()),
        };
        GODOT_LOGIN.with(|cell| *cell.borrow_mut() = Some(login));
    }

    /// A case whose login shell's Godot root holds the install of 4.7.2 only, its program a script that says its words
    /// and its `GODOT_PATH`, `|`-joined; the case and that program's path.
    fn godot_case(name: &str) -> (Case, String) {
        let case = Case::new(name);
        let root = case.base.with_file_name("godot-root");
        let program = fake_godot(&root, "4.7.2", "#!/bin/sh\nprintf '%s|%s\\n' \"$*\" \"${GODOT_PATH-unset}\"\n");
        godot_login_set(None, Some(&root));
        (case, program.display().to_string())
    }

    /// Runs `command` with the payload keys `extra` along [`godot_login`]; the status, the command's own lines on
    /// standard output, and the lines said on standard error, the run's start marker among them.
    fn godot_run(case: &Case, command: &[&str], extra: &Value) -> (u8, Vec<String>, Vec<String>) {
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let mut keys = json!({ "Command": command, "Identity": identity() });
        if let (Some(keys), Some(extra)) = (keys.as_object_mut(), extra.as_object()) {
            keys.extend(extra.clone());
        }
        let payload = decoded(&case.json(&commit, &keys));
        let output = Arc::new(Output::new(RecordingSink::default()));
        let status = run_along(&payload, &output, free_watches(), godot_login);
        let out = output
            .sink()
            .said()
            .into_iter()
            .filter(|(stream, _)| *stream == Stream::Out)
            .map(|(_, line)| line)
            .collect();
        (status, out, err_of(&output))
    }

    /// A command that says its `GODOT_PATH`, or `unset`.
    const SAYS_GODOT_PATH: [&str; 3] = ["/bin/sh", "-c", "printf '%s\\n' \"${GODOT_PATH-unset}\""];

    #[test]
    fn a_godot_word_of_a_run_with_a_version_runs_its_install_with_godot_path_naming_it() {
        let (case, program) = godot_case("godot-installed");
        let version = json!({ "GodotVersion": "4.7.2" });
        let words = ["Godot_console", "--headless", "--path", "src/X", "--build-solutions", "--quit"];
        let (status, out, err) = godot_run(&case, &words, &version);
        assert_eq!(status, 0, "{err:?}");
        assert_eq!(out, [format!("--headless --path src/X --build-solutions --quit|{program}")]);
        let (status, out, err) = godot_run(&case, &SAYS_GODOT_PATH, &version);
        assert_eq!((status, out), (0, vec![program]), "any other word runs as written: {err:?}");
    }

    #[test]
    fn a_payload_env_setting_godot_path_is_refused() {
        let (case, _) = godot_case("godot-path");
        let theirs = json!({ "GodotVersion": "4.7.2", "Env": { "GODOT_PATH": "/theirs/godot" } });
        let (status, out, err) = godot_run(&case, &SAYS_GODOT_PATH, &theirs);
        assert_eq!((status, out), (2, vec![]), "the caller's is refused: {err:?}");
        assert_eq!(err, ["gate: box payload Env sets GODOT_PATH, a machine path the builder keeps"]);
    }

    #[test]
    fn a_godot_word_whose_version_has_no_install_fails_127_never_using_the_path() {
        let (case, _) = godot_case("godot-missing");
        // A Godot console word the login PATH holds, which the run must not fall back to.
        let shims = case.base.with_file_name("path-godot");
        fs::create_dir_all(&shims).expect("the PATH folder is made");
        let on_path = shims.join("Godot_console");
        fs::write(&on_path, "#!/bin/sh\necho from-path\n").expect("the PATH's Godot is written");
        fs::set_permissions(&on_path, std::os::unix::fs::PermissionsExt::from_mode(0o700)).expect("it is made runnable");
        godot_login_set(Some(&shims), Some(&case.base.with_file_name("godot-root")));
        let words = ["Godot_console", "--quit"];
        let (status, out, err) = godot_run(&case, &words, &json!({}));
        assert_eq!(
            (status, out),
            (0, vec!["from-path".to_owned()]),
            "no version runs the word as written: {err:?}"
        );

        let unstarted = "gate: remote could not start Godot_console: No such file or directory (os error 2)".to_owned();
        let (status, out, err) = godot_run(&case, &words, &json!({ "GodotVersion": "4.8.0" }));
        assert_eq!(status, 127, "{err:?}");
        assert!(out.is_empty() && err.contains(&unstarted), "{out:?} {err:?}");
        godot_login_set(Some(&shims), None);
        let (status, out, err) = godot_run(&case, &words, &json!({ "GodotVersion": "4.7.2" }));
        assert_eq!(status, 127, "no Godot root is Godot missing: {err:?}");
        assert!(out.is_empty() && err.contains(&unstarted), "{out:?} {err:?}");
    }

    /// The lines `output` said on stderr.
    fn err_of(output: &Output<RecordingSink>) -> Vec<String> {
        output
            .sink()
            .said()
            .into_iter()
            .filter(|(stream, _)| *stream == Stream::Err)
            .map(|(_, line)| line)
            .collect()
    }

    #[test]
    fn a_busy_run_reads_no_login_path_and_says_no_path_line() {
        let case = Case::new("busy-no-login");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let held = take_lock(Path::new(&case.lock()));
        assert!(matches!(held, Ok(Locked::Held(_))), "the case holds the work lock");
        let payload = decoded(&case.json(&commit, &json!({})));
        let output = Arc::new(Output::new(RecordingSink::default()));
        let status = run_along(&payload, &output, free_watches(), counted_login);
        assert_eq!(status, REFUSED_STATUS);
        assert!(!LOGIN_READ.load(std::sync::atomic::Ordering::SeqCst), "no login shell was asked for");
        let err = err_of(&output);
        assert!(!err.iter().any(|line| line.starts_with("gate: remote path:")), "{err:?}");
        assert_eq!(err, [refused_line(NONCE, Refusal::Busy), busy_line(KEY)]);
    }

    #[test]
    fn a_copied_index_that_is_gone_before_write_tree_fails_the_sync_back_and_commits_nothing() {
        let case = Case::new("back-index-gone");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        shim_after_add(&case, "rm -f \"$GIT_INDEX_FILE\"");
        let payload = decoded(&case.commanded(&commit, &["/bin/sh", "-c", "echo x > x.txt"]));
        let output = Arc::new(Output::new(RecordingSink::default()));
        let status = run_along(&payload, &output, free_watches(), own_path);
        let err = err_of(&output);
        assert_eq!(status, 1, "{err:?}");
        assert_eq!(
            err.last().map(String::as_str),
            Some(
                format!(
                    "gate: remote sync-back failed: write-tree failed: the copied index {}.index is gone",
                    case.work()
                )
                .as_str()
            )
        );
        assert!(!case.has_back_ref(), "no commit is pushed");
    }

    #[test]
    fn a_lost_lock_leaves_the_copied_index_in_place() {
        let case = Case::new("back-lock-lost");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        shim_after_add(&case, &format!("rm -f '{}'", case.lock()));
        let payload = decoded(&case.commanded(&commit, &["/bin/sh", "-c", "echo x > x.txt"]));
        let output = Arc::new(Output::new(RecordingSink::default()));
        let status = run_along(&payload, &output, free_watches(), own_path);
        let err = err_of(&output);
        assert_eq!(status, 1, "{err:?}");
        // The lock goes inside the add step, which may see it lost itself, else the look before write-tree does.
        let lost = format!("the work folder lock {} is no longer held", case.lock());
        assert!(
            err.last()
                .is_some_and(|line| line.starts_with("gate: remote sync-back failed: ") && line.ends_with(&lost)),
            "{err:?}"
        );
        assert!(
            Path::new(&format!("{}.index", case.work())).exists(),
            "the copy is left, the lock being lost"
        );
        assert!(!case.has_back_ref(), "no commit is pushed");
    }

    #[test]
    fn a_link_at_the_index_copy_fails_the_sync_back() {
        let case = Case::new("back-index-link");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let target = case.base.with_file_name("elsewhere");
        fs::write(&target, "kept").expect("the link's target is written");
        let copy = format!("{}.index", case.work());
        symlink(&target, &copy).expect("a link takes the copy's name");
        let payload = decoded(&case.commanded(&commit, &["/bin/sh", "-c", "echo x > x.txt"]));
        let output = Arc::new(Output::new(RecordingSink::default()));
        let status = run_along(&payload, &output, free_watches(), own_path);
        let err = err_of(&output);
        assert_eq!(status, 1, "{err:?}");
        assert!(
            err.last()
                .is_some_and(|line| line.starts_with("gate: remote sync-back failed: cannot copy ")
                    && line.ends_with(&format!("{copy} is a link or not a file"))),
            "{err:?}"
        );
        assert_eq!(
            fs::read_to_string(&target).expect("the target is read"),
            "kept",
            "the link's target is untouched"
        );
        assert!(!case.has_back_ref(), "no commit is pushed");
    }

    /// The `TMPDIR` a case's command wrote to `seen`.
    fn tmpdir_seen(seen: &Path) -> String {
        fs::read_to_string(seen).expect("the command wrote its TMPDIR")
    }

    #[test]
    fn a_run_s_tmp_folder_is_made_by_mkdtemp_private_to_the_builder_and_short() {
        let case = Case::new("tmp-made");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let payload = decoded(&case.json(&commit, &json!({})));
        let output = Arc::new(Output::new(RecordingSink::default()));
        let mut ready = prepare(&payload, &output, quiet_periods(), own_path).expect("the run is ready");
        let temp = ready.temp.as_ref().expect("the run made a temp folder");
        let (path, tmpdir) = (temp.path().to_owned(), temp.tmpdir().to_owned());
        tmp_cleared(&mut ready);
        drop(ready);
        let name = path.to_str().expect("the folder's name is UTF-8").to_owned();
        assert!(name.starts_with("/tmp/gate.") && name.len() == "/tmp/gate.".len() + 8, "{name}");
        assert_eq!(tmpdir, format!("{name}/"));
        assert!(tmpdir.len() < 38, "the TMPDIR leaves room for .NET's pipes: {tmpdir}");
        assert!(!path.exists(), "the case removed it");
        assert_eq!(after_checkout(&stderr_of(&output)), Vec::<String>::new());

        let mut again = prepare(&payload, &output, quiet_periods(), own_path).expect("the run is ready again");
        let other = again.temp.as_ref().expect("the run made a temp folder").path().to_owned();
        let meta = fs::symlink_metadata(&other).expect("the folder is there");
        tmp_cleared(&mut again);
        assert_ne!(other, path, "each run makes a folder of its own");
        assert!(meta.is_dir(), "a folder, not a link");
        assert_eq!(meta.mode() & 0o7777, 0o700, "private to its owner");
        assert_eq!(meta.uid(), effective_uid(), "owned by the builder user");
    }

    #[test]
    fn a_run_s_tmp_folder_keeps_setgid_on_a_file_made_in_it() {
        let temp = temp_made().expect("the run's temp folder is made");
        let folder = fs::symlink_metadata(temp.path());
        let file = temp.path().join("setgid");
        let written = fs::write(&file, "")
            .and_then(|()| fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o2700)))
            .and_then(|()| fs::symlink_metadata(&file));
        let removed = fs::remove_dir_all(temp.path());
        removed.expect("the case removes the folder");
        let folder = folder.expect("the folder is there");
        let mode = written.expect("the file is made setgid").mode();
        // SAFETY: `getegid` takes nothing, touches no memory and always succeeds.
        let egid = unsafe { libc::getegid() };
        assert_eq!(
            mode & 0o7777,
            0o2700,
            "chmod kept S_ISGID in the run's TMPDIR (folder group {})",
            folder.gid()
        );
        assert_eq!(folder.gid(), egid, "the folder takes the builder user's own group");
    }

    #[test]
    fn a_run_s_tmp_folder_is_its_command_s_tmpdir_and_gone_after_a_passing_or_a_failing_command() {
        for (name, code) in [("tmp-passing", 0_u8), ("tmp-failing", 3)] {
            let case = Case::new(name);
            let commit = case.snapshot(&[("a.txt", "a\n")]);
            let seen = case.base.with_file_name("seen");
            let script = format!("printf %s \"$TMPDIR\" > '{}'; test -d \"$TMPDIR\" || exit 9; exit {code}", seen.display());
            let (status, said) = whole_run(&case.commanded(&commit, &["/bin/sh", "-c", &script]));
            assert_eq!(status, code, "{said:?}");
            let tmpdir = tmpdir_seen(&seen);
            assert!(tmpdir.starts_with("/tmp/gate.") && tmpdir.ends_with('/'), "{tmpdir}");
            assert!(!Path::new(&tmpdir).exists(), "the folder is gone after the run: {tmpdir}");
            assert!(matches!(take_lock(Path::new(&case.lock())), Ok(Locked::Held(_))), "the lock is let go");
        }
    }

    #[test]
    fn a_caller_s_tmpdir_is_the_command_s_as_given_and_no_tmp_folder_is_made() {
        let case = Case::new("tmp-caller");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let theirs = case.base.with_file_name("theirs");
        fs::create_dir_all(&theirs).expect("the caller's folder is made");
        let given = format!("{}/", theirs.display());
        let seen = case.base.with_file_name("seen");
        let script = format!("printf %s \"$TMPDIR\" > '{}'", seen.display());
        let json = case.json(
            &commit,
            &json!({"Command": ["/bin/sh", "-c", script], "Identity": identity(), "Env": {"TMPDIR": given}}),
        );
        let payload = decoded(&json);
        let output = Arc::new(Output::new(RecordingSink::default()));
        let ready = prepare(&payload, &output, quiet_periods(), own_path).expect("the run is ready");
        assert!(ready.temp.is_none(), "no folder of the box's own");
        drop(ready);
        let (status, said) = whole_run(&json);
        assert_eq!(status, 0, "{said:?}");
        assert_eq!(tmpdir_seen(&seen), given);
        assert!(theirs.is_dir(), "the caller's folder is left");
    }

    #[test]
    fn a_tmp_folder_that_cannot_be_made_refuses_the_run_75_with_the_layout_tag() {
        let case = Case::new("tmp-unmade");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        test_template::set(c"/nonexistent-gate-box-unix-folder/gate.XXXXXXXX");
        let (status, said) = ran(&case.json(&commit, &json!({})));
        assert_eq!(status, REFUSED_STATUS, "{said:?}");
        let said = after_checkout(&said);
        assert_eq!(said.len(), 2, "{said:?}");
        assert_eq!(said.first(), Some(&refused_line(NONCE, Refusal::Layout)), "{said:?}");
        assert!(
            said.last()
                .is_some_and(|line| line.starts_with("gate: remote could not make the run's temp folder in /tmp: ")),
            "{said:?}"
        );
    }

    #[test]
    fn a_caller_s_lower_case_tmpdir_is_another_name_and_the_run_still_makes_its_tmp_folder() {
        let case = Case::new("tmp-lower");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let seen = case.base.with_file_name("seen");
        let script = format!("printf '%s|%s' \"$TMPDIR\" \"$tmpdir\" > '{}'", seen.display());
        let json = case.json(
            &commit,
            &json!({"Command": ["/bin/sh", "-c", script], "Identity": identity(), "Env": {"tmpdir": "/theirs/"}}),
        );
        let (status, said) = whole_run(&json);
        assert_eq!(status, 0, "{said:?}");
        let both = tmpdir_seen(&seen);
        let (tmpdir, theirs) = both.split_once('|').expect("both names were written");
        assert!(tmpdir.starts_with("/tmp/gate.") && tmpdir.ends_with('/'), "{both}");
        assert_eq!(theirs, "/theirs/");
        assert!(!Path::new(tmpdir).exists(), "the folder is gone: {tmpdir}");
    }

    #[test]
    fn a_folder_that_took_the_tmp_folder_s_name_during_the_run_is_left_and_said() {
        let case = Case::new("tmp-replaced");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let seen = case.base.with_file_name("seen");
        let script = format!(
            "printf %s \"$TMPDIR\" > '{}'; rm -rf \"$TMPDIR\"; mkdir \"$TMPDIR\"; echo theirs > \"$TMPDIR/theirs.txt\"",
            seen.display()
        );
        let (status, said) = whole_run(&case.commanded(&commit, &["/bin/sh", "-c", &script]));
        let tmpdir = tmpdir_seen(&seen);
        let folder = Path::new(tmpdir.trim_end_matches('/')).to_owned();
        let survived = fs::read_to_string(folder.join("theirs.txt"));
        fs::remove_dir_all(&folder).expect("the case removes the folder left");
        assert_eq!(status, 0, "{said:?}");
        assert_eq!(survived.expect("the other folder survived"), "theirs\n");
        let line = format!("gate: remote did not remove {}: it is no longer the run's temp folder", folder.display());
        assert!(said.contains(&(Stream::Err, line)), "{said:?}");
    }

    #[test]
    fn a_link_planted_in_the_run_s_tmp_folder_is_removed_and_what_it_names_is_left() {
        let case = Case::new("tmp-links");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let outside = case.base.with_file_name("outside");
        fs::create_dir_all(&outside).expect("the outside folder is made");
        let kept = outside.join("kept.txt");
        fs::write(&kept, "kept").expect("the outside file is written");
        let seen = case.base.with_file_name("seen");
        let script = format!(
            "printf %s \"$TMPDIR\" > '{seen}'; ln -s '{kept}' \"$TMPDIR/file-link\"; ln -s '{outside}' \"$TMPDIR/folder-link\"; \
             mkdir \"$TMPDIR/sub\"; ln -s '{outside}' \"$TMPDIR/sub/deep-link\"; echo own > \"$TMPDIR/sub/own.txt\"",
            seen = seen.display(),
            kept = kept.display(),
            outside = outside.display()
        );
        let (status, said) = whole_run(&case.commanded(&commit, &["/bin/sh", "-c", &script]));
        assert_eq!(status, 0, "{said:?}");
        let tmpdir = tmpdir_seen(&seen);
        assert!(!Path::new(&tmpdir).exists(), "the folder is gone: {tmpdir}");
        assert_eq!(fs::read_to_string(&kept).expect("the outside file is there"), "kept");
        assert_eq!(
            fs::read_dir(&outside).expect("the outside folder is there").count(),
            1,
            "nothing outside was removed"
        );
    }

    fn stderr_of(output: &Output<RecordingSink>) -> Vec<String> {
        let lines = output.sink().said();
        assert!(lines.iter().all(|(stream, _)| *stream == Stream::Err), "{lines:?}");
        lines.into_iter().map(|(_, line)| line).collect()
    }

    /// `said`, stderr lines of a run of [`NONCE`] from its checkout on, past the box's two checkout lines they start with:
    /// its start line, then its done line, whose seconds are a finite number of at least 0.
    fn after_checkout(said: &[String]) -> &[String] {
        let done = format!("{}done ", checkout_prefix(NONCE));
        let finite = |line: &String| {
            line.strip_prefix(done.as_str())
                .and_then(|seconds| seconds.parse::<f64>().ok())
                .is_some_and(|seconds| seconds.is_finite() && seconds >= 0.0)
        };
        let checked_out = matches!(said, [start, ended, ..] if *start == checkout_start_line(NONCE) && finite(ended));
        assert!(checked_out, "the checkout's start and done lines come first: {said:?}");
        said.get(2..).unwrap_or_default()
    }

    /// `said`, stderr lines of a run of [`NONCE`] whose checkout failed, past the box's checkout start line they start
    /// with, which no done line follows.
    fn after_checkout_start(said: &[String]) -> &[String] {
        assert_eq!(
            said.first(),
            Some(&checkout_start_line(NONCE)),
            "the checkout's start line comes first: {said:?}"
        );
        said.get(1..).unwrap_or_default()
    }

    /// The layout refusal of `why`: its tag, then the layout line.
    fn layout_refusal(why: &str) -> [String; 2] {
        [refused_line(NONCE, Refusal::Layout), layout_line("repo", why)]
    }

    #[test]
    fn a_first_run_lays_the_tree_out_checks_the_snapshot_out_and_is_ready_holding_the_lock() {
        let case = Case::new("first");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let payload = decoded(&case.json(&commit, &json!({})));
        let output = Arc::new(Output::new(RecordingSink::default()));

        let mut ready: Ready<'_> = prepare(&payload, &output, quiet_periods(), own_path).expect("the run is ready");
        tmp_cleared(&mut ready);

        assert_eq!(ready.place, PathBuf::from(case.work()));
        assert_eq!(case.head(), commit);
        assert_eq!(fs::read_to_string(format!("{}/a.txt", case.work())).expect("a.txt is checked out"), "a\n");
        let alternates = fs::read_to_string(format!("{}/.git/objects/info/alternates", case.work())).expect("the alternates are written");
        assert_eq!(alternates, format!("{}/objects\n", case.bare()));
        let config = fs::read_to_string(format!("{}/.git/config", case.work())).expect("the tree's config is written");
        assert!(config.contains(&format!("url = {}", case.bare())), "{config}");
        assert!(config.contains("autocrlf = false") && config.contains("eol = lf"), "{config}");
        assert!(matches!(take_lock(Path::new(&case.lock())), Ok(Locked::Busy)), "the lock is held");
        assert!(
            !after_checkout(&stderr_of(&output)).iter().any(|line| line.starts_with("gate")),
            "no gate line: {:?}",
            output.sink().said()
        );
        drop(ready);
        assert!(matches!(take_lock(Path::new(&case.lock())), Ok(Locked::Held(_))), "let go once dropped");
    }

    #[test]
    fn a_run_on_a_held_lock_is_refused_busy_its_tag_just_before_the_busy_line_and_nothing_else() {
        let case = Case::new("busy");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let held = take_lock(Path::new(&case.lock())).expect("the lock opens");
        assert!(matches!(held, Locked::Held(_)));

        let (status, said) = ran(&case.json(&commit, &json!({})));

        assert_eq!(status, REFUSED_STATUS);
        assert_eq!(said, [refused_line(NONCE, Refusal::Busy), busy_line(KEY)]);
        assert!(!Path::new(&case.work()).exists(), "nothing is laid out or checked out");
    }

    #[test]
    fn a_lock_that_cannot_be_opened_fails_the_run_with_its_line() {
        let case = Case::new("lock-folder");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        fs::create_dir(case.lock()).expect("a folder takes the lock's name");

        let (status, said) = ran(&case.json(&commit, &json!({})));

        assert_eq!(status, 1);
        let start = format!("gate: remote could not open the work folder lock {}: ", case.lock());
        assert!(said.len() == 1 && said[0].starts_with(&start), "{said:?}");
    }

    #[test]
    fn a_bare_repository_outside_the_root_is_a_usage_error_before_the_lock() {
        let case = Case::new("bare-outside");
        let bare = format!("{}/other/repo.git", case.root());

        let (status, said) = ran(&case.json(&"a".repeat(40), &json!({ "Bare": bare })));

        assert_eq!(status, 2);
        assert_eq!(
            said,
            [format!("gate: box payload Bare must be <root>/<name>.git to lock its root down: {bare}")]
        );
        assert!(!Path::new(&case.lock()).exists(), "the lock is not taken");
    }

    #[test]
    fn a_link_at_the_bare_repository_fails_the_run_before_the_lock_and_its_target_is_untouched() {
        let case = Case::new("bare-link");
        let target = format!("{}/elsewhere.git", case.root());
        fs::rename(case.bare(), &target).expect("the bare repository is moved aside");
        symlink(&target, case.bare()).expect("the link is made");

        let (status, said) = ran(&case.json(&"a".repeat(40), &json!({})));

        assert_eq!(status, 1);
        assert_eq!(
            said,
            [format!("gate: remote could not create the folder {}: it is a symbolic link", case.bare())]
        );
        assert!(!Path::new(&case.lock()).exists(), "the lock is not taken");
    }

    #[test]
    fn a_lock_file_a_cut_short_git_left_is_removed_and_a_folder_there_is_refused_layout() {
        let case = Case::new("swept");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        case.laid_out(&commit);
        let index_lock = format!("{}/.git/index.lock", case.work());
        fs::write(&index_lock, "").expect("a stale index lock is planted");
        let back_lock = format!("{}/refs/gate-back/{KEY}.lock", case.bare());
        fs::create_dir_all(Path::new(&back_lock).parent().expect("a lock has a folder")).expect("refs/gate-back is made");
        fs::write(&back_lock, "").expect("a stale back ref lock is planted");

        let (status, said) = ran(&case.json(&commit, &json!({})));

        assert_eq!(status, 0, "{said:?}");
        assert!(
            !Path::new(&index_lock).exists() && !Path::new(&back_lock).exists(),
            "the stale locks are removed"
        );

        let head_lock = format!("{}/.git/HEAD.lock", case.work());
        fs::create_dir(&head_lock).expect("a folder takes the lock's name");
        let (status, said) = ran(&case.json(&commit, &json!({})));
        assert_eq!(status, REFUSED_STATUS, "{said:?}");
        assert_eq!(said, layout_refusal(&format!("{head_lock} is a folder where git's lock file goes")));
    }

    #[test]
    fn a_subfolder_outside_the_work_folder_is_refused_and_the_back_ref_deleted() {
        let case = Case::new("subfolder-outside");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        for sub in ["/x", "../k-sibling", "sub/../../x", "..", "../../x"] {
            case.back_ref_set(&commit);

            let (status, said) = ran(&case.json(&commit, &json!({ "Subfolder": sub })));

            assert_eq!(status, 2, "{sub}: {said:?}");
            assert_eq!(said, [format!("gate: remote subfolder {sub} is not a folder inside the work folder")]);
            assert!(!case.has_back_ref(), "{sub}: the back ref is deleted");
            assert!(!Path::new(&case.work()).exists(), "{sub}: nothing is laid out or checked out");
        }
    }

    #[test]
    fn a_subfolder_the_snapshot_links_out_of_the_work_folder_is_refused_after_the_checkout() {
        let case = Case::new("subfolder-link");
        case.snapshot(&[("a.txt", "a\n")]);
        symlink("../..", case.source().join("sub")).expect("the link is made in the snapshot");
        let commit = case.snapshot(&[]);

        let (status, said) = ran(&case.json(&commit, &json!({ "Subfolder": "sub" })));

        assert_eq!(status, 2, "{said:?}");
        assert_eq!(
            after_checkout(&said),
            ["gate: remote subfolder sub is not a folder inside the work folder"]
        );
    }

    #[test]
    fn a_subfolder_is_placed_below_the_work_folder_its_dots_folded() {
        let work = "/r/work/k";
        assert_eq!(place_of(work, ""), Some(PathBuf::from(work)));
        assert_eq!(place_of(work, "sub"), Some(PathBuf::from("/r/work/k/sub")));
        assert_eq!(place_of(work, "sub/../x/./y"), Some(PathBuf::from("/r/work/k/x/y")));
        assert_eq!(place_of(work, "sub/.."), Some(PathBuf::from(work)));
        assert_eq!(place_of(work, "../k/x"), Some(PathBuf::from("/r/work/k/x")), "it climbs out and back in");
        assert_eq!(place_of(work, "../k-sibling"), None);
        assert_eq!(place_of(work, "/r/work/k/x"), None, "a rooted subfolder");
    }

    #[test]
    fn a_work_folder_ending_in_a_separator_is_a_usage_error() {
        let case = Case::new("work-separator");
        let work = format!("{}/", case.work());

        let (status, said) = ran(&case.json(&"a".repeat(40), &json!({ "Work": work })));

        assert_eq!(status, 2);
        assert_eq!(said, [format!("gate: box payload Work must not end with a separator: {work}")]);
    }

    #[test]
    fn an_index_or_a_lock_other_than_the_work_folders_is_a_usage_error() {
        let case = Case::new("index-lock");
        let precious = format!("{}/precious.txt", case.root());
        fs::write(&precious, "keep").expect("the precious file is written");

        let (status, said) = ran(&case.json(&"a".repeat(40), &json!({ "Index": precious })));
        assert_eq!(status, 2);
        assert_eq!(said, [format!("gate: box payload Index must be {}.index", case.work())]);

        let (status, said) = ran(&case.json(&"a".repeat(40), &json!({ "Lock": precious })));
        assert_eq!(status, 2);
        assert_eq!(said, [format!("gate: box payload Lock must be {}.lock", case.work())]);
        assert_eq!(fs::read_to_string(&precious).expect("the precious file is read"), "keep");
    }

    #[test]
    fn a_work_root_holding_a_colon_is_a_usage_error_before_the_lock() {
        let case = Case::rooted("colon-root", "gate:remote");

        let (status, said) = ran(&case.json(&"a".repeat(40), &json!({})));

        assert_eq!(status, 2);
        assert_eq!(
            said,
            [format!(
                "gate: box payload Work must be <root>/work/<key> with no ':' in <root>, which GIT_CEILING_DIRECTORIES cannot hold: {}",
                case.work()
            )]
        );
        assert!(!Path::new(&case.lock()).exists(), "the lock is not taken");
    }

    #[test]
    fn a_failed_fetch_exits_with_git_s_status() {
        let case = Case::new("fetch-failed");

        let (status, said) = ran(&case.json(&"0".repeat(40), &json!({})));

        assert_eq!(status, 128, "{said:?}");
        assert_eq!(
            said.last().map(String::as_str),
            Some("gate: remote fetch failed (status 128)"),
            "{said:?}"
        );
    }

    #[test]
    fn a_subfolder_the_snapshot_lacks_is_a_usage_error() {
        let case = Case::new("subfolder-missing");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        case.laid_out(&commit);

        let (status, said) = ran(&case.json(&commit, &json!({ "Subfolder": "nope" })));

        assert_eq!(status, 2);
        assert_eq!(after_checkout(&said), ["gate: remote subfolder nope does not exist in the snapshot"]);
    }

    #[test]
    fn the_checkout_keeps_ignored_files_removes_untracked_ones_and_deletes_the_back_ref() {
        let case = Case::new("checkout");
        let first = case.snapshot(&[("a.txt", "a\n"), (".gitignore", "ignored.txt\n")]);
        case.back_ref_set(&first);
        case.laid_out(&first);
        assert_eq!(case.head(), first);
        assert!(!case.has_back_ref(), "the earlier run's back ref is deleted");

        let work = PathBuf::from(case.work());
        fs::write(work.join("ignored.txt"), "build output").expect("an ignored file is written");
        fs::write(work.join("untracked.txt"), "stray").expect("an untracked file is written");
        let second = case.snapshot(&[("a.txt", "b\n")]);

        let (status, said) = ran(&case.json(&second, &json!({})));

        assert_eq!(status, 0, "{said:?}");
        assert_eq!(after_checkout(&said), Vec::<String>::new());
        assert_eq!(case.head(), second);
        assert_eq!(fs::read_to_string(work.join("a.txt")).expect("a.txt is checked out"), "b\n");
        assert!(work.join("ignored.txt").exists(), "an ignored file survives");
        assert!(!work.join("untracked.txt").exists(), "an untracked file is removed");
    }

    #[test]
    fn a_snapshot_that_is_not_40_hex_is_a_usage_error_before_the_lock() {
        let case = Case::new("snapshot-refused");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let held = take_lock(Path::new(&case.lock())).expect("the lock opens");
        assert!(matches!(held, Locked::Held(_)));
        let mut absent = case.json(&commit, &json!({}));
        absent.as_object_mut().expect("the payload is an object").remove("Snapshot");
        let (short, upper) = (commit[..39].to_owned(), commit.to_uppercase());

        for (what, json, quoted) in [
            ("a missing key", absent, ""),
            ("a 39-character value", case.json(&short, &json!({})), short.as_str()),
            ("an upper-case sha", case.json(&upper, &json!({})), upper.as_str()),
            ("a non-string value", case.json("", &json!({ "Snapshot": 3 })), "3"),
        ] {
            let (status, said) = ran(&json);

            assert_eq!(status, 2, "{what}: refused before the lock, never busy");
            assert_eq!(
                said,
                [format!("gate: remote run payload's Snapshot '{quoted}' is not a 40-hex commit sha")],
                "{what}"
            );
        }
    }

    #[test]
    fn the_payload_s_snapshot_is_checked_out_not_the_ref_s_tip() {
        let case = Case::new("not-ref-tip");
        let a = case.snapshot(&[("a.txt", "a\n")]);
        let b = case.snapshot(&[("a.txt", "b\n")]);
        assert_ne!(a, b);

        let (status, said) = ran(&case.json(&a, &json!({})));

        assert_eq!(status, 0, "{said:?}");
        assert_eq!(case.head(), a, "the payload's commit is what is checked out");
        assert_eq!(fs::read_to_string(format!("{}/a.txt", case.work())).expect("a.txt is read"), "a\n");
    }

    #[test]
    fn a_snapshot_missing_after_the_fetch_fails_with_status_1_and_leaves_the_tree_and_the_back_ref() {
        let case = Case::new("snapshot-missing");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        case.laid_out(&commit);
        let untracked = format!("{}/untracked.txt", case.work());
        fs::write(&untracked, "stray").expect("an untracked file is written");
        case.back_ref_set(&commit);
        let absent = "0".repeat(40);

        let (status, said) = ran(&case.json(&absent, &json!({})));

        assert_eq!(status, 1);
        assert_eq!(
            after_checkout_start(&said),
            [format!(
                "gate: remote snapshot {absent} is not in the work folder's repository after the fetch"
            )]
        );
        assert_eq!(case.head(), commit, "HEAD stands");
        assert!(Path::new(&untracked).exists(), "the untracked file stands");
        assert!(case.has_back_ref(), "the back ref stands");
    }

    #[test]
    fn an_identity_name_outside_the_git_identity_is_a_usage_error() {
        let case = Case::new("identity");
        let identity = json!({ "Identity": { "GIT_AUTHOR_NAME": "a", "GIT_CONFIG_COUNT": "1" } });

        let (status, said) = ran(&case.json(&"a".repeat(40), &identity));

        assert_eq!(status, 2);
        assert_eq!(
            said,
            ["gate: box payload Identity may set only GIT_AUTHOR_* and GIT_COMMITTER_* names: GIT_CONFIG_COUNT"]
        );
        assert!(!Path::new(&case.lock()).exists(), "the lock is not taken");
    }

    #[test]
    fn a_malformed_env_is_a_usage_error() {
        let case = Case::new("env");
        for (env, line) in [
            (json!("A=b"), "gate: box payload Env is not an object of texts"),
            (json!({"A": 1}), "gate: box payload Env is not an object of texts"),
            (json!({"ZZ_A_X": "a", "zz_a_x": "b"}), "gate: box payload Env names zz_a_x twice"),
            (
                json!({"MSBUILDUSESERVER": "1"}),
                "gate: box payload Env sets MSBUILDUSESERVER, which the gate sets itself",
            ),
        ] {
            let (status, said) = ran(&case.json(&"a".repeat(40), &json!({ "Env": env })));

            assert_eq!(status, 2, "{line}");
            assert_eq!(said, [line]);
        }
    }

    /// Checks the run of `case` with `extra` set is refused for `refusal` with `line` after its tag, before the lock,
    /// and nothing is laid out.
    fn assert_refused(case: &Case, extra: &Value, refusal: Refusal, line: &str) {
        let (status, said) = ran(&case.json(&"a".repeat(40), extra));
        assert_eq!(status, REFUSED_STATUS, "{said:?}");
        assert_eq!(said, [refused_line(NONCE, refusal), line.to_owned()]);
        assert!(!Path::new(&case.lock()).exists(), "the lock is not taken");
        assert!(!Path::new(&case.work()).exists(), "nothing is laid out");
    }

    /// The layout line of this builder refusing what `key` asks for.
    fn unbuilt_layout(key: &str) -> String {
        layout_line("repo", &format!("{key} is not built on this builder"))
    }

    /// The keys of a pair of the leaf `repo` and the sibling `lib`, whose snapshot is `lib_snapshot`.
    fn pair_keys(lib_snapshot: &str) -> Value {
        json!({ "Pair": { "Leaf": "repo", "Siblings": ["lib"] }, "SiblingSnapshots": { "lib": lib_snapshot } })
    }

    #[test]
    fn a_pair_request_reads_its_repos_the_main_one_first() {
        let case = Case::new("pair-read");
        let (main, lib) = ("a".repeat(40), "b".repeat(40));
        let payload = decoded(&case.json(&main, &pair_keys(&lib)));
        let request = Request::read(&payload).expect("the pair request reads");
        let repos: Vec<(Option<String>, String, String, String)> = request
            .repos()
            .map(|repo| (repo.name.clone(), repo.tree.clone(), repo.bare.clone(), repo.snapshot.to_owned()))
            .collect();
        let (work, root) = (case.work(), case.root());
        assert_eq!(
            repos,
            [
                (Some("repo".to_owned()), format!("{work}/repo"), case.bare(), main),
                (Some("lib".to_owned()), format!("{work}/lib"), format!("{root}/lib.git"), lib),
            ]
        );
        assert_eq!(request.tree(), format!("{work}/repo"), "the main tree is the run's tree");
    }

    impl Case {
        /// The sibling `lib`'s bare repository, beside the main one, made when it is missing.
        fn lib_bare(&self) -> String {
            let bare = format!("{}/lib.git", self.root());
            if !Path::new(&bare).exists() {
                git(&["init", "--bare", "-q", &bare]);
            }
            bare
        }

        /// A commit of the sibling `lib` holding `files`, pushed to its bare repository as `refs/gate/<key>`; its sha.
        fn lib_snapshot(&self, files: &[(&str, &str)]) -> String {
            snapshot_pushed(&self.base.with_file_name("lib-source"), &self.lib_bare(), files)
        }

        /// The run payload of a pair of this case's main repo at `main` and the sibling `lib` at `lib`, with `extra`
        /// keys set over it.
        fn pair_json(&self, main: &str, lib: &str, extra: &Value) -> Value {
            let mut json = self.json(main, &pair_keys(lib));
            if let (Some(object), Some(extra)) = (json.as_object_mut(), extra.as_object()) {
                object.extend(extra.clone());
            }
            json
        }

        /// The tree of the pair's repo `name`.
        fn tree(&self, name: &str) -> String {
            format!("{}/{name}", self.work())
        }

        /// The commit the tree `tree` has checked out.
        fn head_of(tree: &str) -> String {
            git(&["-C", tree, "rev-parse", "HEAD"])
        }

        /// Whether the bare repository `bare` holds this case's back ref.
        fn has_back_ref_in(bare: &str) -> bool {
            Case::back_ref_in(bare).is_some()
        }

        /// The names in the work folder, sorted.
        fn work_names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(self.work())
                .expect("the work folder is read")
                .map(|entry| entry.expect("an entry is read").file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }

        fn layout_record(&self) -> String {
            fs::read_to_string(format!("{}.layout", self.work())).expect("the layout record is read")
        }
    }

    #[test]
    fn a_pair_run_checks_out_every_tree_at_its_snapshot_and_is_placed_in_the_main_tree() {
        let case = Case::new("pair-run");
        let main = case.snapshot(&[("a.txt", "a\n")]);
        let lib = case.lib_snapshot(&[("b.txt", "b\n")]);
        let payload = decoded(&case.pair_json(&main, &lib, &json!({})));
        let output = Arc::new(Output::new(RecordingSink::default()));
        let ready = prepare(&payload, &output, quiet_periods(), own_path);
        assert!(ready.is_ok(), "{:?}", stderr_of(&output));
        let ready = ready.expect("the run is ready");
        assert_eq!(ready.place, PathBuf::from(case.tree("repo")), "the place is the main tree");
        assert_eq!(Case::head_of(&case.tree("repo")), main);
        assert_eq!(Case::head_of(&case.tree("lib")), lib);
        assert!(
            Path::new(&case.tree("lib")).join("b.txt").is_file(),
            "the sibling sits beside the main tree"
        );
        assert_eq!(case.work_names(), ["lib", "repo"]);
        assert_eq!(case.layout_record(), "pair repo lib");
        let alternates = fs::read_to_string(format!("{}/.git/objects/info/alternates", case.tree("lib"))).expect("the alternates are read");
        assert_eq!(
            alternates,
            format!("{}/objects\n", case.lib_bare()),
            "each tree borrows its own bare's objects"
        );
    }

    #[test]
    fn a_pair_run_sweeps_every_tree_s_stale_locks_and_clears_every_repo_s_back_ref() {
        let case = Case::new("pair-swept");
        let main = case.snapshot(&[("a.txt", "a\n")]);
        let lib = case.lib_snapshot(&[("b.txt", "b\n")]);
        let json = case.pair_json(&main, &lib, &json!({}));
        let (status, said) = ran(&json);
        assert_eq!(status, 0, "{said:?}");
        let mut locks = Vec::new();
        for (tree, bare) in [(case.tree("repo"), case.bare()), (case.tree("lib"), case.lib_bare())] {
            for lock in ["index.lock", "HEAD.lock", "config.lock"] {
                locks.push(format!("{tree}/.git/{lock}"));
            }
            fs::create_dir_all(format!("{bare}/refs/gate-back")).expect("the back refs' folder is made");
            locks.push(format!("{bare}/refs/gate-back/{KEY}.lock"));
            git(&[
                &format!("--git-dir={bare}"),
                "update-ref",
                &format!("refs/gate-back/{KEY}"),
                if bare == case.bare() { &main } else { &lib },
            ]);
        }
        for lock in &locks {
            fs::write(lock, "").expect("a stale lock file is written");
        }

        let (status, said) = ran(&json);
        assert_eq!(status, 0, "{said:?}");
        for lock in &locks {
            assert!(!Path::new(lock).exists(), "{lock} is swept");
        }
        for bare in [case.bare(), case.lib_bare()] {
            assert!(!Case::has_back_ref_in(&bare), "{bare}'s back ref is cleared");
        }
    }

    #[test]
    fn a_layout_change_clears_the_whole_work_folder_both_ways() {
        let case = Case::new("pair-relaid");
        let main = case.snapshot(&[("a.txt", "a\n")]);
        let lib = case.lib_snapshot(&[("b.txt", "b\n")]);
        case.laid_out(&main);
        fs::write(format!("{}/left.txt", case.work()), "left\n").expect("a file the single run left is written");
        fs::write(format!("{}.warm", case.work()), "a".repeat(64)).expect("a warm record is written");

        let (status, said) = ran(&case.pair_json(&main, &lib, &json!({})));
        assert_eq!(status, 0, "{said:?}");
        assert_eq!(case.work_names(), ["lib", "repo"], "the single tree is cleared");
        assert_eq!(case.layout_record(), "pair repo lib");
        assert!(
            !Path::new(&format!("{}.warm", case.work())).exists(),
            "the warm record goes with the clear"
        );

        case.laid_out(&main);
        assert_eq!(case.work_names(), [".git", "a.txt"], "the pair's trees are cleared");
        assert_eq!(case.layout_record(), "single");
        assert_eq!(case.head(), main);
    }

    #[test]
    fn a_layout_change_removes_a_link_in_the_work_folder_and_keeps_the_folder_it_leads_to() {
        let case = Case::new("relaid-link");
        let main = case.snapshot(&[("a.txt", "a\n")]);
        let lib = case.lib_snapshot(&[("b.txt", "b\n")]);
        case.laid_out(&main);
        let outside = case.base.with_file_name("outside");
        fs::create_dir_all(&outside).expect("the outside folder is made");
        fs::write(outside.join("kept.txt"), "kept\n").expect("the outside file is written");
        symlink(&outside, format!("{}/out", case.work())).expect("the link in the work folder is made");

        let (status, said) = ran(&case.pair_json(&main, &lib, &json!({})));
        assert_eq!(status, 0, "{said:?}");
        assert_eq!(case.work_names(), ["lib", "repo"], "the link is cleared with the single tree");
        assert_eq!(
            fs::read(outside.join("kept.txt")).expect("the outside file is read"),
            b"kept\n",
            "the folder the link led to keeps its file"
        );
    }

    /// Why `cleared` refused the clear; the case fails unless it was refused.
    #[expect(clippy::panic, reason = "a clear that was not refused must fail the case loudly")]
    fn refusal_of(cleared: Result<(), NotLaidOut>) -> String {
        match cleared {
            Err(NotLaidOut::Refused(why)) => why,
            Ok(()) => panic!("the clear went through"),
            Err(NotLaidOut::Busy | NotLaidOut::Failed(_)) => panic!("the clear failed other than refused"),
        }
    }

    #[test]
    fn a_work_folder_that_is_a_link_is_not_cleared_and_the_folder_it_leads_to_kept() {
        let case = Case::new("clear-link");
        let outside = case.base.with_file_name("outside");
        fs::create_dir_all(&outside).expect("the outside folder is made");
        fs::write(outside.join("kept.txt"), "kept\n").expect("the outside file is written");
        symlink(&outside, case.work()).expect("the link at the work folder is made");

        let why = refusal_of(work_cleared(Path::new(&case.work())));
        assert!(why.starts_with("cannot list "), "{why}");
        assert_eq!(fs::read(outside.join("kept.txt")).expect("the outside file is read"), b"kept\n");
    }

    /// Runs `hdiutil` with `args`; whether it exited 0, and what it said: its exit status, standard output and error.
    #[cfg(target_os = "macos")]
    #[expect(clippy::disallowed_methods, reason = "the spawn lock is the Windows gate's; this sets a case's volume up")]
    fn hdiutil(args: &[&str]) -> (bool, String) {
        let ran = Command::new("hdiutil").args(args).stdin(Stdio::null()).output().expect("hdiutil starts");
        let said = format!(
            "hdiutil {args:?} {}; stdout {:?}; stderr {:?}",
            ran.status,
            String::from_utf8_lossy(&ran.stdout),
            String::from_utf8_lossy(&ran.stderr)
        );
        (ran.status.success(), said)
    }

    /// A volume attached at `mount` from the disk image `image`; detached and its image removed when the case ends.
    #[cfg(target_os = "macos")]
    struct Attached {
        mount: String,
        image: String,
    }

    #[cfg(target_os = "macos")]
    impl Drop for Attached {
        fn drop(&mut self) {
            // Best effort: a volume left attached shows in `hdiutil info`, and an image left behind sits in this
            // process's case folder, both named for this process's case.
            let _ = hdiutil(&["detach", "-force", &self.mount]);
            let _ = fs::remove_file(&self.image);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_clear_refuses_a_volume_mounted_in_the_work_folder_and_keeps_its_files() {
        // The mount point in the work folder, and the work folder's child the refusal names.
        for (at, child) in [("mnt", "mnt"), ("a/b/mnt", "a")] {
            let case = Case::new(&format!("clear-mount-{child}"));
            let work = case.work();
            let mount = format!("{work}/{at}");
            fs::create_dir_all(&mount).expect("the mount point is made");
            let image = case.base.with_file_name("volume.dmg");
            let image = image.to_str().expect("the temp folder is UTF-8");
            let volume = format!("gate-clear-{}-{child}", std::process::id());
            let (made, said) = hdiutil(&["create", "-quiet", "-size", "1m", "-fs", "HFS+", "-volname", &volume, image]);
            assert!(made, "the image is made: {said}");
            let (attached, said) = hdiutil(&["attach", "-quiet", "-nobrowse", "-mountpoint", &mount, image]);
            assert!(attached, "the image is attached: {said}");
            let _attached = Attached {
                mount: mount.clone(),
                image: image.to_owned(),
            };
            fs::write(format!("{mount}/kept.txt"), "kept\n").expect("a file on the volume is written");

            let why = refusal_of(work_cleared(Path::new(&work)));
            assert_eq!(why, format!("cannot remove {work}/{child}: {ANOTHER_VOLUME}"), "{at}; attached by {said}");
            assert_eq!(
                fs::read(format!("{mount}/kept.txt")).expect("the volume's file is read"),
                b"kept\n",
                "{at}; attached by {said}"
            );
        }
    }

    #[test]
    fn a_layout_change_removes_the_inputs_record() {
        let case = Case::new("relaid-inputs");
        let main = case.snapshot(&[("a.txt", "a\n")]);
        let lib = case.lib_snapshot(&[("b.txt", "b\n")]);
        case.laid_out(&main);
        let inputs = format!("{}.inputs", case.work());
        fs::write(&inputs, "a record of the single tree's inputs\n").expect("the inputs record is written");

        let (status, said) = ran(&case.pair_json(&main, &lib, &json!({})));
        assert_eq!(status, 0, "{said:?}");
        assert!(!Path::new(&inputs).exists(), "the inputs record goes with the clear");
        assert_eq!(case.layout_record(), "pair repo lib");
    }

    #[test]
    fn a_pair_run_s_failing_checkout_step_names_its_repo() {
        let case = Case::new("pair-unfetched");
        let main = case.snapshot(&[("a.txt", "a\n")]);
        case.lib_bare();
        let (status, said) = ran(&case.pair_json(&main, &"b".repeat(40), &json!({})));
        assert_ne!(status, 0, "{said:?}");
        let mut from_last = said.iter().rev();
        let (last, fetch) = (from_last.next().expect("a line is said"), from_last.next().expect("two lines are said"));
        assert!(fetch.starts_with("gate: remote fetch lib failed"), "{said:?}");
        assert_eq!(
            last,
            crate::lines::BACK_COMMITS_FAILED,
            "a pair run ending before its command says so last"
        );
        assert_eq!(back_commits_said(said.iter().map(String::as_str)), 1, "{said:?}");
    }

    /// How many of `lines` are back commits lines.
    fn back_commits_said<'a>(lines: impl IntoIterator<Item = &'a str>) -> usize {
        lines.into_iter().filter(|line| line.starts_with("gate: remote back commits:")).count()
    }

    #[test]
    fn a_pair_run_whose_lock_cannot_be_opened_says_its_back_commits_failed() {
        let case = Case::new("pair-lock-folder");
        fs::create_dir_all(case.lock()).expect("a folder takes the lock's name");
        let (status, said) = ran(&case.pair_json(&"a".repeat(40), &"b".repeat(40), &json!({})));
        assert_eq!(status, 1, "{said:?}");
        assert!(
            said.first()
                .is_some_and(|line| line.starts_with("gate: remote could not open the work folder lock ")),
            "{said:?}"
        );
        assert_eq!(said.last().map(String::as_str), Some(crate::lines::BACK_COMMITS_FAILED), "{said:?}");
        assert_eq!(back_commits_said(said.iter().map(String::as_str)), 1, "{said:?}");
    }

    #[test]
    fn a_pair_run_whose_sibling_bare_is_unusable_says_its_back_commits_failed() {
        let case = Case::new("pair-bare-file");
        fs::create_dir_all(case.root()).expect("the root is made");
        fs::write(format!("{}/lib.git", case.root()), "kept\n").expect("a file takes the sibling's bare repository's name");
        let (status, said) = ran(&case.pair_json(&"a".repeat(40), &"b".repeat(40), &json!({})));
        assert_eq!(status, 1, "{said:?}");
        assert_eq!(said.len(), 2, "the bare's line, then the failed line: {said:?}");
        assert_eq!(said.last().map(String::as_str), Some(crate::lines::BACK_COMMITS_FAILED), "{said:?}");
    }

    #[test]
    fn a_pair_run_refused_busy_says_no_back_commits_line() {
        let case = Case::new("pair-busy");
        fs::create_dir_all(Path::new(&case.work()).parent().expect("the work folder has a parent")).expect("the parent is made");
        let held = take_lock(Path::new(&case.lock())).expect("the lock opens");
        assert!(matches!(held, Locked::Held(_)), "the case takes the lock");
        let (status, said) = ran(&case.pair_json(&"a".repeat(40), &"b".repeat(40), &json!({})));
        assert_eq!(status, REFUSED_STATUS, "{said:?}");
        assert_eq!(back_commits_said(said.iter().map(String::as_str)), 0, "{said:?}");
    }

    #[test]
    fn a_pair_run_whose_git_step_exits_75_says_its_back_commits_failed() {
        let case = Case::new("pair-git-75");
        let main = case.snapshot(&[("a.txt", "a\n")]);
        let lib = case.lib_snapshot(&[("b.txt", "b\n")]);
        let shim = case.base.with_file_name("git-shim-75");
        // Every fetch exits 75, the status a refusal ends on, before any back commit is made.
        let script = format!(
            "#!/bin/sh\ncase \" $* \" in\n*\" fetch \"*) echo 'fatal: fetch stopped' >&2; exit 75;;\nesac\nexec '{}' \"$@\"\n",
            real_git().display()
        );
        fs::write(&shim, script).expect("the shim is written");
        fs::set_permissions(&shim, std::os::unix::fs::PermissionsExt::from_mode(0o700)).expect("the shim is made runnable");
        test_program::set(&shim);
        let (status, said) = ran(&case.pair_json(&main, &lib, &json!({})));
        assert_eq!(status, REFUSED_STATUS, "the git step's status ends the run: {said:?}");
        assert!(said.iter().any(|line| line.contains("(status 75)")), "{said:?}");
        assert_eq!(said.last().map(String::as_str), Some(crate::lines::BACK_COMMITS_FAILED), "{said:?}");
        assert_eq!(back_commits_said(said.iter().map(String::as_str)), 1, "{said:?}");
    }

    #[test]
    fn a_pair_run_whose_subfolder_leaves_the_main_tree_clears_every_back_ref_and_says_its_back_commits_failed() {
        let case = Case::new("pair-subfolder-out");
        let main = case.snapshot(&[("a.txt", "a\n")]);
        let lib = case.lib_snapshot(&[("b.txt", "b\n")]);
        for (bare, commit) in [(case.bare(), &main), (case.lib_bare(), &lib)] {
            git(&[&format!("--git-dir={bare}"), "update-ref", &format!("refs/gate-back/{KEY}"), commit]);
        }
        let (status, said) = ran(&case.pair_json(&main, &lib, &json!({"Subfolder": "../lib"})));
        assert_eq!(status, crate::lines::USAGE_STATUS, "{said:?}");
        assert_eq!(
            said,
            [
                "gate: remote subfolder ../lib is not a folder inside the work folder".to_owned(),
                crate::lines::BACK_COMMITS_FAILED.to_owned()
            ]
        );
        for bare in [case.bare(), case.lib_bare()] {
            assert!(!Case::has_back_ref_in(&bare), "{bare}'s back ref is cleared");
        }
    }

    #[test]
    fn a_pair_run_whose_command_the_box_killed_says_its_back_commits_failed() {
        let case = Case::new("pair-killed");
        let main = case.snapshot(&[("a.txt", "a\n")]);
        let lib = case.lib_snapshot(&[("b.txt", "b\n")]);
        let extra = json!({"Command": ["/bin/sh", "-c", "echo x > x.txt; sleep 600"], "Identity": identity(), "Timeout": 1});
        let (status, said) = whole_run(&case.pair_json(&main, &lib, &extra));
        assert_eq!(status, 124, "{said:?}");
        let err: Vec<&str> = said
            .iter()
            .filter(|(stream, _)| *stream == Stream::Err)
            .map(|(_, line)| line.as_str())
            .collect();
        assert_eq!(err.last().copied(), Some(crate::lines::BACK_COMMITS_FAILED), "{said:?}");
        assert_eq!(back_commits_said(err.iter().copied()), 1, "{said:?}");
        assert!(!Case::has_back_ref_in(&case.bare()), "no back ref in the main repo");
        assert!(!Case::has_back_ref_in(&case.lib_bare()), "no back ref in the sibling");
    }

    impl Case {
        /// The commit of this case's back ref in the bare repository `bare`, `None` when there is none.
        fn back_ref_in(bare: &str) -> Option<String> {
            let back = format!("refs/gate-back/{KEY}");
            let (said, found) = git_said(&[&format!("--git-dir={bare}"), "rev-parse", "--verify", "-q", &back]);
            found.then_some(said)
        }

        /// The whole pair run of this case's main repo holding `a.txt` and the sibling `lib` holding `b.txt`, running
        /// `script` in the main tree; the main snapshot, the sibling's, the status and the lines said on stderr.
        fn pair_ran(&self, script: &str) -> (String, String, u8, Vec<String>) {
            let main = self.snapshot(&[("a.txt", "a\n")]);
            let lib = self.lib_snapshot(&[("b.txt", "b\n")]);
            let extra = json!({"Command": ["/bin/sh", "-c", script], "Identity": identity()});
            let (status, said) = whole_run(&self.pair_json(&main, &lib, &extra));
            let err = said
                .into_iter()
                .filter(|(stream, _)| *stream == Stream::Err)
                .map(|(_, line)| line)
                .collect();
            (main, lib, status, err)
        }
    }

    #[test]
    fn a_pair_run_changing_both_trees_pushes_both_back_refs_and_names_them() {
        let case = Case::new("pair-back-both");
        let (main, lib, status, err) = case.pair_ran("echo c > c.txt; echo d > ../lib/d.txt");
        assert_eq!(status, 0, "{err:?}");
        let main_back = Case::back_ref_in(&case.bare()).expect("the main repo's back ref is pushed");
        let lib_back = Case::back_ref_in(&case.lib_bare()).expect("the sibling's back ref is pushed");
        let parent = |bare: &str, commit: &str| git(&[&format!("--git-dir={bare}"), "show", "-s", "--format=%P", commit]);
        assert_eq!(parent(&case.bare(), &main_back), main, "the main back commit is on its snapshot");
        assert_eq!(parent(&case.lib_bare(), &lib_back), lib, "the sibling's back commit is on its snapshot");
        assert_eq!(
            git(&[&format!("--git-dir={}", case.lib_bare()), "show", &format!("{lib_back}:d.txt")]),
            "d"
        );
        assert_eq!(
            err.last(),
            Some(&format!("gate: remote back commits: repo {main_back}, lib {lib_back}")),
            "{err:?}"
        );
        assert_eq!(back_commits_said(err.iter().map(String::as_str)), 1, "{err:?}");
    }

    #[test]
    fn a_pair_run_changing_only_the_sibling_names_the_main_repo_none() {
        let case = Case::new("pair-back-sibling");
        let (_, _, status, err) = case.pair_ran("echo d > ../lib/d.txt");
        assert_eq!(status, 0, "{err:?}");
        assert!(!Case::has_back_ref_in(&case.bare()), "no back ref in the unchanged main repo");
        let lib_back = Case::back_ref_in(&case.lib_bare()).expect("the sibling's back ref is pushed");
        assert_eq!(
            err.last(),
            Some(&format!("gate: remote back commits: repo none, lib {lib_back}")),
            "{err:?}"
        );
        assert_eq!(back_commits_said(err.iter().map(String::as_str)), 1, "{err:?}");
    }

    #[test]
    fn a_pair_run_changing_nothing_names_every_repo_none() {
        let case = Case::new("pair-back-none");
        let (_, _, status, err) = case.pair_ran("cat a.txt");
        assert_eq!(status, 0, "{err:?}");
        assert!(
            !Case::has_back_ref_in(&case.bare()) && !Case::has_back_ref_in(&case.lib_bare()),
            "no back ref"
        );
        assert_eq!(
            err.last().map(String::as_str),
            Some("gate: remote back commits: repo none, lib none"),
            "{err:?}"
        );
        assert_eq!(back_commits_said(err.iter().map(String::as_str)), 1, "{err:?}");
    }

    #[test]
    fn a_pair_run_whose_second_push_fails_deletes_the_first_and_says_its_back_commits_failed() {
        let case = Case::new("pair-back-push-fails");
        // A folder holding a file at the sibling's back ref, put there once the layout cleared the ref, fails its push.
        let in_the_way = format!("{}/refs/gate-back/{KEY}", case.lib_bare());
        let script = format!("echo c > c.txt; echo d > ../lib/d.txt; mkdir -p '{in_the_way}'; echo x > '{in_the_way}/in-the-way'");
        let (_, _, status, err) = case.pair_ran(&script);
        assert_eq!(status, 1, "{err:?}");
        assert!(
            !Case::has_back_ref_in(&case.bare()),
            "the main repo's pushed back ref is deleted: {err:?}"
        );
        let mut from_last = err.iter().rev();
        let (sync_back, failed) = (from_last.next().expect("a line is said"), from_last.next().expect("two lines are said"));
        assert_eq!(failed, crate::lines::BACK_COMMITS_FAILED, "{err:?}");
        assert!(sync_back.starts_with("gate: remote sync-back failed: push lib failed"), "{err:?}");
        assert_eq!(back_commits_said(err.iter().map(String::as_str)), 1, "{err:?}");
    }

    #[test]
    fn a_pair_run_whose_lock_is_lost_between_its_pushes_undoes_nothing() {
        let case = Case::new("pair-back-lock-lost");
        let shim = case.base.with_file_name("git-shim-lock");
        // The main repo's push, the first, takes the lock's name from the run once it is done, as another run would
        // take it: the push step's own lock check fails it with its back ref pushed, and the sibling is never pushed.
        let script = format!(
            concat!(
                "#!/bin/sh\n'{git}' \"$@\"\nstatus=$?\ncase \" $* \" in\n",
                "*\" push -q --no-recurse-submodules --receive-pack=git-receive-pack {bare} \"*) rm -f '{lock}';;\n",
                "esac\nexit $status\n"
            ),
            git = real_git().display(),
            bare = case.bare(),
            lock = case.lock()
        );
        fs::write(&shim, script).expect("the shim is written");
        fs::set_permissions(&shim, std::os::unix::fs::PermissionsExt::from_mode(0o700)).expect("the shim is made runnable");
        test_program::set(&shim);
        let (_, _, status, err) = case.pair_ran("echo c > c.txt; echo d > ../lib/d.txt");
        assert_eq!(status, 1, "{err:?}");
        assert!(
            Case::has_back_ref_in(&case.bare()),
            "the main repo's pushed back ref is left to whoever holds the lock: {err:?}"
        );
        assert!(!Case::has_back_ref_in(&case.lib_bare()), "the sibling is never pushed: {err:?}");
        assert!(
            err.iter().all(|line| !line.contains("back ref reset")),
            "no back ref deletion is tried without the lock: {err:?}"
        );
        let mut from_last = err.iter().rev();
        let (sync_back, failed) = (from_last.next().expect("a line is said"), from_last.next().expect("two lines are said"));
        assert_eq!(failed, crate::lines::BACK_COMMITS_FAILED, "{err:?}");
        assert_eq!(
            sync_back,
            &format!(
                "gate: remote sync-back failed: push repo failed: the work folder lock {} is no longer held",
                case.lock()
            ),
            "{err:?}"
        );
        assert_eq!(back_commits_said(err.iter().map(String::as_str)), 1, "{err:?}");
    }

    #[test]
    fn a_malformed_pair_is_a_usage_error_and_nothing_is_laid_out() {
        let case = Case::new("pair-malformed");
        let commit = "a".repeat(40);
        for (extra, line) in [
            (
                json!({ "Pair": { "Leaf": "repo", "Siblings": ["lib"] } }),
                "gate: box payload SiblingSnapshots must name exactly the Pair's siblings: lib".to_owned(),
            ),
            (
                json!({ "Pair": { "Leaf": "other", "Siblings": ["lib"] }, "SiblingSnapshots": { "lib": commit } }),
                format!("gate: box payload Key {KEY} is not the work-folder key of the Pair's Leaf other"),
            ),
        ] {
            let (status, said) = ran(&case.json(&commit, &extra));
            assert_eq!(status, 2, "{line}: {said:?}");
            assert_eq!(said, [line]);
            assert!(!Path::new(&case.lock()).exists(), "the lock is not taken");
            assert!(!Path::new(&case.work()).exists(), "nothing is laid out");
        }
    }

    #[test]
    fn a_desktop_run_is_refused_desktop() {
        let case = Case::new("desktop");
        let desktop = json!({ "Desktop": true, "DesktopWait": 60 });
        assert_refused(&case, &desktop, Refusal::Desktop, "gate: remote Desktop is not built on this builder");
    }

    /// The `inputs` refusal of `why`: its tag, then the inputs line.
    fn inputs_refusal(why: &str) -> [String; 2] {
        [
            refused_line(NONCE, Refusal::Inputs),
            format!("gate: remote inputs of work folder {KEY} are not the ones sent: {}", escaped(why)),
        ]
    }

    impl Case {
        /// Writes `files` in the laid-out work folder as installed inputs, and their record sealed with [`DIGEST`].
        fn inputs_installed(&self, files: &[(&str, &str)]) {
            let mut recorded = serde_json::Map::new();
            for (path, text) in files {
                let full = format!("{}/{path}", self.work());
                fs::write(&full, text).expect("the input is written");
                let metadata = fs::metadata(&full).expect("the input is looked at");
                let file = json!({
                    "size": metadata.len(),
                    "sha256": sha256_of(text.as_bytes()).expect("the input hashes").0,
                    "modified": metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec(),
                    "id": format!("{}-{}", metadata.dev(), metadata.ino()),
                });
                recorded.insert((*path).to_owned(), file);
            }
            let record = json!({ "digest": DIGEST, "files": recorded }).to_string();
            fs::write(format!("{}.inputs", self.work()), record).expect("the inputs record is written");
        }

        /// The tree's exclude file, `None` when there is none.
        fn exclude_text(&self) -> Option<String> {
            fs::read_to_string(format!("{}/.git/info/exclude", self.work())).ok()
        }
    }

    #[test]
    fn an_inputs_digest_is_refused_inputs() {
        let case = Case::new("inputs");
        let commit = case.snapshot(&[("a.txt", "a\n")]);

        let (status, said) = ran(&case.json(&commit, &json!({ "InputsDigest": DIGEST })));

        assert_eq!(status, REFUSED_STATUS, "{said:?}");
        assert_eq!(said, inputs_refusal(&format!("{}.inputs does not exist", case.work())));
        assert!(Path::new(&format!("{}/.git", case.work())).is_dir(), "the work folder is laid out first");
    }

    #[test]
    fn a_matching_inputs_record_lets_the_run_start_its_command_and_its_input_outlives_the_checkout() {
        let case = Case::new("inputs-matched");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        case.laid_out(&commit);
        case.inputs_installed(&[("in.txt", "input\n")]);
        let mut json = case.commanded(&commit, &["/bin/sh", "-c", "cat in.txt"]);
        json["InputsDigest"] = json!(DIGEST);

        let (status, said) = whole_run(&json);

        assert_eq!(status, 0, "{said:?}");
        let out: Vec<&str> = said
            .iter()
            .filter(|(stream, _)| *stream == Stream::Out)
            .map(|(_, line)| line.as_str())
            .collect();
        assert_eq!(out, ["input"], "{said:?}");
        assert_eq!(case.exclude_text().as_deref(), Some("/in.txt\n"));
    }

    #[test]
    fn a_changed_input_refuses_the_run_inputs_and_never_starts_its_command() {
        let case = Case::new("inputs-changed");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        case.laid_out(&commit);
        case.inputs_installed(&[("in.txt", "input\n")]);
        fs::write(format!("{}/in.txt", case.work()), "changed input\n").expect("the input changes");
        let marker = case.base.with_file_name("started");
        let mut json = case.commanded(&commit, &["/bin/sh", "-c", &format!("touch '{}'", marker.display())]);
        json["InputsDigest"] = json!(DIGEST);

        let (status, said) = whole_run(&json);

        let lines: Vec<String> = said.into_iter().map(|(_, line)| line).collect();
        assert_eq!(status, REFUSED_STATUS, "{lines:?}");
        assert_eq!(lines, inputs_refusal("the input in.txt is not the copy installed"));
        assert!(!marker.exists(), "the command never started");
    }

    #[test]
    fn a_run_without_an_inputs_digest_leaves_the_exclude_file_alone() {
        let case = Case::new("inputs-none");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        case.laid_out(&commit);
        case.inputs_installed(&[("in.txt", "input\n")]);
        let before = case.exclude_text();

        let (status, said) = ran(&case.json(&commit, &json!({})));

        assert_eq!(status, 0, "{said:?}");
        assert_eq!(case.exclude_text(), before, "the exclude file is as the layout left it");
    }

    #[test]
    fn a_work_lock_lost_before_the_inputs_check_writes_no_exclude_file_and_fails_the_run() {
        let case = Case::new("inputs-lock-lost");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        case.laid_out(&commit);
        case.inputs_installed(&[("in.txt", "input\n")]);
        let exclude = format!("{}/.git/info/exclude", case.work());
        fs::write(&exclude, "before\n").expect("the exclude file is written");
        let payload = decoded(&case.json(&commit, &json!({ "InputsDigest": DIGEST })));
        let request = Request::read(&payload).expect("the request reads");
        let lock = files::held_for_test(Path::new(&case.lock()));
        fs::remove_file(case.lock())
            .and_then(|()| fs::write(case.lock(), ""))
            .expect("another file takes the lock's name");
        let output = Output::new(RecordingSink::default());

        let checked = inputs_matched(&request, &lock, &output);

        assert!(matches!(checked, Err(Unready::Ended(1))), "{:?}", output.sink().said());
        assert_eq!(
            stderr_of(&output),
            [format!(
                "gate: remote could not write {exclude}: the work folder lock {} is no longer held",
                case.lock()
            )]
        );
        assert_eq!(case.exclude_text().as_deref(), Some("before\n"), "no exclude file is written");
    }

    #[test]
    fn a_folder_at_the_exclude_file_s_name_is_moved_aside_said_and_the_run_goes_on() {
        let case = Case::new("inputs-moved");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        case.laid_out(&commit);
        case.inputs_installed(&[("in.txt", "input\n")]);
        let exclude = format!("{}/.git/info/exclude", case.work());
        if Path::new(&exclude).exists() {
            fs::remove_file(&exclude).expect("the exclude file is removed");
        }
        fs::create_dir_all(&exclude).expect("a folder takes the exclude file's name");

        let (status, said) = ran(&case.json(&commit, &json!({ "InputsDigest": DIGEST })));

        assert_eq!(status, 0, "{said:?}");
        assert_eq!(
            said.first(),
            Some(&format!("gate: remote moved aside the folder {exclude} to {exclude}.gate-moved-1")),
            "{said:?}"
        );
        assert_eq!(after_checkout(said.get(1..).unwrap_or_default()), Vec::<String>::new());
        assert_eq!(case.exclude_text().as_deref(), Some("/in.txt\n"));
    }

    #[test]
    fn the_inputs_check_runs_after_a_layout_change_cleared_the_record() {
        let case = Case::new("inputs-relaid");
        let main = case.snapshot(&[("a.txt", "a\n")]);
        let lib = case.lib_snapshot(&[("b.txt", "b\n")]);
        case.laid_out(&main);
        case.inputs_installed(&[]);

        let (status, said) = ran(&case.pair_json(&main, &lib, &json!({ "InputsDigest": DIGEST })));

        assert_eq!(status, REFUSED_STATUS, "{said:?}");
        assert_eq!(said, inputs_refusal(&format!("{}.inputs does not exist", case.work())));
        assert_eq!(case.layout_record(), "pair repo lib");
    }

    #[test]
    fn a_sent_digest_is_refused_sent() {
        let case = Case::new("sent");
        let sent = json!({ "SentDigest": DIGEST });
        assert_refused(&case, &sent, Refusal::Sent, "gate: remote SentDigest is not built on this builder");
    }

    #[test]
    fn an_lfs_digest_is_refused_lfs() {
        let case = Case::new("lfs");
        let lfs = json!({ "LfsDigest": DIGEST });
        assert_refused(&case, &lfs, Refusal::Lfs, "gate: remote LfsDigest is not built on this builder");
    }

    #[test]
    fn a_command_named_like_a_package_manager_but_not_one_restores_nothing() {
        for (name, command) in [("pnpm-helper", ["pnpm-helper"]), ("npmrc-check", ["./npmrc-check"])] {
            let case = Case::new(&format!("not-packages-{name}"));
            let payload = decoded(&case.json(&"a".repeat(40), &json!({ "Command": command })));
            let request = Request::read(&payload).expect("the request reads");

            assert!(request.unbuilt.is_none(), "{command:?} is not refused");
            let work = Path::new(request.tree());
            // Every path a lockfile and every folder a folder: only a manager's command could lead to an install.
            let found = crate::packages::lockfile_for(&request.command, work, work, &|_| Ok(true), &|_| Ok(true));
            assert!(matches!(found, Ok(None)), "{command:?} is no package manager's");
        }
    }

    #[test]
    fn a_warm_up_is_refused_layout() {
        let case = Case::new("warm-up");
        let warm = json!({ "Command": null, "WarmCommands": [["true"]], "WarmHash": DIGEST, "Timeout": 60 });
        assert_refused(&case, &warm, Refusal::Layout, &unbuilt_layout("WarmCommands"));
    }

    #[test]
    fn outputs_or_outputs_full_are_taken() {
        let (case, commit) = outputs_case("outputs-taken");
        let (status, said) = run_fed(
            &case.outputs_json(&commit, "true", &json!(["bin/**"])),
            &mut fed(NO_FILES),
            free_watches(),
        );
        assert_eq!(status, 0, "{said:?}");
        assert_eq!(outputs_said(&said), ["gate-remote-outputs: 0"], "{said:?}");

        let full = case.json(&commit, &json!({ "OutputsFull": true }));
        let (status, said) = run_fed(&full, &mut fed(NO_FILES), free_watches());
        assert_eq!(status, 0, "{said:?}");
        assert!(outputs_said(&said).is_empty(), "no globs bring nothing: {said:?}");
    }

    /// The PC's outputs manifest line holding no file.
    const NO_FILES: &str = "{\"files\":{}}\n";

    /// The PC's input after the request line: `text`.
    fn fed(text: &str) -> Cursor<Vec<u8>> {
        Cursor::new(text.as_bytes().to_vec())
    }

    /// Runs the whole run of `json` on the box's own `PATH`, watched as `watches` says, the PC's input after the
    /// request line read from `input`; the status and every line said, with its stream.
    fn run_fed(json: &Value, input: &mut Cursor<Vec<u8>>, watches: Watches) -> (u8, Vec<(Stream, String)>) {
        let payload = decoded(json);
        let output = Arc::new(Output::new(RecordingSink::default()));
        let status = run_reading(&payload, &output, watches, own_path, input);
        (status, output.sink().said())
    }

    /// A case whose snapshot holds `a.txt` and ignores `bin/`, so a file a command writes there is in no back commit;
    /// the case and its snapshot.
    fn outputs_case(name: &str) -> (Case, String) {
        let case = Case::new(name);
        let commit = case.snapshot(&[("a.txt", "a\n"), (".gitignore", "bin/\n")]);
        (case, commit)
    }

    /// The name of a case's outputs archive.
    fn archive_name() -> String {
        format!("{KEY}-{NONCE}.tar.zst")
    }

    impl Case {
        /// The run payload of `snapshot` running `script` in `sh`, with the case's identity, asking for the outputs
        /// `globs`.
        fn outputs_json(&self, snapshot: &str, script: &str, globs: &Value) -> Value {
            self.json(
                snapshot,
                &json!({ "Command": ["/bin/sh", "-c", script], "Identity": identity(), "Outputs": globs }),
            )
        }

        /// The case's outputs archive, `<root>/outputs/<key>-<nonce>.tar.zst`.
        fn archive(&self) -> PathBuf {
            PathBuf::from(format!("{}/outputs/{}", self.root(), archive_name()))
        }
    }

    /// The `gate-remote-outputs:` lines of `said`.
    fn outputs_said(said: &[(Stream, String)]) -> Vec<&str> {
        said.iter()
            .filter(|(_, line)| line.starts_with("gate-remote-outputs:"))
            .map(|(_, line)| line.as_str())
            .collect()
    }

    /// The path of each member of the outputs archive at `path`, in its list's order.
    fn members_of(path: &Path) -> Vec<String> {
        let (_, items) = read_archive(path, &Manifest::default(), &[]);
        items.into_iter().map(|item| item.entry.path).collect()
    }

    #[test]
    fn a_run_with_globs_packs_a_new_file_and_names_its_archive() {
        let (case, commit) = outputs_case("outputs-packed");
        let json = case.outputs_json(&commit, "mkdir -p bin && echo new > bin/new.txt", &json!(["bin/**"]));
        let (status, said) = run_fed(&json, &mut fed(NO_FILES), free_watches());
        assert_eq!(status, 0, "{said:?}");
        let line = format!("gate-remote-outputs: 1 {}", archive_name());
        assert_eq!(outputs_said(&said), [line.as_str()], "{said:?}");
        let size = format!(
            "gate-remote-outputs-size: {}",
            fs::metadata(case.archive()).expect("the archive is there").len()
        );
        let before = said
            .iter()
            .position(|(_, said)| *said == line)
            .and_then(|at| at.checked_sub(1))
            .and_then(|before| said.get(before));
        assert!(
            before.is_some_and(|(_, before)| *before == size),
            "the archive's size line, just before the outputs line: {said:?}"
        );
        assert_eq!(members_of(&case.archive()), ["bin/new.txt"]);
    }

    #[test]
    fn a_run_without_globs_reads_only_the_request_line() {
        let case = Case::new("outputs-unread");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let mut input = fed(NO_FILES);
        let (status, said) = run_fed(&case.commanded(&commit, &["true"]), &mut input, free_watches());
        assert_eq!(status, 0, "{said:?}");
        assert_eq!(input.position(), 0, "nothing past the request line is read");
        assert!(outputs_said(&said).is_empty(), "{said:?}");
    }

    /// An input that, at its first read, waits within a hang guard for `marker`, which the first heartbeat makes, and
    /// notes whether it came and whether the work folder `work` was there yet; then reads `bytes`.
    struct Watched {
        marker: PathBuf,
        work: PathBuf,
        seen: Option<(bool, bool)>,
        bytes: Cursor<Vec<u8>>,
    }

    impl Watched {
        fn look(&mut self) {
            if self.seen.is_some() {
                return;
            }
            let guard = Instant::now() + HANG_GUARD;
            while !self.marker.exists() && Instant::now() < guard {
                thread::sleep(Duration::from_millis(10));
            }
            self.seen = Some((self.marker.exists(), self.work.exists()));
        }
    }

    impl Read for Watched {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.look();
            self.bytes.read(buf)
        }
    }

    impl BufRead for Watched {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            self.look();
            self.bytes.fill_buf()
        }

        fn consume(&mut self, amount: usize) {
            self.bytes.consume(amount);
        }
    }

    #[test]
    fn the_outputs_manifest_is_read_after_the_first_beat_and_before_the_layout() {
        let case = Case::new("outputs-beat");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let marker = case.base.with_file_name("beaten");
        let payload = decoded(&case.outputs_json(&commit, "true", &json!(["bin/**"])));
        let output = Arc::new(Output::new(MarkingSink {
            lines: Mutex::new(Vec::new()),
            marker: marker.clone(),
        }));
        let periods = Periods {
            heartbeat: Duration::from_millis(1),
            ..Periods::default()
        };
        let mut input = Watched {
            marker,
            work: PathBuf::from(case.work()),
            seen: None,
            bytes: fed(NO_FILES),
        };
        let sources = Sources {
            login: own_path,
            start: heartbeat::start,
            input: &mut input,
            drain: DrainSeams::default(),
        };
        let ready = prepared(&payload, &output, periods, sources);
        let lines = || output.sink().said();
        assert!(ready.is_ok(), "the run is ready: {:?}", lines());
        let mut ready = ready.expect("the run is ready");
        tmp_cleared(&mut ready);
        let Ready { pump, .. } = ready;
        if let Some(pump) = pump {
            heartbeat::stop(pump);
        }
        assert_eq!(input.seen, Some((true, false)), "read after a beat, before the layout");
        assert_eq!(input.bytes.position(), NO_FILES.len() as u64, "the manifest line is read whole");
    }

    #[test]
    fn a_failed_command_with_no_test_results_glob_packs_nothing_and_says_no_outputs_line() {
        let (case, commit) = outputs_case("outputs-failed");
        let json = case.outputs_json(&commit, "mkdir -p bin && echo new > bin/new.txt; exit 3", &json!(["bin/**"]));
        let (status, said) = run_fed(&json, &mut fed(NO_FILES), free_watches());
        assert_eq!(status, 3, "{said:?}");
        assert!(outputs_said(&said).is_empty(), "{said:?}");
        assert!(!said.iter().any(|(_, line)| line.starts_with("gate: remote outputs")), "{said:?}");
        assert!(!case.archive().exists(), "nothing is packed");
    }

    /// A case whose snapshot holds `a.txt` and ignores `bin/` and `TestResults/`, so neither a file a command writes in
    /// `bin` nor a report it leaves in `TestResults` is in a back commit; the case and its snapshot.
    fn reports_case(name: &str) -> (Case, String) {
        let case = Case::new(name);
        let commit = case.snapshot(&[("a.txt", "a\n"), (".gitignore", "bin/\nTestResults/\n")]);
        (case, commit)
    }

    /// A command that writes `bin/new.txt` and the report `TestResults/r.trx`, then fails with 3.
    const FAILS_WITH_A_REPORT: &str = "mkdir -p bin TestResults && echo new > bin/new.txt && echo trx > TestResults/r.trx; exit 3";

    /// The output globs of a run that writes [`FAILS_WITH_A_REPORT`]'s files: one of its build, one of its reports.
    const BIN_AND_REPORTS: [&str; 2] = ["bin/**", "**/TestResults/**"];

    #[test]
    fn a_failed_command_packs_its_test_reports_alone() {
        let (case, commit) = reports_case("outputs-failed-reports");
        let json = case.outputs_json(&commit, FAILS_WITH_A_REPORT, &json!(BIN_AND_REPORTS));
        let (status, said) = run_fed(&json, &mut fed(NO_FILES), free_watches());
        assert_eq!(status, 3, "the command's own status: {said:?}");
        let line = format!("gate-remote-outputs: 1 {}", archive_name());
        assert_eq!(outputs_said(&said), [line.as_str()], "{said:?}");
        assert_eq!(members_of(&case.archive()), ["TestResults/r.trx"], "only the report comes back");
    }

    /// A command that writes `bin/new.txt`, the report `TestResults/r.trx` and the dump `dumps/a.dmp`, then fails with
    /// 3.
    const FAILS_WITH_A_DUMP: &str =
        "mkdir -p bin TestResults dumps && echo new > bin/new.txt && echo trx > TestResults/r.trx && echo dmp > dumps/a.dmp; exit 3";

    /// The payload of a run of `command` asking for the outputs `outputs` and the failure outputs `failure`.
    fn failure_json(case: &Case, commit: &str, command: &str, (outputs, failure): (&[&str], &[&str])) -> Value {
        case.json(
            commit,
            &json!({
                "Command": ["/bin/sh", "-c", command],
                "Identity": identity(),
                "Outputs": outputs,
                "FailureOutputs": failure,
            }),
        )
    }

    /// A case like [`reports_case`] whose snapshot also ignores `dumps/`.
    fn dumps_case(name: &str) -> (Case, String) {
        let case = Case::new(name);
        let commit = case.snapshot(&[("a.txt", "a\n"), (".gitignore", "bin/\nTestResults/\ndumps/\n")]);
        (case, commit)
    }

    #[test]
    fn a_failed_command_with_a_failure_output_glob_packs_its_file() {
        let (case, commit) = dumps_case("outputs-failed-dump");
        let json = failure_json(&case, &commit, FAILS_WITH_A_DUMP, (&["bin/**"], &["dumps/*.dmp"]));
        let (status, said) = run_fed(&json, &mut fed(NO_FILES), free_watches());
        assert_eq!(status, 3, "the command's own status: {said:?}");
        let line = format!("gate-remote-outputs: 1 {}", archive_name());
        assert_eq!(outputs_said(&said), [line.as_str()], "{said:?}");
        assert_eq!(members_of(&case.archive()), ["dumps/a.dmp"], "the dump comes back, the build does not");
    }

    #[test]
    fn a_failed_command_with_both_kinds_of_glob_packs_both() {
        let (case, commit) = dumps_case("outputs-failed-both");
        let json = failure_json(&case, &commit, FAILS_WITH_A_DUMP, (&BIN_AND_REPORTS, &["dumps/*.dmp"]));
        let (status, said) = run_fed(&json, &mut fed(NO_FILES), free_watches());
        assert_eq!(status, 3, "the command's own status: {said:?}");
        let line = format!("gate-remote-outputs: 2 {}", archive_name());
        assert_eq!(outputs_said(&said), [line.as_str()], "{said:?}");
        assert_eq!(members_of(&case.archive()), ["TestResults/r.trx", "dumps/a.dmp"]);
    }

    #[test]
    fn a_passing_command_does_not_pack_a_file_only_a_failure_output_glob_matches() {
        let (case, commit) = dumps_case("outputs-passed-dump");
        let command = "mkdir -p dumps && echo dmp > dumps/a.dmp";
        let json = failure_json(&case, &commit, command, (&[], &["dumps/*.dmp"]));
        let (status, said) = run_fed(&json, &mut fed(NO_FILES), free_watches());
        assert_eq!(status, 0, "{said:?}");
        assert_eq!(outputs_said(&said), [crate::outputs::NOTHING_PACKED], "{said:?}");
        assert!(!case.archive().exists(), "nothing is packed");
    }

    #[test]
    fn a_failed_command_with_a_cross_build_packs_its_test_reports_without_it() {
        let (case, commit) = reports_case("outputs-failed-cross");
        let marker = case.base.with_file_name("cross-ran");
        let cross = format!(": > '{}'", marker.display());
        let json = case.json(
            &commit,
            &json!({
                "Command": ["/bin/sh", "-c", FAILS_WITH_A_REPORT],
                "Identity": identity(),
                "Outputs": BIN_AND_REPORTS,
                "CrossBuild": { "Rid": "win-x64", "Words": ["/bin/sh", "-c", cross] },
            }),
        );
        let (status, said) = run_fed(&json, &mut fed(NO_FILES), free_watches());
        assert_eq!(status, 3, "the command's own status: {said:?}");
        assert!(!marker.exists(), "no cross build ran");
        assert!(!said.iter().any(|(_, line)| line.starts_with("gate: the cross build")), "{said:?}");
        let line = format!("gate-remote-outputs: 1 {}", archive_name());
        assert_eq!(outputs_said(&said), [line.as_str()], "{said:?}");
        assert_eq!(members_of(&case.archive()), ["TestResults/r.trx"], "only the report comes back");
    }

    #[test]
    fn a_failed_command_whose_test_results_glob_matches_nothing_says_nothing_packed() {
        let (case, commit) = reports_case("outputs-failed-unmatched");
        let script = "mkdir -p bin && echo new > bin/new.txt; exit 3";
        let json = case.outputs_json(&commit, script, &json!(BIN_AND_REPORTS));
        let (status, said) = run_fed(&json, &mut fed(NO_FILES), free_watches());
        assert_eq!(status, 3, "the command's own status: {said:?}");
        assert_eq!(outputs_said(&said), ["gate-remote-outputs: 0"], "{said:?}");
        assert!(!case.archive().exists(), "nothing is packed");
    }

    #[test]
    fn a_failed_command_whose_reports_are_not_brought_back_keeps_its_status() {
        let (case, commit) = reports_case("outputs-failed-malformed");
        let json = case.outputs_json(&commit, FAILS_WITH_A_REPORT, &json!(BIN_AND_REPORTS));
        let (status, said) = run_fed(&json, &mut fed("not a manifest\n"), free_watches());
        assert_eq!(status, 3, "the command's own status, not 1: {said:?}");
        assert!(
            said.iter()
                .any(|(stream, line)| *stream == Stream::Err && line.starts_with("gate: remote outputs not brought back: ")),
            "{said:?}"
        );
        assert!(!case.archive().exists(), "nothing is packed");
    }

    #[test]
    fn a_back_commit_s_path_is_not_packed() {
        let (case, commit) = outputs_case("outputs-back");
        let script = "mkdir -p out bin && echo c > out/committed.txt && echo k > bin/kept.txt";
        let (status, said) = run_fed(
            &case.outputs_json(&commit, script, &json!(["out/**", "bin/**"])),
            &mut fed(NO_FILES),
            free_watches(),
        );
        assert_eq!(status, 0, "{said:?}");
        let back = case.back_commit().expect("the back ref is pushed");
        assert_eq!(
            git(&[&format!("--git-dir={}", case.bare()), "show", &format!("{back}:out/committed.txt")]),
            "c"
        );
        assert_eq!(members_of(&case.archive()), ["bin/kept.txt"], "the back commit's path stays out");
    }

    #[test]
    fn a_malformed_manifest_is_said_after_the_command_and_ends_the_run_1() {
        let (case, commit) = outputs_case("outputs-malformed");
        let json = case.outputs_json(&commit, "echo ran; mkdir -p bin && echo new > bin/new.txt", &json!(["bin/**"]));
        let (status, said) = run_fed(&json, &mut fed("not a manifest\n"), free_watches());
        assert_eq!(status, 1, "{said:?}");
        let ran = said.iter().position(|(stream, line)| *stream == Stream::Out && line == "ran");
        let failed = said
            .iter()
            .position(|(stream, line)| *stream == Stream::Err && line.starts_with("gate: remote outputs not brought back: "));
        assert!(ran.is_some() && failed.is_some() && ran < failed, "said after the command: {said:?}");
        assert!(!case.archive().exists(), "nothing is packed");
    }

    #[test]
    fn the_budget_seam_trims_an_archive_alone_over_it_and_says_so() {
        let (case, commit) = outputs_case("outputs-budget");
        let json = case.outputs_json(&commit, "mkdir -p bin && echo new > bin/new.txt", &json!(["bin/**"]));
        let watches = Watches { budget: 1, ..free_watches() };
        let (status, said) = run_fed(&json, &mut fed(NO_FILES), watches);
        assert_eq!(status, 1, "{said:?}");
        let line = format!("gate-remote-outputs: 1 {}", archive_name());
        assert_eq!(outputs_said(&said), [line.as_str()], "{said:?}");
        let over = "gate: remote outputs not brought back: the archive is 0.0 GiB, more than the box keeps (50 GiB)";
        assert_eq!(said.last().map(|(_, line)| line.as_str()), Some(over), "{said:?}");
        assert!(!case.archive().exists(), "the archive alone over the budget is removed");
    }

    #[test]
    fn a_back_commit_listing_more_than_a_mebibyte_of_paths_still_brings_the_outputs_back() {
        let (case, commit) = outputs_case("outputs-many-paths");
        // 6000 names of 200 and more bytes: diff-tree lists over 1 MiB of paths.
        let script = "mkdir -p bin d && echo new > bin/new.txt && long=$(printf '%0200d' 0) && i=0 && \
                      while [ $i -lt 6000 ]; do : > \"d/${long}_$i\"; i=$((i+1)); done";
        let (status, said) = run_fed(
            &case.outputs_json(&commit, script, &json!(["bin/**"])),
            &mut fed(NO_FILES),
            free_watches(),
        );
        assert_eq!(status, 0, "{:?}", said.iter().rev().take(5).collect::<Vec<_>>());
        let line = format!("gate-remote-outputs: 1 {}", archive_name());
        assert_eq!(outputs_said(&said), [line.as_str()]);
        assert_eq!(members_of(&case.archive()), ["bin/new.txt"]);
    }

    /// A sink that keeps every line until one starts with `from`: that write, and every one after it, finds the client
    /// gone.
    struct GoneFrom {
        from: &'static str,
        lines: Mutex<Vec<(Stream, String)>>,
        gone: AtomicBool,
    }

    impl GoneFrom {
        fn new(from: &'static str) -> GoneFrom {
            GoneFrom {
                from,
                lines: Mutex::new(Vec::new()),
                gone: AtomicBool::new(false),
            }
        }
    }

    impl LineSink for GoneFrom {
        fn write_line(&self, stream: Stream, line: &str) -> std::io::Result<()> {
            if gate_wire::stamp::unstamped(line).text.starts_with(self.from) {
                self.gone.store(true, Ordering::SeqCst);
            }
            if self.gone.load(Ordering::SeqCst) {
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            self.lines.lock().unwrap_or_else(PoisonError::into_inner).push((stream, line.to_owned()));
            Ok(())
        }
    }

    /// Runs the whole run of `json` as [`run_fed`] does, its lines said to a client that is gone from the first line
    /// starting with `from`; the status.
    fn run_gone_from(json: &Value, from: &'static str, watches: Watches) -> u8 {
        let payload = decoded(json);
        let output = Arc::new(Output::new(GoneFrom::new(from)));
        run_reading(&payload, &output, watches, own_path, &mut fed(NO_FILES))
    }

    #[test]
    fn a_client_gone_before_the_pack_or_the_trim_packs_or_trims_nothing_and_ends_closed() {
        let (case, commit) = outputs_case("outputs-gone");
        let lib = case.lib_snapshot(&[("b.txt", "b\n")]);
        let script = "mkdir -p bin && echo new > bin/new.txt && echo d > ../lib/d.txt";
        let extra = json!({ "Command": ["/bin/sh", "-c", script], "Identity": identity(), "Outputs": ["bin/**"] });
        let status = run_gone_from(&case.pair_json(&commit, &lib, &extra), "gate: remote back commits:", free_watches());
        assert_eq!(status, CLOSED_STATUS, "gone at the back commits line, before the pack");
        assert!(!case.archive().exists(), "nothing is packed for a client gone");

        let (case, commit) = outputs_case("outputs-gone-trim");
        let json = case.outputs_json(&commit, "mkdir -p bin && echo new > bin/new.txt", &json!(["bin/**"]));
        let watches = Watches { budget: 1, ..free_watches() };
        let status = run_gone_from(&json, "gate-remote-outputs:", watches);
        assert_eq!(status, CLOSED_STATUS, "gone at the outputs line, before the trim");
        assert!(case.archive().exists(), "the archive over the budget is not trimmed for a client gone");
    }

    #[test]
    fn an_unreal_version_or_engine_env_is_refused_layout_naming_the_key() {
        let case = Case::new("engines");
        assert_refused(
            &case,
            &json!({ "UnrealVersion": "5.4.0" }),
            Refusal::Layout,
            &unbuilt_layout("UnrealVersion"),
        );
        let engine_env = json!({ "EngineEnv": { "MY_ENGINE": "unreal" } });
        assert_refused(&case, &engine_env, Refusal::Layout, &unbuilt_layout("EngineEnv"));
    }

    #[test]
    fn an_interrupted_command_record_removes_the_warm_tools_and_packages_records_before_the_checkout() {
        let case = Case::new("interrupted");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        case.laid_out(&commit);
        let (warm, tools) = (format!("{}.warm", case.work()), format!("{}.tools", case.work()));
        let packages = format!("{}.packages", case.work());
        for record in [&warm, &tools, &packages] {
            fs::write(record, DIGEST).expect("the record is written");
        }

        case.laid_out(&commit);
        assert!(
            Path::new(&warm).exists() && Path::new(&tools).exists() && Path::new(&packages).exists(),
            "no interrupted command, the records stand"
        );

        fs::write(format!("{}.command", case.work()), "x").expect("the interrupted-command record is written");
        let (status, said) = ran(&case.json(&commit, &json!({})));

        assert_eq!(status, 0, "{said:?}");
        assert!(!Path::new(&warm).exists(), "the warm record is removed");
        assert!(!Path::new(&tools).exists(), "the tools record is removed");
        assert!(!Path::new(&packages).exists(), "the packages record is removed");
    }

    /// The preflight of `json`, a run payload: whether it says the work folder's tree is new.
    fn preflight_says_new(json: &Value) -> bool {
        let mut console = Console::captured();
        let status = crate::preflight::preflight(&decoded(json), &gate_args::BoxSeams::default(), &mut console);
        assert_eq!(status, 0, "{:?}", console.err);
        console.out.iter().any(|line| line == crate::lines::NEW_WORK_TREE)
    }

    #[test]
    fn a_preflight_after_a_run_started_in_the_folder_says_it_is_warm() {
        let case = Case::new("ran-warm");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let json = case.json(&commit, &json!({}));
        assert!(preflight_says_new(&json), "no command has started in the folder yet");

        let (status, said) = run_fed(&json, &mut fed(NO_FILES), free_watches());

        assert_eq!(status, 0, "{said:?}");
        let text = fs::read_to_string(crate::command::ran_record(&case.work())).expect("the run wrote the ran record");
        assert!(
            text.ends_with('Z') && text.len() == "2026-01-01T00:00:00.0000000Z".len(),
            "the UTC start: {text:?}"
        );
        assert!(!preflight_says_new(&json), "a command started in the folder");
    }

    #[test]
    fn a_folder_at_the_ran_record_s_name_reads_as_new() {
        let case = Case::new("ran-folder");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let json = case.json(&commit, &json!({}));
        case.laid_out(&commit);
        let record = crate::command::ran_record(&case.work());
        fs::create_dir(&record).expect("a folder takes the record's name");

        assert!(preflight_says_new(&json), "only a plain file counts as ran");
        let (status, said) = run_fed(&json, &mut fed(NO_FILES), free_watches());
        assert_eq!(status, 0, "the folder refuses no run: {said:?}");
        fs::write(format!("{}.layout", case.work()), "changing").expect("the layout record is written");
        let (status, said) = ran(&json);
        assert_eq!(status, 0, "nor a run that clears the work folder: {said:?}");
        assert!(!Path::new(&record).exists(), "the clear removed the folder");
    }

    #[test]
    fn a_relaid_folder_loses_its_ran_record() {
        let case = Case::new("relaid-ran");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let json = case.json(&commit, &json!({}));
        let (status, said) = run_fed(&json, &mut fed(NO_FILES), free_watches());
        assert_eq!(status, 0, "{said:?}");
        let record = crate::command::ran_record(&case.work());
        assert!(Path::new(&record).is_file(), "{said:?}");
        fs::write(format!("{}.layout", case.work()), "changing").expect("the layout record is written");

        // Laid out again with no command run, so nothing writes the record after the clear.
        let (status, said) = ran(&json);

        assert_eq!(status, 0, "{said:?}");
        assert!(!Path::new(&record).exists(), "the clear removed the ran record");
        assert!(preflight_says_new(&json), "no command has started in the cleared folder");
    }

    #[test]
    fn a_folder_at_the_warm_tools_or_packages_record_of_an_interrupted_command_is_refused_layout() {
        for suffix in [".warm", ".tools", ".packages"] {
            let case = Case::new("record-folder");
            let commit = case.snapshot(&[("a.txt", "a\n")]);
            case.laid_out(&commit);
            fs::write(format!("{}.command", case.work()), "x").expect("the interrupted-command record is written");
            let record = format!("{}{suffix}", case.work());
            fs::create_dir(&record).expect("a folder takes the record's name");

            let (status, said) = ran(&case.json(&commit, &json!({})));

            assert_eq!(status, REFUSED_STATUS, "{suffix}: {said:?}");
            let start = layout_line("repo", &format!("cannot remove {record}: "));
            assert!(said.len() == 2 && said[1].starts_with(&start), "{suffix}: {said:?}");
            assert_eq!(said[0], refused_line(NONCE, Refusal::Layout));
        }
    }

    #[test]
    fn a_folder_at_the_interrupted_command_record_is_refused_layout_with_no_started_line() {
        let case = Case::new("command-folder");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        case.laid_out(&commit);
        let record = format!("{}.command", case.work());
        fs::create_dir(&record).expect("a folder takes the record's name");

        let (status, said) = ran(&case.json(&commit, &json!({})));

        assert_eq!(status, REFUSED_STATUS, "{said:?}");
        assert_eq!(said, layout_refusal(&format!("{record} is a link or not a file")));
        assert!(Path::new(&record).is_dir(), "the folder is left");
    }

    #[test]
    fn a_link_or_a_file_at_the_work_folder_fails_the_run_with_the_folder_line() {
        for linked in [true, false] {
            let case = Case::new("work-flaw");
            let commit = case.snapshot(&[("a.txt", "a\n")]);
            if linked {
                let target = format!("{}/elsewhere", case.root());
                fs::create_dir(&target).expect("the link's target is made");
                symlink(&target, case.work()).expect("the link is made");
            } else {
                fs::write(case.work(), "kept\n").expect("a file takes the work folder's name");
            }

            let (status, said) = ran(&case.json(&commit, &json!({})));

            let why = if linked { "it is a symbolic link" } else { "it is not a folder" };
            assert_eq!(status, 1, "{said:?}");
            assert_eq!(said, [format!("gate: remote could not create the folder {}: {why}", case.work())]);
        }
    }

    #[test]
    fn another_user_s_folder_is_not_one_the_layout_uses() {
        if effective_uid() == 0 {
            // The case runs as root, whose own folder `/usr` is; no other user's folder is at hand.
            return;
        }
        let line = files::unusable_folder(Path::new("/usr"), false).expect("a root-owned folder is not the box user's");
        let user = files::user_name(effective_uid());
        assert_eq!(
            line,
            crate::lines::FolderFlaw::Failed(format!("gate: remote could not create the folder /usr: it is not owned by {user}"))
        );
    }

    #[test]
    fn a_tree_without_its_head_is_made_again_with_git_init() {
        let case = Case::new("no-head");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        case.laid_out(&commit);
        fs::remove_file(format!("{}/.git/HEAD", case.work())).expect("HEAD is removed");

        let (status, said) = ran(&case.json(&commit, &json!({})));

        assert_eq!(status, 0, "{said:?}");
        assert_eq!(case.head(), commit, "the tree is made again and checked out");
    }

    #[test]
    fn a_link_at_the_tree_s_git_folders_or_files_is_refused_layout_and_its_target_untouched() {
        for (at, kind) in [
            (".git", "folder"),
            (".git/objects", "folder"),
            (".git/objects/info", "folder"),
            (".git/config", "file"),
            (".git/HEAD", "file"),
        ] {
            let case = Case::new("git-link");
            let commit = case.snapshot(&[("a.txt", "a\n")]);
            case.laid_out(&commit);
            let linked = format!("{}/{at}", case.work());
            let target = format!("{}/elsewhere", case.root());
            fs::rename(&linked, &target).expect("the original is moved aside");
            symlink(&target, &linked).expect("the link is made");

            let (status, said) = ran(&case.json(&commit, &json!({})));

            assert_eq!(status, REFUSED_STATUS, "{at}: {said:?}");
            assert_eq!(said, layout_refusal(&format!("{linked} is a link or not a {kind}")), "{at}");
            assert!(fs::symlink_metadata(&linked).expect("the link stands").file_type().is_symlink(), "{at}");
        }
    }

    #[test]
    fn a_tree_s_git_folder_or_file_others_may_write_is_refused_layout_and_left_alone() {
        for (at, mode) in [
            (".git", 0o775),
            (".git/objects", 0o757),
            (".git/objects/info", 0o770),
            (".git/config", 0o666),
            (".git/HEAD", 0o620),
        ] {
            let case = Case::new("git-open");
            let commit = case.snapshot(&[("a.txt", "a\n")]);
            case.laid_out(&commit);
            let path = format!("{}/{at}", case.work());
            fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(mode)).expect("the mode is set");

            let (status, said) = ran(&case.json(&commit, &json!({})));

            assert_eq!(status, REFUSED_STATUS, "{at}: {said:?}");
            assert_eq!(said, layout_refusal(&format!("{path} is writable by others")), "{at}");
            let left = fs::symlink_metadata(&path).expect("it is looked at").mode() & 0o777;
            assert_eq!(left, mode, "{at} is not changed");
        }
    }

    #[test]
    fn a_lock_file_another_user_owns_is_refused_layout_and_any_other_lock_failure_fails() {
        let lock = "/g/work/k.lock";
        let not_owned = files::lock_not_owned(Path::new(lock));
        let owned = std::io::Error::new(std::io::ErrorKind::PermissionDenied, not_owned.clone());
        assert!(
            matches!(super::lock_untaken(&owned, lock), NotLaidOut::Refused(why) if why == not_owned),
            "another user's lock is a layout refusal"
        );

        for other in [
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
            std::io::Error::other(not_owned.clone()),
        ] {
            let line = format!("gate: remote could not open the work folder lock {lock}: {other}");
            assert!(
                matches!(super::lock_untaken(&other, lock), NotLaidOut::Failed(failed) if failed.lines == [line.clone()] && failed.status == 1),
                "{line}"
            );
        }
    }

    #[test]
    fn a_tree_whose_git_names_another_repository_s_folders_is_refused_layout_and_left_alone() {
        let case = Case::new("commondir");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        case.laid_out(&commit);
        let commondir = format!("{}/.git/commondir", case.work());
        fs::write(&commondir, "../elsewhere\n").expect("the commondir file is written");

        let (status, said) = ran(&case.json(&commit, &json!({})));

        assert_eq!(status, REFUSED_STATUS, "{said:?}");
        assert_eq!(said, layout_refusal(&format!("{commondir} names another repository's folders")));
        assert_eq!(fs::read(&commondir).expect("the file is read"), b"../elsewhere\n");
    }

    #[test]
    fn a_tree_others_may_only_read_is_laid_out_and_run() {
        let case = Case::new("git-readable");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        case.laid_out(&commit);
        for (at, mode) in [
            ("", 0o755),
            (".git", 0o755),
            (".git/objects", 0o755),
            (".git/objects/info", 0o755),
            (".git/config", 0o644),
            (".git/HEAD", 0o644),
        ] {
            let path = format!("{}/{at}", case.work());
            fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(mode)).expect("the mode is set");
        }

        let (status, said) = ran(&case.json(&commit, &json!({})));

        assert_eq!(status, 0, "{said:?}");
        assert_eq!(case.head(), commit);
    }

    #[test]
    fn a_folder_at_the_alternates_file_is_refused_layout_and_kept() {
        let case = Case::new("alternates-folder");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        case.laid_out(&commit);
        let alternates = format!("{}/.git/objects/info/alternates", case.work());
        fs::remove_file(&alternates).expect("the alternates file is removed");
        fs::create_dir(&alternates).expect("a folder takes its name");

        let (status, said) = ran(&case.json(&commit, &json!({})));

        assert_eq!(status, REFUSED_STATUS, "{said:?}");
        assert_eq!(said, layout_refusal(&format!("cannot write {alternates}: it is not a plain file")));
        assert!(Path::new(&alternates).is_dir(), "the folder is kept");
    }

    /// A `sleep 600` leading a process group of its own, as a git an earlier run left running does.
    #[expect(
        clippy::disallowed_methods,
        reason = "the spawn lock is the Windows gate's; this test stands in for an earlier run's git"
    )]
    fn sleeping_group() -> Child {
        Command::new("sleep").arg("600").process_group(0).spawn().expect("sleep starts")
    }

    /// The group record of the case's work folder.
    fn record_of(case: &Case) -> String {
        format!("{}.pgid", case.work())
    }

    #[test]
    fn each_git_step_of_a_run_is_given_the_windows_box_s_bound_class_and_no_record_is_left() {
        let case = Case::new("classes");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let _ = test_limit::classes();

        let (status, said) = ran(&case.json(&commit, &json!({})));

        assert_eq!(status, 0, "{said:?}");
        let classes = test_limit::classes();
        let of = |class: GitClass| -> Vec<String> {
            classes
                .iter()
                .filter(|(_, given)| *given == class)
                .map(|(step, _)| step.clone())
                .collect()
        };
        assert_eq!(of(GitClass::Long), ["fetch", "checkout", "clean"]);
        assert_eq!(
            of(GitClass::Short),
            [
                "git init",
                "git config remote.origin.url",
                "git remote add",
                "git config core.autocrlf",
                "git config core.eol",
                "cat-file",
                "back ref reset"
            ]
        );
        assert!(!Path::new(&record_of(&case)).exists(), "each step's group record is removed");
    }

    #[test]
    fn a_run_finding_an_earlier_run_s_group_still_running_is_refused_busy_and_leaves_it_and_its_locks() {
        let case = Case::new("earlier-live");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        case.laid_out(&commit);
        let mut child = sleeping_group();
        let record = record_of(&case);
        fs::write(&record, live_record_text(&format!("gate-remote-{KEY}-ffffffffffff"), child.id())).expect("the record is written");
        let index_lock = format!("{}/.git/index.lock", case.work());
        fs::write(&index_lock, "").expect("the earlier git's lock is planted");

        let (status, said) = ran(&case.json(&commit, &json!({})));

        let alive = child.try_wait().expect("the sleep is looked at").is_none();
        // Best effort: the sleep is this case's own.
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(status, REFUSED_STATUS, "{said:?}");
        assert_eq!(said, [refused_line(NONCE, Refusal::Busy), busy_line(KEY)]);
        assert!(alive, "the earlier group is never signalled");
        assert!(Path::new(&index_lock).exists(), "its git's lock file is not swept");
        assert!(Path::new(&record).exists(), "its record stands");
    }

    #[test]
    fn a_record_naming_a_group_with_no_process_left_is_removed_and_the_run_goes_on() {
        let case = Case::new("earlier-dead");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let mut child = sleeping_group();
        let group = i32::try_from(child.id()).expect("a pid is a pid_t");
        child.kill().expect("the sleep is killed");
        child.wait().expect("the sleep is reaped");
        // SAFETY: signal 0 only asks whether the group exists; it touches no memory.
        assert!(unsafe { libc::killpg(group, 0) } != 0, "the group is gone");
        let record = record_of(&case);
        fs::write(
            &record,
            job_record_text(&format!("gate-remote-{KEY}-ffffffffffff"), group, own_session(), (1, 0)),
        )
        .expect("the record is written");

        let (status, said) = ran(&case.json(&commit, &json!({})));

        assert_eq!(status, 0, "{said:?}");
        assert_eq!(case.head(), commit);
        assert!(!Path::new(&record).exists(), "the stale record is removed");
    }

    /// This process's session.
    fn own_session() -> i32 {
        // SAFETY: `getsid(0)` asks for the caller's own session and touches no memory.
        unsafe { libc::getsid(0) }
    }

    /// The text of the record of the job `job` naming the group of the live process `pid`, made in this process's
    /// session, with that process's start, as a run writes it.
    fn live_record_text(job: &str, pid: u32) -> String {
        let pid = i32::try_from(pid).expect("a pid is a pid_t");
        job_record_text(job, pid, own_session(), start_of(pid).expect("the process's start is read"))
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_record_whose_group_id_is_a_process_with_another_start_is_removed_and_the_process_left_alone() {
        let case = Case::new("earlier-restarted");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let mut child = sleeping_group();
        let pid = i32::try_from(child.id()).expect("a pid is a pid_t");
        let (seconds, micros) = start_of(pid).expect("the sleep's start is read");
        let record = record_of(&case);
        // The process with the recorded id started a second after the recorded leader: a pid reused since.
        let text = job_record_text(&format!("gate-remote-{KEY}-ffffffffffff"), pid, own_session(), (seconds - 1, micros));
        fs::write(&record, text).expect("the record is written");

        let (status, said) = ran(&case.json(&commit, &json!({})));

        let alive = child.try_wait().expect("the sleep is looked at").is_none();
        // Best effort: the sleep is this case's own.
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(status, 0, "{said:?}");
        assert!(alive, "the process the stale record's id now names is never signalled");
        assert!(!Path::new(&record).exists(), "the stale record is removed");
    }

    /// Writes the group record of the case naming `group` in this process's session, as an older box's git step wrote
    /// it, naming no leader start, its time set `back` before now.
    fn record_written_back(case: &Case, group: u32, back: Duration) -> String {
        let record = record_of(case);
        let session = own_session();
        fs::write(&record, format!("gate-remote-{KEY}-ffffffffffff\n{group}\n{session}\n")).expect("the record is written");
        let file = fs::File::options().write(true).open(&record).expect("the record opens");
        file.set_modified(SystemTime::now() - back).expect("the record's time is set");
        record
    }

    #[test]
    fn a_record_whose_group_id_is_a_process_started_after_it_was_written_is_removed_and_the_process_left_alone() {
        let case = Case::new("earlier-reused");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let mut child = sleeping_group();
        // The record is older than the process its id now names: a pid reused since the record was written.
        let record = record_written_back(&case, child.id(), Duration::from_secs(120));

        let (status, said) = ran(&case.json(&commit, &json!({})));

        let alive = child.try_wait().expect("the sleep is looked at").is_none();
        // Best effort: the sleep is this case's own.
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(status, 0, "{said:?}");
        assert!(alive, "the process the stale record names is never signalled");
        assert!(!Path::new(&record).exists(), "the stale record is removed");
    }

    /// A shell leading a group of its own that leaves a `sleep 600` in it and exits, reaped: the sleep's pid.
    #[expect(
        clippy::disallowed_methods,
        reason = "the spawn lock is the Windows gate's; this test stands in for an earlier run's git"
    )]
    fn group_without_its_leader() -> (u32, i32) {
        let shell = Command::new("/bin/sh")
            .args(["-c", "sleep 600 >/dev/null 2>&1 & echo $!"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("sh starts");
        let group = shell.id();
        let said = shell.wait_with_output().expect("sh is reaped");
        let member = String::from_utf8_lossy(&said.stdout).trim().parse().expect("the sleep's pid is said");
        (group, member)
    }

    #[test]
    fn a_record_whose_group_lost_its_leader_but_keeps_a_member_is_refused_busy() {
        let case = Case::new("earlier-leaderless");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        case.laid_out(&commit);
        let (group, member) = group_without_its_leader();
        let record = record_written_back(&case, group, Duration::ZERO);

        let (status, said) = ran(&case.json(&commit, &json!({})));

        // SAFETY: `member` is the sleep this case started; the kill only ends it.
        let alive = unsafe { libc::kill(member, 0) } == 0;
        // SAFETY: as above.
        unsafe {
            libc::kill(member, libc::SIGKILL);
        }
        assert_eq!(status, REFUSED_STATUS, "{said:?}");
        assert_eq!(said, [refused_line(NONCE, Refusal::Busy), busy_line(KEY)]);
        assert!(alive, "the group's member is never signalled");
        assert!(Path::new(&record).exists(), "its record stands");
    }

    #[test]
    fn a_malformed_record_is_removed_and_a_folder_at_its_name_refused_layout() {
        let case = Case::new("earlier-malformed");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let record = record_of(&case);
        fs::write(&record, "not a record").expect("the record is written");
        let (status, said) = ran(&case.json(&commit, &json!({})));
        assert_eq!(status, 0, "{said:?}");
        assert!(!Path::new(&record).exists());

        fs::create_dir(&record).expect("a folder takes the record's name");
        let (status, said) = ran(&case.json(&commit, &json!({})));
        assert_eq!(status, REFUSED_STATUS, "{said:?}");
        assert_eq!(said[0], refused_line(NONCE, Refusal::Layout));
    }

    #[test]
    fn a_malformed_outputs_glob_is_a_usage_error_before_any_unbuilt_refusal() {
        let case = Case::new("outputs-shape");
        for (outputs, line) in [
            (json!([""]), "gate: box payload Outputs holds an empty glob"),
            (json!(["a[b"]), "gate: box payload Outputs glob 'a[b' holds '[' or ']'"),
            (json!("bin"), "gate: box payload Outputs is not a list of texts"),
        ] {
            let extra = json!({ "Outputs": outputs, "Desktop": true, "DesktopWait": 60, "LfsDigest": DIGEST });
            let (status, said) = ran(&case.json(&"a".repeat(40), &extra));
            assert_eq!(status, 2, "{line}: {said:?}");
            assert_eq!(said, [line]);
        }
    }

    /// How long a case waits for a run or a restore it expects to end: a hang guard only.
    const HANG_GUARD: Duration = Duration::from_secs(120);
    /// A dotnet tool manifest's text.
    const MANIFEST: &str = "{\n  \"version\": 1\n}\n";
    /// Where `dotnet` reads its tool manifest.
    const MANIFEST_PATH: &str = ".config/dotnet-tools.json";

    thread_local! {
        /// The login shell's `PATH` [`shim_login`] gives this thread's runs.
        static SHIM_PATH: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
    }

    /// The login shell's `PATH` this thread's case set ([`Case::dotnet_shim`]).
    #[expect(clippy::unnecessary_wraps, reason = "a run reads its login PATH through a `LoginShell`, which may fail")]
    fn shim_login() -> Result<Login, String> {
        Ok(Login {
            path: SHIM_PATH.with(|path| path.borrow().clone()),
            godot_root: None,
        })
    }

    /// Writes `script` at `path`, runnable.
    fn runnable(path: &Path, script: &str) {
        fs::write(path, script).expect("the script is written");
        fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o700)).expect("the script is made runnable");
    }

    impl Case {
        /// A `dotnet` in a folder of its own, found first along this thread's login `PATH` ([`shim_login`]), that notes
        /// the folder it ran in and its words in a log, says a line on stderr, then runs `then`; the log's path.
        fn dotnet_shim(&self, then: &str) -> PathBuf {
            let shims = self.base.with_file_name("dotnet-shims");
            fs::create_dir_all(&shims).expect("the shim folder is made");
            let log = self.base.with_file_name("dotnet.log");
            let script = format!(
                "#!/bin/sh\necho \"$(pwd -P) $*\" >> '{log}'\necho restoring >&2\n{then}\n",
                log = log.display()
            );
            runnable(&shims.join("dotnet"), &script);
            let path = std::env::var("PATH").unwrap_or_default();
            SHIM_PATH.with(|cell| *cell.borrow_mut() = format!("{}:{path}", shims.display()));
            log
        }

        /// The work folder's tools record, `<work>.tools`.
        fn tools_record(&self) -> PathBuf {
            PathBuf::from(format!("{}.tools", self.work()))
        }
    }

    /// The restores the dotnet shim noted in `log`, each its folder and words; none when it never ran.
    fn restores(log: &Path) -> Vec<String> {
        fs::read_to_string(log)
            .map(|text| text.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    /// Readies the run of `json` along this thread's login `PATH` ([`shim_login`]); the status, 0 for a run ready for
    /// its command (its lock let go), and the lines it said, each checked to be on stderr.
    fn ran_along_shim(json: &Value) -> (u8, Vec<String>) {
        let payload = decoded(json);
        let output = Arc::new(Output::new(RecordingSink::default()));
        let status = prepare(&payload, &output, quiet_periods(), shim_login).map_or_else(
            |status| status,
            |mut ready| {
                tmp_cleared(&mut ready);
                0
            },
        );
        (status, stderr_of(&output))
    }

    /// Whether `said` holds the line of a restore's time.
    fn said_took(said: &[String]) -> bool {
        said.iter().any(|line| line.starts_with("gate: dotnet tool restore took "))
    }

    #[test]
    fn a_tree_with_no_tool_manifest_restores_nothing() {
        let case = Case::new("tools-none");
        let log = case.dotnet_shim("exit 0");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let (status, said) = ran_along_shim(&case.json(&commit, &json!({})));
        assert_eq!(status, 0, "{said:?}");
        assert!(restores(&log).is_empty(), "no restore ran: {said:?}");
        assert!(!said_took(&said), "{said:?}");
        assert!(!case.tools_record().exists(), "no tools record");
    }

    #[test]
    fn a_changed_manifest_runs_the_restore_once_and_records_its_hash() {
        let case = Case::new("tools-changed");
        let log = case.dotnet_shim("exit 0");
        let first = case.snapshot(&[(MANIFEST_PATH, MANIFEST)]);
        let (status, said) = ran_along_shim(&case.json(&first, &json!({})));
        assert_eq!(status, 0, "{said:?}");
        let work = fs::canonicalize(case.work()).expect("the work folder is there");
        assert_eq!(
            restores(&log),
            [format!("{} tool restore", work.display())],
            "one restore in the work folder"
        );
        assert!(said_took(&said), "{said:?}");
        let recorded = fs::read_to_string(case.tools_record()).expect("the tools record is written");
        assert!(
            recorded.len() == 64 && recorded.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "a SHA-256 in hex: {recorded:?}"
        );

        let second = case.snapshot(&[(MANIFEST_PATH, "{\n  \"version\": 2\n}\n")]);
        let (status, said) = ran_along_shim(&case.json(&second, &json!({})));
        assert_eq!(status, 0, "{said:?}");
        assert_eq!(restores(&log).len(), 2, "one more restore for the changed manifest: {said:?}");
        let rerecorded = fs::read_to_string(case.tools_record()).expect("the tools record is written");
        assert_ne!(rerecorded, recorded, "the changed manifest's hash is recorded");
    }

    #[test]
    fn a_pair_run_never_restores_from_a_manifest_above_its_main_tree() {
        let case = Case::new("tools-pair-bound");
        let log = case.dotnet_shim("exit 0");
        let main = case.snapshot(&[("a.txt", "a\n")]);
        let lib = case.lib_snapshot(&[("b.txt", "b\n")]);
        let json = case.pair_json(&main, &lib, &json!({}));
        let (status, said) = ran_along_shim(&json);
        assert_eq!(status, 0, "{said:?}");
        // The work folder holds the pair's trees; a manifest beside them, as a command may leave one, is outside the
        // main tree the walk is bounded by.
        let above = Path::new(&case.work()).join(MANIFEST_PATH);
        fs::create_dir_all(above.parent().expect("the manifest has a folder")).expect("the manifest's folder is made");
        fs::write(&above, MANIFEST).expect("the manifest is written");
        let (status, said) = ran_along_shim(&json);
        assert_eq!(status, 0, "{said:?}");
        assert!(above.is_file(), "the layout kept the work folder's own files: {said:?}");
        assert!(restores(&log).is_empty(), "no restore ran: {said:?}");
        assert!(!case.tools_record().exists(), "no tools record");
    }

    #[test]
    fn an_unchanged_manifest_skips_the_restore() {
        let case = Case::new("tools-unchanged");
        let log = case.dotnet_shim("exit 0");
        let first = case.snapshot(&[(MANIFEST_PATH, MANIFEST)]);
        let (status, said) = ran_along_shim(&case.json(&first, &json!({})));
        assert_eq!(status, 0, "{said:?}");
        assert_eq!(restores(&log).len(), 1, "the first run restores: {said:?}");

        let second = case.snapshot(&[("b.txt", "b\n")]);
        let (status, said) = ran_along_shim(&case.json(&second, &json!({})));
        assert_eq!(status, 0, "{said:?}");
        assert_eq!(restores(&log).len(), 1, "no restore for the unchanged manifest: {said:?}");
        assert!(!said_took(&said), "{said:?}");
    }

    #[test]
    fn a_restore_that_fails_refuses_the_run_75_with_the_tools_tag() {
        let case = Case::new("tools-fails");
        let seen = case.base.with_file_name("restore-tmpdir");
        let log = case.dotnet_shim(&format!("printf %s \"$TMPDIR\" > '{}'\nexit 3", seen.display()));
        let commit = case.snapshot(&[(MANIFEST_PATH, MANIFEST)]);
        let (status, said) = ran_along_shim(&case.json(&commit, &json!({})));
        assert_eq!(status, REFUSED_STATUS, "{said:?}");
        assert_eq!(restores(&log).len(), 1, "{said:?}");
        let tmpdir = fs::read_to_string(&seen).expect("the restore wrote its TMPDIR");
        assert!(
            tmpdir.starts_with("/tmp/gate.") && tmpdir.ends_with('/'),
            "the restore ran in the run's folder: {tmpdir}"
        );
        assert!(!Path::new(&tmpdir).exists(), "the refused run removed its folder: {tmpdir}");
        let failed = said.iter().position(|line| line == "gate: remote tool restore failed (status 3)");
        assert!(
            failed.is_some_and(|at| said[at + 1..] == [refused_line(NONCE, Refusal::Tools), tools_line(KEY)]),
            "the failed line, then the tools tag and line, last: {said:?}"
        );
        assert!(!case.tools_record().exists(), "no tools record");
    }

    #[test]
    fn a_missing_dotnet_refuses_the_run_75_with_the_tools_tag() {
        let case = Case::new("tools-missing");
        let empty = case.base.with_file_name("no-dotnet");
        fs::create_dir_all(&empty).expect("the empty folder is made");
        // The git steps run the real git by its path; the login PATH holds no dotnet.
        test_program::set(&real_git());
        SHIM_PATH.with(|cell| *cell.borrow_mut() = empty.display().to_string());
        let commit = case.snapshot(&[(MANIFEST_PATH, MANIFEST)]);
        let (status, said) = ran_along_shim(&case.json(&commit, &json!({})));
        assert_eq!(status, REFUSED_STATUS, "{said:?}");
        let start = said
            .iter()
            .position(|line| line.starts_with("gate: remote could not start the dotnet tool restore: "));
        let after = [
            "gate: remote tool restore failed (status 1)".to_owned(),
            refused_line(NONCE, Refusal::Tools),
            tools_line(KEY),
        ];
        assert!(
            start.is_some_and(|at| said[at + 1..] == after),
            "the could-not-start line, the failed line, then the tools tag and line, last: {said:?}"
        );
        assert!(!case.tools_record().exists(), "no tools record");
    }

    #[test]
    fn a_stop_during_the_restore_ends_its_group() {
        let case = Case::new("tools-stop");
        let (status, said) = stopped_restore(&case);
        assert_eq!(status, 1, "{said:?}");
        assert!(said.iter().any(|line| line == "gate: remote tool restore failed (signal 15)"), "{said:?}");
    }

    #[test]
    fn a_restore_under_a_box_that_ignores_sigterm_still_ends_on_the_first_signal() {
        let case = Case::new("tools-stop-ignored");
        // An ssh session can start the box with SIGTERM ignored, which a child keeps across its exec unless reset.
        // SAFETY: `SIG_IGN` for SIGTERM in this case's own process (nextest runs each case in one) touches no memory.
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        }
        let (status, said) = stopped_restore(&case);
        assert_eq!(status, 1, "{said:?}");
        assert!(
            said.iter().any(|line| line == "gate: remote tool restore failed (signal 15)"),
            "the restore ends on the stop's SIGTERM, not its SIGKILL: {said:?}"
        );
    }

    /// Readies a run of `case` whose restore waits, stops it with `box stop` once the restore has started, and checks
    /// that its group record is removed, no tools record written and the restore ran once; the run's status and lines.
    fn stopped_restore(case: &Case) -> (u8, Vec<String>) {
        let started = case.base.with_file_name("restore-started");
        let log = case.dotnet_shim(&format!(": > '{}'\nsleep 600", started.display()));
        let commit = case.snapshot(&[(MANIFEST_PATH, MANIFEST)]);
        let json = case.json(&commit, &json!({}));
        let (path, run_json) = (shim_login().expect("the shim's PATH is set").path, json.clone());
        let (sent, got) = mpsc::channel();
        thread::spawn(move || {
            SHIM_PATH.with(|cell| *cell.borrow_mut() = path);
            let _ = sent.send(ran_along_shim(&run_json));
        });
        let guard = Instant::now() + HANG_GUARD;
        while !started.exists() {
            let ended = got.try_recv();
            assert!(ended.is_err(), "the run ended before its restore started: {ended:?}");
            assert!(Instant::now() < guard, "the restore never started");
            thread::sleep(Duration::from_millis(50));
        }
        let record = format!("{}.tools.pgid", case.work());
        // The run adds the restore's leader's start to the record once it started; only then may a stop signal it.
        let names_a_start = || {
            fs::read(&record)
                .ok()
                .and_then(|bytes| recorded_group(&bytes))
                .is_some_and(|recorded| recorded.started.is_some())
        };
        while !names_a_start() {
            assert!(Instant::now() < guard, "the restore's group and its leader's start are never recorded");
            thread::sleep(Duration::from_millis(50));
        }
        let mut console = Console::captured();
        assert_eq!(stop(&decoded(&json), &mut console), 0, "the stop ends the restore's group");
        let ended = got.recv_timeout(HANG_GUARD).expect("the run ends once its restore is stopped");
        assert!(!Path::new(&record).exists(), "the restore's group record is removed");
        assert!(!case.tools_record().exists(), "no tools record");
        assert_eq!(restores(&log).len(), 1);
        ended
    }

    /// The restore's group record of the case's work folder.
    fn tools_group_of(case: &Case) -> String {
        format!("{}.tools.pgid", case.work())
    }

    #[test]
    fn a_live_restore_group_left_by_an_earlier_run_refuses_the_next_run_busy() {
        let case = Case::new("tools-earlier-live");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        case.laid_out(&commit);
        let mut child = sleeping_group();
        let record = tools_group_of(&case);
        fs::write(&record, live_record_text(&format!("gate-remote-{KEY}-ffffffffffff-tools"), child.id())).expect("the record is written");

        let (status, said) = ran(&case.json(&commit, &json!({})));

        let alive = child.try_wait().expect("the sleep is looked at").is_none();
        // Best effort: the sleep is this case's own.
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(status, REFUSED_STATUS, "{said:?}");
        assert_eq!(said, [refused_line(NONCE, Refusal::Busy), busy_line(KEY)]);
        assert!(alive, "the earlier restore's group is never signalled");
        assert!(Path::new(&record).exists(), "its record stands");
    }

    #[test]
    fn a_restore_group_record_naming_no_process_is_removed_and_the_run_goes_on() {
        let case = Case::new("tools-earlier-dead");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let mut child = sleeping_group();
        let group = i32::try_from(child.id()).expect("a pid is a pid_t");
        child.kill().expect("the sleep is killed");
        child.wait().expect("the sleep is reaped");
        // SAFETY: signal 0 only asks whether the group exists; it touches no memory.
        assert!(unsafe { libc::killpg(group, 0) } != 0, "the group is gone");
        let record = tools_group_of(&case);
        fs::create_dir_all(Path::new(&record).parent().expect("the record has a folder")).expect("the folder is made");
        let text = job_record_text(&format!("gate-remote-{KEY}-ffffffffffff-tools"), group, own_session(), (1, 0));
        fs::write(&record, text).expect("the record is written");

        let (status, said) = ran(&case.json(&commit, &json!({})));

        assert_eq!(status, 0, "{said:?}");
        assert_eq!(case.head(), commit);
        assert!(!Path::new(&record).exists(), "the stale record is removed");
    }

    #[test]
    fn a_live_packages_group_left_by_an_earlier_run_refuses_the_next_run_busy() {
        let case = Case::new("packages-earlier-live");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        case.laid_out(&commit);
        let mut child = sleeping_group();
        let record = format!("{}.packages.pgid", case.work());
        fs::write(&record, live_record_text(&format!("gate-remote-{KEY}-ffffffffffff-packages"), child.id())).expect("the record is written");

        let (status, said) = ran(&case.json(&commit, &json!({})));

        let alive = child.try_wait().expect("the sleep is looked at").is_none();
        // Best effort: the sleep is this case's own.
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(status, REFUSED_STATUS, "{said:?}");
        assert_eq!(said, [refused_line(NONCE, Refusal::Busy), busy_line(KEY)]);
        assert!(alive, "the earlier install's group is never signalled");
        assert!(Path::new(&record).exists(), "its record stands");
    }

    #[test]
    fn a_packages_group_record_naming_no_process_is_removed_and_the_run_goes_on() {
        let case = Case::new("packages-earlier-dead");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let mut child = sleeping_group();
        let group = i32::try_from(child.id()).expect("a pid is a pid_t");
        child.kill().expect("the sleep is killed");
        child.wait().expect("the sleep is reaped");
        // SAFETY: signal 0 only asks whether the group exists; it touches no memory.
        assert!(unsafe { libc::killpg(group, 0) } != 0, "the group is gone");
        let record = format!("{}.packages.pgid", case.work());
        fs::create_dir_all(Path::new(&record).parent().expect("the record has a folder")).expect("the folder is made");
        let text = job_record_text(&format!("gate-remote-{KEY}-ffffffffffff-packages"), group, own_session(), (1, 0));
        fs::write(&record, text).expect("the record is written");

        let (status, said) = ran(&case.json(&commit, &json!({})));

        assert_eq!(status, 0, "{said:?}");
        assert_eq!(case.head(), commit);
        assert!(!Path::new(&record).exists(), "the stale record is removed");
    }

    #[test]
    fn a_restore_whose_record_cannot_be_written_refuses_the_run_75() {
        let case = Case::new("tools-record-unwritable");
        let log = case.dotnet_shim("exit 0");
        let commit = case.snapshot(&[(MANIFEST_PATH, MANIFEST)]);
        // A folder at every temp name the restore's group record can be staged under in this process, which the
        // staging cannot remove: the record is never readied, so the restore never starts.
        let record = tools_group_of(&case);
        for number in 0..1000 {
            let temp = format!("{record}.gate-new-{}-{number}", std::process::id());
            fs::create_dir_all(Path::new(&temp).join("kept")).expect("a folder takes the temp name");
        }
        let (status, said) = ran_along_shim(&case.json(&commit, &json!({})));
        assert_eq!(status, REFUSED_STATUS, "{said:?}");
        let start = said
            .iter()
            .position(|line| line.starts_with("gate: remote could not start the dotnet tool restore: "));
        let after = [
            "gate: remote tool restore failed (status 1)".to_owned(),
            refused_line(NONCE, Refusal::Tools),
            tools_line(KEY),
        ];
        assert!(
            start.is_some_and(|at| said[at + 1..] == after),
            "the could-not-start line, the failed line, then the tools tag and line, last: {said:?}"
        );
        assert!(restores(&log).is_empty(), "the restore never ran");
        assert!(!case.tools_record().exists(), "no tools record");
    }

    #[test]
    fn an_index_source_that_is_a_fifo_fails_the_sync_back_without_hanging() {
        let case = Case::new("back-index-fifo");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        let json = case.commanded(&commit, &["/bin/sh", "-c", "echo x > x.txt; rm -f .git/index; mkfifo .git/index"]);
        let (sent, got) = mpsc::channel();
        thread::spawn(move || {
            let _ = sent.send(whole_run(&json));
        });
        let (status, said) = got.recv_timeout(HANG_GUARD).expect("the run ends without waiting on the FIFO");
        assert_eq!(status, 1, "{said:?}");
        let work = case.work();
        let failed = format!("gate: remote sync-back failed: cannot copy {work}/.git/index to {work}.index: ");
        assert!(
            said.last()
                .is_some_and(|(stream, line)| *stream == Stream::Err && line.starts_with(&failed)),
            "{said:?}"
        );
        assert!(!case.has_back_ref(), "no back commit");
    }
}
