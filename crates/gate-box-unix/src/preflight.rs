//! `gate-box box preflight`, the first call of every routed run: judges the builder's work root, readies the bare
//! repository and the work folder's tree, then says the version, warm, new work tree, boot, SDK and tool lines in the
//! order the PC's client reads them.

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::thread;

use gate_args::BoxSeams;
use gate_wire::{
    NEW_WORK_TREE, Pair, PairSibling, PathForm, Payload, ROOT_STATUS, WARM_READ_LIMIT, bare_stem, created_line, escaped, layout_text_for,
    names_another_layout, readies_single, root_unsafe_line, sdks_failure, sdks_line, tool_line, trees, version_line, warm_line, warm_record_hash,
};

use crate::console::Console;
use crate::git::{self, AlternatesFailure, Failed, Git, Steps, Tree, failed_for, for_repo, line_ends_set, origin_set};
use crate::lines::{
    FAILED_STATUS, FolderFlaw, NEEDS_REFUSED, USAGE_STATUS, WORK_ROOT_STEP, folder_failure, login_path_line, root_flaw, unsynced_failure,
    work_layout, write_failure,
};
use crate::stop::open_folder_line;
use crate::{boot, drain, files, probe, process, run, slots};

/// Why the work root is not used.
pub(crate) enum Refusal {
    /// A folder could not be made or is not the box user's own: this line, [`FAILED_STATUS`].
    Folder(String),
    /// The root, or a folder of it another account may reach into, is not trusted: the version line, then this
    /// work-root line, [`ROOT_STATUS`].
    Untrusted(String),
}

/// The refusal of a folder of the work root `flaw` says the box does not use, `private` when it is `<root>/work`: the
/// folder's line, or for a folder another account may reach into the work-root line naming its remedy
/// ([`open_folder_line`]).
fn folder_refusal(flaw: FolderFlaw, private: bool) -> Refusal {
    match flaw {
        FolderFlaw::Failed(line) => Refusal::Folder(line),
        FolderFlaw::Open { path, why } => Refusal::Untrusted(open_folder_line(&path, &why, private)),
    }
}

/// Answers the preflight of `payload` through `console`, its drain clock as `seams` set it; the status. Usage errors (a
/// key the payload lacks or that does not fit the work folder, a drain session named wrong) exit [`USAGE_STATUS`]
/// before anything is made; a work root another account may write in is refused after the version line with the
/// work-root line, as the Windows box refuses one. Once the root is trusted and before any record of the work folder is
/// read, the call is admitted by the drain ([`drain::admit`]) or refused after the version line, as the Windows box
/// refuses it, so a refused preflight writes nothing under the work folder. It says [`NEW_WORK_TREE`] after the warm
/// line when a run of one repo's tree was made now ([`ready`]), when anything is at the interrupted-command record
/// `<work>.command`, when the layout record names another layout than the run's, a pair's first run having none, and
/// when no plain file is at the ran record ([`crate::command::has_run`]), as no command has started in the folder since
/// it was laid out. Its last line is the builder's own slots as a look without the drain lock finds them
/// ([`slots::looked`]), for a root with a count file or a `slots/` folder.
pub fn preflight(payload: &Payload, seams: &BoxSeams, console: &mut Console) -> u8 {
    let checked = folders(payload).and_then(|(bare, work, pair)| Ok((bare, work, checked_root(work, bare, pair.is_some())?, pair)));
    let drain_read = drain::session_of(payload, "", "").and_then(|session| Ok((session, payload.no_ticket()?)));
    let ((bare, work, (root, parent), pair), (session, no_ticket)) = match checked.and_then(|checked| Ok((checked, drain_read?))) {
        Ok(checked) => checked,
        Err(line) => {
            console.err(&line);
            return USAGE_STATUS;
        }
    };
    // Every repo's bare repository: `Bare`, the main repo's, then on a pair each sibling's beside it.
    let bares = bares(bare, pair.as_ref());
    let trusted = trusted_root(&root, parent).and_then(|()| {
        std::iter::once(work)
            .chain(bares.iter().map(|(bare, _)| *bare))
            .find_map(|folder| files::unusable_folder(Path::new(folder), false))
            .map_or(Ok(()), |flaw| Err(folder_refusal(flaw, false)))
    });
    match trusted {
        Ok(()) => {}
        Err(Refusal::Folder(line)) => {
            console.err(&line);
            return FAILED_STATUS;
        }
        Err(Refusal::Untrusted(line)) => {
            console.out(&version_line());
            console.err(&line);
            return ROOT_STATUS;
        }
    }
    // Held until the preflight returns, so a drain counts it as going.
    let _admitted = match drain::admission_of(drain::admit(Path::new(&root), session.as_ref(), no_ticket, seams, console)) {
        Ok(admitted) => admitted,
        Err(unpassed) => {
            console.out(&version_line());
            return unpassed.said(payload, console);
        }
    };
    // Before anything else is done for the run, so a builder whose own slots are all held refuses it with nothing made.
    let claimed = match slots::taken(Path::new(&root), session.as_ref(), payload, seams, console) {
        Ok(claimed) => claimed,
        Err(status) => return status,
    };
    // Made only when the session holds a claim, and dropped at every failing end of the preflight, before its
    // admission and so while its session is held: the slot it took is let go as a run's end lets it go. Kept once the
    // preflight passes.
    let slot = claimed.then(|| slots::Release::at_end(Path::new(&root), payload));
    // Read before the tree is readied, which would make a missing `.git`, and so a stale record, look current. A
    // record naming another layout than the run's, a pair's first run having none, has the run clear the whole folder
    // and lay its trees out again.
    let layout = layout_record(work);
    let relaid = names_another_layout(&layout, &layout_text_for(pair.as_ref()));
    let warm = warm_state(work, pair.as_ref(), relaid);
    let interrupted = files::exists(Path::new(&format!("{work}.command")));
    // Only a command started in the folder makes it warm: a tree laid out by a preflight whose run never came is new.
    let ran = crate::command::has_run(work);
    let git = Git::of(&root, parent);
    // A pair's trees are laid out by its run, under the work lock.
    let readied = match &pair {
        None => ready(&git, bare, work, &layout, console),
        Some(_) => bares
            .iter()
            .try_for_each(|(bare, name)| bare_made(&git, bare, *name, console))
            .map(|()| false),
    };
    let made = match readied {
        Ok(made) => made,
        Err(failed) => {
            failed.lines.iter().for_each(|line| console.err(line));
            return failed.status;
        }
    };
    console.out(&version_line());
    console.out(&warm_line(warm.as_deref()));
    if made || interrupted || relaid || !ran {
        console.out(NEW_WORK_TREE);
    }
    boot::say(console);
    say_sdks_and_tools(payload, console);
    // Said last, so every line before it keeps its place.
    slots::look_said(slots::looked(Path::new(&root), seams), console);
    if let Some(slot) = slot {
        slot.kept();
    }
    0
}

/// The payload's `Bare`, `Work` and `Pair`, once `Key`, `Lock`, `Index`, `OutputFolder` and `Job` fit the work folder;
/// else the usage line.
fn folders(payload: &Payload) -> Result<(&str, &str, Option<Pair>), String> {
    let bare = payload.path("Bare")?;
    let (key, work) = payload.work_folder()?;
    payload.beside_work("Lock", work, "lock")?;
    payload.beside_work("Index", work, "index")?;
    payload.output_folder(work)?;
    payload.job(key)?;
    Ok((bare, work, payload.pair()?))
}

/// The work root of `work`, `<root>/work/<key>`, and its `<root>/work`, with `bare` at `<root>/<name>.git` unless
/// `paired` (a pair's bare repositories were checked as its layout was read); else the usage line.
pub(crate) fn checked_root<'w>(work: &'w str, bare: &str, paired: bool) -> Result<(String, &'w Path), String> {
    let (root, parent) = work_layout(work)?;
    if !paired && bare_stem(bare, &root, PathForm::Unix).is_none() {
        return Err(format!(
            "gate: box payload Bare must be <root>/<name>.git to lock its root down: {}",
            escaped(bare)
        ));
    }
    Ok((root, parent))
}

/// Makes the work root `root` with mode 0700 when it is missing, judges it ([`trusted_work_root`]), then makes
/// `parent`, its `<root>/work`, with mode 0700 when it is missing, and refuses it unless it is the box user's own plain
/// folder open to no group or other.
fn trusted_root(root: &str, parent: &Path) -> Result<(), Refusal> {
    trusted_work_root(root)?;
    if !files::exists(parent) {
        made_folder(parent, false)?;
    }
    files::unusable_folder(parent, true).map_or(Ok(()), |flaw| Err(folder_refusal(flaw, true)))
}

/// Makes the work root `root`, and the folders above it, with mode 0700 when it is missing, then refuses it unless it
/// is trusted ([`root_flaw`]): the line saying why not.
pub(crate) fn trusted_work_root(root: &str) -> Result<(), Refusal> {
    let path = Path::new(root);
    if !files::exists(path) {
        made_folder(path, true)?;
    }
    let untrusted = |why: &str| Refusal::Untrusted(root_unsafe_line(why, WORK_ROOT_STEP));
    let facts = files::facts(path).map_err(|error| untrusted(&format!("cannot read {root}: {error}")))?;
    match root_flaw(facts, root, &files::user_name(facts.euid)) {
        Some(why) => Err(untrusted(&why)),
        None => Ok(()),
    }
}

/// Makes the folder `path` with mode 0700, and the folders above it when `recursive`.
fn made_folder(path: &Path, recursive: bool) -> Result<(), Refusal> {
    DirBuilder::new()
        .recursive(recursive)
        .mode(0o700)
        .create(path)
        .map_err(|error| Refusal::Folder(folder_failure(&path.display().to_string(), &error.to_string())))
}

/// The work folder's layout record, `<work>.layout`, read as a plain file; `None` when it is missing.
pub(crate) fn layout_record(work: &str) -> io::Result<Option<String>> {
    match files::read_plain(Path::new(&format!("{work}.layout")), files::RECORD_LIMIT) {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Each bare repository of the run, with the repo's name on a pair run: `bare`, the main repo's, then each sibling's of
/// `pair`.
fn bares<'a>(bare: &'a str, pair: Option<&'a Pair>) -> Vec<(&'a str, Option<&'a str>)> {
    let Some(pair) = pair else {
        return vec![(bare, None)];
    };
    let siblings = pair.siblings().iter().map(|sibling| (sibling.bare(), Some(sibling.name())));
    std::iter::once((bare, Some(pair.leaf()))).chain(siblings).collect()
}

/// The hash the work folder's warm record, `<work>.warm`, names ([`warm_record_hash`]); `None` when the layout record names
/// another layout than the run's (`relaid`), a tree the run's layout holds ([`trees`]) has no plain folder at its
/// `.git`, or the record is missing or not a plain file.
fn warm_state(work: &str, pair: Option<&Pair>, relaid: bool) -> Option<String> {
    let names: Vec<&str> = pair
        .map(|pair| pair.siblings().iter().map(PairSibling::name).collect())
        .unwrap_or_default();
    let (main, siblings) = trees(work, pair.map(|pair| (pair.leaf(), names.as_slice())), PathForm::Unix);
    let plain = std::iter::once(&main)
        .chain(&siblings)
        .all(|tree| files::plain_folder(&Path::new(tree).join(".git")));
    if relaid || !plain {
        return None;
    }
    let bytes = files::read_plain(Path::new(&format!("{work}.warm")), WARM_READ_LIMIT).ok()?;
    warm_record_hash(&bytes)
}

/// Readies the bare repository `bare`, made when missing, and, when the layout record lets it ([`readies_single`]),
/// the tree at `work`: made, its stale warm record removed first, when nothing is at its `.git`; then, a folder at
/// `.git`, its `origin` set to `bare`, its line-ending config written and its alternates file naming `bare`'s objects.
/// A link or a file at `.git`, `.git/objects` or `.git/objects/info` is never followed, and a tree the run's layout
/// check refuses ([`run::tree_checked`]: another account may write in it, or its `.git/commondir` names another
/// repository's folders) is left as it is, no git step run on it. Whether the tree was made now.
fn ready(git: &Git<'_>, bare: &str, work: &str, layout: &io::Result<Option<String>>, console: &mut Console) -> Result<bool, Failed> {
    bare_made(git, bare, None, console)?;
    if !readies_single(layout) {
        return Ok(false);
    }
    // A tree another account may write in, or whose `.git` names another repository's folders: the run refuses it
    // under its lock.
    if run::tree_checked(Path::new(work), false).is_err() {
        return Ok(false);
    }
    let made = match fs::symlink_metadata(Path::new(work).join(".git")) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            warm_removed(work)?;
            git.step("git init", &["init", "-q", work])?;
            true
        }
        Ok(metadata) if metadata.is_dir() => false,
        // A link, a file or anything that cannot be looked at: the run repairs it under its lock.
        _ => return Ok(false),
    };
    let tree = Tree::at(work);
    origin_set(git, &tree, bare)?;
    line_ends_set(git, &tree)?;
    alternates_written(work, bare)?;
    Ok(made)
}

/// Makes the bare repository `bare` of the repo `name` (`None` on a run of one repo) when nothing is at its name, said
/// as created; its step and its failure name the repo on a pair run ([`for_repo`], [`failed_for`]).
fn bare_made(git: &Git<'_>, bare: &str, name: Option<&str>, console: &mut Console) -> Result<(), Failed> {
    if files::exists(Path::new(bare)) {
        return Ok(());
    }
    console.out(&created_line(bare));
    let step = "git init --bare";
    git.step(&for_repo(step, name), &["init", "--bare", "-q", bare])
        .map(drop)
        .map_err(|failed| failed_for(failed, step, name))
}

/// Removes the warm record `<work>.warm` of a tree that is gone, so the tree made in its place is not taken as warm,
/// and its ran record ([`crate::command::ran_removed`]), so it is new until a command starts in it; a record already
/// missing is fine.
fn warm_removed(work: &str) -> Result<(), Failed> {
    let warm = format!("{work}.warm");
    let unremoved = |record: &str, error: &io::Error| {
        Failed::line(format!(
            "gate: remote could not remove {}: {}",
            escaped(record),
            escaped(&error.to_string())
        ))
    };
    if let Err(error) = fs::remove_file(&warm)
        && error.kind() != io::ErrorKind::NotFound
    {
        return Err(unremoved(&warm, &error));
    }
    crate::command::ran_removed(work).map_err(|error| unremoved(&crate::command::ran_record(work), &error))
}

/// Writes `<work>/.git/objects/info/alternates` as `<bare>/objects` and a line feed unless it already holds exactly
/// that, whole, so a git reading it meanwhile never finds it empty ([`files::replace_plain`]); `info` is made when
/// missing. A link or anything but a folder at `objects` or `info` leaves it unwritten. A folder that cannot be synced
/// after the rename fails the preflight with its own line, the new text in place.
fn alternates_written(work: &str, bare: &str) -> Result<(), Failed> {
    let objects = Path::new(work).join(".git").join("objects");
    if !files::plain_folder(&objects) {
        return Ok(());
    }
    let info = objects.join("info");
    if files::exists(&info) && !files::plain_folder(&info) {
        return Ok(());
    }
    git::alternates_written(&info, bare).map_err(|failure| {
        Failed::line(match failure {
            AlternatesFailure::Folder { path, error } => folder_failure(&path.display().to_string(), &error.to_string()),
            AlternatesFailure::Write { path, failure } => {
                let shown = path.display().to_string();
                match failure {
                    files::ReplaceFailure::NotWritten(error) => write_failure(&shown, &error.to_string()),
                    files::ReplaceFailure::Unsynced(error) => unsynced_failure(&shown, &error.to_string()),
                }
            }
        })
    })
}

/// Says the SDK line and one line for each tool the payload's `Needs` names, in its order, the SDK list and the probes
/// read at the same time along the login shell's `PATH` ([`process::login_shell`]), or the box's own `PATH`, said so,
/// when that cannot be read; Godot is looked for under the login shell's `GATE_GODOT_ROOT`, and is missing when the
/// login shell cannot be read.
fn say_sdks_and_tools(payload: &Payload, console: &mut Console) {
    let (path, godot_root) = match process::login_shell() {
        Ok(login) => (Some(login.path), login.godot_root),
        Err(why) => {
            console.err(&login_path_line(&why));
            (None, None)
        }
    };
    let names: Vec<String> = if let Ok(needs) = payload.pairs("Needs") {
        needs.into_iter().map(|(name, _)| name).collect()
    } else {
        console.err(NEEDS_REFUSED);
        Vec::new()
    };
    let (sdks, tools) = thread::scope(|scope| {
        let sdks = scope.spawn(|| probe::sdks(path.as_deref()));
        let tools = probe::probe_all(&names, path.as_deref(), godot_root.as_deref());
        (sdks.join().unwrap_or_else(|_| Err("the SDK read failed".to_owned())), tools)
    });
    match sdks {
        Ok(list) => console.out(&sdks_line(&list)),
        Err(message) => console.err(&sdks_failure(&message)),
    }
    for (name, probed) in names.iter().zip(&tools) {
        console.out(&tool_line(name, probed));
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose temp folder or payload cannot be made must fail loudly")]
mod tests {
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use gate_wire::slots::{BOX_SLOTS_LINE, BoxSlots, Claim, HoldClaim};
    use gate_wire::{PathForm, Payload, root_unsafe_line};
    use serde_json::{Value, json};

    use super::preflight;
    use crate::console::Console;
    use crate::files::{HeldLock, Locked, effective_uid, take_lock, user_name};
    use crate::git::Git;
    use crate::lines::WORK_ROOT_STEP;
    use gate_wire::{NEW_WORK_TREE, created_line, version_line, warm_line};

    const KEY: &str = "repo-0123abcd";
    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// Makes the folder `path` and each missing one above it with mode 0700, as the box makes a root and its
    /// `<root>/work`, whatever the umask.
    fn private_folders(path: &str) {
        std::os::unix::fs::DirBuilderExt::mode(fs::DirBuilder::new().recursive(true), 0o700)
            .create(path)
            .expect("the private folders are made");
    }

    /// A temp folder of the case `name`, private to this process, removed when the case ends.
    struct Case {
        base: PathBuf,
        /// The work root's name in [`Case::base`].
        leaf: &'static str,
    }

    impl Case {
        fn new(name: &str) -> Case {
            Case::rooted(name, "gate-remote")
        }

        /// A case whose work root is named `leaf`.
        fn rooted(name: &str, leaf: &'static str) -> Case {
            let base = std::env::temp_dir().join(format!("gate-box-unix-{}-{name}", std::process::id()));
            if base.exists() {
                fs::remove_dir_all(&base).expect("a stale case folder is removed");
            }
            fs::create_dir_all(&base).expect("the case folder is made");
            Case { base, leaf }
        }

        fn root(&self) -> String {
            self.base.join(self.leaf).to_str().expect("the temp folder is UTF-8").to_owned()
        }

        fn work(&self) -> String {
            format!("{}/work/{KEY}", self.root())
        }

        fn bare(&self) -> String {
            format!("{}/repo.git", self.root())
        }

        /// The preflight payload of this case's work folder, with `extra` keys set over it.
        fn payload(&self, extra: &Value) -> Payload {
            let (root, work) = (self.root(), self.work());
            let mut json = json!({
                "Key": KEY,
                "Bare": self.bare(),
                "Work": work,
                "Lock": format!("{work}.lock"),
                "Index": format!("{work}.index"),
                "OutputFolder": format!("{root}/outputs"),
                "Job": format!("gate-remote-{KEY}-0123456789ab"),
            });
            if let (Some(object), Some(extra)) = (json.as_object_mut(), extra.as_object()) {
                object.extend(extra.clone());
            }
            Payload::decode(&STANDARD.encode(json.to_string()), PathForm::Unix).expect("the payload decodes")
        }
    }

    impl Drop for Case {
        fn drop(&mut self) {
            // Best effort: a folder left behind is in the temp folder and named for this process.
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    /// Runs the preflight of `payload`; its status and console.
    fn preflighted(payload: &Payload) -> (u8, Console) {
        let mut console = Console::captured();
        let status = preflight(payload, &gate_args::BoxSeams::default(), &mut console);
        (status, console)
    }

    /// Checks `lines`, what follows the boot line, are at most one sdks line, then exactly `tools`.
    fn assert_sdks_then_tools(lines: &[String], tools: &[&str]) {
        let split = lines.len().saturating_sub(tools.len());
        assert_eq!(&lines[split..], tools, "{lines:?}");
        assert!(split <= 1, "{lines:?}");
        assert!(lines[..split].iter().all(|line| line.starts_with("gate: remote sdks ")), "{lines:?}");
    }

    #[test]
    fn a_first_preflight_readies_the_bare_and_the_tree_and_says_its_lines_in_order() {
        let case = Case::new("first");
        let payload = case.payload(&json!({ "Needs": { "unreal": "", "bogus": "1.0" } }));
        let (status, console) = preflighted(&payload);
        assert_eq!(status, 0, "{:?}", console.err);
        let out = &console.out;
        assert_eq!(
            out[..4],
            [created_line(&case.bare()), version_line(), warm_line(None), NEW_WORK_TREE.to_owned()],
            "{out:?}"
        );
        assert!(out[4].starts_with("gate: remote boot "), "{out:?}");
        assert_sdks_then_tools(
            &out[5..],
            &["gate: remote tool unreal missing", "gate: remote tool bogus skipped: unknown tool"],
        );

        let work = case.work();
        assert!(PathBuf::from(case.bare()).join("HEAD").is_file(), "the bare repository is made");
        let alternates = fs::read_to_string(format!("{work}/.git/objects/info/alternates")).expect("the alternates file is written");
        assert_eq!(alternates, format!("{}/objects\n", case.bare()));
        let config = fs::read_to_string(format!("{work}/.git/config")).expect("the tree's config is written");
        assert!(config.contains(&format!("url = {}", case.bare())), "{config}");
        assert!(config.contains("autocrlf = false") && config.contains("eol = lf"), "{config}");
        let root_mode = fs::metadata(case.root()).expect("the root is made").mode();
        assert_eq!(root_mode & 0o077, 0, "the root is the box user's alone: {root_mode:o}");
    }

    #[test]
    fn a_tree_whose_git_names_another_repository_s_folders_is_left_alone_for_the_run() {
        let case = Case::new("commondir");
        let payload = case.payload(&json!({}));
        assert_eq!(preflighted(&payload).0, 0);
        let commondir = format!("{}/.git/commondir", case.work());
        fs::write(&commondir, "../elsewhere\n").expect("the commondir file is written");
        let alternates = format!("{}/.git/objects/info/alternates", case.work());
        fs::remove_file(&alternates).expect("the alternates file is removed");

        let (status, console) = preflighted(&payload);

        assert_eq!(status, 0, "{:?}", console.err);
        assert!(!Path::new(&alternates).exists(), "the tree is not readied");
        assert_eq!(fs::read(&commondir).expect("the file is read"), b"../elsewhere\n");
    }

    /// Writes `case`'s ran record, as a command started in its work folder does.
    fn ran_written(case: &Case) {
        fs::write(crate::command::ran_record(&case.work()), "2026-01-01T00:00:00.0000000Z").expect("the ran record is written");
    }

    #[test]
    fn a_second_preflight_with_no_run_between_still_says_new_work_tree() {
        let case = Case::new("new-until-a-run");
        let payload = case.payload(&json!({}));
        let (status, first) = preflighted(&payload);
        assert_eq!(status, 0, "{:?}", first.err);
        assert_eq!(first.out[3], NEW_WORK_TREE, "{:?}", first.out);

        let (status, second) = preflighted(&payload);

        assert_eq!(status, 0, "{:?}", second.err);
        assert_eq!(
            second.out[..3],
            [version_line(), warm_line(None), NEW_WORK_TREE.to_owned()],
            "{:?}",
            second.out
        );
    }

    #[test]
    fn a_remade_tree_with_a_stale_ran_record_says_new_until_a_run_starts() {
        let case = Case::new("remade-stale-ran");
        let payload = case.payload(&json!({}));
        assert_eq!(preflighted(&payload).0, 0);
        ran_written(&case);
        fs::remove_dir_all(Path::new(&case.work()).join(".git")).expect("the tree's .git is removed");

        for _ in 0..2 {
            let (status, console) = preflighted(&payload);
            assert_eq!(status, 0, "{:?}", console.err);
            assert_eq!(
                console.out[2], NEW_WORK_TREE,
                "no command has started in the remade tree: {:?}",
                console.out
            );
        }
        assert!(
            !Path::new(&crate::command::ran_record(&case.work())).exists(),
            "the remade tree's record is gone"
        );
    }

    #[test]
    fn a_single_payload_against_a_pair_recorded_folder_gets_the_cold_raise() {
        let case = Case::new("single-on-pair");
        let payload = case.payload(&json!({}));
        assert_eq!(preflighted(&payload).0, 0);
        ran_written(&case);
        case.layout_written("pair repo lib");

        let (status, console) = preflighted(&payload);

        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(
            console.out[2], NEW_WORK_TREE,
            "a run of one repo clears a pair's folder: {:?}",
            console.out
        );
    }

    #[test]
    fn a_second_preflight_is_idempotent_and_reads_the_warm_record() {
        let case = Case::new("again");
        let payload = case.payload(&json!({}));
        assert_eq!(preflighted(&payload).0, 0);

        ran_written(&case);
        let (status, console) = preflighted(&payload);
        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(console.out[..2], [version_line(), warm_line(None)], "no created, {:?}", console.out);
        assert!(console.out[2].starts_with("gate: remote boot "), "no new work tree: {:?}", console.out);
        assert_sdks_then_tools(&console.out[3..], &[]);

        fs::write(format!("{}.warm", case.work()), HASH).expect("the warm record is written");
        let (_, console) = preflighted(&payload);
        assert_eq!(console.out[1], warm_line(Some(HASH)));

        fs::write(format!("{}.warm", case.work()), format!("{HASH}\n")).expect("the warm record is written");
        let (_, console) = preflighted(&payload);
        assert_eq!(console.out[1], warm_line(None), "a line end is not a hash's, as on the Windows box");
    }

    /// Readies `case`'s root and its `slots/` at mode 0700 with a count file of `count` slots and a hold's claim in slot
    /// 0, its lock held by the returned open as a live hold holds it.
    fn held_by_a_hold(case: &Case, count: u32) -> HeldLock {
        let slots = format!("{}/slots", case.root());
        private_folders(&slots);
        fs::write(format!("{}/slots.json", case.root()), json!({ "v": 1, "slots": count }).to_string()).expect("the count file is written");
        let hold = Claim::Hold(HoldClaim {
            holder: "github-actions repo ci/test run 7".to_owned(),
            ceiling: 600,
            pid: 42,
            started: "2026-10-27T10:00:00Z".to_owned(),
        });
        fs::write(format!("{slots}/0.json"), hold.text()).expect("the hold's claim is written");
        match take_lock(Path::new(&format!("{slots}/0.lock"))).expect("the hold's lock is taken") {
            Locked::Held(lock) => Some(lock),
            Locked::Busy => None,
        }
        .expect("no one else holds the hold's lock")
    }

    /// `case`'s payload with the drain session `id`.
    fn sessioned(case: &Case, id: &str) -> Payload {
        case.payload(&json!({ "Session": id, "Pc": "pc1", "Repo": "repo" }))
    }

    /// The box slots lines of `lines`.
    fn slots_lines(lines: &[String]) -> Vec<BoxSlots> {
        lines
            .iter()
            .filter(|line| line.starts_with(BOX_SLOTS_LINE))
            .map(|line| BoxSlots::of_line(line).expect("the box slots line reads"))
            .collect()
    }

    #[test]
    fn a_passing_preflight_says_the_builder_s_held_slots_after_its_sdks_and_tools_lines() {
        let case = Case::new("slots-pass");
        let _hold = held_by_a_hold(&case, 2);

        let (status, console) = preflighted(&sessioned(&case, "s1"));

        assert_eq!(status, 0, "{:?}", console.err);
        let (last, before) = console.out.split_last().expect("the preflight says its lines");
        assert!(slots_lines(before).is_empty(), "said last: {:?}", console.out);
        let said = BoxSlots::of_line(last).expect("the last line is the box slots line");
        assert_eq!(said.count, Some(2), "{last}");
        assert_eq!(said.holders.iter().map(|holder| holder.slot).collect::<Vec<_>>(), [0, 1], "{last}");
        assert!(
            matches!(&said.holders[0].claim, Claim::Hold(hold) if hold.pid == 42 && hold.ceiling == 600),
            "{last}"
        );
        assert_eq!(said.holders[1].claim.session(), Some("s1"), "{last}");
    }

    #[test]
    fn a_full_refusal_says_the_builder_s_held_slots_after_the_version_line() {
        let case = Case::new("slots-full");
        let _hold = held_by_a_hold(&case, 1);

        let (status, console) = preflighted(&sessioned(&case, "s1"));

        assert_eq!(status, crate::run::REFUSED_STATUS, "{:?}", console.err);
        assert_eq!(console.out.len(), 2, "{:?}", console.out);
        assert_eq!(console.out[0], version_line());
        let said = BoxSlots::of_line(&console.out[1]).expect("the box slots line reads");
        assert_eq!((said.count, said.holders.len()), (Some(1), 1), "{:?}", console.out);
        assert_eq!(
            console.err.last().map(String::as_str),
            Some("gate: remote builder is full: 1 of 1 slots held"),
            "{:?}",
            console.err
        );
    }

    #[test]
    fn a_sessionless_preflight_says_the_builder_s_held_slots_and_takes_none() {
        let case = Case::new("slots-sessionless");
        let _hold = held_by_a_hold(&case, 1);

        let (status, console) = preflighted(&case.payload(&json!({})));

        assert_eq!(
            status, 0,
            "a preflight with no session takes no slot, so a full builder passes it: {:?}",
            console.err
        );
        let said = slots_lines(&console.out);
        assert_eq!(said.len(), 1, "{:?}", console.out);
        assert_eq!(said[0].holders.len(), 1, "only the hold: {:?}", console.out);
        assert_eq!(console.out.last(), Some(&said[0].line()), "said last");
    }

    #[test]
    fn a_stray_claim_name_beside_a_live_claim_is_no_holder_of_the_look() {
        let case = Case::new("slots-stray");
        let _hold = held_by_a_hold(&case, 1);
        let slots = format!("{}/slots", case.root());
        fs::copy(format!("{slots}/0.json"), format!("{slots}/00.json")).expect("the stray claim name is written");

        let (status, console) = preflighted(&case.payload(&json!({})));

        assert_eq!(status, 0, "{:?}", console.err);
        let said = slots_lines(&console.out);
        assert_eq!(said.len(), 1, "{:?}", console.out);
        assert_eq!(
            said[0].holders.iter().map(|holder| holder.slot).collect::<Vec<_>>(),
            [0],
            "{:?}",
            console.out
        );
        assert_eq!(said[0].unreadable, 0, "{:?}", console.out);
    }

    #[test]
    fn a_slots_link_or_file_fails_the_look_and_says_no_box_slots_line() {
        for (name, made) in [("slots-link", true), ("slots-file", false)] {
            let case = Case::new(name);
            let root = case.root();
            private_folders(&root);
            fs::write(format!("{root}/slots.json"), json!({ "v": 1, "slots": 1 }).to_string()).expect("the count file is written");
            if made {
                let elsewhere = format!("{root}/elsewhere");
                private_folders(&elsewhere);
                symlink(&elsewhere, format!("{root}/slots")).expect("the link is made");
            } else {
                fs::write(format!("{root}/slots"), "").expect("the file is written");
            }

            let (status, console) = preflighted(&case.payload(&json!({})));

            assert_eq!(status, 0, "{name}: {:?}", console.err);
            assert!(slots_lines(&console.out).is_empty(), "{name}: {:?}", console.out);
            assert!(
                console.err.iter().any(|line| line.starts_with("gate: slots look failed: ")),
                "{name}: {:?}",
                console.err
            );
        }
    }

    #[test]
    fn a_preflight_under_a_root_without_slots_says_no_box_slots_line() {
        let case = Case::new("slots-none");

        let (status, console) = preflighted(&sessioned(&case, "s1"));

        assert_eq!(status, 0, "{:?}", console.err);
        assert!(slots_lines(&console.out).is_empty(), "{:?}", console.out);
    }

    /// Runs git with `args` to set a case up, its output dropped; fails the case unless git exits 0.
    #[expect(
        clippy::disallowed_methods,
        reason = "the spawn lock is the Windows gate's; this sets a case's repository up"
    )]
    fn git_in_case(args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git starts");
        assert!(status.success(), "git {args:?}: {status}");
    }

    #[test]
    fn a_git_folder_that_is_no_repository_fails_its_step_and_leaves_a_repository_above_alone() {
        let case = Case::new("parent-repo");
        git_in_case(&["init", "-q", case.base.to_str().expect("the temp folder is UTF-8")]);
        let config = case.base.join(".git").join("config");
        let before = fs::read(&config).expect("the parent's config is read");
        private_folders(&format!("{}/.git", case.work()));

        let (status, console) = preflighted(&case.payload(&json!({})));
        assert_ne!(status, 0, "{:?}", console.out);
        let last = console.err.last().expect("the failure is said");
        assert!(last.starts_with("gate: remote git ") && last.contains(" failed"), "{:?}", console.err);
        assert_eq!(
            fs::read(&config).expect("the parent's config is read"),
            before,
            "the parent repository is not touched"
        );
    }

    #[test]
    fn a_git_step_that_names_no_repository_finds_none_above_the_work_folders() {
        let case = Case::new("ceiling");
        git_in_case(&["init", "-q", case.base.to_str().expect("the temp folder is UTF-8")]);
        let (root, parent) = (case.root(), format!("{}/work", case.root()));
        private_folders(&parent);

        let git = Git::of(&root, Path::new(&parent));
        let ran = git.run("git rev-parse", &["rev-parse", "--git-dir"]).expect("git runs");
        assert!(
            !ran.status.success(),
            "the repository above the root is not found: {}",
            String::from_utf8_lossy(&ran.stdout)
        );
    }

    /// Checks the preflight of `case` is refused with the folder line for `folder` and `why`, status 1.
    fn assert_folder_refused(case: &Case, folder: &str, why: &str) {
        assert_folder_refused_with(&case.payload(&json!({})), folder, why);
    }

    /// [`assert_folder_refused`] for the preflight of `payload`.
    fn assert_folder_refused_with(payload: &Payload, folder: &str, why: &str) {
        let (status, console) = preflighted(payload);
        assert_eq!(status, 1, "{:?}", console.out);
        assert_eq!(console.err, [format!("gate: remote could not create the folder {folder}: {why}")]);
    }

    #[test]
    fn a_link_at_the_work_folders_parent_is_refused_and_its_target_untouched() {
        let case = Case::new("linked-parent");
        let target = case.base.join("elsewhere");
        fs::create_dir_all(&target).expect("the link's target is made");
        private_folders(&case.root());
        let parent = format!("{}/work", case.root());
        symlink(&target, &parent).expect("the link is made");

        assert_folder_refused(&case, &parent, "it is a symbolic link");
        assert_eq!(
            fs::read_dir(&target).expect("the target is read").count(),
            0,
            "nothing is made through the link"
        );
    }

    #[test]
    fn a_link_at_the_bare_repository_is_refused_and_its_target_untouched() {
        let case = Case::new("linked-bare");
        let target = case.base.join("elsewhere.git");
        fs::create_dir_all(&target).expect("the link's target is made");
        private_folders(&format!("{}/work", case.root()));
        symlink(&target, case.bare()).expect("the link is made");

        assert_folder_refused(&case, &case.bare(), "it is a symbolic link");
        assert_eq!(
            fs::read_dir(&target).expect("the target is read").count(),
            0,
            "nothing is made through the link"
        );
    }

    #[test]
    fn a_file_where_the_work_folder_belongs_is_refused_and_kept() {
        let case = Case::new("file-work");
        private_folders(&format!("{}/work", case.root()));
        fs::write(case.work(), "kept\n").expect("the file is written");

        assert_folder_refused(&case, &case.work(), "it is not a folder");
        assert_eq!(fs::read(case.work()).expect("the file is read"), b"kept\n");
    }

    #[test]
    fn a_new_tree_drops_a_stale_warm_record() {
        let case = Case::new("stale-warm");
        private_folders(&format!("{}/work", case.root()));
        let warm = format!("{}.warm", case.work());
        fs::write(&warm, HASH).expect("the warm record is written");

        let (status, console) = preflighted(&case.payload(&json!({})));
        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(
            console.out[..3],
            [created_line(&case.bare()), version_line(), warm_line(None)],
            "{:?}",
            console.out
        );
        assert!(!Path::new(&warm).exists(), "the record of a tree that is gone is removed");
    }

    #[test]
    fn a_tree_without_the_line_end_config_gets_it_on_its_next_preflight() {
        let case = Case::new("config");
        private_folders(&case.work());
        git_in_case(&["init", "-q", &case.work()]);

        let (status, console) = preflighted(&case.payload(&json!({})));
        assert_eq!(status, 0, "{:?}", console.err);
        let config = fs::read_to_string(format!("{}/.git/config", case.work())).expect("the tree's config is read");
        assert!(config.contains("autocrlf = false") && config.contains("eol = lf"), "{config}");
    }

    #[test]
    fn a_link_at_the_work_folder_is_refused_and_its_target_untouched() {
        let case = Case::new("linked-work");
        let target = case.base.join("elsewhere");
        fs::create_dir_all(&target).expect("the link's target is made");
        private_folders(&format!("{}/work", case.root()));
        symlink(&target, case.work()).expect("the link is made");

        let (status, console) = preflighted(&case.payload(&json!({})));
        assert_eq!(status, 1, "{:?}", console.out);
        assert_eq!(
            console.err,
            [format!("gate: remote could not create the folder {}: it is a symbolic link", case.work())]
        );
        assert_eq!(
            fs::read_dir(&target).expect("the target is read").count(),
            0,
            "nothing is made through the link"
        );
    }

    #[test]
    fn an_interrupted_command_record_makes_the_tree_new() {
        let case = Case::new("interrupted");
        let payload = case.payload(&json!({}));
        assert_eq!(preflighted(&payload).0, 0);
        fs::write(format!("{}.command", case.work()), "x").expect("the record is written");

        let (_, console) = preflighted(&payload);
        assert_eq!(console.out[2], NEW_WORK_TREE, "{:?}", console.out);
    }

    #[test]
    fn a_root_others_may_write_in_is_refused_as_not_locked_down() {
        let case = Case::new("open-root");
        let root = case.root();
        fs::create_dir(&root).expect("the root is made");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).expect("the root is opened to everyone");

        let (status, console) = preflighted(&case.payload(&json!({})));
        assert_eq!(status, 1);
        assert_eq!(console.out, [version_line()]);
        let why = format!("{root} is not owned by {} or is open to others", user_name(effective_uid()));
        assert_eq!(console.err, [root_unsafe_line(&why, WORK_ROOT_STEP)]);
        assert!(!PathBuf::from(&root).join("work").exists(), "nothing is made in a refused root");
    }

    /// Checks the preflight of the case `name` with the folder `folder` of it made at 0775 is refused as not locked
    /// down, naming the folder to remove.
    fn assert_open_folder_refused(name: &str, folder: fn(&Case) -> String) {
        let case = Case::new(name);
        let path = folder(&case);
        private_folders(&path);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o775)).expect("the folder is opened to the group");

        let (status, console) = preflighted(&case.payload(&json!({})));
        assert_eq!(status, 1);
        assert_eq!(console.out, [version_line()]);
        let line =
            format!("gate: the builder's work root is not locked down: {path} is writable by others; remove {path} and the box makes it again");
        assert_eq!(console.err, [line]);
    }

    #[test]
    fn a_bare_repository_others_may_write_is_refused_naming_it_to_remove() {
        assert_open_folder_refused("open-bare", Case::bare);
    }

    #[test]
    fn a_work_folder_others_may_write_is_refused_naming_it_to_remove() {
        assert_open_folder_refused("open-work", Case::work);
    }

    #[test]
    fn a_tree_whose_git_others_may_write_is_left_alone_for_the_run() {
        let case = Case::new("open-git");
        let payload = case.payload(&json!({}));
        assert_eq!(preflighted(&payload).0, 0);
        let dot_git = format!("{}/.git", case.work());
        fs::set_permissions(&dot_git, fs::Permissions::from_mode(0o775)).expect("the .git is opened to the group");
        let alternates = format!("{dot_git}/objects/info/alternates");
        fs::remove_file(&alternates).expect("the alternates file is removed");

        let (status, console) = preflighted(&payload);

        assert_eq!(status, 0, "{:?}", console.err);
        assert!(!Path::new(&alternates).exists(), "the tree is not readied");
        let mode = fs::symlink_metadata(&dot_git).expect("the .git is looked at").permissions().mode() & 0o777;
        assert_eq!(mode, 0o775, "the .git is left as it is");
    }

    #[test]
    fn a_bare_repository_outside_the_root_is_a_usage_error() {
        let case = Case::new("bare-outside");
        let bare = format!("{}/other/repo.git", case.root());
        let (status, console) = preflighted(&case.payload(&json!({ "Bare": bare })));
        assert_eq!(status, 2);
        assert_eq!(
            console.err,
            [format!("gate: box payload Bare must be <root>/<name>.git to lock its root down: {bare}")]
        );
        assert!(!PathBuf::from(case.root()).exists(), "nothing is made");
    }

    #[test]
    fn a_work_root_holding_a_colon_is_a_usage_error_and_nothing_is_made() {
        let case = Case::rooted("colon-root", "gate:remote");
        let (status, console) = preflighted(&case.payload(&json!({})));
        assert_eq!(status, 2, "{:?}", console.out);
        assert_eq!(
            console.err,
            [format!(
                "gate: box payload Work must be <root>/work/<key> with no ':' in <root>, which GIT_CEILING_DIRECTORIES cannot hold: {}",
                case.work()
            )]
        );
        assert!(console.out.is_empty(), "{:?}", console.out);
        assert!(!PathBuf::from(case.root()).exists(), "nothing is made");
    }

    #[test]
    fn a_job_of_another_work_folder_is_a_usage_error() {
        let case = Case::new("job");
        let (status, console) = preflighted(&case.payload(&json!({ "Job": "gate-remote-other-0123456789ab" })));
        assert_eq!(status, 2);
        assert_eq!(
            console.err,
            ["gate: box payload Job is not this work folder's job name: gate-remote-other-0123456789ab"]
        );
    }

    /// The pair of the leaf `repo` and the sibling `lib`.
    fn pair() -> Value {
        json!({ "Pair": { "Leaf": "repo", "Siblings": ["lib"] } })
    }

    impl Case {
        /// The sibling `lib`'s bare repository, beside the main one.
        fn lib_bare(&self) -> String {
            format!("{}/lib.git", self.root())
        }

        /// Writes the work folder's layout record as `text`.
        fn layout_written(&self, text: &str) {
            private_folders(&format!("{}/work", self.root()));
            fs::write(format!("{}.layout", self.work()), text).expect("the layout record is written");
        }
    }

    #[test]
    fn a_pair_s_first_preflight_readies_every_bare_and_no_tree_and_is_new() {
        let case = Case::new("pair");
        let (status, console) = preflighted(&case.payload(&pair()));
        assert_eq!(status, 0, "{:?}", console.err);
        let out = &console.out;
        assert_eq!(
            out[..5],
            [
                created_line(&case.bare()),
                created_line(&case.lib_bare()),
                version_line(),
                warm_line(None),
                NEW_WORK_TREE.to_owned()
            ],
            "{out:?}"
        );
        assert!(out[5].starts_with("gate: remote boot "), "{out:?}");
        for bare in [case.bare(), case.lib_bare()] {
            assert!(PathBuf::from(&bare).join("HEAD").is_file(), "{bare} is made");
        }
        assert!(!Path::new(&case.work()).join("repo").exists(), "the run lays the trees out");
    }

    #[test]
    fn a_pair_s_bare_that_cannot_be_made_names_its_repo() {
        let case = Case::new("pair-bare");
        private_folders(&case.root());
        fs::write(case.lib_bare(), "kept\n").expect("a file takes the sibling's bare repository's name");

        assert_folder_refused_with(&case.payload(&pair()), &case.lib_bare(), "it is not a folder");
        assert_eq!(fs::read(case.lib_bare()).expect("the file is read"), b"kept\n");
    }

    #[test]
    fn a_layout_record_naming_the_pair_is_not_a_new_work_tree() {
        let case = Case::new("pair-kept");
        let payload = case.payload(&pair());
        assert_eq!(preflighted(&payload).0, 0);
        case.layout_written("pair repo lib");
        ran_written(&case);

        let (status, console) = preflighted(&payload);
        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(console.out[..2], [version_line(), warm_line(None)], "{:?}", console.out);
        assert!(console.out[2].starts_with("gate: remote boot "), "no new work tree: {:?}", console.out);
    }

    #[test]
    fn a_pair_whose_sibling_tree_has_no_git_is_warm_none_and_not_a_new_work_tree_as_on_windows() {
        let case = Case::new("pair-sibling-no-git");
        let payload = case.payload(&pair());
        assert_eq!(preflighted(&payload).0, 0);
        case.layout_written("pair repo lib");
        ran_written(&case);
        let hash = "a".repeat(64);
        fs::write(format!("{}.warm", case.work()), &hash).expect("the warm record is written");
        for name in ["repo", "lib"] {
            fs::create_dir_all(Path::new(&case.work()).join(name).join(".git")).expect("the tree's .git is made");
        }
        let (status, console) = preflighted(&payload);
        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(console.out[..2], [version_line(), warm_line(Some(hash.as_str()))], "{:?}", console.out);
        assert!(console.out[2].starts_with("gate: remote boot "), "both trees whole: {:?}", console.out);

        fs::remove_dir(Path::new(&case.work()).join("lib").join(".git")).expect("the sibling's .git is removed");
        let (status, console) = preflighted(&payload);
        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(console.out[..2], [version_line(), warm_line(None)], "{:?}", console.out);
        assert!(
            console.out[2].starts_with("gate: remote boot "),
            "the sibling's .git missing: {:?}",
            console.out
        );
    }

    #[test]
    fn a_pair_s_bare_whose_git_init_fails_says_the_windows_box_s_step() {
        let case = Case::new("pair-init-fails");
        let git = crate::git::test_program::real_git();
        let shim = case.base.join("git-shim");
        // The sibling's `git init --bare` says an error and fails; every other step is the real git's.
        let script = format!(
            "#!/bin/sh\ncase \" $* \" in\n*\" init --bare \"*lib.git*) echo 'fatal: cannot init' >&2; exit 128;;\nesac\nexec '{}' \"$@\"\n",
            git.display()
        );
        fs::write(&shim, script).expect("the shim is written");
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o700)).expect("the shim is made runnable");
        crate::git::test_program::set(&shim);

        let (status, console) = preflighted(&case.payload(&pair()));
        assert_ne!(status, 0, "{:?}", console.err);
        assert_eq!(
            console.err.last().map(String::as_str),
            Some("gate: remote git init --bare failed for lib (status 128)"),
            "{:?}",
            console.err
        );
        assert!(console.err.iter().all(|line| !line.contains("for lib failed")), "{:?}", console.err);
    }

    #[test]
    fn a_layout_record_of_another_layout_has_the_run_clear_the_work_folder() {
        let case = Case::new("relaid");
        for (extra, record) in [(pair(), "single"), (json!({}), "pair repo lib")] {
            case.layout_written(record);
            let (status, console) = preflighted(&case.payload(&extra));
            assert_eq!(status, 0, "{record}: {:?}", console.err);
            assert!(console.out.iter().any(|line| line == NEW_WORK_TREE), "{record}: {:?}", console.out);
            assert!(
                !Path::new(&case.work()).join(".git").exists(),
                "{record}: no tree is readied for the run to clear"
            );
        }
    }
}
