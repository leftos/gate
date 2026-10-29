//! `gate box preflight`, the first call of every routed run: readies the work folder and its bare repository as the
//! PowerShell wrapper's `Initialize-WorkFolder` does, then says the version and warm lines, the new work tree line when
//! it laid the work folder's tree out now, the boot and SDK lines, a line for each tool the payload's `Needs` names,
//! and the path line when no trusted Git `bin` was found for the command's `PATH`.

use std::cell::Cell;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::{OnceLock, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use gate_args::BoxSeams;
use gate_router::escaped;
use gate_win32::{Job, LaunchError, Make, NamedEvent, PipedConsole, PipedLaunch, Share};
use gate_wire::build_version;
use gate_wire::{
    Pair, PathForm, Payload, ROOT_STATUS, WARM_READ_LIMIT, bare_stem, layout_text_for, names_another_layout, readies_single, root_busy_line,
    root_unsafe_line, trees, warm_record_hash, work_root,
};
use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
};

use crate::answer::USAGE_STATUS;
use crate::boxside::boot;
use crate::boxside::drain::{self, HeldRoot};
use crate::boxside::forward::{ClientGone, DRAIN, Forwarders, OutputLock};
use crate::boxside::heartbeat;
use crate::boxside::helper_trusted;
use crate::boxside::msys;
use crate::boxside::probe;
use crate::boxside::run::{self, FILE_FLAG_OPEN_REPARSE_POINT, Stamp, StopSignal, WaitEnd, Watched, Watching};
use crate::boxside::slots;
use crate::boxside::stamping;
use crate::boxside::stop::terminate_and_wait;
use crate::console::Console;
use crate::environment::Variables;
use crate::remote::{INSTALL_STATUS, NEW_WORK_TREE, git, install_untrusted_line, launch};
use crate::run::{ERROR_FILE_NOT_FOUND, Written, find_program};

/// The Windows installer step that locks the builder's work root down, as the work-root refusal names it
/// ([`gate_wire::root_unsafe_line`]).
pub(crate) const WORK_ROOT_STEP: &str = "install-toolchains.ps1 -Only \"Gate work root\"";

/// The status of a step that failed with no status of its own: git or a folder that could not be made or written.
const FAILED_STATUS: u32 = 1;

/// The git variables that would point git at another repository, index, object store or config than the work folder's,
/// the git variables that would have it read another global or system config file or none, and the git-lfs variables
/// that would let a checkout leave pointer text or skip an object it cannot download.
pub(crate) const GIT_REDIRECTS: [&str; 13] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_LFS_SKIP_SMUDGE",
    "GIT_LFS_SKIP_DOWNLOAD_ERRORS",
];

/// A preflight step that failed: the line saying so, and the status the gate exits with.
pub(crate) struct Failed {
    pub(crate) line: String,
    pub(crate) status: u32,
}

impl Failed {
    pub(crate) fn without_status(line: String) -> Failed {
        Failed { line, status: FAILED_STATUS }
    }
}

/// The failure of git `step` that exited with `code`: the line shows the code signed, as `gate run` and the wrapper's
/// `$LASTEXITCODE` show it, and the gate exits with its whole 32 bits.
pub(crate) fn git_failed(step: &str, code: i32) -> Failed {
    Failed {
        line: format!("gate: remote {step} failed (status {code})"),
        status: code.cast_unsigned(),
    }
}

/// Readies the folders the payload's `Bare` and `Work` name, then says the version and warm lines, [`NEW_WORK_TREE`]
/// when the work folder of a run of one repo had no `.git` ([`ready_new_tree`]), when the work folder holds an
/// interrupted-command record ([`crate::boxside::run::command_record`]; anything at its name counts, looked at as
/// itself and never opened or followed), when its layout record names another layout than the run's, or a pair's has
/// none yet, as the run then clears the whole folder ([`names_another_layout`]), and when no plain file is at its ran
/// record ([`crate::boxside::run::ran_record`]), as no command has started in the folder since it was laid out, so
/// the client raises the ceiling of a run that meets a cold folder; then the boot line, the path line
/// [`msys::NO_GIT_BIN`] when no trusted Git `bin` was found for the command's `PATH`, the SDK line, its list read
/// within `--test-dotnet-limit-ms` when `seams` gives it, else [`git::DOTNET_LIMIT`], and the tool lines, probed
/// within `--test-probe-limit-ms` when `seams` gives it, else [`probe::PROBE_LIMIT`]; returns the status.
///
/// First, after its version line, it refuses with the install line and [`INSTALL_STATUS`] a desktop helper beside
/// the box that is not trusted ([`helper_trusted`]), judged as the box's own image is ([`crate::boxside::judges_trust`]),
/// so a builder whose helper any other process could replace is not routed, and `gate check --builder` says so. The
/// version line comes first, so a PC client of another version falls back on the version rather than a bare status 1.
/// Then, once the payload is read and before any record is read or folder readied, it makes the work root locked when
/// it is missing and refuses one another account may write in, after its version line, with the root line and
/// [`ROOT_STATUS`] ([`held_work_root`]), holding it until the folders are readied. Under that hold it passes the drain
/// ([`drain::admit`]), the payload's `NoTicket` saying whether a refusal takes a place in line, and holds its session
/// until it returns; a refusal or a drain that fails says the version line, then the drain's
/// ([`drain::admission_said`]). Right after its admission, before anything else is done for the run, it takes a builder
/// slot for its session when the root has a count file ([`slots::taken`]), refused `full` when none is free, and lets
/// it go again when a later step fails ([`slots::Release`]). It readies the bare repositories, then takes the key's
/// work-folder lock `Lock` around its reads of the key's records and its writes in the work folder ([`ready`]), and
/// lets it go before its version line, so the run right after it is never refused busy by it. Its last line is the
/// builder's own slots as a look without the drain lock finds them ([`slots::looked`]), for a root with a count file or
/// a `slots\` folder.
pub(crate) fn preflight(payload: &Payload, seams: &BoxSeams, console: &mut Console) -> u32 {
    if crate::boxside::judges_trust()
        && let Err(why) = gate_desktop::helper_path()
            .map_err(|error| format!("the desktop helper's path cannot be read: {error}"))
            .and_then(|helper| helper_trusted(&helper))
    {
        stamping::out(console, &format!("gate: remote version {}", build_version()));
        stamping::err(console, &install_untrusted_line(&why));
        return u32::from(INSTALL_STATUS);
    }
    let read = payload.path("Bare").and_then(|bare| {
        let work = payload.work_folder()?.1;
        Ok((bare, work, payload.beside_work("Lock", work, "lock")?, payload.pair()?))
    });
    let drain_read = drain::session_of(payload, "", "").and_then(|session| Ok((session, payload.no_ticket()?)));
    let ((bare, work, lock, pair), (session, no_ticket)) = match read.and_then(|folders| Ok((folders, drain_read?))) {
        Ok(read) => read,
        Err(line) => {
            stamping::err(console, &line);
            return u32::from(USAGE_STATUS);
        }
    };
    // Held until the folders are readied, so no one can rename or remove the root meanwhile.
    let (held_root, root) = match held_work_root(work, bare, pair.is_some(), seams, console) {
        Ok(held) => held,
        Err(status) => return status,
    };
    // Held until the preflight returns, so a drain counts it as going.
    let admission = drain::admit(
        &HeldRoot {
            held: &held_root,
            path: Path::new(&root),
        },
        session.as_ref(),
        no_ticket,
        seams,
        console,
    );
    let version = format!("gate: remote version {}", build_version());
    let _admitted = match drain::admission_said(admission, payload, Some(&version), console) {
        Ok(admitted) => admitted,
        Err(status) => return status,
    };
    // Before anything else is done for the run, so a builder whose own slots are all held refuses it with nothing made.
    let root_held = HeldRoot {
        held: &held_root,
        path: Path::new(&root),
    };
    let claimed = match slots::taken(&root_held, session.as_ref(), payload, &version, seams, console) {
        Ok(claimed) => claimed,
        Err(status) => return status,
    };
    // Made only when the session holds a claim, and dropped at every failing end of the preflight, before its
    // admission and so while its session is held: the slot it took is let go as a run's end lets it go. Kept once the
    // preflight passes.
    let slot = claimed.then(|| slots::Release::at_end(work, payload));
    let Readied { made, warm, cleared, ran } = match ready(bare, work, lock, pair.as_ref(), console) {
        Ok(readied) => readied,
        Err(failed) => {
            stamping::err(console, &failed.line);
            return failed.status;
        }
    };
    // Looked at while the root is held, and said last, so every line before it keeps its place.
    let slots_look = slots::looked(&root_held, seams);
    drop(held_root);
    stamping::out(console, &version);
    stamping::out(console, &format!("gate: remote warm {}", warm.as_deref().unwrap_or("none")));
    if made || cleared || !ran {
        stamping::out(console, NEW_WORK_TREE);
    }
    boot::say(console);
    // The run puts Git's `bin` at the front of its command's `PATH` when it finds a trusted one; a machine `PATH` that
    // leaves it untrusted resolves a recipe's bare `bash` to the system folder's WSL launcher, so the log says so
    // before the command runs. The client relays a preflight's lines into the log only, so this one never reaches the
    // console.
    if msys::git_bin(&Git::find(), &refused_folders(payload, work)).is_none() {
        stamping::out(console, msys::NO_GIT_BIN);
    }
    say_sdks_and_tools(payload, work, seams, console);
    slots::look_said(slots_look, console);
    if let Some(slot) = slot {
        slot.kept();
    }
    0
}

/// The builder's work root, `<root>` of the work folder `work` (`<root>\work\<key>`), made locked when it is missing,
/// judged and held by [`gate_win32::check_work_root`], its walk above ending at `--test-box-root-walk-stop` when
/// `seams` gives it, beside the root's path. Else the status to exit with, said first: the usage line and
/// [`USAGE_STATUS`] for a `work` laid out otherwise, or, unless `paired`, a bare repository `bare` that is not
/// `<root>\<name>.git`; or the version line and [`ROOT_STATUS`] with the refusal ([`root_unsafe_line`]) for a root
/// another account may write in, or the busy line ([`root_busy_line`]) for one another process holds open with delete
/// access, or without sharing reading.
fn held_work_root(work: &str, bare: &str, paired: bool, seams: &BoxSeams, console: &mut Console) -> Result<(File, String), u32> {
    let Some(root) = work_root(work, PathForm::Windows) else {
        stamping::err(
            console,
            &format!(
                r"gate: box payload Work must be <root>\work\<key> to lock its root down: {}",
                escaped(work)
            ),
        );
        return Err(u32::from(USAGE_STATUS));
    };
    // A pair's bare repositories were checked against the root as its layout was read. A drive's own root is compared
    // without its separator, as the bare repository's folder is cut.
    if !paired && bare_stem(bare, root.trim_end_matches(['\\', '/']), PathForm::Windows).is_none() {
        stamping::err(
            console,
            &format!(
                r"gate: box payload Bare must be <root>\<name>.git to lock its root down: {}",
                escaped(bare)
            ),
        );
        return Err(u32::from(USAGE_STATUS));
    }
    let held = gate_win32::check_work_root(Path::new(&root), seams.root_walk_stop.as_deref()).map_err(|why| {
        stamping::out(console, &format!("gate: remote version {}", build_version()));
        let line = if why.is_busy() {
            root_busy_line(&why.to_string())
        } else {
            root_unsafe_line(&why.to_string(), WORK_ROOT_STEP)
        };
        stamping::err(console, &line);
        u32::from(ROOT_STATUS)
    })?;
    Ok((held, root))
}

/// Says the SDK line, the list read within `--test-dotnet-limit-ms` when `seams` gives it, else
/// [`git::DOTNET_LIMIT`], then one line on stdout for each tool the payload's `Needs` names, in its order. The tools are
/// probed within `--test-probe-limit-ms` when `seams` gives it, else [`probe::PROBE_LIMIT`], while dotnet lists the
/// SDKs; the readers of dotnet and of each probed tool get `--test-probe-drain-ms` to end once it has exited, when
/// `seams` gives it. The netfx-sdk probe reads the key `--test-netfx-key` names, when `seams` gives it, in place of the
/// machine's.
/// No probe runs a program from the run's own folders ([`refused_folders`]). A `Needs` that is not an object of
/// texts is said on stderr, probes nothing, and fails nothing. The minimum each need carries is not read: the box
/// reports, the client compares.
fn say_sdks_and_tools(payload: &Payload, work: &str, seams: &BoxSeams, console: &mut Console) {
    let names: Result<Vec<String>, String> = payload
        .pairs("Needs")
        .map(|needs| needs.into_iter().map(|(name, _)| name).collect())
        .map_err(|line| format!("{line}; probing nothing"));
    let windows = probe::Windows::of(seams.probe_limit_ms, seams.probe_drain_ms);
    let netfx_key = seams.netfx_key.as_deref();
    let refused = refused_folders(payload, work);
    let worktrees: Vec<&Path> = refused.iter().map(PathBuf::as_path).collect();
    thread::scope(|scope| {
        let probes = names.as_ref().ok().filter(|names| !names.is_empty()).map(|names| {
            (
                names,
                scope.spawn(move || probe::probe(names, windows, Ok(Variables::from_process()), &worktrees, netfx_key)),
            )
        });
        say_sdks(
            seams.dotnet_limit_ms.map_or(git::DOTNET_LIMIT, Duration::from_millis),
            windows.drain,
            console,
        );
        if let Err(line) = &names {
            stamping::err(console, line);
        }
        let Some((names, probes)) = probes else {
            return;
        };
        let found = probes.join().unwrap_or_else(|panic| {
            let why = probe::panic_reason(&*panic);
            names.iter().map(|name| (name.clone(), gate_wire::Probed::Skipped(why.clone()))).collect()
        });
        for (name, probed) in found {
            stamping::out(console, &gate_wire::tool_line(&name, &probed));
        }
    });
}

/// The folders of the payload's run that no tool probe runs a program from: the folder above `work`, `work` itself,
/// the bare folder and the outputs folder, each one the payload has. `work` is the work folder the preflight has
/// already read, so the folder above it and itself are always refused; a payload whose `Bare` or `OutputFolder`
/// cannot be read refuses the rest.
pub(super) fn refused_folders(payload: &Payload, work: &str) -> Vec<PathBuf> {
    let mut folders: Vec<PathBuf> = Vec::new();
    if let Some(above) = Path::new(work).parent() {
        folders.push(above.to_path_buf());
    }
    folders.push(PathBuf::from(work));
    if let Ok(bare) = payload.path("Bare") {
        folders.push(PathBuf::from(bare));
    }
    if let Ok(outputs) = payload.output_folder(work) {
        folders.push(PathBuf::from(outputs));
    }
    folders
}

/// The hash the work folder `work`'s warm record, `{Work}.warm`, holds; `None` unless every tree its layout record
/// `layout` names ([`laid_out_trees`]) has a plain `.git` ([`git_is_plain`]), as a work folder that is missing has
/// not, and `None` when there is no record, when it cannot be opened or read or is not a plain file, and when it holds
/// anything but exactly 64 lower-case hex digits, a line end included. The record is opened sharing reading, writing
/// and deletion, so this read never makes a run's removal of it fail.
fn warm_state(work: &str, layout: &io::Result<Option<String>>) -> Option<String> {
    let trees = laid_out_trees(work, layout)?;
    if !trees.iter().all(|tree| git_is_plain(Path::new(tree))) {
        return None;
    }
    let mut bytes = Vec::new();
    shared_record(&crate::boxside::run::warm_record(work))
        .ok()?
        .take(WARM_READ_LIMIT)
        .read_to_end(&mut bytes)
        .ok()?;
    warm_record_hash(&bytes)
}

/// The record `record` beside the work folder opened as the preflight reads one: sharing reading, writing and
/// deletion, without following a link there, and only as a plain file.
fn shared_record(record: &str) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .share_mode(Share::All.mode())
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(record)?;
    gate_win32::check_plain_file(&file)?;
    Ok(file)
}

/// The text of the work folder `work`'s layout record, `{work}.layout`, read once by the preflight ([`shared_record`]);
/// `Ok(None)` when there is none.
fn layout_record(work: &str) -> io::Result<Option<String>> {
    let mut file = match shared_record(&format!("{work}.layout")) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(Some(text))
}

/// The trees the layout record of the work folder `work`, `layout`, says it holds: `work` itself for a missing record
/// or one of a run of one repo's layout ([`readies_single`]), `{work}\{name}` for the leaf and each sibling of a
/// pair's ([`trees`]); `None` for a clear in progress, a record that cannot be read, or one that names no layout or a
/// name that is not a folder name.
fn laid_out_trees(work: &str, layout: &io::Result<Option<String>>) -> Option<Vec<String>> {
    if readies_single(layout) {
        return Some(vec![work.to_owned()]);
    }
    let Ok(Some(text)) = layout else {
        return None;
    };
    let names: Vec<&str> = text.strip_prefix("pair ")?.split(' ').collect();
    if names.len() < 2 || !names.iter().all(|name| gate_router::is_layout_name(name)) {
        return None;
    }
    let (leaf, siblings) = names.split_first()?;
    let (main, sibling_trees) = trees(work, Some((*leaf, siblings)), PathForm::Windows);
    Some(std::iter::once(main).chain(sibling_trees).collect())
}

/// What the preflight read of the key's records under the key's lock, and whether the work folder's tree is new.
struct Readied {
    /// The tree is new to this run ([`ready_new_tree`]); when the preflight could not take the lock, it has no
    /// `.git\HEAD` file, read with `HEAD` itself not followed (a link at `.git` is followed).
    made: bool,
    /// The warm record's hash ([`warm_state`]).
    warm: Option<String>,
    /// The run clears what the folder holds: a command ended mid-run left its record, or the layout record names
    /// another layout than the run's ([`names_another_layout`]).
    cleared: bool,
    /// A command of a run or a warm-up started in the folder: its ran record ([`run::ran_record`]) is there.
    ran: bool,
}

/// The folder above `work`; the bare repository `bare`, or each of the pair `pair`'s ([`ready_bares`]), said as
/// created; then, under the key's lock `lock` ([`run::take_lock`], unstamped), the key's records read and, for a run
/// of one repo, the work folder's repository readied ([`ready_one_tree`]); the lock is let go before this returns. A
/// lock another process holds, or one that cannot be opened (its line said on stderr), leaves the tree to the run,
/// which is refused busy or fails on the same lock: nothing is written in the work folder.
fn ready(bare: &str, work: &str, lock: &str, pair: Option<&Pair>, console: &mut Console) -> Result<Readied, Failed> {
    let git = Git::find();
    create_parent(work)?;
    match pair {
        None => create_bare(&git, bare, None, console)?,
        Some(pair) => ready_bares(&git, bare, pair, console)?,
    }
    let held = match run::take_lock(lock, run::WORK_LOCK, Stamp::NoUse, console) {
        Ok(held) => held,
        Err(line) => {
            stamping::err(console, &line);
            None
        }
    };
    // Read before the tree is readied, which would make a missing `.git` and so a stale record look current.
    let layout = layout_record(work);
    let warm = warm_state(work, &layout);
    // A command ended mid-run left the record: the run clears what it wrote, so the folder is cold again. A record
    // naming another layout than the run's, a pair's first run having none, or one cut short mid-change, has the run
    // clear the whole folder and lay its trees out again. Any entry at the record's name counts, a folder or a link
    // included, looked at as itself, never followed: the run takes one it cannot read as a record of no form.
    let interrupted = fs::symlink_metadata(run::command_record(work)).is_ok();
    // Only a command started in the folder makes it warm: a tree laid out by a preflight whose run never came is new.
    // Only a plain file counts: a folder, a link or a junction there, or a name that cannot be looked at, is no record.
    let ran = fs::symlink_metadata(run::ran_record(work)).is_ok_and(|metadata| metadata.is_file());
    let wanted = layout_text_for(pair);
    let relaid = names_another_layout(&layout, &wanted);
    let made = match (pair, &held) {
        (Some(_), _) => false,
        (None, Some(_)) => ready_one_tree(&git, bare, work, &layout, console)?,
        (None, None) => !fs::symlink_metadata(Path::new(work).join(".git").join("HEAD")).is_ok_and(|metadata| metadata.is_file()),
    };
    drop(held);
    Ok(Readied {
        made,
        warm,
        cleared: interrupted || relaid,
        ran,
    })
}

/// The work folder `work`'s repository ([`ready_tree`], which readies nothing through a link at its `.git`), unless
/// the work folder is laid out for another layout ([`readies_single`] of its record `layout`), which a run of
/// the key changes. The preflight's lock only guards its writes ([`TreeGit::holds_the_lock`] is false). Whether the
/// work folder's tree is new to this run ([`ready_new_tree`]).
fn ready_one_tree(git: &Git, bare: &str, work: &str, layout: &io::Result<Option<String>>, console: &mut Console) -> Result<bool, Failed> {
    if !readies_single(layout) {
        return Ok(false);
    }
    let (warm, ran) = (run::warm_record(work), run::ran_record(work));
    let at = TreeAt {
        tree: work,
        bare,
        warm: &warm,
        ran: &ran,
    };
    ready_new_tree(git, &at, None, console).map_err(|unready| match unready {
        Unready::Git(failed) => failed,
        Unready::Folder(why) => Failed::without_status(format!("gate: remote {why}")),
    })
}

/// For a run of the sibling pair `pair`: the bare repository of the main checkout, `bare`, and of each sibling, each
/// said as created, a failure naming its repo. Nothing inside the work folder, which only a run changes.
fn ready_bares(git: &Git, bare: &str, pair: &Pair, console: &mut Console) -> Result<(), Failed> {
    create_bare(git, bare, Some(pair.leaf()), console)?;
    for sibling in pair.siblings() {
        create_bare(git, sibling.bare(), Some(sibling.name()), console)?;
    }
    Ok(())
}

/// Creates the folder above `path` when it has one.
fn create_parent(path: &str) -> Result<(), Failed> {
    match Path::new(path).parent().filter(|parent| !parent.as_os_str().is_empty()) {
        Some(parent) => create_folder(parent),
        None => Ok(()),
    }
}

/// Creates `folder` and the folders above it; else the line saying it could not.
fn create_folder(folder: &Path) -> Result<(), Failed> {
    fs::create_dir_all(folder)
        .map_err(|error| Failed::without_status(format!("gate: remote could not create the folder {}: {error}", folder.display())))
}

/// Creates the bare repository `bare` when it is missing or lacks what `git init --bare` makes ([`is_whole_bare`]), as
/// one an `init` cut short leaves, said as created; a failure's line names the repo `name` on a pair run
/// ([`failed_for`]). A write of the shared bare repository, which a gone client does not cut short
/// ([`GitClass::ShortBare`]). The repair runs under the bare's own lock, `{bare}.lock` ([`run::take_lock`], never
/// deleted), since every key of the repo shares the bare: one another preflight holds is waited for, opened again every
/// [`BARE_LOCK_POLL`] up to [`BARE_LOCK_WAIT`], and the bare is looked at again once it is taken.
fn create_bare(git: &Git, bare: &str, name: Option<&str>, console: &mut Console) -> Result<(), Failed> {
    if is_whole_bare(Path::new(bare)) {
        return Ok(());
    }
    let lock = format!("{bare}.lock");
    let held = lock_waited(
        || run::take_lock(&lock, "bare lock", Stamp::NoUse, console),
        BARE_LOCK_WAIT,
        BARE_LOCK_POLL,
    )
    .map_err(Failed::without_status)?;
    let Some(_held) = held else {
        return Err(Failed::without_status(format!(
            "gate: remote {bare} is being made by another preflight and was not whole within {} s",
            BARE_LOCK_WAIT.as_secs()
        )));
    };
    // The preflight that held the lock may have made it.
    if is_whole_bare(Path::new(bare)) {
        return Ok(());
    }
    let step = "git init --bare";
    unlock_half_made_bare(Path::new(bare))
        .map_err(|why| failed_for(Failed::without_status(format!("gate: remote {step} failed: {why}")), step, name))?;
    git.step(&for_repo(step, name), &["init", "--bare", "-q", "--", bare], GitClass::ShortBare, console)
        .map_err(|failed| failed_for(failed, step, name))?;
    stamping::out(console, &format!("gate: remote created {bare}"));
    Ok(())
}

/// How often a preflight opens a bare repository's lock again while another preflight holds it ([`create_bare`]).
const BARE_LOCK_POLL: Duration = Duration::from_millis(100);

/// How long a preflight waits for a bare repository's lock another preflight holds ([`create_bare`]): the holder's
/// `git init --bare` bound, [`GIT_SHORT_LIMIT`], and a margin, so a holder whose init takes nearly its whole bound
/// still lets the waiter find the bare whole.
const BARE_LOCK_WAIT: Duration = GIT_SHORT_LIMIT.saturating_add(Duration::from_secs(30));

/// What `open` gives once it gives a held lock, opened again every `poll` while it says the lock is held (`Ok(None)`),
/// until `bound` has passed: `Ok(None)` when the lock was still held then; an open's error as it came.
fn lock_waited<T>(mut open: impl FnMut() -> Result<Option<T>, String>, bound: Duration, poll: Duration) -> Result<Option<T>, String> {
    let deadline = Instant::now() + bound;
    loop {
        if let Some(held) = open()? {
            return Ok(Some(held));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(poll);
    }
}

/// Removes the `HEAD.lock` and `config.lock` an `init` cut short can leave in the root of the bare repository `bare`
/// that is not whole ([`is_whole_bare`]), so the `init` that makes it again does not fail on them: through handles, the
/// bare repository held as itself and each lock removed as [`removed_lock`] removes it, one a live git holds left to
/// it. No git can use a bare repository with no `HEAD`, so these are the only names of a bare repository's root the box
/// ever removes. Nothing when `bare` is missing or not a folder itself; else why not.
fn unlock_half_made_bare(bare: &Path) -> Result<(), String> {
    let Some(held) = held_folder_at(bare).map_err(|error| format!("cannot open {}: {error}", bare.display()))? else {
        return Ok(());
    };
    for lock in ["HEAD.lock", "config.lock"] {
        removed_lock(&held, lock, &bare.join(lock))?;
    }
    Ok(())
}

/// Whether the bare repository `bare` holds what `git init --bare` makes first: its `HEAD` file and its `objects` and
/// `refs` folders. `git init` over a repository that has them changes nothing in it.
fn is_whole_bare(bare: &Path) -> bool {
    bare.join("HEAD").is_file() && bare.join("objects").is_dir() && bare.join("refs").is_dir()
}

/// Where [`ready_tree`] readies a repository: the tree, its repo's bare repository, and the warm record of the work
/// folder the tree lies in, `{Work}.warm`, and its ran record, `{Work}.ran`.
pub(super) struct TreeAt<'a> {
    pub(super) tree: &'a str,
    pub(super) bare: &'a str,
    pub(super) warm: &'a str,
    pub(super) ran: &'a str,
}

/// How [`ready_tree`] runs git: the preflight's [`Git`], or the run's, with its safety options and its output forwarded
/// under the box's output lock.
pub(super) trait TreeGit {
    /// Runs git with `args`; a non-zero status, or a git that cannot be started, fails `step`.
    fn run_step(&self, step: &str, args: &[&str], console: &mut Console) -> Result<(), Failed>;
    /// git's exit code for `args` as the short step `step`, its stdout discarded; else why it did not run to its end.
    fn run_code(&self, step: &str, args: &[&str], console: &mut Console) -> Result<i32, GitFailure>;
    /// Whether the caller holds the work folder's lock, so a link where a folder should be may be removed and the
    /// folder made again ([`write_alternates`]).
    fn holds_the_lock(&self) -> bool;
}

impl TreeGit for Git {
    fn run_step(&self, step: &str, args: &[&str], console: &mut Console) -> Result<(), Failed> {
        self.step(step, args, GitClass::Short, console)
    }

    /// Its stderr forwarded to the box's ([`Git::alone`]).
    fn run_code(&self, step: &str, args: &[&str], console: &mut Console) -> Result<i32, GitFailure> {
        let streams = Streams {
            stdout: Pipe::Discard,
            stderr: Pipe::Forward,
            feed: None,
        };
        let step = GitStep {
            name: step,
            class: GitClass::Short,
        };
        self.alone(args, step, &streams, console).map(|ran| ran.code)
    }

    /// The preflight's hold of the work folder's lock only guards its writes: it repairs nothing through a link.
    fn holds_the_lock(&self) -> bool {
        false
    }
}

/// Readies the repository of the tree `tree`: created when nothing has the name `.git` there or its `.git` has no
/// `HEAD`, read without following a
/// link, its `origin` the bare repository `bare`, its line-ending config, and its alternates file pointing at `bare`'s
/// objects ([`write_alternates`]). Without the lock, a link, a junction or a file at the tree's `.git`, `.git\objects`
/// or `.git\objects\info`, or anything but a file at its `.git\config` ([`has_a_link_in_its_git`]), readies nothing:
/// no git step runs and nothing is written, and the run repairs it under the lock. When `git` holds the work folder's
/// lock, those are repaired before the first git step ([`repair_git`]). So each git call goes through `git` and names
/// the tree's own `.git` folder, config and the tree, and no other repository's config, objects or work tree is
/// reached. A step's line names the repo `name` when given, as a pair run's lines do ([`tree_step`]).
/// A `.git` made anew is no warm tree and has run nothing: the work folder's warm and ran records go first, before
/// `git init`. A record that
/// cannot be removed readies nothing: the preflight leaves the tree to the run, whose own removal refuses it.
pub(super) fn ready_tree(git: &impl TreeGit, at: &TreeAt<'_>, name: Option<&str>, console: &mut Console) -> Result<(), Unready> {
    ready_new_tree(git, at, name, console).map(|_| ())
}

/// [`ready_tree`], and whether the tree is new to this run: nothing had the name `.git` there, or its `.git` had no
/// `HEAD`, so its repository is made now, or by the run when the preflight leaves it to the run.
fn ready_new_tree(git: &impl TreeGit, at: &TreeAt<'_>, name: Option<&str>, console: &mut Console) -> Result<bool, Unready> {
    let (bare, tree) = (at.bare, at.tree);
    if git.holds_the_lock() {
        repair_git(tree).map_err(Unready::Folder)?;
    } else if has_a_link_in_its_git(Path::new(tree)) {
        return Ok(false);
    }
    // A run removes a link, a junction or a file named `.git` before it readies a tree (`plain_tree` in the run). A
    // `.git` with no `HEAD`, as an `init` cut short leaves it, is made again: `git init` over it keeps what is there.
    let made = !fs::symlink_metadata(Path::new(tree).join(".git").join("HEAD")).is_ok_and(|metadata| metadata.is_file());
    if made {
        for record in [at.warm, at.ran] {
            if let Err(error) = remove_entry(Path::new(record)) {
                if !git.holds_the_lock() {
                    // The preflight leaves the tree to the run, which removes the record under the lock or is refused.
                    stamping::err(
                        console,
                        &format!(
                            "gate: remote could not remove {}: {}",
                            gate_router::escaped(record),
                            gate_router::escaped(&error.to_string())
                        ),
                    );
                    return Ok(true);
                }
                return Err(Unready::Folder(format!("cannot remove {record}: {error}")));
            }
        }
        tree_step(git, "git init", name, &["init", "-q", "--", tree], console)?;
    }
    if !made || !fresh_config_written(bare, tree, name)? {
        configure(git, bare, tree, name, console)?;
    }
    if let Some(line) = write_alternates(bare, tree, git.holds_the_lock()).map_err(Unready::Folder)? {
        stamping::err(console, &line);
    }
    Ok(made)
}

/// The tree `tree`'s `origin`, the bare repository `bare`, added unless it has one, and its line-ending config, each
/// written by a git step through `git`, its line naming the repo `name` when given ([`tree_step`]).
fn configure(git: &impl TreeGit, bare: &str, tree: &str, name: Option<&str>, console: &mut Console) -> Result<(), Unready> {
    let git_dir = format!(r"--git-dir={tree}\.git");
    let work_tree = format!("--work-tree={tree}");
    let (git_dir, work_tree) = (git_dir.as_str(), work_tree.as_str());
    // Checked apart from .git, so a preflight cut short between the two steps is finished by the next one. A git that
    // cannot be started here fails the next step, which names it; one that started and did not end fails this one.
    let step = "git config";
    match git.run_code(
        &for_repo(step, name),
        &[git_dir, work_tree, "config", "--get", "remote.origin.url"],
        console,
    ) {
        Ok(0) => {}
        Ok(_) | Err(GitFailure::NotStarted(_)) => {
            let args = [git_dir, work_tree, "remote", "add", "--", "origin", bare];
            tree_step(git, "git remote add", name, &args, console)?;
        }
        Err(failure) => return Err(Unready::Git(failed_for(failure.of_step(&for_repo(step, name)), step, name))),
    }
    // The tree's own config, so the checkout and the sync-back keep the snapshot's bytes whatever the builder's system
    // and global git config say.
    for (setting, value) in [("core.autocrlf", "false"), ("core.eol", "lf")] {
        let args = [git_dir, work_tree, "config", setting, value];
        tree_step(git, &format!("git config {setting}"), name, &args, console)?;
    }
    Ok(())
}

/// The step whose line a failed [`fresh_config_written`] says, as the `git remote add` it stands in for would.
const REMOTE_ADD: &str = "git remote add";
/// The refspec `git remote add` gives a remote's `fetch`.
const ORIGIN_FETCH: &str = "+refs/heads/*:refs/remotes/origin/*";

/// The config's name in the tree's `.git` folder, and the lock git takes to replace it.
const CONFIG: &str = "config";
const CONFIG_LOCK: &str = "config.lock";

/// Writes the config `git init` just made in the tree `tree` once, with the `origin` remote `bare` and the line-ending
/// config in it, as [`configure`]'s git steps would leave it ([`fresh_config`]): whether it did. The tree's `.git` is
/// opened as itself and held, and git's own `config.lock` taken beneath it before the config is read as itself and
/// replaced whole ([`replace_config`]). `false`, with nothing written, so the git steps write it,
/// when `.git` is not a folder itself, the config is missing, not a plain file or not UTF-8, it holds more than `git
/// init`'s own `[core]` section, or its lock is held. A read or a write that fails says the `git remote add` step's
/// line, naming the repo `name` when given, and leaves the config as it was.
fn fresh_config_written(bare: &str, tree: &str, name: Option<&str>) -> Result<bool, Unready> {
    let git_folder = Path::new(tree).join(".git");
    let failed = |why: String| {
        let line = format!("gate: remote {} failed: {why}", for_repo(REMOTE_ADD, name));
        Unready::Git(failed_for(Failed::without_status(line), REMOTE_ADD, name))
    };
    let config = git_folder.join(CONFIG);
    let folder = OpenOptions::new()
        .read(true)
        .share_mode(Share::NoDelete.mode())
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&git_folder)
        .map_err(|error| failed(format!("cannot open {}: {error}", git_folder.display())))?;
    if !is_folder_itself(&folder).map_err(|error| failed(format!("cannot read {}: {error}", git_folder.display())))? {
        return Ok(false);
    }
    replace_config(&folder, bare, |lock, text| lock.write_all(text.as_bytes()))
        .map_err(|error| failed(format!("cannot write {}: {error}", config.display())))
}

/// The text of the config beneath the held `.git` folder `folder`, opened as itself and read through that handle;
/// `None` when it is missing, not a plain file (a link, a junction or a folder) or not UTF-8. Else why it could not be
/// read.
fn plain_config(folder: &File) -> io::Result<Option<String>> {
    let mut config = match gate_win32::open_beneath(folder, CONFIG, Share::All) {
        Ok(config) => config,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = config.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    config.read_to_end(&mut bytes)?;
    Ok(String::from_utf8(bytes).ok())
}

/// Replaces the config beneath the held `.git` folder `folder` whole, in git's order: `config.lock` made new beneath
/// the handle, then the config read as itself ([`plain_config`]) and its new text, with the `origin` remote `bare`
/// ([`fresh_config`]), written into the lock by `fill`, and the lock renamed over the config by handle. `false`, with
/// nothing written, when the lock is there already, a git or another preflight holding it, or when the config cannot
/// be written once. Every way but the rename deletes the lock through its handle and leaves the config as it was;
/// else why it failed.
fn replace_config(folder: &File, bare: &str, fill: impl FnOnce(&mut File, &str) -> io::Result<()>) -> io::Result<bool> {
    let made = gate_win32::open_beneath_as(folder, CONFIG_LOCK, FILE_GENERIC_WRITE | DELETE, Share::NoDelete, Make::NewFile);
    let mut lock = match made {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => return Err(error),
    };
    let replaced = locked_replace(folder, &mut lock, bare, fill);
    if matches!(replaced, Ok(true)) {
        return replaced;
    }
    match (replaced, gate_win32::delete_opened(&lock)) {
        (replaced, Ok(())) => replaced,
        (Ok(_), Err(left)) => Err(io::Error::new(left.kind(), format!("{CONFIG_LOCK} could not be removed: {left}"))),
        (Err(error), Err(left)) => Err(io::Error::new(
            error.kind(),
            format!("{error}; {CONFIG_LOCK} could not be removed: {left}"),
        )),
    }
}

/// [`replace_config`] once its lock `lock` is held: whether the config was replaced, `false` when it cannot be
/// written once; else why the read, the write or the rename failed.
fn locked_replace(folder: &File, lock: &mut File, bare: &str, fill: impl FnOnce(&mut File, &str) -> io::Result<()>) -> io::Result<bool> {
    let read = plain_config(folder).map_err(|error| io::Error::new(error.kind(), format!("cannot read {CONFIG}: {error}")))?;
    let Some(text) = read.and_then(|text| fresh_config(&text, bare)) else {
        return Ok(false);
    };
    fill(lock, &text)?;
    gate_win32::rename_beneath(lock, folder, CONFIG)?;
    Ok(true)
}

/// The config `text` `git init` wrote, with `core.autocrlf=false` and `core.eol=lf` added to its `[core]` section and
/// the `origin` remote `bare` after it, as `git remote add` and `git config` write them; `None` when `text` holds any
/// section but `[core]`, either of those keys already, a U+FEFF anywhere, which could hide a section's header, or does
/// not end its last line, or ends it with a `\` that would carry it on, which the git steps handle.
fn fresh_config(text: &str, bare: &str) -> Option<String> {
    let headers: Vec<&str> = text.lines().map(str::trim).filter(|line| line.starts_with('[')).collect();
    let set = text.lines().any(|line| {
        let key = line.trim_start().split(['=', ' ', '\t']).next().unwrap_or_default();
        key.eq_ignore_ascii_case("autocrlf") || key.eq_ignore_ascii_case("eol")
    });
    let continued = text.lines().last().is_some_and(|line| line.ends_with('\\'));
    if headers != ["[core]"] || set || continued || text.contains('\u{feff}') || !text.ends_with('\n') {
        return None;
    }
    Some(format!(
        "{text}\tautocrlf = false\n\teol = lf\n[remote \"origin\"]\n\turl = {}\n\tfetch = {ORIGIN_FETCH}\n",
        config_value(bare)
    ))
}

/// The config value `value` as git writes it: its `\` and `"` escaped, a line end and a tab written as `\n` and `\t`,
/// and the whole quoted when it starts or ends with a space or holds a `;`, a `#` or a carriage return.
fn config_value(value: &str) -> String {
    let quoted = value.starts_with(' ') || value.ends_with(' ') || value.contains([';', '#', '\r']);
    let mut said = String::new();
    if quoted {
        said.push('"');
    }
    for character in value.chars() {
        match character {
            '\n' => said.push_str("\\n"),
            '\t' => said.push_str("\\t"),
            '"' | '\\' => {
                said.push('\\');
                said.push(character);
            }
            other => said.push(other),
        }
    }
    if quoted {
        said.push('"');
    }
    said
}

/// Runs the git step `step` with `args` through `git`, named for the repo `name` on a pair run ([`for_repo`]), its
/// failure's line as [`failed_for`] gives it.
fn tree_step(git: &impl TreeGit, step: &str, name: Option<&str>, args: &[&str], console: &mut Console) -> Result<(), Unready> {
    git.run_step(&for_repo(step, name), args, console)
        .map_err(|failed| Unready::Git(failed_for(failed, step, name)))
}

/// The step `step` as a pair run's git step naming the repo `name` is said: `{step} for {name}`, which cannot be read
/// as one of the step's own words; `step` as it is on a run of one repo.
fn for_repo(step: &str, name: Option<&str>) -> String {
    match name {
        Some(name) => format!("{step} for {name}"),
        None => step.to_owned(),
    }
}

/// The failure `failed` of the git step `step`, run as [`for_repo`] names it: its line says `{step} failed for {name}`
/// on a pair run, and is as it is on a run of one repo.
fn failed_for(failed: Failed, step: &str, name: Option<&str>) -> Failed {
    let Some(name) = name else {
        return failed;
    };
    let line = failed
        .line
        .replacen(&format!("{step} for {name} failed"), &format!("{step} failed for {name}"), 1);
    Failed { line, ..failed }
}

/// The step `step` as its lines name it: followed by the repo's name `name` on a pair run, as it is on a run of one
/// repo.
pub(super) fn named_step(step: &str, name: Option<&str>) -> String {
    match name {
        Some(name) => format!("{step} {name}"),
        None => step.to_owned(),
    }
}

/// Why a tree could not be readied ([`ready_tree`]).
pub(super) enum Unready {
    /// A git step failed.
    Git(Failed),
    /// A step on the file system failed, for this reason, which says what could not be done.
    Folder(String),
}

/// The tree `tree`'s `.git`, `.git\objects` and `.git\objects\info`.
fn git_folders(tree: &Path) -> [PathBuf; 3] {
    let git = tree.join(".git");
    let objects = git.join("objects");
    let info = objects.join("info");
    [git, objects, info]
}

/// Whether the config file `config` is missing or a file itself, read without following a link: a link there would
/// lead git's config writes, which replace the file through its lock file, into another repository's config.
fn config_is_plain(config: &Path) -> bool {
    !fs::symlink_metadata(config).is_ok_and(|metadata| !metadata.is_file())
}

/// Whether any of the tree `tree`'s `.git`, `.git\objects` and `.git\objects\info` is there and is not a folder itself,
/// or its `.git\config` is there and is not a file itself, read without following a link: a link, a junction or a
/// file, through which git or the alternates file would reach another repository.
fn has_a_link_in_its_git(tree: &Path) -> bool {
    let [git, objects, info] = git_folders(tree);
    let not_a_folder = |folder: &PathBuf| fs::symlink_metadata(folder).is_ok_and(|metadata| !metadata.is_dir());
    [&git, &objects, &info].into_iter().any(not_a_folder) || !config_is_plain(&git.join("config"))
}

/// Whether the tree `tree`'s `.git`, `.git\objects` and `.git\objects\info` are each a folder itself
/// ([`is_real_folder`]) and its `.git\config` is missing or a file itself ([`config_is_plain`]).
pub(super) fn git_is_plain(tree: &Path) -> bool {
    let folders = git_folders(tree);
    folders.iter().all(|folder| is_real_folder(folder)) && config_is_plain(&folders[0].join("config"))
}

/// Under the work folder's lock, before any git step: a `.git\config` that is not a file itself removed as itself,
/// for git to write afresh, and `.git\objects` and `.git\objects\info` made folders of their own ([`made_folder`]), so
/// no git step reads or writes through a link there. Nothing when the tree has no `.git` folder, which `git init` then
/// makes. Else why not.
fn repair_git(tree: &str) -> Result<(), String> {
    let [git, objects, info] = git_folders(Path::new(tree));
    if !is_real_folder(&git) {
        return Ok(());
    }
    let config = git.join("config");
    if !config_is_plain(&config) {
        remove_entry(&config).map_err(|error| format!("cannot remove {}: {error}", config.display()))?;
    }
    let alternates = info.join("alternates");
    for folder in [&objects, &info] {
        made_folder(folder, true).map_err(|why| format!("could not write {}: {why}", alternates.display()))?;
    }
    Ok(())
}

/// Writes `<bare with / separators>/objects` and LF, UTF-8 without a BOM, as the tree's alternates file unless it
/// already holds exactly that, every name read without following a link. The folders `.git\objects` and
/// `.git\objects\info` are made when missing ([`made_folder`]), and the file written ([`written`]), the line saying
/// where a folder at its name was moved aside, if one was. Else why not: `could not write {file}: ` and the reason.
fn write_alternates(bare: &str, tree: &str, repair: bool) -> Result<Option<String>, String> {
    let objects = Path::new(tree).join(".git").join("objects");
    let info = objects.join("info");
    let alternates = info.join("alternates");
    let cannot = |why: String| format!("could not write {}: {why}", alternates.display());
    for folder in [&objects, &info] {
        made_folder(folder, repair).map_err(cannot)?;
    }
    let text = format!("{}/objects\n", bare.replace('\\', "/"));
    written(&alternates, &text).map_err(cannot)
}

/// Makes the folder `folder` when nothing has its name, read without following a link; one that is a link, a junction
/// or a file is removed as itself and made again when `repair`, else refused. Else why not.
pub(super) fn made_folder(folder: &Path, repair: bool) -> Result<(), String> {
    match fs::symlink_metadata(folder) {
        Ok(metadata) if metadata.is_dir() => return Ok(()),
        Ok(_) if !repair => return Err(format!("{} is a link, a junction or a file", folder.display())),
        Ok(_) => remove_entry(folder).map_err(|error| format!("cannot remove {}: {error}", folder.display()))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    fs::create_dir(folder).map_err(|error| format!("cannot create {}: {error}", folder.display()))
}

/// Writes `text` as the file `file` unless it already holds exactly that, its folder opened as itself and held, and the
/// file read, removed and made beneath that handle ([`written_beneath`]); the line saying where a folder at the name
/// was moved aside, when one was ([`moved_aside_line`]). Else why not, a folder that is a link, a junction or not a
/// folder refused.
pub(super) fn written(file: &Path, text: &str) -> Result<Option<String>, String> {
    let (Some(parent), Some(name)) = (file.parent(), file.file_name().and_then(|name| name.to_str())) else {
        return Err(format!("{} names no file in a folder", file.display()));
    };
    let folder = OpenOptions::new()
        .read(true)
        .share_mode(Share::NoDelete.mode())
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(parent)
        .map_err(|error| error.to_string())?;
    if !is_folder_itself(&folder).map_err(|error| error.to_string())? {
        return Err(format!("{} is a link, a junction or not a folder", parent.display()));
    }
    Ok(written_beneath(&folder, name, text)?.map(|moved| moved_aside_line(file, &moved)))
}

/// The line saying the folder that was at the path `file` is now `moved`, one name, beside it.
pub(super) fn moved_aside_line(file: &Path, moved: &str) -> String {
    format!(
        "gate: remote moved aside the folder {} to {}",
        file.display(),
        file.with_file_name(moved).display()
    )
}

/// Whether the open `opened` is a folder itself: a folder, not a link or a junction.
pub(super) fn is_folder_itself(opened: &File) -> io::Result<bool> {
    let metadata = opened.metadata()?;
    Ok(metadata.is_dir() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0)
}

/// Writes `text` as the file `name` in the held folder `folder` unless it already holds exactly that: whatever is at the
/// name opened as itself beneath the handle, never followed; a folder of its own, which another account may have
/// filled, moved aside beneath the handle to `{name}.gate-moved-{n}` for the first free `n` and never walked
/// ([`moved_aside`]); a plain file of other bytes, a link, a junction or anything else deleted as itself; and a new
/// file made beneath the handle, refused when anything took the name meanwhile. The name a folder was moved aside to,
/// when one was. Else why not.
pub(super) fn written_beneath(folder: &File, name: &str, text: &str) -> Result<Option<String>, String> {
    let moved = match gate_win32::open_beneath_as(folder, name, FILE_GENERIC_READ | DELETE, Share::All, Make::Open) {
        Ok(found) => match found_at_name(folder, name, found, text)? {
            Found::Holds => return Ok(None),
            Found::Deleted => None,
            Found::MovedAside(moved) => Some(moved),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.to_string()),
    };
    let mut made =
        gate_win32::open_beneath_as(folder, name, FILE_GENERIC_WRITE, Share::NoDelete, Make::NewFile).map_err(|error| error.to_string())?;
    made.write_all(text.as_bytes()).map_err(|error| error.to_string())?;
    Ok(moved)
}

/// What [`written_beneath`] found at its name, and did with it.
enum Found {
    /// A plain file holding exactly the text, left as it is.
    Holds,
    /// Anything else but a folder of its own, deleted as itself through its handle.
    Deleted,
    /// A folder of its own, moved aside to this name beside it.
    MovedAside(String),
}

/// Whether `found`, the entry `name` of the held folder `folder` opened as itself with `DELETE` access, is a plain file
/// holding exactly `text`; else a folder of its own is moved aside ([`moved_aside`]) and anything else deleted as
/// itself, through its handle. Else why it could not be moved or deleted.
fn found_at_name(folder: &File, name: &str, mut found: File, text: &str) -> Result<Found, String> {
    let metadata = found.metadata().map_err(|error| error.to_string())?;
    let reparse = metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    if metadata.is_file() && !reparse {
        let mut held = Vec::new();
        if found.read_to_end(&mut held).is_ok() && held == text.as_bytes() {
            return Ok(Found::Holds);
        }
    }
    if metadata.is_dir() && !reparse {
        return moved_aside(folder, name, &found)
            .map(Found::MovedAside)
            .map_err(|unmoved| unmoved.why(name));
    }
    gate_win32::delete_opened(&found).map_err(|error| error.to_string())?;
    Ok(Found::Deleted)
}

/// The most names `{name}.gate-moved-{n}` tried for a folder moved aside.
const MOVED_ASIDE_LIMIT: u32 = 100;

/// Renames `found`, the folder `name` of the held folder `folder` opened with `DELETE` access, to
/// `{name}.gate-moved-{n}` beside it, by handle and never replacing what has that name
/// ([`gate_win32::rename_beneath_no_replace`]), for the first `n` from 1 the rename finds free: the new name. Else why
/// not: the rename's error, or every `n` up to [`MOVED_ASIDE_LIMIT`] taken.
pub(super) fn moved_aside(folder: &File, name: &str, found: &File) -> Result<String, Unmoved> {
    for n in 1..=MOVED_ASIDE_LIMIT {
        let moved = format!("{name}.gate-moved-{n}");
        match gate_win32::rename_beneath_no_replace(found, folder, &moved) {
            Ok(()) => return Ok(moved),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(Unmoved::Renamed(error)),
        }
    }
    Err(Unmoved::Taken)
}

/// Why [`moved_aside`] did not move a folder aside.
pub(super) enum Unmoved {
    /// The rename failed with this error.
    Renamed(io::Error),
    /// Every name `{name}.gate-moved-{n}` up to [`MOVED_ASIDE_LIMIT`] is taken.
    Taken,
}

impl Unmoved {
    /// The refusal of moving aside the folder `name` for this reason.
    pub(super) fn why(&self, name: &str) -> String {
        match self {
            Unmoved::Renamed(error) => format!("cannot move aside the folder {name}: {error}"),
            Unmoved::Taken => {
                format!("cannot move aside the folder {name}: {name}.gate-moved-1 to {name}.gate-moved-{MOVED_ASIDE_LIMIT} are all taken")
            }
        }
    }
}

/// Removes the entry `path` as itself, never what a link or a junction there leads to: a folder, or a link or a
/// junction to one, with std's `remove_dir_all`, which removes a link it meets rather than following it; anything else
/// with `remove_file`. Nothing when nothing has the name.
pub(super) fn remove_entry(path: &Path) -> io::Result<()> {
    let kind = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata.file_type(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if kind.is_dir() || kind.is_symlink_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Whether `path` is a folder itself, read without following a link: a link or a junction there is none.
pub(super) fn is_real_folder(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// The folder `path` opened by its path as itself, sharing no deletion, so it is neither removed nor renamed while
/// held; `None` when nothing has the name or it is a link, a junction or a file. Else the system's error.
pub(super) fn held_folder_at(path: &Path) -> io::Result<Option<File>> {
    let opened = OpenOptions::new()
        .read(true)
        .share_mode(Share::NoDelete.mode())
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path);
    match opened {
        Ok(folder) => Ok(is_folder_itself(&folder)?.then_some(folder)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// The entry `name` of the held folder `folder` opened beneath its handle as itself and held as [`held_folder_at`]
/// holds a folder; `None` when nothing has the name or it is a link, a junction or a file. Else the system's error.
pub(super) fn held_folder_beneath(folder: &File, name: &str) -> io::Result<Option<File>> {
    match gate_win32::open_beneath_as(folder, name, FILE_GENERIC_READ, Share::NoDelete, Make::Open) {
        Ok(entry) => Ok(is_folder_itself(&entry)?.then_some(entry)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Removes git's lock file `name` beneath the held folder `folder`, at `path`: opened beneath the handle as itself with
/// `DELETE` and no sharing at all, and deleted through that handle, so a link or a junction of that name is deleted
/// itself, never what it leads to. Nothing when nothing has the name, or when another handle holds it open: a live git
/// or preflight writes through it, and git says so itself if it needs it. Else why not, a folder of its own at the
/// name refused and left, as git could not have left it.
pub(super) fn removed_lock(folder: &File, name: &str, path: &Path) -> Result<(), String> {
    let lock = match gate_win32::open_beneath_as(folder, name, FILE_GENERIC_READ | DELETE, Share::None, Make::Open) {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION.cast_signed()) => return Ok(()),
        Err(error) => return Err(format!("cannot open {}: {error}", path.display())),
    };
    if is_folder_itself(&lock).map_err(|error| format!("cannot read {}: {error}", path.display()))? {
        return Err(format!("{} is a folder where git's lock file goes", path.display()));
    }
    gate_win32::delete_opened(&lock).map_err(|error| format!("cannot remove {}: {error}", path.display()))
}

/// The bound of a box git step that reads or writes refs, config or a listing: `init`, `config`, `remote add`,
/// `cat-file`, `grep`, `check-ignore`, `write-tree`, `commit-tree`, `rev-parse`, `diff-tree`, `update-ref` and the LFS
/// listing.
pub(crate) const GIT_SHORT_LIMIT: Duration = Duration::from_mins(5);
/// The bound of a box git step that moves a tree's or a repository's content: `fetch`, `checkout`, `lfs checkout`,
/// `clean`, `add -A` and `push`.
pub(crate) const GIT_LONG_LIMIT: Duration = Duration::from_mins(30);

/// `--test-box-git-limit <step>:<ms>`, set once before the box's verb runs ([`set_git_limit`]): the step, as the box's
/// lines name it, and the bound it takes in place of its class's.
static GIT_LIMIT_SEAM: OnceLock<(String, Duration)> = OnceLock::new();

/// `--test-box-git-armed-by <path>`, set once beside the limit it arms ([`set_git_limit`]): the file whose appearance
/// starts the bound of the step the limit names, that step having none until then.
static GIT_ARMED_BY: OnceLock<PathBuf> = OnceLock::new();

/// Makes `--test-box-git-limit`, when `seams` gives it, the bound of the git step it names in place of
/// [`GIT_SHORT_LIMIT`] or [`GIT_LONG_LIMIT`], every other step keeping its own, and `--test-box-git-armed-by` the file
/// that step's bound waits for before it starts counting; called once, before the box's verb runs.
pub(crate) fn set_git_limit(seams: &BoxSeams) {
    if let Some((step, ms)) = &seams.git_limit {
        GIT_LIMIT_SEAM.get_or_init(|| (step.clone(), Duration::from_millis(*ms)));
    }
    if let Some(path) = &seams.git_armed_by {
        GIT_ARMED_BY.get_or_init(|| path.clone());
    }
}

/// Which bound a box git step takes, and whether a gone client or a stop ends it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GitClass {
    /// Bounded by [`GIT_SHORT_LIMIT`], and ended once the client is gone or a stop came.
    Short,
    /// Bounded by [`GIT_LONG_LIMIT`], and ended once the client is gone or a stop came.
    Long,
    /// A write of the shared bare repository bounded by [`GIT_SHORT_LIMIT`], run to its end or its bound when the
    /// client is gone or a stop came: a killed one could leave `packed-refs.lock`, which the box never removes.
    ShortBare,
    /// A write of the shared bare repository bounded by [`GIT_LONG_LIMIT`], run on past a gone client and a stop as
    /// [`GitClass::ShortBare`] is.
    LongBare,
}

impl GitClass {
    /// Its built-in bound.
    fn limit(self) -> Duration {
        match self {
            GitClass::Short | GitClass::ShortBare => GIT_SHORT_LIMIT,
            GitClass::Long | GitClass::LongBare => GIT_LONG_LIMIT,
        }
    }

    /// Whether a gone client or a stop ends the step: every step but a write of the shared bare repository.
    fn ends_early(self) -> bool {
        matches!(self, GitClass::Short | GitClass::Long)
    }
}

/// A box git step: its name, as the box's lines name it (`checkout`, `git init --bare`, `push lib`), and its class.
#[derive(Clone, Copy)]
pub(crate) struct GitStep<'a> {
    pub(crate) name: &'a str,
    pub(crate) class: GitClass,
}

impl GitStep<'_> {
    /// Its bound: `--test-box-git-limit`'s when the box was given it for this step, else its class's.
    fn limit(&self) -> Duration {
        match GIT_LIMIT_SEAM.get() {
            Some((step, limit)) if step == self.name => *limit,
            _ => self.class.limit(),
        }
    }

    /// The file `--test-box-git-armed-by` names when this is the step `--test-box-git-limit` names and the box was
    /// given both; `None` for a step whose bound counts from its start.
    fn armed_by(&self) -> Option<&'static Path> {
        match GIT_LIMIT_SEAM.get() {
            Some((step, _)) if step == self.name => GIT_ARMED_BY.get().map(PathBuf::as_path),
            _ => None,
        }
    }
}

/// How the box takes one of git's output streams.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pipe {
    /// Read to its end and dropped.
    Discard,
    /// Forwarded line by line to the box's own stream of its name, under the box's output lock ([`Forwarders`]).
    Forward,
    /// Read to its end and given back.
    Read,
}

/// How a box git step's two output streams are taken, and the bytes fed on its standard input, `NUL` without.
pub(crate) struct Streams<'a> {
    pub(crate) stdout: Pipe,
    pub(crate) stderr: Pipe,
    pub(crate) feed: Option<&'a [u8]>,
}

impl Streams<'static> {
    /// Both streams forwarded, nothing fed.
    pub(crate) const FORWARDED: Streams<'static> = Streams {
        stdout: Pipe::Forward,
        stderr: Pipe::Forward,
        feed: None,
    };
    /// Both streams dropped, nothing fed.
    pub(crate) const QUIET: Streams<'static> = Streams {
        stdout: Pipe::Discard,
        stderr: Pipe::Discard,
        feed: None,
    };
}

/// The run's stop signal as a git step looks at it, the run's job, which the signal belongs to and the line of a step
/// it ended names, and the run's flag of whether the signal's unreadable line was said ([`StopSignal::sharing`]).
#[derive(Clone, Copy)]
pub(crate) struct GitStop<'a> {
    pub(crate) event: &'a NamedEvent,
    pub(crate) job: &'a str,
    pub(crate) unread_said: &'a Rc<Cell<bool>>,
}

impl GitStop<'_> {
    /// The run's signal as one look of a git step reads it, sharing the run's unreadable line.
    fn signal(&self) -> StopSignal<'_> {
        StopSignal::sharing(self.event, self.job, self.unread_said)
    }

    /// The failure of a step the stop ended or kept from starting: its job named.
    fn stopped(&self) -> GitFailure {
        GitFailure::Failed(format!("the job {} was stopped", self.job))
    }
}

/// The box's output lock, which git's forwarded lines and the wait's stderr probe take; the run's client-gone flag,
/// which a failed forward or probe sets and every git wait reads; and the run's stop signal, which the wait of every
/// step a stop ends looks at, `None` where no stop reaches git: the preflight and the inputs and LFS verbs.
#[derive(Clone, Copy)]
pub(crate) struct Shared<'a> {
    pub(crate) lock: &'a OutputLock,
    pub(crate) gone: &'a ClientGone,
    pub(crate) stop: Option<GitStop<'a>>,
}

impl<'a> Shared<'a> {
    /// `lock` and `gone`, with no stop signal.
    pub(crate) fn alone(lock: &'a OutputLock, gone: &'a ClientGone) -> Shared<'a> {
        Shared { lock, gone, stop: None }
    }
}

/// A git step that ran to its end: its exit code, and what it wrote on each stream taken as [`Pipe::Read`], empty
/// for the others.
pub(crate) struct GitOutput {
    pub(crate) code: i32,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

/// git, found once along the gate's own `PATH` and `PATHEXT`.
pub(crate) struct Git {
    pub(crate) program: Result<PathBuf, String>,
}

impl Git {
    pub(crate) fn find() -> Git {
        let program = locate("git").and_then(|found| found.ok_or_else(|| gate_win32::win32_message(ERROR_FILE_NOT_FOUND)));
        Git { program }
    }

    /// Runs git with `args` as `step` of `class` ([`Git::alone`]), its output forwarded to the box's own streams; a
    /// non-zero status, or a git that cannot be started or did not run to its end, fails `step`, named as the wrapper
    /// names it, and a gone client fails it with [`run::client_gone`]'s line and status.
    pub(crate) fn step(&self, step: &str, args: &[&str], class: GitClass, console: &mut Console) -> Result<(), Failed> {
        match self.alone(args, GitStep { name: step, class }, &Streams::FORWARDED, console) {
            Ok(GitOutput { code: 0, .. }) => Ok(()),
            Ok(ran) => Err(git_failed(step, ran.code)),
            Err(failure) => Err(failure.of_step(step)),
        }
    }

    /// git with `args` run as `step` ([`Git::run`]) under an output lock and a client-gone flag of its own, with no
    /// stop signal: the preflight has no heartbeat to share them with and no stop to watch.
    fn alone(&self, args: &[&str], step: GitStep<'_>, streams: &Streams<'_>, console: &mut Console) -> Result<GitOutput, GitFailure> {
        let (lock, gone) = (OutputLock::default(), ClientGone::default());
        self.run(args, step, streams, Shared::alone(&lock, &gone), console)
    }

    /// git with `args` run as `step` ([`run_git`]), its streams taken as `streams` says; else why it did not run to
    /// its end.
    pub(crate) fn run(
        &self,
        args: &[&str],
        step: GitStep<'_>,
        streams: &Streams<'_>,
        shared: Shared<'_>,
        console: &mut Console,
    ) -> Result<GitOutput, GitFailure> {
        let command = self.command(args).map_err(GitFailure::NotStarted)?;
        run_git(&command, step, streams, shared, console)
    }

    /// git with `args`, reading no standard input, none of the variables that would point it elsewhere and no
    /// system-wide attributes file ([`crate::remote::lfs::NO_SYSTEM_ATTRIBUTES`]); else why it cannot be started.
    /// A git started with a caller's `MSYSTEM` and no `usr\bin` on its PATH cannot find `sh` and crashes (0xC0000005),
    /// so the box never passes `MSYSTEM` on to its git.
    pub(crate) fn command(&self, args: &[&str]) -> Result<Command, String> {
        let program = self.program.as_ref().map_err(Clone::clone)?;
        let mut command = Command::new(program);
        for name in GIT_REDIRECTS {
            command.env_remove(name);
        }
        command.env_remove("MSYSTEM");
        command.env(crate::remote::lfs::NO_SYSTEM_ATTRIBUTES, "1");
        command.args(args).stdin(Stdio::null());
        Ok(command)
    }
}

/// The line said when a leftover process still holds git's output open once git exited.
pub(crate) const GIT_LEFTOVER: &str = "gate: remote git's output is still held open by a leftover process; stopped reading";

/// Why a box git step did not run to its end.
#[derive(Debug)]
pub(crate) enum GitFailure {
    /// It could not be started, for this reason: no git was found, or the system would not create its process.
    NotStarted(String),
    /// It could not be read or waited for, or its bound or a stop ended it, for this reason.
    Failed(String),
    /// The client closed the connection: a line of git's could not be written to the box's own output, or the
    /// heartbeat or the wait's own probe found stderr's reader gone. git's tree is ended, unless the step writes the
    /// shared bare repository, which ran to its end or its bound.
    ClientGone,
}

impl GitFailure {
    /// The failure of the git step `step` this is: `gate: remote {step} failed: {why}` with status 1, or, for a gone
    /// client, [`run::client_gone`]'s.
    pub(crate) fn of_step(self, step: &str) -> Failed {
        match self {
            GitFailure::NotStarted(why) | GitFailure::Failed(why) => Failed::without_status(format!("gate: remote {step} failed: {why}")),
            GitFailure::ClientGone => run::client_gone(step),
        }
    }
}

/// How the wait for a git step ended other than in git's exit.
enum GitEnd {
    /// The step's bound passed.
    Bound,
    /// The client is gone, and the step is one a gone client ends.
    Gone,
}

/// A thread reading one of git's output streams to its end: the bytes it read, kept or dropped; else why it could not.
type Reading = JoinHandle<io::Result<Vec<u8>>>;

/// The readers of a started git's output: the forwarders of the streams taken as [`Pipe::Forward`], a thread for
/// each other one, and the thread feeding its input.
struct Readers {
    forwarders: Forwarders,
    stdout: Option<Reading>,
    stderr: Option<Reading>,
    fed: Option<JoinHandle<io::Result<()>>>,
}

/// Runs a box git step, every box git call going through here: once `shared`'s flag is set, none starts but a write of
/// the shared bare repository, so a failed push's undo still tries every ref. git is created suspended inside a
/// kill-on-close job of its own that refuses breakaway, and only then run, so ending the job ends git's whole tree,
/// the inner git Git for Windows' launcher starts included; a batch stand-in runs through cmd.exe
/// ([`crate::run::written`]). The wait ([`Watched::wait`]) looks every [`run::WAIT_STEP`] whether git exited, probes
/// the box's stderr ([`heartbeat::probe_stderr`]), and ends the job at `step`'s bound, failing the step with
/// [`git::late_git_line`]'s text; for a step a gone client or a stop ends ([`GitClass::ends_early`]), once the flag is
/// set, as [`GitFailure::ClientGone`], or once `shared`'s stop signal is, naming the run's job. A step that fails while
/// the flag is set is [`GitFailure::ClientGone`], whatever ended it. Once git exited, the readers drain for [`DRAIN`],
/// a leftover process still holding git's output then said ([`GIT_LEFTOVER`]) and ended with the job, and a flag set
/// by then is a gone client.
pub(crate) fn run_git(
    command: &Command,
    step: GitStep<'_>,
    streams: &Streams<'_>,
    shared: Shared<'_>,
    console: &mut Console,
) -> Result<GitOutput, GitFailure> {
    let (ran, all_ended) = git_ran(command, step, streams, shared, console)?;
    if !all_ended {
        let _held = shared.lock.lock().unwrap_or_else(PoisonError::into_inner);
        stamping::err(console, Watching::Git.leftover());
    }
    if shared.gone.load(Ordering::SeqCst) {
        return Err(GitFailure::ClientGone);
    }
    Ok(ran)
}

/// [`run_git`] up to the drain: what git gave, and whether every forwarder reached the end of its pipe.
fn git_ran(
    command: &Command,
    step: GitStep<'_>,
    streams: &Streams<'_>,
    shared: Shared<'_>,
    console: &mut Console,
) -> Result<(GitOutput, bool), GitFailure> {
    if step.class.ends_early() && shared.gone.load(Ordering::SeqCst) {
        return Err(GitFailure::ClientGone);
    }
    // One signal for the step, so its look before the start and its wait's looks share the run's unreadable line.
    let stop = shared.stop.filter(|_| step.class.ends_early());
    let signal = stop.as_ref().map(GitStop::signal);
    if let (Some(stop), Some(signal)) = (&stop, &signal) {
        let _held = shared.lock.lock().unwrap_or_else(PoisonError::into_inner);
        if signal.seen(Watching::Git.while_waiting(), console) {
            return Err(stop.stopped());
        }
    }
    let failed = |error: &dyn std::fmt::Display| GitFailure::Failed(error.to_string());
    let launch = git_launch(command, step.class).map_err(GitFailure::NotStarted)?;
    let job = launch::call_job(false).map_err(|error| GitFailure::Failed(format!("cannot make its job: {error}")))?;
    let (suspended, stdout, stderr, stdin) = launch::launched(&launch, &job, streams.feed.is_some()).map_err(|error| match error {
        LaunchError::CreateProcess { .. } => GitFailure::NotStarted(error.to_string()),
        LaunchError::Setup { .. } => failed(&error),
    })?;
    let running = suspended.assign_and_resume(&job).map_err(|error| failed(&error))?;
    // A reader that cannot start leaves git to the job, which ends it as it is dropped.
    let readers = readers(stdout, stderr, stdin, streams, shared).map_err(|error| GitFailure::Failed(format!("cannot read its output: {error}")))?;
    let watched = Watched {
        job: &job,
        running: &running,
        forwarders: readers.forwarders,
        watching: Watching::Git,
    };
    let mut bound = Bound::new(step);
    let end = waited(&watched, &mut bound, signal.as_ref(), shared, console);
    let Watched { forwarders, .. } = watched;
    let failure = match end {
        WaitEnd::Exited { code, stopped: false } => {
            let drained = forwarders.drain(DRAIN);
            // Whatever git left running ends with its job, so the readers of the other streams reach their ends.
            drop(job);
            let ran = GitOutput {
                code: code.cast_signed(),
                stdout: joined(readers.stdout)?,
                stderr: joined(readers.stderr)?,
            };
            fed_whole(&ran, readers.fed)?;
            return Ok((ran, drained.all_ended));
        }
        // Only a wait given the run's stop signal sees a stop, and only when it ended the job.
        WaitEnd::Exited { stopped: true, .. } => stop.map_or_else(|| GitFailure::Failed("its job was stopped".to_owned()), |stop| stop.stopped()),
        WaitEnd::Killed(GitEnd::Bound) => GitFailure::Failed(git::late_git_line(step.limit())),
        WaitEnd::Killed(GitEnd::Gone) | WaitEnd::ClientGone => GitFailure::ClientGone,
        WaitEnd::Failed { why, .. } => GitFailure::Failed(why),
    };
    // A bare write the bound ended after the client went ends the run as a gone client does.
    let failure = if shared.gone.load(Ordering::SeqCst) {
        GitFailure::ClientGone
    } else {
        failure
    };
    Err(ended(&job, forwarders, failure, step, shared.lock, console))
}

/// The bound of a git step's wait: counted from the step's start, or, for the step `--test-box-git-armed-by` arms, only
/// from the moment its file appears, so a step that has not reached that moment has no bound to beat.
struct Bound {
    /// Whether a gone client or a stop ends the step.
    class: GitClass,
    /// How long the step may take once the bound is set.
    limit: Duration,
    /// The file the seam arms the bound by; `None` for a bound set from the step's start, and set then.
    armed_by: Option<&'static Path>,
    /// When the bound fires, once it is set.
    deadline: Option<Instant>,
}

impl Bound {
    /// `step`'s bound: set now, unless the seam arms it by a file, which each look then waits for ([`Bound::fired`]).
    fn new(step: GitStep<'_>) -> Bound {
        let limit = step.limit();
        let armed_by = step.armed_by();
        Bound {
            class: step.class,
            limit,
            armed_by,
            deadline: armed_by.is_none().then(|| Instant::now() + limit),
        }
    }

    /// Whether the bound has fired, setting it first once the seam's file exists: before that, nothing can fire it.
    fn fired(&mut self) -> bool {
        if self.deadline.is_none() {
            match self.armed_by {
                Some(path) if path.exists() => self.deadline = Some(Instant::now() + self.limit),
                _ => return false,
            }
        }
        self.deadline.is_some_and(|deadline| Instant::now() >= deadline)
    }
}

/// The wait for a started git ([`Watched::wait`]), each look probing stderr and ending the wait at `bound`; for a step
/// a gone client or a stop ends, once `shared`'s flag is set, and on the run's stop `signal`, which ends the job, given
/// only to such a step. A write of the shared bare repository waits on past both, to its exit or its bound.
fn waited(watched: &Watched<'_>, bound: &mut Bound, signal: Option<&StopSignal<'_>>, shared: Shared<'_>, console: &mut Console) -> WaitEnd<GitEnd> {
    let class = bound.class;
    loop {
        let end = watched.wait(
            signal,
            |_| {
                heartbeat::probe_stderr(shared.lock, shared.gone);
                if bound.fired() {
                    return Some(GitEnd::Bound);
                }
                (class.ends_early() && shared.gone.load(Ordering::SeqCst)).then_some(GitEnd::Gone)
            },
            console,
        );
        match end {
            // Each wait looks for git's exit for a step before it answers a gone client, so this loop is not a spin.
            WaitEnd::ClientGone if !class.ends_early() => {
                if bound.fired() {
                    return WaitEnd::Killed(GitEnd::Bound);
                }
            }
            other => return other,
        }
    }
}

/// `failure`, the end of the git step `step` whose wait did not end in git's exit, once its whole job is ended and its
/// forwarders drained, so no line of git's comes after the box's next one. A job that still holds processes, or could
/// not be ended, is said under `lock` naming the step, as a lost connection's job is said
/// ([`crate::boxside::stop::Ending::say`]).
fn ended(job: &Job, forwarders: Forwarders, failure: GitFailure, step: GitStep<'_>, lock: &OutputLock, console: &mut Console) -> GitFailure {
    let ending = terminate_and_wait(job);
    forwarders.drain(DRAIN);
    let name = format!("git {}", step.name.strip_prefix("git ").unwrap_or(step.name));
    let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
    match ending {
        Ok(ending) => ending.say(&name, console),
        Err(error) => stamping::err(console, &format!("gate: remote could not stop job {name}: {error}")),
    }
    failure
}

/// The launch of the git `command` as a step of `class`, created suspended: its program, words, working folder and
/// environment, a batch file run through the system folder's cmd.exe ([`crate::run::written`]); else why it cannot be
/// launched.
fn git_launch(command: &Command, class: GitClass) -> Result<PipedLaunch, String> {
    // A write of the shared bare repository runs on a hidden console of its own, as the PC's git calls that hold a lock
    // do, so no console event of the box's ends it mid-write; every other step stays on the box's console, as a plain
    // start left it, which a gone client or a stop ends anyway, so no conhost is made per call.
    let console = if class.ends_early() {
        PipedConsole::Attached
    } else {
        PipedConsole::Hidden
    };
    let mut launch = launch::piped_launch(command, console).map_err(|error| format!("cannot build its launch: {error}"))?;
    let rest: Vec<OsString> = command.get_args().map(OsStr::to_owned).collect();
    match crate::run::written(Path::new(command.get_program()), &rest, &Variables::from_process()) {
        Written::Line(application, command_line) => {
            launch.application = application;
            launch.command_line = command_line;
            Ok(launch)
        }
        Written::Refused(why) | Written::NoShell(why) => Err(why),
    }
}

/// The readers of a started git's `stdout` and `stderr`, each taken as `streams` says, the forwarded ones under
/// `shared`'s lock, a failed write setting its flag; and a thread writing `streams`' feed to `stdin` and closing it.
fn readers(stdout: File, stderr: File, stdin: Option<File>, streams: &Streams<'_>, shared: Shared<'_>) -> io::Result<Readers> {
    let (forward_out, read_out) = taken(stdout, streams.stdout)?;
    let (forward_err, read_err) = taken(stderr, streams.stderr)?;
    let forwarders = Forwarders::start_git(
        forward_out,
        forward_err,
        stamping::forwarded(),
        crate::boxside::forward::Sink::Own {
            lock: shared.lock,
            gone: shared.gone,
        },
    )?;
    let fed = match stdin.zip(streams.feed) {
        Some((mut pipe, bytes)) => {
            let bytes = bytes.to_vec();
            Some(
                thread::Builder::new()
                    .name("git input".to_owned())
                    .spawn(move || pipe.write_all(&bytes))?,
            )
        }
        None => None,
    };
    Ok(Readers {
        forwarders,
        stdout: read_out,
        stderr: read_err,
        fed,
    })
}

/// `pipe` taken as `how`: given back to be forwarded, or read to its end on a thread of its own, kept or dropped.
fn taken(mut pipe: File, how: Pipe) -> io::Result<(Option<File>, Option<Reading>)> {
    let keep = match how {
        Pipe::Forward => return Ok((Some(pipe), None)),
        Pipe::Read => true,
        Pipe::Discard => false,
    };
    let reading = thread::Builder::new().name("git output".to_owned()).spawn(move || {
        if keep {
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).map(|_| bytes)
        } else {
            io::copy(&mut pipe, &mut io::sink()).map(|_| Vec::new())
        }
    })?;
    Ok((None, Some(reading)))
}

/// What the reader `reading` read, empty for a stream it did not read; else why it could not.
fn joined(reading: Option<Reading>) -> Result<Vec<u8>, GitFailure> {
    match reading.map(JoinHandle::join) {
        None => Ok(Vec::new()),
        Some(Ok(Ok(bytes))) => Ok(bytes),
        Some(Ok(Err(error))) => Err(GitFailure::Failed(format!("cannot read its output: {error}"))),
        Some(Err(_)) => Err(GitFailure::Failed("the reader of its output panicked".to_owned())),
    }
}

/// Whether git, which ran to its end as `ran`, was fed its whole input by `fed`: a git that failed says why in its own
/// lines, so a feed it cut short by exiting is no failure of its own, and a step with no input has nothing to check.
fn fed_whole(ran: &GitOutput, fed: Option<JoinHandle<io::Result<()>>>) -> Result<(), GitFailure> {
    match fed.filter(|_| ran.code == 0).map(JoinHandle::join) {
        None | Some(Ok(Ok(()))) => Ok(()),
        Some(Ok(Err(error))) => Err(GitFailure::Failed(format!("its input could not be written: {error}"))),
        Some(Err(_)) => Err(GitFailure::Failed("the writer of its input panicked".to_owned())),
    }
}

/// The file `word` names, found as `gate run` finds a command's program: along the gate's own `PATH` and `PATHEXT`,
/// never in the working folder unless `PATH` names it. `None` when it is not found; else why it could not be looked
/// for.
pub(crate) fn locate(word: &str) -> Result<Option<PathBuf>, String> {
    let folder = std::env::current_dir().map_err(|error| format!("cannot read the working directory: {error}"))?;
    match find_program(word, &Variables::from_process(), &folder) {
        Ok(program) => Ok(Some(program)),
        // `find_program` says a word it does not find with this message, and every other failure with another.
        Err(message) if message == gate_win32::win32_message(ERROR_FILE_NOT_FOUND) => Ok(None),
        Err(message) => Err(message),
    }
}

/// Says the SDK line on stdout, the list read within `limit`, dotnet's readers given `drain` once it has exited; a list
/// that cannot be read is said on stderr instead, and fails nothing.
fn say_sdks(limit: Duration, drain: Duration, console: &mut Console) {
    match sdks(limit, drain) {
        Ok(list) => stamping::out(console, &format!("gate: remote sdks {list}")),
        Err(message) => stamping::err(console, &format!("gate: remote could not read the SDK list: {message}")),
    }
}

/// The SDK list of `dotnet --list-sdks`, `none` when no dotnet is found along the gate's own `PATH`; else why it could
/// not be read. dotnet is run as [`git::sdk_listing`] runs it, with no console interrupt and no worktree to keep it out
/// of, ended with its job past `limit`, its readers given `drain` once it has exited.
fn sdks(limit: Duration, drain: Duration) -> Result<String, String> {
    let listing = git::sdk_listing(Variables::from_process(), limit, drain, None, &[])?;
    Ok(gate_wire::sdk_list(&listing))
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::{self, Write as _};
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Stdio};
    use std::rc::Rc;
    use std::sync::PoisonError;
    use std::time::{Duration, Instant};

    use base64::Engine as _;
    use gate_win32::NamedEvent;
    use serde_json::json;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

    use super::{
        Bound, Git, GitClass, GitFailure, GitStep, Shared, Streams, TreeAt, fresh_config, fresh_config_written, git_failed, git_launch, git_ran,
        lock_waited, readers, ready_tree, refused_folders, replace_config, waited,
    };
    use crate::boxside::forward::{ClientGone, OutputLock};
    use crate::boxside::helper_trusted;
    use crate::boxside::run::{StopSignal, WaitEnd, Watched, Watching};
    use crate::console::Console;
    use crate::remote::launch;
    use gate_wire::Payload;

    #[test]
    #[expect(clippy::expect_used, reason = "a git that cannot be named must fail the case loudly")]
    fn a_box_git_call_reads_no_system_wide_attributes_file() {
        let git = Git {
            program: Ok(PathBuf::from("git.exe")),
        };

        let command = git.command(&["status"]).expect("the command is built");

        assert!(
            command
                .get_envs()
                .any(|pair| pair == (std::ffi::OsStr::new("GIT_ATTR_NOSYSTEM"), Some(std::ffi::OsStr::new("1")))),
            "{:?}",
            command.get_envs().collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_bare_lock_wait_takes_the_lock_once_it_is_free() {
        let mut calls = 0;

        let taken = lock_waited(
            || {
                calls += 1;
                Ok((calls == 3).then_some(calls))
            },
            Duration::from_hours(1),
            Duration::ZERO,
        );

        assert_eq!(taken, Ok(Some(3)));
        assert_eq!(calls, 3, "the lock is opened until it is taken, and no more");
    }

    #[test]
    fn the_bare_lock_wait_gives_up_after_its_bound() {
        let taken = lock_waited(|| Ok(None::<()>), Duration::ZERO, Duration::ZERO);

        assert_eq!(taken, Ok(None));
    }

    #[test]
    fn the_bare_lock_wait_passes_an_open_error_on() {
        let taken = lock_waited(|| Err::<Option<()>, _>("cannot open".to_owned()), Duration::from_hours(1), Duration::ZERO);

        assert_eq!(taken, Err("cannot open".to_owned()));
    }

    #[test]
    fn no_desktop_helper_is_no_refusal() {
        let missing = crate::test_support::temp_dir().join(format!("gate-preflight-no-helper-{}.exe", std::process::id()));
        assert_eq!(helper_trusted(&missing), Ok(()));
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a test that cannot make or remove its file must fail loudly")]
    fn a_desktop_helper_this_user_made_is_refused_naming_it() {
        let folder = fs::canonicalize(crate::test_support::temp_dir()).expect("the scratch folder's final path can be read");
        let text = folder.to_string_lossy().into_owned();
        let helper = PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text)).join(format!("gate-preflight-helper-{}.exe", std::process::id()));
        fs::write(&helper, "made").expect("the file can be written");

        let why = helper_trusted(&helper);

        fs::remove_file(&helper).expect("the file can be removed");
        let named = format!("{} ", helper.display());
        assert!(why.as_ref().is_err_and(|why| why.starts_with(&named)), "{why:?}");
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a test whose git cannot start must fail loudly")]
    fn git_writes_its_stderr_through_the_output_lock() {
        let output = OutputLock::default();
        let missing = crate::test_support::temp_dir().join(format!("gate-preflight-no-such-repository-{}", std::process::id()));
        let git_dir = format!("--git-dir={}", missing.display());
        let command = Git::find()
            .command(&[&git_dir, "rev-parse", "HEAD"])
            .expect("git is on PATH; the unit tests need it");
        let held = output.lock().unwrap_or_else(PoisonError::into_inner);
        let gone = ClientGone::default();
        let step = GitStep {
            name: "rev-parse",
            class: GitClass::Short,
        };

        // git says on stderr that the repository does not exist, then exits.
        let ran = git_ran(&command, step, &Streams::FORWARDED, Shared::alone(&output, &gone), &mut Console::new());
        drop(held);

        let (ran, all_ended) = ran.expect("git starts and runs to its end");
        assert_eq!(ran.code, 128, "git's own status");
        assert!(
            !all_ended,
            "the reader of git's stderr waits for the held output lock, so it cannot end within the drain"
        );
    }

    /// A git that exited by itself while the run's stop signal is set was not stopped: `gate box stop` cannot reach
    /// git's own job, so a stop counts for a git wait only when that wait ended the job.
    #[test]
    #[expect(clippy::expect_used, reason = "a test whose git or event cannot be made must fail loudly")]
    fn a_git_that_exited_while_a_stop_is_set_is_not_reported_stopped() {
        let command = Git::find().command(&["--version"]).expect("git is on PATH; the unit tests need it");
        let launch = git_launch(&command, GitClass::Short).expect("git's launch is built");
        let job = launch::call_job(false).expect("git's job is made");
        let (suspended, stdout, stderr, _) = launch::launched(&launch, &job, false).expect("git is created");
        let running = suspended.assign_and_resume(&job).expect("git runs");
        let (lock, gone) = (OutputLock::default(), ClientGone::default());
        let shared = Shared::alone(&lock, &gone);
        let readers = readers(stdout, stderr, None, &Streams::QUIET, shared).expect("git's output is read");
        assert!(running.wait(Duration::from_secs(600)).expect("git is waited for").is_some(), "git exits");
        let event = NamedEvent::create_new_manual(&format!(r"Local\gate-test-git-exited-stop-{}", std::process::id())).expect("the event is made");
        event.set().expect("the stop is set");
        let signal = StopSignal::sharing(&event, "k", &Rc::default());
        let watched = Watched {
            job: &job,
            running: &running,
            forwarders: readers.forwarders,
            watching: Watching::Git,
        };

        let mut bound = Bound {
            class: GitClass::Short,
            limit: Duration::from_secs(600),
            armed_by: None,
            deadline: Some(Instant::now() + Duration::from_secs(600)),
        };
        let end = waited(&watched, &mut bound, Some(&signal), shared, &mut Console::new());

        assert!(
            matches!(end, WaitEnd::Exited { code: 0, stopped: false }),
            "git ran to its end, unstopped"
        );
    }

    /// git's exit code for `args` as a short step with both of its streams dropped; else why it did not run to its end.
    fn quiet_code(git: &Git, args: &[&str]) -> Result<i32, String> {
        let step = GitStep {
            name: "git",
            class: GitClass::Short,
        };
        git.alone(args, step, &Streams::QUIET, &mut Console::new())
            .map(|ran| ran.code)
            .map_err(|failure| match failure {
                GitFailure::NotStarted(why) | GitFailure::Failed(why) => why,
                GitFailure::ClientGone => "the client is gone".to_owned(),
            })
    }

    /// The payload `json` spells, as the box reads it off its own standard input.
    #[expect(clippy::expect_used, reason = "a test payload that does not decode must fail loudly")]
    fn decoded(json: &str) -> Payload {
        Payload::decode(&base64::engine::general_purpose::STANDARD.encode(json), gate_wire::PathForm::Windows).expect("the test payload decodes")
    }

    /// All four folders of a run's own layout are refused, the folder above `Work` among them: dropping any one of
    /// them fails this.
    #[test]
    #[expect(clippy::expect_used, reason = "a test payload with no work folder must fail loudly")]
    fn the_refused_folders_are_the_run_s_own() {
        let payload = decoded(
            &json!({
                "Key": "k",
                "Work": r"D:\gate-remote\work\k",
                "Bare": r"D:\gate-remote\app.git",
                "OutputFolder": r"D:\gate-remote\outputs",
            })
            .to_string(),
        );
        let (_, work) = payload.work_folder().expect("the test payload has a work folder");

        assert_eq!(
            refused_folders(&payload, work),
            [
                PathBuf::from(r"D:\gate-remote\work"),
                PathBuf::from(r"D:\gate-remote\work\k"),
                PathBuf::from(r"D:\gate-remote\app.git"),
                PathBuf::from(r"D:\gate-remote\outputs"),
            ]
        );
    }

    #[test]
    fn a_git_status_is_said_signed_and_exited_with_whole() {
        let failed = git_failed("git init", -1_073_741_819);
        assert_eq!(failed.line, "gate: remote git init failed (status -1073741819)");
        assert_eq!(failed.status, 3_221_225_477);
        let failed = git_failed("git config core.eol", 128);
        assert_eq!(failed.line, "gate: remote git config core.eol failed (status 128)");
        assert_eq!(failed.status, 128);
    }

    /// The `git config --local --list` of the tree `tree`, and its config file's bytes.
    #[expect(clippy::expect_used, reason = "a test whose git cannot start must fail loudly")]
    fn config_of(git: &Git, tree: &Path) -> (String, Vec<u8>) {
        let git_dir = format!(r"--git-dir={}\.git", tree.display());
        let mut command = git.command(&[&git_dir, "config", "--local", "--list"]).expect("git is on PATH");
        let output = gate_win32::spawn(command.stdout(Stdio::piped()).stderr(Stdio::null()))
            .and_then(Child::wait_with_output)
            .expect("git config runs");
        assert_eq!(output.status.code(), Some(0), "git config --list in {}", tree.display());
        let list = String::from_utf8(output.stdout).expect("the config list is UTF-8");
        (list, fs::read(tree.join(".git").join("config")).expect("the config file reads"))
    }

    /// A tree the preflight makes has the config `git remote add` and `git config` would have given it, written once,
    /// for a bare repository whose path holds a backslash and a space, and characters git escapes or quotes.
    #[test]
    #[expect(clippy::expect_used, reason = "a test whose git cannot start must fail loudly")]
    fn a_fresh_tree_s_config_is_what_the_git_steps_write() {
        let git = Git::find();
        for (at, bare) in [r"C:\Box Work\x.git", r#"C:\Box Work\a"b;c #.git"#, r" C:\Box Work\x.git "]
            .into_iter()
            .enumerate()
        {
            let folder = crate::test_support::temp_dir().join(format!("gate-preflight-config-{at}-{}", std::process::id()));
            if folder.exists() {
                fs::remove_dir_all(&folder).expect("an earlier run's folder can be removed");
            }
            let (written, called) = (folder.join("written"), folder.join("called"));
            fs::create_dir_all(&folder).expect("the test's folder can be made");

            let (tree, warm) = (written.to_string_lossy(), folder.join("written.warm").to_string_lossy().into_owned());
            let ran = folder.join("written.ran").to_string_lossy().into_owned();
            let at = TreeAt {
                tree: &tree,
                bare,
                warm: &warm,
                ran: &ran,
            };
            let readied = ready_tree(&git, &at, None, &mut Console::new());

            assert!(readied.is_ok(), "the tree is readied for {bare}");
            let called_dir = format!(r"--git-dir={}\.git", called.display());
            let called_tree = called.to_string_lossy();
            assert_eq!(quiet_code(&git, &["init", "-q", "--", &called_tree]), Ok(0), "git init");
            let initialised = fs::read_to_string(called.join(".git").join("config")).expect("git init's config reads");
            assert!(
                fresh_config(&initialised, bare).is_some(),
                "the readied tree took the one write for {bare}: {initialised:?}"
            );
            for args in [
                &[&called_dir, "remote", "add", "--", "origin", bare][..],
                &[&called_dir, "config", "core.autocrlf", "false"],
                &[&called_dir, "config", "core.eol", "lf"],
            ] {
                assert_eq!(quiet_code(&git, args), Ok(0), "git {args:?}");
            }
            assert_eq!(config_of(&git, &written), config_of(&git, &called), "{bare}");
            fs::remove_dir_all(&folder).expect("the test's folder can be removed");
        }
    }

    /// The config `git init` writes on this machine's git, for the tests that hand it to the one write.
    const INIT_CONFIG: &str = "[core]\n\trepositoryformatversion = 0\n\tfilemode = false\n\tbare = false\n";

    /// A tree in the unit tests' temp folder for the test `name`, its `.git` folder holding `config` as `config`'s
    /// bytes when given, and `config.lock` when `locked`; the tree's path.
    #[expect(clippy::expect_used, reason = "a test that cannot make its tree must fail loudly")]
    fn planted_tree(name: &str, config: Option<&[u8]>, locked: bool) -> std::path::PathBuf {
        let tree = crate::test_support::temp_dir().join(format!("gate-preflight-{name}-{}", std::process::id()));
        if tree.exists() {
            fs::remove_dir_all(&tree).expect("an earlier run's tree can be removed");
        }
        fs::create_dir_all(tree.join(".git")).expect("the tree's .git can be made");
        if let Some(config) = config {
            fs::write(tree.join(".git").join("config"), config).expect("the config can be planted");
        }
        if locked {
            fs::write(tree.join(".git").join("config.lock"), "held").expect("the lock can be planted");
        }
        tree
    }

    /// The one write leaves to the git steps, writing nothing, a config it cannot take whole: one that is missing, one
    /// that is not UTF-8, one that holds another section, and one whose `config.lock` a git or another preflight holds;
    /// a lock the write took itself is gone.
    #[test]
    #[expect(clippy::expect_used, reason = "a test whose tree cannot be read must fail loudly")]
    fn a_config_the_one_write_cannot_take_is_left_to_the_git_steps() {
        let not_utf8: &[u8] = b"[core]\n\tbare = \xff\n";
        let other = format!("{INIT_CONFIG}[user]\n\tname = x\n");
        for (name, config, locked) in [
            ("config-missing", None, false),
            ("config-not-utf8", Some(not_utf8), false),
            ("config-other-section", Some(other.as_bytes()), false),
            ("config-locked", Some(INIT_CONFIG.as_bytes()), true),
        ] {
            let tree = planted_tree(name, config, locked);

            let written = fresh_config_written(r"C:\Box\x.git", &tree.to_string_lossy(), None);

            assert!(matches!(written, Ok(false)), "{name}: left to the git steps");
            let left = fs::read(tree.join(".git").join("config")).ok();
            assert_eq!(left.as_deref(), config, "{name}: the config is as it was");
            assert_eq!(
                tree.join(".git").join("config.lock").exists(),
                locked,
                "{name}: no lock of the write's is left"
            );
            fs::remove_dir_all(&tree).expect("the tree can be removed");
        }
    }

    /// A folder at the config's name is no plain config: the one write leaves it to the git steps and no lock behind.
    #[test]
    #[expect(clippy::expect_used, reason = "a test whose tree cannot be made must fail loudly")]
    fn a_folder_at_the_config_s_name_is_left_to_the_git_steps() {
        let tree = planted_tree("config-a-folder", None, false);
        let git_folder = tree.join(".git");
        fs::create_dir(git_folder.join("config")).expect("a folder can be planted at the config's name");

        let written = fresh_config_written(r"C:\Box\x.git", &tree.to_string_lossy(), None);

        assert!(matches!(written, Ok(false)), "left to the git steps");
        assert!(git_folder.join("config").is_dir(), "the folder is as it was");
        assert!(!git_folder.join("config.lock").exists(), "the write's lock is gone");
        fs::remove_dir_all(&tree).expect("the tree can be removed");
    }

    /// A write into `config.lock` that fails before the rename leaves the config byte-identical and no lock behind.
    #[test]
    #[expect(clippy::expect_used, reason = "a test whose tree cannot be read must fail loudly")]
    fn a_config_write_that_fails_before_the_rename_leaves_the_config_as_it_was() {
        let tree = planted_tree("config-write-fails", Some(INIT_CONFIG.as_bytes()), false);
        let git_folder = tree.join(".git");
        let folder = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(&git_folder)
            .expect("the .git folder opens");
        let replaced = replace_config(&folder, r"C:\Box\x.git", |lock, text| {
            lock.write_all(&text.as_bytes()[..10])?;
            Err(io::Error::other("the disk is full"))
        });

        drop(folder);
        assert_eq!(replaced.map_err(|error| error.to_string()), Err("the disk is full".to_owned()));
        assert_eq!(fs::read(git_folder.join("config")).expect("the config reads"), INIT_CONFIG.as_bytes());
        assert!(!git_folder.join("config.lock").exists(), "the lock is removed");
        fs::remove_dir_all(&tree).expect("the tree can be removed");
    }

    /// A config holding U+FEFF anywhere, which can hide a section header, or whose last line ends with `\`, which would
    /// carry the next line on, is left to the git steps.
    #[test]
    fn a_config_with_a_byte_order_mark_or_a_continued_last_line_is_not_written_once() {
        let bare = r"C:\Box\x.git";
        assert!(fresh_config(INIT_CONFIG, bare).is_some(), "git init's own config is written once");
        assert_eq!(
            fresh_config(&format!("\u{feff}[remote \"x\"]\n{INIT_CONFIG}"), bare),
            None,
            "a hidden header"
        );
        assert_eq!(fresh_config(&format!("{INIT_CONFIG}\u{feff}"), bare), None, "U+FEFF at the end");
        assert_eq!(fresh_config(&format!("{INIT_CONFIG}\tx = y \\\n"), bare), None, "a continued last line");
    }
}
