//! A routed run's outputs on the PC, up to its run call's end: the manifest of the files the PC holds under the run's
//! output globs, hashed before the run call and sent on its standard input; the pending marker, written after the push
//! and before the run call and removed once the outputs step completes, whose presence makes the next routed run of the
//! worktree ask for every output that differs from the PC's (`OutputsFull`); the box's `gate-remote-outputs:` line the
//! client takes from the run call; and the outputs call that fetches the archive it names into the worktree's `.tmp`
//! folder, for [`install`](super::install) to put into the worktree.

use std::cell::Cell;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use gate_state::v1;

use super::RemoteCall;
use super::client::{self, Attempt, Called, End, Limits, RawSink};
use super::git::FAILURE_BYTES;
use super::install::{self, Fetched};
use super::payload::Location;
use crate::boxside::payload::{archive_file_name, escaped};
use crate::outputs::archive::final_path;
use crate::outputs::glob::Globs;
use crate::outputs::hash::sha256_of;
use crate::outputs::manifest::{MANIFEST_LIMIT, Manifest};
use crate::outputs::walk::{in_context, walk};

/// Why a manifest line longer than [`MANIFEST_LIMIT`] is not sent.
const PAST_THE_CAP: &str = "it is larger than the box takes (64 MiB)";
/// The start of the box's line saying what it packed: `gate-remote-outputs: 0`, or `gate-remote-outputs: <n> <archive>`.
const OUTPUTS: &str = "gate-remote-outputs: ";
/// Why a passing run whose box said no outputs line the client accepts brings nothing back, as the PowerShell client
/// says it.
const NO_OUTPUTS_LINE: &str = "the box printed no 'gate-remote-outputs: <n> <archive>' line";
/// The worktree's folder the fetched archive is kept in, made when missing.
const TMP: &str = ".tmp";
/// Why a fetch the watchdog stopped brings nothing back, as the PowerShell client says it.
const STOPPED: &str = "the fetch was stopped by the watchdog";
/// What an archive a fetch was cut short of, or that was not installed for the run's end, is called when it cannot be
/// removed.
const PARTIAL: &str = "partial";
/// What an archive fetched whole and handed to the install is called when it cannot be removed.
pub(super) const FETCHED: &str = "fetched";

/// How a run's run call ended, for its outputs step: the work folder's place on the builder, the outputs nonce the run
/// sent, and the call's status, the command's own.
pub(super) struct Ran<'r> {
    pub(super) location: &'r Location,
    pub(super) nonce: &'r str,
    pub(super) status: i64,
}

/// The file in the worktree a fetched archive is written to, and whether an attempt of this run made it.
pub(super) struct Archive {
    path: PathBuf,
    made: bool,
}

impl Archive {
    /// The file at `path`, which no attempt made yet.
    pub(super) fn new(path: PathBuf) -> Archive {
        Archive { path, made: false }
    }

    /// The file's path.
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// The file an outputs call's attempt writes its standard output to raw: made new, its folder made when missing,
    /// once the file an earlier attempt of this run made is removed; a file of the name this run did not make is never
    /// removed, and fails the make. Else why it cannot be written.
    pub(super) fn create(&mut self) -> Result<RawSink, String> {
        let cannot = |error: io::Error| format!("cannot write {}: {error}", self.path.display());
        if self.made {
            match fs::remove_file(&self.path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(cannot(error)),
                _ => self.made = false,
            }
        }
        if let Some(folder) = self.path.parent() {
            fs::create_dir_all(folder).map_err(cannot)?;
        }
        let file = OpenOptions::new().write(true).create_new(true).open(&self.path).map_err(cannot)?;
        self.made = true;
        Ok(RawSink {
            file,
            shown: self.path.display().to_string(),
        })
    }

    /// Removes the file an attempt of this run made, when one did; one already gone counts as removed. Else why it
    /// could not be removed, the archive called `which`: [`PARTIAL`] or [`FETCHED`].
    fn remove(&self, which: &str) -> Result<(), String> {
        if !self.made {
            return Ok(());
        }
        match fs::remove_file(&self.path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                Err(format!("cannot remove the {which} outputs archive {}: {error}", self.path.display()))
            }
            _ => Ok(()),
        }
    }
}

/// Why the manifest was not built: the console interrupt was raised while it was, or a reason it could not be.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Unbuilt {
    Interrupted,
    Failed(String),
}

/// What the box's accepted outputs line says it packed: nothing, or `count` files in the archive `name`.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Brought {
    Nothing,
    Archive { count: u64, name: String },
}

/// What a run call carries of the run's outputs, and what the client keeps for after it.
pub(super) struct Sent<'a> {
    /// The output globs the run call carries: the run's, or none when it has none or its manifest was not built.
    pub(super) globs: &'a [String],
    /// Whether the run asks for every output that differs from the PC's: the pending marker was there.
    pub(super) full: bool,
    /// The manifest line, fed on the run call's standard input.
    pub(super) manifest: Option<Arc<str>>,
    /// The pending marker, written after the push by [`marked`]; `None` for a run with no output globs.
    marker: Option<PathBuf>,
    /// The manifest the line holds, its files' stamps, which the install checks the PC's files against; empty for a
    /// run with no output globs. Else why it could not be built, which a passing run says as it fails.
    built: Result<Manifest, String>,
}

/// What `attempt`'s run call carries of its outputs. A run with no output globs carries none, and reads no pending
/// marker. One with globs reads whether the marker is there, builds the manifest, polling the console interrupt as it
/// hashes, and records the marker's path for [`marked`] to write once the snapshot is pushed; a manifest that cannot
/// be built is kept for the outputs step, and the run call then carries no globs. Else the interrupt that ended the
/// build.
pub(super) fn sent<'a>(attempt: &mut Attempt<'_, 'a>) -> Result<Sent<'a>, End> {
    let call: &RemoteCall<'a> = attempt.call;
    let mut sent = Sent {
        globs: &[],
        full: false,
        manifest: None,
        marker: None,
        built: Ok(Manifest::default()),
    };
    if call.outputs.is_empty() {
        return Ok(sent);
    }
    let marker = v1::outputs_pending_path(call.state_dir, call.key);
    let full = pending(&marker);
    match manifest_line(Path::new(call.root), call.outputs, MANIFEST_LIMIT, || attempt.go_on().is_ok()) {
        Ok((line, files)) => {
            sent.globs = call.outputs;
            sent.full = full;
            sent.manifest = Some(Arc::from(line));
            sent.built = Ok(files);
        }
        Err(Unbuilt::Interrupted) => return Err(End::Interrupted),
        Err(Unbuilt::Failed(reason)) => sent.built = Err(reason),
    }
    sent.marker = Some(marker);
    Ok(sent)
}

/// Writes the pending marker `sent` records, once the snapshot is pushed and before the run call, so a run the client
/// sends after the push leaves it on every end but a passing run whose box packed nothing, and a run refused before
/// the push, at the preflight or by the parity check, leaves none. A run with no output globs records none. Else why
/// it could not be written.
pub(super) fn marked(sent: &Sent<'_>) -> Result<(), End> {
    match &sent.marker {
        Some(marker) => mark_pending(marker).map_err(End::Failed),
        None => Ok(()),
    }
}

/// The end of `attempt`'s run once its sync-back ended it `synced`, its run call having ended as `ran` says, and the
/// box having said it `brought` its outputs or not, for a run that carried `sent`: the outputs step of a run with
/// output globs. A run whose passing status the sync-back turned to 1 says its outputs were not brought back, the
/// worktree having changed. A passing run whose manifest was not built, or whose box said no outputs line the client
/// accepts, says why and ends 1. A passing run whose box packed an archive fetches it, as [`fetched`] says. A passing
/// run whose box packed nothing removes the pending marker, which every other end leaves; one that cannot be removed
/// is said, and the run's status stands.
pub(super) fn ended(attempt: &mut Attempt<'_, '_>, sent: &Sent<'_>, ran: &Ran<'_>, synced: End, brought: Option<&Brought>) -> End {
    let (Some(marker), End::Status(now)) = (&sent.marker, &synced) else {
        return synced;
    };
    if ran.status == 0 && *now == 1 {
        attempt.say(not_brought_back("the worktree changed during the run"));
        return synced;
    }
    if *now != 0 {
        return synced;
    }
    let reason = match (&sent.built, brought) {
        (Err(reason), _) => format!("the manifest could not be built: {reason}"),
        (Ok(_), None) => NO_OUTPUTS_LINE.to_owned(),
        (Ok(files), Some(Brought::Archive { .. })) => {
            let kept = Kept {
                globs: sent.globs,
                files,
                marker,
            };
            return fetched(attempt, &kept, ran, synced);
        }
        (Ok(_), Some(Brought::Nothing)) => {
            if let Err(why) = clear_pending(marker) {
                attempt.say(format!("gate: {why}"));
            }
            return synced;
        }
    };
    attempt.say(not_brought_back(&reason));
    End::Status(1)
}

/// What the outputs step of a passing run whose box packed an archive keeps for its install: the run's output globs,
/// the PC's manifest and the pending marker.
struct Kept<'k> {
    globs: &'k [String],
    files: &'k Manifest,
    marker: &'k Path,
}

/// The outputs call of a passing run whose box packed an archive, `synced` its end so far: the archive the run's own
/// key and nonce name, never the box's line, is fetched raw into the worktree's `.tmp` folder. One fetched whole is
/// installed, as [`install::installed`] says, through a stage folder beside it named for the same key and nonce. A
/// fetch the watchdog stops says the watchdog's line and ends the run killed; a Ctrl+C ends it as an interrupt, with no
/// `stop` call; any other failure says why, the box's last standard error line when it said one, and fails the run.
/// Each of these removes what was fetched, and says so when it cannot; each says the outputs were not brought back but
/// the interrupt.
fn fetched(attempt: &mut Attempt<'_, '_>, kept: &Kept<'_>, ran: &Ran<'_>, synced: End) -> End {
    let started = Instant::now();
    let name = archive_file_name(attempt.call.key, ran.nonce);
    let tmp = Path::new(attempt.call.root).join(TMP);
    let mut archive = Archive::new(tmp.join(&name));
    let limits = attempt.limits_for("outputs");
    match attempt.outputs_call(ran.location, &name, &mut archive, limits) {
        // `judged` hands back the run's passing end only when the archive was fetched whole.
        Ok(called) => match judged(attempt, &archive, &called, limits, synced) {
            End::Status(0) => {
                let fetched = Fetched {
                    archive: &archive,
                    stage: tmp.join(format!("{}-{}", attempt.call.key, ran.nonce)),
                    marker: kept.marker,
                    manifest: kept.files,
                    globs: kept.globs,
                    box_work: &ran.location.work,
                    started,
                };
                install::installed(attempt, &fetched)
            }
            end => end,
        },
        Err(End::Failed(reason)) => unfetched(attempt, &archive, &reason, End::Status(1)),
        Err(end) => {
            removed(attempt, &archive, PARTIAL);
            end
        }
    }
}

/// The end of the outputs step once its outputs call, watched by `limits`, ended as `called`, having written to
/// `archive`, the run's end so far `synced`: as [`fetched`] says.
pub(super) fn judged(attempt: &mut Attempt<'_, '_>, archive: &Archive, called: &Called, limits: Limits, synced: End) -> End {
    // A Ctrl+C raised by the call's end is the run's end, however the call ended, as after the run call.
    if attempt.go_on().is_err() {
        removed(attempt, archive, PARTIAL);
        return End::Interrupted;
    }
    if let Some(cut) = called.cut {
        if let Some(line) = client::watchdog_line(cut, limits) {
            attempt.say(line);
            attempt.noted(called);
            return unfetched(attempt, archive, STOPPED, End::Killed);
        }
        attempt.noted(called);
        return unfetched(attempt, archive, &cut.reason("the outputs call"), End::Status(1));
    }
    if called.status != 0 {
        let reason = match &called.last_error {
            Some(line) => client::first_bytes(line, FAILURE_BYTES).to_owned(),
            None => format!("the outputs call failed (status {})", called.status),
        };
        return unfetched(attempt, archive, &reason, End::Status(1));
    }
    synced
}

/// Says the run's outputs were not brought back, for `reason`, and removes what `archive` holds of them; `end`.
fn unfetched(attempt: &mut Attempt<'_, '_>, archive: &Archive, reason: &str, end: End) -> End {
    attempt.say(not_brought_back(reason));
    removed(attempt, archive, PARTIAL);
    end
}

/// Removes what `archive`, called `which` ([`Archive::remove`]), holds of a fetch; a removal that fails is said, and
/// changes no end.
pub(super) fn removed(attempt: &mut Attempt<'_, '_>, archive: &Archive, which: &str) {
    if let Err(why) = archive.remove(which) {
        attempt.say(format!("gate: {why}"));
    }
}

/// The manifest line, newline included, of the files under `globs` in the worktree `root`, each hashed as a stream
/// that asks `go_on` before each of its reads, and refused when its final path is not inside `root`'s. A line longer
/// than `cap` bytes is refused, its files' entries counted before each is hashed and the whole line checked at the
/// end. [`Unbuilt::Interrupted`] once `go_on` says no; else why the globs, the walk or a file cannot be read, or why
/// the line is not sent.
fn manifest_line(root: &Path, globs: &[String], cap: usize, go_on: impl Fn() -> bool) -> Result<(String, Manifest), Unbuilt> {
    let failed = |error: io::Error| Unbuilt::Failed(error.to_string());
    let globs = Globs::new(globs).map_err(Unbuilt::Failed)?;
    let found = walk(root, &globs).map_err(failed)?;
    let inside = fs::canonicalize(root).map_err(|error| failed(in_context(root, &error)))?;
    let stopped = Cell::new(false);
    let built = Manifest::build_with(&found, cap, |file| hashed(&file.full, &inside, &go_on, &stopped));
    if stopped.get() {
        return Err(Unbuilt::Interrupted);
    }
    let manifest = match built {
        Ok(manifest) => manifest,
        Err(error) if error.kind() == io::ErrorKind::FileTooLarge => return Err(Unbuilt::Failed(PAST_THE_CAP.to_owned())),
        Err(error) => return Err(failed(error)),
    };
    let line = manifest.line();
    within_cap(line.len(), cap).map_err(Unbuilt::Failed)?;
    Ok((line, manifest))
}

/// The SHA-256 of the file at `path`, read through a [`Polled`] reader, once its final path, as the system resolves the
/// open file, is found inside `inside`, the worktree's own final path: a folder swapped for a junction after the walk
/// is not followed out of it. Else why it cannot be read or is refused, its path named.
fn hashed(path: &Path, inside: &Path, go_on: &dyn Fn() -> bool, stopped: &Cell<bool>) -> io::Result<String> {
    let file = File::open(path).map_err(|error| in_context(path, &error))?;
    if !final_path(&file).map_err(|error| in_context(path, &error))?.starts_with(inside) {
        return Err(io::Error::other(format!("{} resolves outside the worktree", path.display())));
    }
    let polled = Polled { inner: file, go_on, stopped };
    sha256_of(polled).map(|(sha256, _)| sha256).map_err(|error| in_context(path, &error))
}

/// A reader that asks `go_on` before each read, and fails the read once it says no, marking `stopped`.
pub(super) struct Polled<'p, R> {
    pub(super) inner: R,
    pub(super) go_on: &'p dyn Fn() -> bool,
    pub(super) stopped: &'p Cell<bool>,
}

impl<R: Read> Read for Polled<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if !(self.go_on)() {
            self.stopped.set(true);
            // Not `ErrorKind::Interrupted`, which a copy retries.
            return Err(io::Error::other("the console interrupt was raised"));
        }
        self.inner.read(buffer)
    }
}

/// Whether a manifest line `length` bytes long, its newline included, is within `cap`; else why not.
fn within_cap(length: usize, cap: usize) -> Result<(), String> {
    if length > cap {
        return Err(PAST_THE_CAP.to_owned());
    }
    Ok(())
}

/// The line saying a run's outputs are not brought back, for `reason`.
pub(super) fn not_brought_back(reason: &str) -> String {
    format!("gate: remote outputs not brought back: {}", escaped(reason))
}

/// Whether the pending marker at `marker` is there. One whose presence cannot be read counts as there: the run then
/// asks for every output that differs from the PC's, which is never wrong, only slower.
fn pending(marker: &Path) -> bool {
    marker.try_exists().unwrap_or(true)
}

/// Writes the pending marker at `marker`, its folder made when missing; else why it could not be written.
fn mark_pending(marker: &Path) -> Result<(), String> {
    let cannot = |error: io::Error| format!("cannot write the outputs pending marker {}: {error}", marker.display());
    if let Some(folder) = marker.parent() {
        fs::create_dir_all(folder).map_err(cannot)?;
    }
    fs::write(marker, "").map_err(cannot)
}

/// Removes the pending marker at `marker`; one already gone counts as removed. Else why it could not be removed.
pub(super) fn clear_pending(marker: &Path) -> Result<(), String> {
    match fs::remove_file(marker) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(format!("cannot remove the outputs pending marker {}: {error}", marker.display()))
        }
        _ => Ok(()),
    }
}

/// Keeps `line` as `kept`, a call's last outputs line, when it came on standard error (`stderr`) and starts
/// `gate-remote-outputs: `: a line on standard output never counts, and a later line replaces an earlier one.
pub(super) fn keep_outputs_line(kept: &mut Option<String>, line: &str, stderr: bool) {
    if stderr && line.starts_with(OUTPUTS) {
        *kept = Some(line.to_owned());
    }
}

/// What the box packed, from `line`, the run call's last outputs line on standard error ([`keep_outputs_line`]): when
/// it is `gate-remote-outputs: 0` or names `<count> <key>-<nonce>.tar.zst`, the archive of the work folder `key` and of
/// this run's outputs `nonce`. Any other line, or none, brings nothing back: the command's own output can print such a
/// line, but not after the box's own, which comes last.
pub(super) fn accepted(line: Option<&str>, key: &str, nonce: &str) -> Option<Brought> {
    let said = line?.strip_prefix(OUTPUTS)?;
    if said == "0" {
        return Some(Brought::Nothing);
    }
    let (count, name) = said.split_once(' ')?;
    let digits = !count.is_empty() && count.bytes().all(|byte| byte.is_ascii_digit());
    let count: u64 = count.parse().ok().filter(|count| digits && *count > 0)?;
    (name == archive_file_name(key, nonce)).then(|| Brought::Archive {
        count,
        name: name.to_owned(),
    })
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its files must fail loudly")]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Instant;

    use gate_router::DEFAULT_OUTPUTS;

    use super::{Archive, Brought, MANIFEST_LIMIT, PARTIAL, Unbuilt, accepted, hashed, keep_outputs_line, manifest_line, pending, within_cap};
    use crate::outputs::glob::Globs;
    use crate::outputs::walk::walk;

    const KEY: &str = "app-1a2b3c4d";
    const NONCE: &str = "0123456789abcdef0123456789abcdef";
    const OTHER_NONCE: &str = "fedcba9876543210fedcba9876543210";
    const ERR: bool = true;
    const OUT: bool = false;

    /// What the client accepts of a run call that wrote `lines`, each on standard error or not, in order.
    fn accepted_of(lines: &[(bool, &str)]) -> Option<Brought> {
        let mut kept = None;
        for (stderr, line) in lines {
            keep_outputs_line(&mut kept, line, *stderr);
        }
        accepted(kept.as_deref(), KEY, NONCE)
    }

    /// The box's line for `count` files packed in the archive of `key` and `nonce`.
    fn named(count: u64, key: &str, nonce: &str) -> String {
        format!("gate-remote-outputs: {count} {key}-{nonce}.tar.zst")
    }

    /// What the box's line for `count` files of this run's archive says it brought.
    fn archive(count: u64) -> Brought {
        Brought::Archive {
            count,
            name: format!("{KEY}-{NONCE}.tar.zst"),
        }
    }

    /// A scratch folder for one test, emptied first.
    fn scratch(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("gate-remote-outputs-{}-{name}", std::process::id()));
        // A folder left by an earlier run of the test may not be there.
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).expect("the scratch folder can be made");
        folder
    }

    /// Makes `link` a junction to the folder `target`.
    #[expect(clippy::disallowed_methods, reason = "a test's one mklink takes no spawn lock")]
    fn junction(link: &Path, target: &Path) {
        let made = Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .expect("cmd.exe runs");
        assert!(made.status.success(), "a junction needs no privilege: {made:?}");
    }

    #[test]
    fn the_last_outputs_line_on_stderr_wins() {
        let named_line = named(2, KEY, NONCE);
        let zero_then_named = [(ERR, "gate-remote-outputs: 0"), (OUT, "built"), (ERR, named_line.as_str()), (ERR, "done")];
        assert_eq!(accepted_of(&zero_then_named), Some(archive(2)));
        let named_then_zero = [(ERR, named_line.as_str()), (ERR, "gate-remote-outputs: 0")];
        assert_eq!(accepted_of(&named_then_zero), Some(Brought::Nothing));
        assert_eq!(accepted_of(&[(ERR, "gate: remote said something else")]), None, "no outputs line");
        assert_eq!(accepted_of(&[]), None, "no line at all");
    }

    #[test]
    fn a_line_naming_another_nonce_is_not_accepted() {
        let foreign = named(1, KEY, OTHER_NONCE);
        assert_eq!(accepted_of(&[(ERR, foreign.as_str())]), None);
        assert_eq!(accepted_of(&[(ERR, named(1, "app-99999999", NONCE).as_str())]), None, "another key");
        let own = named(1, KEY, NONCE);
        assert_eq!(
            accepted_of(&[(ERR, own.as_str()), (ERR, foreign.as_str())]),
            None,
            "only the last line counts"
        );
    }

    #[test]
    fn an_outputs_line_on_stdout_is_ignored() {
        let box_line = named(3, KEY, NONCE);
        assert_eq!(
            accepted_of(&[(ERR, box_line.as_str()), (OUT, "gate-remote-outputs: 0")]),
            Some(archive(3))
        );
        assert_eq!(accepted_of(&[(OUT, "gate-remote-outputs: 0")]), None);
        assert_eq!(accepted_of(&[(OUT, named(1, KEY, NONCE).as_str())]), None);
    }

    #[test]
    fn outputs_zero_is_accepted_without_a_name() {
        assert_eq!(accepted_of(&[(ERR, "gate-remote-outputs: 0")]), Some(Brought::Nothing));
        let zero_named = format!("gate-remote-outputs: 0 {KEY}-{NONCE}.tar.zst");
        assert_eq!(accepted_of(&[(ERR, zero_named.as_str())]), None, "no archive holds no file");
    }

    #[test]
    fn a_malformed_outputs_line_is_not_accepted() {
        let name = format!("{KEY}-{NONCE}.tar.zst");
        for line in [
            "gate-remote-outputs: ".to_owned(),
            "gate-remote-outputs: 1".to_owned(),
            "gate-remote-outputs: 00".to_owned(),
            "gate-remote-outputs: 0 ".to_owned(),
            format!("gate-remote-outputs: x {name}"),
            format!("gate-remote-outputs: +1 {name}"),
            format!("gate-remote-outputs: -1 {name}"),
            format!("gate-remote-outputs:  1 {name}"),
            format!("gate-remote-outputs: 1 {name} "),
            format!("gate-remote-outputs: 1 {name} more"),
            format!("gate-remote-outputs: 1 {KEY}-{NONCE}.tar.xz"),
        ] {
            assert_eq!(accepted_of(&[(ERR, line.as_str())]), None, "{line:?}");
        }
    }

    #[test]
    fn a_manifest_over_64_mib_is_refused() {
        assert_eq!(MANIFEST_LIMIT, 64 * 1024 * 1024);
        assert_eq!(within_cap(MANIFEST_LIMIT, MANIFEST_LIMIT), Ok(()), "64 MiB, its newline included, fits");
        assert_eq!(
            within_cap(MANIFEST_LIMIT + 1, MANIFEST_LIMIT),
            Err("it is larger than the box takes (64 MiB)".to_owned())
        );
    }

    /// The entries' count stops short of the line's outer braces, so only the check of the whole line refuses a line
    /// one byte past the cap.
    #[test]
    fn the_whole_manifest_line_is_checked_against_the_cap() {
        let root = scratch("cap");
        fs::create_dir_all(root.join("bin")).expect("the folder can be made");
        fs::write(root.join("bin").join("a.dll"), "abc").expect("the file can be written");
        let globs = ["**".to_owned()];
        let (line, _) = manifest_line(&root, &globs, MANIFEST_LIMIT, || true).expect("the manifest is built");

        let fits = manifest_line(&root, &globs, line.len(), || true).map(|(fitting, _)| fitting);
        assert_eq!(fits, Ok(line.clone()));
        let refused = Err(Unbuilt::Failed("it is larger than the box takes (64 MiB)".to_owned()));
        assert_eq!(manifest_line(&root, &globs, line.len() - 1, || true), refused, "the whole line");
        assert_eq!(manifest_line(&root, &globs, 10, || true), refused, "the entries, before any hash");
        assert_eq!(manifest_line(&root, &globs, MANIFEST_LIMIT, || false), Err(Unbuilt::Interrupted));
    }

    #[test]
    fn a_file_whose_final_path_leaves_the_worktree_is_refused() {
        let folder = scratch("final-path");
        let (root, elsewhere) = (folder.join("worktree"), folder.join("elsewhere"));
        fs::create_dir_all(&root).expect("the worktree can be made");
        fs::create_dir_all(&elsewhere).expect("the outside folder can be made");
        fs::write(root.join("own.dll"), "abc").expect("the file can be written");
        fs::write(elsewhere.join("secret.dll"), "abc").expect("the file can be written");
        junction(&root.join("bin"), &elsewhere);
        let inside = fs::canonicalize(&root).expect("the worktree resolves");
        let stopped = Cell::new(false);

        let own = hashed(&root.join("own.dll"), &inside, &|| true, &stopped);
        let through = root.join("bin").join("secret.dll");
        let outside = hashed(&through, &inside, &|| true, &stopped);

        assert!(own.is_ok(), "{own:?}");
        let refused = outside.expect_err("a file reached through a junction is refused").to_string();
        assert_eq!(refused, format!("{} resolves outside the worktree", through.display()));
    }

    #[test]
    fn an_archive_file_is_made_anew_for_each_attempt_and_removed_only_when_made() {
        let path = scratch("archive").join(".tmp").join("app.tar.zst");
        let mut archive = Archive::new(path.clone());
        assert_eq!(archive.remove(PARTIAL), Ok(()), "nothing made, nothing removed");

        let mut first = archive.create().expect("the first attempt's file is made, its folder too");
        first.file.write_all(b"partial").expect("the file can be written");
        drop(first);
        drop(archive.create().expect("a retry removes the first attempt's file and makes it anew"));
        assert_eq!(
            fs::read(&path).expect("the file can be read"),
            Vec::<u8>::new(),
            "the retry's file starts empty"
        );
        assert_eq!(archive.remove(PARTIAL), Ok(()));
        assert!(!path.exists(), "a file this run made is removed");

        fs::write(&path, "another run's").expect("the file can be written");
        let mut fresh = Archive::new(path.clone());
        let refused = fresh.create().map(drop);
        let cannot = format!("cannot write {}: ", path.display());
        assert!(refused.as_ref().is_err_and(|why| why.starts_with(&cannot)), "{refused:?}");
        assert_eq!(fresh.remove(PARTIAL), Ok(()));
        assert_eq!(
            fs::read_to_string(&path).expect("the file is there"),
            "another run's",
            "a file this run did not make stays"
        );
    }

    #[test]
    fn a_marker_whose_presence_cannot_be_read_counts_as_pending() {
        let folder = scratch("pending");
        assert!(!pending(&folder.join("app-1a2b3c4d.outputs-pending")), "a missing marker");
        fs::write(folder.join("app-1a2b3c4d.outputs-pending"), "").expect("the marker can be written");
        assert!(pending(&folder.join("app-1a2b3c4d.outputs-pending")), "a marker that is there");
        assert!(pending(Path::new("app\0.outputs-pending")), "a path the system cannot look up");
    }

    /// A lab measurement, not a check: how long the walk and the whole manifest build take on a real built worktree,
    /// so the write of a `(path, size, mtime)` hash cache can be decided on a number. Reads the tree from
    /// `GATE_MEASURE_ROOT` and its globs from `GATE_MEASURE_GLOBS` (`;`-separated, [`DEFAULT_OUTPUTS`] when unset),
    /// prints one line of figures, and asserts nothing about time.
    #[test]
    #[ignore = "a lab measurement: set GATE_MEASURE_ROOT to a built worktree"]
    #[expect(clippy::print_stdout, reason = "a lab measurement prints its figures for the operator to read")]
    #[expect(clippy::cast_precision_loss, reason = "a size said to one decimal of a megabyte")]
    fn measure_the_manifest_time_of_a_real_tree() {
        let Ok(root) = std::env::var("GATE_MEASURE_ROOT") else {
            println!("measure: set GATE_MEASURE_ROOT to a built worktree to run this measurement");
            return;
        };
        let root = PathBuf::from(root);
        let globs: Vec<String> = match std::env::var("GATE_MEASURE_GLOBS") {
            Ok(list) => list.split(';').map(str::to_owned).collect(),
            Err(_) => DEFAULT_OUTPUTS.map(str::to_owned).to_vec(),
        };

        let started = Instant::now();
        let parsed = Globs::new(&globs).expect("the measurement's globs are well formed");
        let found = walk(&root, &parsed).expect("the measured tree can be walked");
        let walk_seconds = started.elapsed().as_secs_f64();
        let bytes: u64 = found.iter().map(|file| file.stamp.size).sum();

        let started = Instant::now();
        let (line, _) = manifest_line(&root, &globs, usize::MAX, || true).expect("the measured tree can be hashed");
        let manifest_seconds = started.elapsed().as_secs_f64();

        let mib = bytes as f64 / (1024.0 * 1024.0);
        println!(
            "manifest: {}: {} files, {mib:.1} MiB; walk {walk_seconds:.3} s; manifest_line {manifest_seconds:.3} s; line {} bytes",
            root.display(),
            found.len(),
            line.len()
        );
    }
}
