//! `gate-box box run`: the request read and checked as the Windows box reads it, what this builder does not build
//! refused so the PC runs the command itself, the work root checked, the work lock taken, the subfolder placed, the work
//! folder laid out and the payload's snapshot checked out; then the command run and the tree it left synced back
//! ([`run`]). Each line is said as the Windows box says it, byte for byte.

use std::ffi::{CStr, OsStr};
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gate_args::BoxSeams;
use gate_wire::{PathForm, Payload, Refusal, bare_stem, busy_line, escaped, layout_line, refused_line};

use gate_watchdog::Pump;

use crate::back::{self, BackFailed};
use crate::command::{Launch, Sampler, run_command};
use crate::console::{Forwarding, LineSink, Output, Stream};
use crate::files::{self, HeldLock, Locked};
use crate::git::{self, AlternatesFailure, Failed, Git, GitClass, RunGit, Steps, Tree, line_ends_set, origin_set};
use crate::heartbeat::{self, Periods};
use crate::lines::{
    Along, CommandEnd, FAILED_STATUS, Translation, USAGE_STATUS, command_status, login_path_line, recorded_group, rosetta_lines, signal_guard,
    translation, work_layout,
};
use crate::preflight::checked_root;
use crate::process::{self, GroupRecord};
use crate::stop::{Record, own_group, record_current, records_folder};
use crate::tools;

/// The status of a run refused: another run holds the work folder, it could not be laid out, or it asks for what this
/// builder does not build. Said with a refusal tag and before any start marker, so the PC runs the command itself.
pub const REFUSED_STATUS: u8 = 75;

/// What a run asks for that this builder does not build, refused with the tag of its refusal.
#[derive(Clone, Copy, Debug)]
struct Unbuilt {
    refusal: Refusal,
    /// The payload key that asks for it.
    key: &'static str,
}

impl Unbuilt {
    /// The line said after the refusal's tag: the layout line naming the repo of the bare repository `bare` for a
    /// `layout` refusal ([`layout_line`]), else `gate: remote <key> is not built on this builder`.
    fn line(self, bare: &str) -> String {
        let why = format!("{} is not built on this builder", self.key);
        if self.refusal == Refusal::Layout {
            layout_line(repo_name(bare), &escaped(&why))
        } else {
            format!("gate: remote {why}")
        }
    }
}

/// What a run reads of its payload.
pub struct Request<'p> {
    /// The commit checked out, 40 lower-case hex digits.
    pub snapshot: &'p str,
    /// The work folder's key.
    pub key: &'p str,
    /// The work folder, `<root>/work/<key>`, which holds the repo's tree.
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
    /// The repo's bare repository, which the snapshot is fetched from.
    pub bare: &'p str,
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
    /// The first thing the payload asks for that this builder does not build, in the order the payload is read.
    unbuilt: Option<Unbuilt>,
}

impl<'p> Request<'p> {
    /// Reads `payload` in the order the Windows box reads it, each key checked as it does; then notes the first key
    /// asking for what this builder does not build: `Outputs`, `OutputsFull`, `Desktop`, a warm-up's
    /// `WarmCommands`, `GodotVersion`, `EngineEnv`, `UnrealVersion`, `InputsDigest`, `SentDigest`, `LfsDigest` or
    /// `Pair`. A work root holding a `:` is refused just after the work folder is read ([`work_layout`]).
    ///
    /// # Errors
    ///
    /// The usage line of the first key that cannot be read.
    pub fn read(payload: &'p Payload) -> Result<Request<'p>, String> {
        let snapshot = payload.snapshot()?;
        let (key, work) = payload.work_folder()?;
        let (root, parent) = work_layout(work)?;
        let (globs, full) = (payload.output_globs()?, payload.outputs_full()?);
        let nonce = payload.nonce()?;
        payload.output_folder(work)?;
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
        let godot = payload.godot_version()?.is_some();
        let engine_env = !payload.engine_env()?.is_empty();
        let unreal = payload.unreal_version()?.is_some();
        let inputs = payload.inputs_digest()?.is_some();
        let sent = payload.sent_digest()?.is_some();
        let lfs = payload.lfs_digest()?.is_some();
        let pair = payload.pair()?.is_some();
        let unbuilt = [
            (!globs.is_empty(), Refusal::Layout, "Outputs"),
            (full, Refusal::Layout, "OutputsFull"),
            (desktop, Refusal::Desktop, "Desktop"),
            (warm, Refusal::Layout, "WarmCommands"),
            (godot, Refusal::Layout, "GodotVersion"),
            (engine_env, Refusal::Layout, "EngineEnv"),
            (unreal, Refusal::Layout, "UnrealVersion"),
            (inputs, Refusal::Inputs, "InputsDigest"),
            (sent, Refusal::Sent, "SentDigest"),
            (lfs, Refusal::Lfs, "LfsDigest"),
            (pair, Refusal::Layout, "Pair"),
        ]
        .into_iter()
        .find_map(|(asked, refusal, key)| asked.then_some(Unbuilt { refusal, key }));
        Ok(Request {
            snapshot,
            key,
            work,
            root,
            parent,
            nonce,
            timeout,
            host,
            bare,
            lock,
            index,
            job,
            command: commands.words.into_iter().next().unwrap_or_default(),
            subfolder,
            identity,
            env,
            unbuilt,
        })
    }
}

/// A run ready for its command: the work lock held until this is dropped, the snapshot checked out, and the folder
/// the command runs in.
pub struct Ready<'p> {
    /// What the run read of its payload.
    pub request: Request<'p>,
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
    /// The run's own temp folder, the command's `TMPDIR`, removed at the run's end ([`temp_removed`]); `None` when the
    /// caller's `Env` sets `TMPDIR`.
    pub temp: Option<RunTemp>,
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
/// user, and never at a name that is already there, so no other account can plant it or a link in its place.
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
    let made = fs::symlink_metadata(&path).inspect_err(|_| {
        // The run is refused with this look's error; the folder just made is empty, and `remove_dir` takes no link.
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
fn temp_for<S: LineSink>(request: &Request<'_>, output: &Output<S>) -> Result<Option<RunTemp>, u8> {
    if request.env.iter().any(|(name, _)| name == "TMPDIR") {
        return Ok(None);
    }
    temp_made().map(Some).map_err(|error| {
        let line = format!("gate: remote could not make the run's temp folder in /tmp: {error}");
        refused(output, request, Refusal::Layout, &line)
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
            let _ = output.say(Stream::Err, &line);
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
            let _ = output.say(Stream::Err, &line);
        }
    }
}

/// Where a run reads the login shell's `PATH` from: [`process::login_path`], or a test's own.
pub type LoginPath = fn() -> Result<String, String>;

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
/// manifest changed ([`tools::update`]), with the folder as its `TMPDIR`, a restore that fails refused `tools` once the
/// folder is removed ([`temp_removed`]). Each git step's process group is recorded in
/// `<work>.pgid` under the run's job while it runs. The heartbeat starts once the subfolder is placed, before the layout
/// and the checkout, beating every `periods.heartbeat` ([`heartbeat::start`]). The login shell's `PATH` is read from
/// `login` just before the first git step, after every refusal said before it ([`login_path_read`]). No start marker
/// is said.
///
/// # Errors
///
/// The status the run ends on, its lines said.
pub fn prepare<'p, S: LineSink + 'static>(
    payload: &'p Payload,
    output: &Arc<Output<S>>,
    periods: Periods,
    login: LoginPath,
) -> Result<Ready<'p>, u8> {
    prepared(payload, output, periods, login, heartbeat::start)
}

/// The login shell's `PATH` read from `login`, its empty and relative folders left out; `None`, with the box's own
/// `PATH` kept and [`login_path_line`] said, when it cannot be read or holds no absolute folder.
fn login_path_read<S: LineSink>(output: &Output<S>, login: LoginPath) -> Option<String> {
    let read = login().and_then(|path| {
        let folders: Vec<&str> = path.split(':').filter(|folder| folder.starts_with('/')).collect();
        if folders.is_empty() {
            Err("said a PATH with no absolute folder".to_owned())
        } else {
            Ok(folders.join(":"))
        }
    });
    read.inspect_err(|why| {
        // A note: the run goes on, and a client gone finds out at the next line.
        let _ = output.say(Stream::Err, &login_path_line(why));
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
    )
}

/// How a run's watches look and where its ceiling reads the machine's load from: the defaults, or the test seams'.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Watches {
    /// How often the heartbeat looks and the ceiling samples.
    pub periods: Periods,
    /// Where the ceiling reads the machine's load from.
    pub sampler: Sampler,
}

impl Watches {
    /// The watches `seams` ask for: `--test-heartbeat-ms` and `--test-box-sample-ms` over [`Periods::default`];
    /// `--test-box-sampler-fail`, else `--test-box-free-percent`, over [`Sampler::Machine`], as the Windows box reads
    /// them. All defaults in a box built without the `test-seams` feature.
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
        Watches { periods, sampler }
    }
}

/// Runs `payload`'s run, saying its lines through `output`, watched as `watches` says; the status the box exits with.
///
/// The login shell's `PATH` is read once, by [`prepare`] ([`process::login_path`]); the git steps and the command run
/// along it ([`run_along`]).
pub fn run<S: LineSink + 'static>(payload: &Payload, output: &Arc<Output<S>>, watches: Watches) -> u8 {
    run_along(payload, output, watches, process::login_path)
}

/// [`run`], the login shell's `PATH` read from `login`, in the Windows box's order: the run readied ([`prepare`]), the
/// heartbeat beating from the subfolder's check; the command run ([`run_command`]); the back commit made and pushed
/// when the command reached an end of its own ([`synced_back`]); then the run's temp folder removed
/// ([`temp_removed`]), the heartbeat stopped ([`heartbeat::stop`]), and the work lock let go.
fn run_along<S: LineSink + 'static>(payload: &Payload, output: &Arc<Output<S>>, watches: Watches, login: LoginPath) -> u8 {
    let ready = match prepare(payload, output, watches.periods, login) {
        Ok(ready) => ready,
        Err(status) => return status,
    };
    let status = commanded(&ready, output, watches.sampler);
    let Ready { pump, lock, temp, .. } = ready;
    if let Some(temp) = temp {
        temp_removed(temp, output);
    }
    if let Some(pump) = pump {
        heartbeat::stop(pump);
    }
    drop(lock);
    status
}

/// The status of the run `ready` once its command has run, sampled from `sampler`: the command's own
/// ([`command_status`]), the back commit made only after an end of its own ([`of_its_own`]).
fn commanded<S: LineSink + 'static>(ready: &Ready<'_>, output: &Arc<Output<S>>, sampler: Sampler) -> u8 {
    let request = &ready.request;
    let login_path = ready.login_path.as_deref();
    let launch = Launch {
        words: &request.command,
        place: &ready.place,
        work: request.work,
        job: request.job,
        nonce: request.nonce,
        env: &request.env,
        login_path,
        tmp: ready.temp.as_ref().map(RunTemp::tmpdir),
        lock: &ready.lock,
        timeout: request.timeout,
        host: request.host,
        sample: ready.periods.sample,
        sampler,
        pump: ready.pump.as_ref(),
    };
    let end = run_command(&launch, output);
    let status = command_status(end);
    if !of_its_own(end) {
        return status;
    }
    synced_back(ready, output, status)
}

/// Whether the command that ended as `end` reached an end of its own, an exit or a signal the box did not send, which
/// alone is followed by a back commit: one the box killed for its ceiling or backstop, whose client is gone, that never
/// started or that the box could not see to its end makes none, as the PC applies nothing of it. A `box stop`'s
/// `SIGTERM` reads as any other signal.
fn of_its_own(end: CommandEnd) -> bool {
    matches!(end, CommandEnd::Exited(_) | CommandEnd::Signalled(_))
}

/// The status of the run `ready`, whose command ended with `status`, once its back commit was made and pushed
/// ([`back::back_commit`]): `status`; a back commit that failed says `gate: remote sync-back failed: <why>` and gives
/// [`FAILED_STATUS`] for a command that exited 0, else `status`; a client gone gives the closed status.
fn synced_back<S: LineSink + 'static>(ready: &Ready<'_>, output: &Arc<Output<S>>, status: u8) -> u8 {
    match back::back_commit(&ready.request, &ready.lock, ready.login_path.as_deref(), output) {
        Ok(_) => status,
        Err(BackFailed::Gone(failed)) => failure(output, &failed),
        Err(BackFailed::Step(why)) => {
            let failed = if status == 0 { FAILED_STATUS } else { status };
            said(output, &[&format!("gate: remote sync-back failed: {why}")], failed)
        }
    }
}

/// How a run starts its heartbeat: [`heartbeat::start`], or a test's own.
type StartBeat = fn(Arc<dyn Forwarding>, Duration) -> io::Result<Pump>;

/// [`prepare`], the heartbeat started with `start`; a heartbeat that cannot start is said and the run goes on without
/// one.
fn prepared<'p, S: LineSink + 'static>(
    payload: &'p Payload,
    output: &Arc<Output<S>>,
    periods: Periods,
    login: LoginPath,
    start: StartBeat,
) -> Result<Ready<'p>, u8> {
    let request = Request::read(payload).map_err(|line| said(output, &[&line], USAGE_STATUS))?;
    if let Some(unbuilt) = request.unbuilt {
        return Err(refused(output, &request, unbuilt.refusal, &unbuilt.line(request.bare)));
    }
    match translation(crate::command::proc_translated()) {
        Translation::Native => {}
        Translation::Unread(line) => {
            // A note: the run goes on, and a client gone finds out at the next line.
            let _ = output.say(Stream::Err, &line);
        }
        Translation::Rosetta => {
            let [tag, line] = rosetta_lines(request.nonce);
            return Err(said(output, &[&tag, &line], REFUSED_STATUS));
        }
    }
    root_checked(&request).map_err(|(line, status)| said(output, &[&line], status))?;
    let lock = match files::take_lock(Path::new(request.lock)) {
        Ok(Locked::Held(lock)) => lock,
        Ok(Locked::Busy) => return Err(refused(output, &request, Refusal::Busy, &busy_line(request.key))),
        Err(error) => {
            let line = format!("gate: remote could not open the work folder lock {}: {error}", request.lock);
            return Err(said(output, &[&line], FAILED_STATUS));
        }
    };
    // Before the sweep, so a git an earlier run left running keeps its lock files.
    earlier_group_cleared(&request).map_err(|not| not_laid_out(output, &request, not))?;
    // Before the run's first git step, so no lock file an earlier run's ended git left fails it.
    swept(&request).map_err(|not| not_laid_out(output, &request, not))?;
    // Refused before the layout, so a payload naming a folder outside the work folder changes nothing there; the
    // earlier run's back ref goes, as the checkout's would, so the client finds none to fetch.
    let Some(place) = place_of(request.work, request.subfolder) else {
        said(output, &[&not_inside(request.subfolder)], USAGE_STATUS);
        let login_path = login_path_read(output, login);
        let git = run_git(&request, &lock, login_path.as_deref(), output);
        back_ref_cleared(&git, &request).map_err(|failed| failure(output, &failed))?;
        return Err(USAGE_STATUS);
    };
    // A heartbeat that cannot start ends the heartbeats, never the run. On an error below the pump is dropped, which
    // tells it to stop, and the box exits right after.
    let pump = start(Arc::clone(output) as Arc<dyn Forwarding>, periods.heartbeat)
        .inspect_err(|error| {
            // A note: the run goes on, and a client gone finds out at the next line.
            let _ = output.say(
                Stream::Err,
                &format!("gate: remote heartbeat stopped: cannot start the heartbeat: {error}"),
            );
        })
        .ok();
    let fresh = layout_checked(&request).map_err(|not| not_laid_out(output, &request, not))?;
    let login_path = login_path_read(output, login);
    let git = run_git(&request, &lock, login_path.as_deref(), output);
    laid_out(&request, &git, fresh).map_err(|not| not_laid_out(output, &request, not))?;
    checked_out(&request, &git).map_err(|failed| failure(output, &failed))?;
    if !place.is_dir() {
        let line = format!("gate: remote subfolder {} does not exist in the snapshot", escaped(request.subfolder));
        return Err(said(output, &[&line], USAGE_STATUS));
    }
    // A link the snapshot holds may lead the place out of the work folder, which only the checkout shows.
    if !inside_work(request.work, &place) {
        return Err(said(output, &[&not_inside(request.subfolder)], USAGE_STATUS));
    }
    let temp = temp_for(&request, output)?;
    let along = Along {
        path: login_path.as_deref(),
        tmp: temp.as_ref().map(RunTemp::tmpdir),
    };
    if let Err(status) = tools::update(&request, &place, &lock, along, output) {
        if let Some(temp) = temp {
            temp_removed(temp, output);
        }
        return Err(status);
    }
    Ok(Ready {
        request,
        lock,
        place,
        pump,
        periods,
        login_path,
        temp,
    })
}

/// Says `lines` on stderr together, unless the client is gone; `status`.
fn said<S: LineSink>(output: &Output<S>, lines: &[&str], status: u8) -> u8 {
    // A client that has gone has no one left to tell, and the status still ends the ssh session; one still connected
    // but no longer reading would hold the write, and the output lock, for good.
    if output.client_gone() {
        return status;
    }
    let lines: Vec<(Stream, &str)> = lines.iter().map(|line| (Stream::Err, *line)).collect();
    let _ = output.say_together(&lines);
    status
}

/// Says the lines of `failed`; its status.
fn failure<S: LineSink>(output: &Output<S>, failed: &Failed) -> u8 {
    let lines: Vec<&str> = failed.lines.iter().map(String::as_str).collect();
    said(output, &lines, failed.status)
}

/// Refuses the run of `request` for `refusal`: its tag, then `line`, said together; [`REFUSED_STATUS`].
fn refused<S: LineSink>(output: &Output<S>, request: &Request<'_>, refusal: Refusal, line: &str) -> u8 {
    said(output, &[&refused_line(request.nonce, refusal), line], REFUSED_STATUS)
}

/// Ends the run of `request` whose work folder could not be laid out as `not` says: refused `layout` with the layout
/// line naming the repo and why, or failed with a step's lines; the status.
fn not_laid_out<S: LineSink>(output: &Output<S>, request: &Request<'_>, not: NotLaidOut) -> u8 {
    match not {
        NotLaidOut::Busy => refused(output, request, Refusal::Busy, &busy_line(request.key)),
        NotLaidOut::Refused(why) => {
            let line = layout_line(repo_name(request.bare), &escaped(&why));
            refused(output, request, Refusal::Layout, &line)
        }
        NotLaidOut::Failed(failed) => failure(output, &failed),
    }
}

/// `gate: remote subfolder <sub> is not a folder inside the work folder`, as the Windows box says it.
fn not_inside(sub: &str) -> String {
    format!("gate: remote subfolder {} is not a folder inside the work folder", escaped(sub))
}

/// Checks, making nothing and before the lock, what the run's lock and git steps lie in, as the preflight checks it:
/// `Bare` at `<root>/<name>.git` ([`checked_root`]); the work root trusted and `<root>/work` the box user's own folder
/// when they are there ([`records_folder`]); and `Bare` the box user's own folder when it is there
/// ([`files::unusable_folder`]). Else the line and the status.
fn root_checked(request: &Request<'_>) -> Result<(), (String, u8)> {
    checked_root(request.work, request.bare, false).map_err(|line| (line, USAGE_STATUS))?;
    records_folder(&request.root, request.parent)?;
    files::unusable_folder(Path::new(request.bare)).map_or(Ok(()), |line| Err((line, FAILED_STATUS)))
}

/// The name routing gives the repo of the bare repository `bare`: the stem of `<name>.git` ([`bare_stem`]), or the
/// bare repository's whole file name when it is not of that form.
fn repo_name(bare: &str) -> &str {
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
enum NotLaidOut {
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

/// Looks at each group record an earlier run of `request`'s work folder left, its git step's `<work>.pgid` and its tool
/// restore's `<work>.tools.pgid` ([`earlier_record_cleared`]), a box that ended without removing it:
/// whatever job it names, a group that is still the one its run made ([`record_current`]) refuses the run
/// [`NotLaidOut::Busy`], the group never signalled and its git's lock files never swept; a record naming no group a
/// run makes (malformed, init, or the box's own), or a group that is no longer its run's (no process left, one the box
/// may not signal, or its id another's since), is removed under the work lock and the run goes on. A record that
/// cannot be read, a link or a folder at its name, refuses the run `layout`.
fn earlier_group_cleared(request: &Request<'_>) -> Result<(), NotLaidOut> {
    for record in [group_record(request.work), tools::tools_group_record(request.work)] {
        earlier_record_cleared(&record)?;
    }
    Ok(())
}

/// [`earlier_group_cleared`] for the one group record `record`: a git step's `<work>.pgid` or a tool restore's
/// `<work>.tools.pgid`.
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
/// git step, as the Windows box sweeps them: the copied index's `<work>.index.lock`; the tree's `.git/index.lock`,
/// `.git/HEAD.lock` and `.git/config.lock`; and in the bare repository the run's own back ref's
/// `refs/gate-back/<key>.lock`, nothing else there touched. A folder on the way that is a link, not a folder or missing
/// has none of its locks looked at; a link, a folder or anything but a file at a lock's name refuses the run `layout`.
fn swept(request: &Request<'_>) -> Result<(), NotLaidOut> {
    if files::plain_folder(request.parent) {
        lock_removed(Path::new(&format!("{}.index.lock", request.work)))?;
    }
    let work = Path::new(request.work);
    let dot_git = work.join(".git");
    if files::plain_folder(work) && files::plain_folder(&dot_git) {
        for lock in ["index.lock", "HEAD.lock", "config.lock"] {
            lock_removed(&dot_git.join(lock))?;
        }
    }
    let bare = Path::new(request.bare);
    let refs = bare.join("refs");
    let gate_back = refs.join("gate-back");
    if [bare, &refs, &gate_back].into_iter().all(files::plain_folder) {
        lock_removed(&gate_back.join(format!("{}.lock", request.key)))?;
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

/// Checks the work folder of `request` before its layout, under the work lock: the interrupted-command record
/// `<work>.command` refused unless it is missing or a file itself, not followed, as the run could neither write nor
/// remove anything else there; when it is there, the warm record `<work>.warm` and the tools record `<work>.tools` removed, so neither
/// trusts a tree a command was cut short in, the checkout remaking the tree; the work folder failed unless it is
/// missing or the box user's own folder ([`files::unusable_folder`]); `.git`, `.git/objects` and `.git/objects/info`
/// refused unless each is missing or a folder itself, and `.git/config` and `.git/HEAD` unless each is missing or a
/// file itself, none followed; and, for a tree with no `.git/HEAD` or no `.git/objects`, which [`laid_out`] makes
/// anew, the warm record removed. No git step runs. Whether the tree is made anew.
fn layout_checked(request: &Request<'_>) -> Result<bool, NotLaidOut> {
    let work = request.work;
    let command = format!("{work}.command");
    plain_or_missing(Path::new(&command), false)?;
    if files::exists(Path::new(&command)) {
        for suffix in [".warm", ".tools"] {
            removed(&format!("{work}{suffix}"))?;
        }
    }
    if let Some(line) = files::unusable_folder(Path::new(work)) {
        return Err(NotLaidOut::Failed(Failed::line(line)));
    }
    let dot_git = Path::new(work).join(".git");
    let objects = dot_git.join("objects");
    let info = objects.join("info");
    for folder in [&dot_git, &objects, &info] {
        plain_or_missing(folder, true)?;
    }
    for file in [dot_git.join("config"), dot_git.join("HEAD")] {
        plain_or_missing(&file, false)?;
    }
    let fresh = !files::exists(&dot_git.join("HEAD")) || !files::exists(&objects);
    if fresh {
        removed(&format!("{work}.warm"))?;
    }
    Ok(fresh)
}

/// Lays out the work folder of `request`, its files checked ([`layout_checked`]): a `fresh` tree made with `git init`;
/// then its `origin`, its line-ending config and its alternates file naming the bare repository's objects, written
/// whole ([`git::alternates_written`]).
fn laid_out<S: LineSink>(request: &Request<'_>, git: &RunGit<'_, S>, fresh: bool) -> Result<(), NotLaidOut> {
    let work = request.work;
    if fresh {
        git.step("git init", &["init", "-q", work])?;
    }
    let info = Path::new(work).join(".git").join("objects").join("info");
    let tree = Tree::at(work);
    origin_set(git, &tree, request.bare)?;
    line_ends_set(git, &tree)?;
    git::alternates_written(&info, request.bare).map_err(|failure| {
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

/// Whether `path` is missing or, looked at without following a link, a folder itself (`folder`) or a file itself.
fn plain_or_missing(path: &Path, folder: bool) -> Result<(), NotLaidOut> {
    let shown = path.display();
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(NotLaidOut::Refused(format!("cannot read {shown}: {error}"))),
        Ok(metadata) if folder && metadata.is_dir() => Ok(()),
        Ok(metadata) if !folder && metadata.is_file() => Ok(()),
        Ok(_) if folder => Err(NotLaidOut::Refused(format!("{shown} is a link or not a folder"))),
        Ok(_) => Err(NotLaidOut::Refused(format!("{shown} is a link or not a file"))),
    }
}

/// The snapshot of `request` fetched from its bare repository at `refs/gate/<key>`, its commit found, and checked out
/// by its commit over whatever the last run left, untracked files removed and ignored ones (build output) kept; then
/// the earlier run's back ref deleted. The commit, never the ref's tip, is what is checked out, so a later push of the
/// key changes nothing this run runs.
fn checked_out<S: LineSink>(request: &Request<'_>, git: &RunGit<'_, S>) -> Result<(), Failed> {
    let tree = Tree::at(request.work);
    let refname = format!("refs/gate/{}", request.key);
    git.step_as("fetch", &tree.args(&["fetch", "-q", request.bare, &refname]), GitClass::Long)?;
    let commit = format!("{}^{{commit}}", request.snapshot);
    if !git
        .quiet("cat-file", &tree.args(&["cat-file", "-e", &commit]), GitClass::Short)?
        .status
        .success()
    {
        return Err(Failed::line(format!(
            "gate: remote snapshot {} is not in the work folder's repository after the fetch",
            request.snapshot
        )));
    }
    git.step_as(
        "checkout",
        &tree.args(&["checkout", "-q", "--force", "--detach", request.snapshot]),
        GitClass::Long,
    )?;
    git.step_as("clean", &tree.args(&["clean", "-fdq"]), GitClass::Long)?;
    back_ref_cleared(git, request)
}

/// Deletes `refs/gate-back/<key>` in the bare repository, an earlier run's back commit, so the client finds none to
/// fetch after a run that makes none.
fn back_ref_cleared<S: LineSink>(git: &RunGit<'_, S>, request: &Request<'_>) -> Result<(), Failed> {
    let bare = format!("--git-dir={}", request.bare);
    let back = format!("refs/gate-back/{}", request.key);
    git.bare_step("back ref reset", &[&bare, "update-ref", "-d", &back], GitClass::Short)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test whose temp folder, repository or payload cannot be made must fail loudly"
)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{MetadataExt, symlink};
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::{Arc, Mutex, PoisonError, mpsc};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime};

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use gate_wire::{PathForm, Payload, Refusal, busy_line, layout_line, refused_line, started_line, tools_line};
    use serde_json::{Value, json};

    use gate_watchdog::Pump;

    use super::{REFUSED_STATUS, Ready, Watches, place_of, prepare, prepared, run_along, test_template};
    use crate::command::{Launch, Sampler, run_command};
    use crate::console::{Console, Forwarding};
    use crate::console::{LineSink, Output, RecordingSink, Stream};
    use crate::files::{self, Locked, effective_uid, take_lock};
    use crate::git::{GitClass, test_limit, test_program};
    use crate::heartbeat::{self, Periods};
    use crate::lines::command_status;
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
            fs::create_dir_all(format!("{}/work", case.root())).expect("the work folders' parent is made");
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
            let source = self.source();
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
            git(&["-C", source, "push", "-q", "--force", &self.bare(), &refspec]);
            git(&["-C", source, "rev-parse", "HEAD"])
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

    impl LineSink for MarkingSink {
        fn write_line(&self, stream: Stream, line: &str) -> std::io::Result<()> {
            if line.starts_with("gate-remote-heartbeat:") && !self.marker.exists() {
                fs::write(&self.marker, "")?;
            }
            self.lines.lock().unwrap_or_else(PoisonError::into_inner).push((stream, line.to_owned()));
            Ok(())
        }
    }

    /// The first folder along `PATH` holding `git`.
    fn real_git() -> PathBuf {
        let path = std::env::var_os("PATH").expect("PATH is set");
        std::env::split_paths(&path)
            .map(|folder| folder.join("git"))
            .find(|git| git.is_file())
            .expect("git is on PATH")
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
        let mut ready = prepared(&payload, &output, quiet_periods(), own_path, no_beat);
        if let Ok(ready) = ready.as_mut() {
            tmp_cleared(ready);
        }
        assert!(ready.as_ref().is_ok_and(|ready| ready.pump.is_none()), "{:?}", output.sink().lines());
        assert_eq!(
            stderr_of(&output),
            ["gate: remote heartbeat stopped: cannot start the heartbeat: no thread"]
        );
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
        let lines = || output.sink().lines.lock().unwrap_or_else(PoisonError::into_inner).clone();
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
            .position(|(stream, line)| *stream == Stream::Out && *line == started_line(NONCE));
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
        (status, output.sink().lines())
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
    fn a_command_that_changes_the_tree_pushes_a_back_commit_on_head_with_the_payload_s_identity() {
        let case = Case::new("back-changed");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        case.back_ref_set(&commit);
        let (status, said) = whole_run(&case.commanded(&commit, &["/bin/sh", "-c", "echo new > new.txt; echo b > a.txt"]));
        assert_eq!(status, 0, "{said:?}");
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
            .lines()
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
    }

    /// The box's own `PATH` as the login shell's, so a case starts no login shell.
    fn own_path() -> Result<String, String> {
        std::env::var("PATH").map_err(|error| error.to_string())
    }

    /// Whether [`counted_login`] was called.
    static LOGIN_READ: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// A login shell's `PATH` that cannot be read, noting that it was asked for.
    fn counted_login() -> Result<String, String> {
        LOGIN_READ.store(true, std::sync::atomic::Ordering::SeqCst);
        Err("the case's login shell".to_owned())
    }

    /// Runs every git step of this thread through a shim that runs `after_add` once git's `add -A` on the copied index
    /// has run, `$GIT_INDEX_FILE` naming the copy; every other step is the real git's.
    fn shim_after_add(case: &Case, after_add: &str) {
        let shim = case.base.with_file_name("git-shim");
        let script = format!(
            "#!/bin/sh\n'{git}' \"$@\"\nstatus=$?\ncase \" $* \" in\n*\" add -A \"*) [ -n \"$GIT_INDEX_FILE\" ] && {{ {after_add}; }};;\nesac\nexit $status\n",
            git = real_git().display()
        );
        fs::write(&shim, script).expect("the shim is written");
        fs::set_permissions(&shim, std::os::unix::fs::PermissionsExt::from_mode(0o700)).expect("the shim is made runnable");
        test_program::set(&shim);
    }

    /// The lines `output` said on stderr.
    fn err_of(output: &Output<RecordingSink>) -> Vec<String> {
        output
            .sink()
            .lines()
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
        assert_eq!(stderr_of(&output), Vec::<String>::new());

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
        let lines = output.sink().lines();
        assert!(lines.iter().all(|(stream, _)| *stream == Stream::Err), "{lines:?}");
        lines.into_iter().map(|(_, line)| line).collect()
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
            !stderr_of(&output).iter().any(|line| line.starts_with("gate")),
            "no gate line: {:?}",
            output.sink().lines()
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
        assert_eq!(said, ["gate: remote subfolder sub is not a folder inside the work folder"]);
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
        assert_eq!(said, ["gate: remote subfolder nope does not exist in the snapshot"]);
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
        assert_eq!(said, Vec::<String>::new());
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
            said,
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

    #[test]
    fn a_pair_is_refused_layout() {
        let case = Case::new("pair");
        let pair = json!({ "Pair": { "Leaf": "repo", "Siblings": ["lib"] } });
        assert_refused(&case, &pair, Refusal::Layout, &unbuilt_layout("Pair"));
    }

    #[test]
    fn a_desktop_run_is_refused_desktop() {
        let case = Case::new("desktop");
        let desktop = json!({ "Desktop": true, "DesktopWait": 60 });
        assert_refused(&case, &desktop, Refusal::Desktop, "gate: remote Desktop is not built on this builder");
    }

    #[test]
    fn an_inputs_digest_is_refused_inputs() {
        let case = Case::new("inputs");
        let inputs = json!({ "InputsDigest": DIGEST });
        assert_refused(&case, &inputs, Refusal::Inputs, "gate: remote InputsDigest is not built on this builder");
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
    fn a_warm_up_is_refused_layout() {
        let case = Case::new("warm-up");
        let warm = json!({ "Command": null, "WarmCommands": [["true"]], "WarmHash": DIGEST, "Timeout": 60 });
        assert_refused(&case, &warm, Refusal::Layout, &unbuilt_layout("WarmCommands"));
    }

    #[test]
    fn outputs_or_outputs_full_are_refused_layout_naming_the_key() {
        let case = Case::new("outputs");
        assert_refused(&case, &json!({ "Outputs": ["bin/**"] }), Refusal::Layout, &unbuilt_layout("Outputs"));
        assert_refused(&case, &json!({ "OutputsFull": true }), Refusal::Layout, &unbuilt_layout("OutputsFull"));
    }

    #[test]
    fn an_engine_version_or_engine_env_is_refused_layout_naming_the_key() {
        let case = Case::new("engines");
        assert_refused(
            &case,
            &json!({ "GodotVersion": "4.3.0" }),
            Refusal::Layout,
            &unbuilt_layout("GodotVersion"),
        );
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
    fn an_interrupted_command_record_removes_the_warm_and_tools_records_before_the_checkout() {
        let case = Case::new("interrupted");
        let commit = case.snapshot(&[("a.txt", "a\n")]);
        case.laid_out(&commit);
        let (warm, tools) = (format!("{}.warm", case.work()), format!("{}.tools", case.work()));
        for record in [&warm, &tools] {
            fs::write(record, DIGEST).expect("the record is written");
        }

        case.laid_out(&commit);
        assert!(
            Path::new(&warm).exists() && Path::new(&tools).exists(),
            "no interrupted command, the records stand"
        );

        fs::write(format!("{}.command", case.work()), "x").expect("the interrupted-command record is written");
        let (status, said) = ran(&case.json(&commit, &json!({})));

        assert_eq!(status, 0, "{said:?}");
        assert!(!Path::new(&warm).exists(), "the warm record is removed");
        assert!(!Path::new(&tools).exists(), "the tools record is removed");
    }

    #[test]
    fn a_folder_at_the_warm_or_tools_record_of_an_interrupted_command_is_refused_layout() {
        for suffix in [".warm", ".tools"] {
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
        let line = files::unusable_folder(Path::new("/usr")).expect("a root-owned folder is not the box user's");
        let user = files::user_name(effective_uid());
        assert_eq!(line, format!("gate: remote could not create the folder /usr: it is not owned by {user}"));
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
        fs::write(&record, format!("gate-remote-{KEY}-ffffffffffff\n{}\n", child.id())).expect("the record is written");
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
        fs::write(&record, format!("gate-remote-{KEY}-ffffffffffff\n{group}\n")).expect("the record is written");

        let (status, said) = ran(&case.json(&commit, &json!({})));

        assert_eq!(status, 0, "{said:?}");
        assert_eq!(case.head(), commit);
        assert!(!Path::new(&record).exists(), "the stale record is removed");
    }

    /// Writes the group record of the case naming `group` in this process's session, as a box's git step writes it,
    /// its time set `back` before now.
    fn record_written_back(case: &Case, group: u32, back: Duration) -> String {
        let record = record_of(case);
        // SAFETY: `getsid(0)` asks for the caller's session and touches no memory.
        let session = unsafe { libc::getsid(0) };
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
            let extra = json!({ "Outputs": outputs, "Desktop": true, "DesktopWait": 60, "Pair": { "Leaf": "repo", "Siblings": ["lib"] } });
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
    #[expect(clippy::unnecessary_wraps, reason = "a run reads its login PATH through a `LoginPath`, which may fail")]
    fn shim_login() -> Result<String, String> {
        Ok(SHIM_PATH.with(|path| path.borrow().clone()))
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
        let (path, run_json) = (shim_login().expect("the shim's PATH is set"), json.clone());
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
        assert!(Path::new(&record).exists(), "the restore's group is recorded");
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
        fs::write(&record, format!("gate-remote-{KEY}-ffffffffffff-tools\n{}\n", child.id())).expect("the record is written");

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
        fs::write(&record, format!("gate-remote-{KEY}-ffffffffffff-tools\n{group}\n")).expect("the record is written");

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
