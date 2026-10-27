//! The builder drain as the PC reads it: the two lines a box says after its `drain` refusal tag, `gate: drain place
//! {n|none}` and `gate: drain barrier {raised} {lapses}` (or `gate: drain barrier none`); and the drain markers a run
//! waiting behind a builder's drain holds, which keep the remote-slot and key waits of other runs from their limits.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use gate_state::v1;
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};

use crate::console::Console;

/// `ERROR_SHARING_VIOLATION`: another handle holds the file without sharing it.
const SHARING_VIOLATION: i32 = 32;
/// `ERROR_LOCK_VIOLATION`: another handle has a region of the file locked.
const LOCK_VIOLATION: i32 = 33;

/// What a `drain` refusal said: the call's place in line, `None` for a call that cannot queue, and the standing
/// barrier's raise and lapse times in Unix seconds, both 0 when the box said no barrier stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Drain {
    pub place: Option<u32>,
    pub raised: i64,
    pub lapses: i64,
}

/// What one of the box's drain lines says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DrainLine {
    /// `gate: drain place {n}`, or `none`.
    Place(Option<u32>),
    /// `gate: drain barrier {raised} {lapses}`, or `None` for `gate: drain barrier none`.
    Barrier(Option<(i64, i64)>),
}

const PLACE: &str = "gate: drain place ";
const BARRIER: &str = "gate: drain barrier ";

impl Drain {
    /// This drain with what `line` says taken in.
    pub(crate) fn with(self, line: DrainLine) -> Drain {
        match line {
            DrainLine::Place(place) => Drain { place, ..self },
            DrainLine::Barrier(barrier) => {
                let (raised, lapses) = barrier.unwrap_or_default();
                Drain { raised, lapses, ..self }
            }
        }
    }

    /// The barrier's times as the drain's lines say them, `raised {hh:mm} UTC, lapses {hh:mm} UTC`; `None` when the box
    /// said no barrier stands.
    pub(crate) fn times(self) -> Option<String> {
        (self.raised != 0 || self.lapses != 0).then(|| format!("raised {} UTC, lapses {} UTC", utc_clock(self.raised), utc_clock(self.lapses)))
    }
}

/// The drain `lines` say, read in order ([`line_of`]); `None` when none of them is a drain line.
pub(crate) fn said_in<'l>(lines: impl IntoIterator<Item = &'l str>) -> Option<Drain> {
    lines
        .into_iter()
        .filter_map(line_of)
        .fold(None, |drain: Option<Drain>, line| Some(drain.unwrap_or_default().with(line)))
}

/// The Unix second `seconds` as the drain's lines say a time: `hh:mm`, in UTC.
pub(crate) fn utc_clock(seconds: i64) -> String {
    let day = seconds.rem_euclid(86_400);
    format!("{:02}:{:02}", day / 3_600, day % 3_600 / 60)
}

/// The drain marker a routed run holds while it waits behind a builder's drain: its file in the builder's marker folder
/// ([`v1::drain_marker_dir`]), open sharing nothing until [`Marker::end`].
pub(crate) struct Marker {
    path: PathBuf,
    file: File,
}

impl Marker {
    /// Makes and holds the marker of this process's wait of `session` behind the drain of the builder whose id is `id`
    /// (`None` for `host`) under the state root `state`, its folders made when missing.
    ///
    /// # Errors
    ///
    /// Why the folder cannot be made, or the file cannot be made and held as a plain file.
    pub(crate) fn hold(state: &Path, id: Option<&str>, session: &str) -> io::Result<Marker> {
        let folder = v1::drain_marker_dir(state, id);
        fs::create_dir_all(&folder)?;
        let path = folder.join(format!("{}-{session}", std::process::id()));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&path)?;
        gate_win32::check_plain_file(&file)?;
        Ok(Marker { path, file })
    }

    /// The marker's file.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Lets the marker go and removes its file; one a probe removed first is removed.
    ///
    /// # Errors
    ///
    /// Why the file cannot be removed.
    pub(crate) fn end(self) -> io::Result<()> {
        let Marker { path, file } = self;
        drop(file);
        match fs::remove_file(&path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
}

/// Whether a live drain marker stands for the builder whose id is `id` (`None` for `host`) under the state root
/// `state`: a file of its marker folder another handle holds. Each file found that opens is dead and is removed; a
/// missing folder holds no marker. An entry that cannot be probed or removed, a folder among them, is passed to
/// `skipped` with why, and the probe goes on.
///
/// # Errors
///
/// Why the folder cannot be listed.
pub(crate) fn marker_stands(state: &Path, id: Option<&str>, skipped: &mut dyn FnMut(&Path, &io::Error)) -> io::Result<bool> {
    stands_in(&v1::drain_marker_dir(state, id), skipped)
}

/// Whether a live drain marker stands for any builder under the state root `state` ([`marker_stands`]); a marker
/// folder that cannot be listed is passed to `skipped` as an entry is.
///
/// # Errors
///
/// Why the drain folder cannot be listed.
pub(crate) fn any_marker_stands(state: &Path, skipped: &mut dyn FnMut(&Path, &io::Error)) -> io::Result<bool> {
    let folders = match fs::read_dir(v1::drain_dir(state)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        listed => listed?,
    };
    for folder in folders {
        let path = folder?.path();
        match stands_in(&path, skipped) {
            Ok(true) => return Ok(true),
            Ok(false) => {}
            Err(error) => skipped(&path, &error),
        }
    }
    Ok(false)
}

/// What a run's drain-marker probes have said, each once: a probe that failed, and an entry one skipped.
#[derive(Default)]
pub(crate) struct ProbeSaid {
    failed: bool,
    skipped: bool,
}

impl ProbeSaid {
    /// Notes in the log, the first time alone, that a probe skipped the entry at `path` it cannot read for `error`.
    pub(crate) fn skipped(&mut self, console: &mut Console, path: &Path, error: &io::Error) {
        if !std::mem::replace(&mut self.skipped, true) {
            console.note(&format!(
                "gate: cannot read the drain marker {}: {}; skipped",
                gate_router::escaped(&path.to_string_lossy()),
                gate_router::escaped(&error.to_string())
            ));
        }
    }

    /// Says, the first time alone, that the probe of the marker folder `folder` failed for `error`.
    fn failed(&mut self, console: &mut Console, folder: &Path, error: &io::Error) {
        if !std::mem::replace(&mut self.failed, true) {
            console.err(&format!(
                "gate: cannot probe the drain markers {}: {}",
                gate_router::escaped(&folder.to_string_lossy()),
                gate_router::escaped(&error.to_string())
            ));
        }
    }
}

/// [`marker_stands`] for a run of the state root `state`, a probe that fails read as no marker, so the limit it would
/// suspend still holds; what it says goes through `said`.
pub(crate) fn marker_stood(state: &Path, id: Option<&str>, console: &mut Console, said: &mut ProbeSaid) -> bool {
    let probed = marker_stands(state, id, &mut |path, error| said.skipped(console, path, error));
    probed.unwrap_or_else(|error| {
        said.failed(console, &v1::drain_marker_dir(state, id), &error);
        false
    })
}

/// [`any_marker_stands`] for a run of the state root `state`, read as [`marker_stood`] reads its probe.
pub(crate) fn any_marker_stood(state: &Path, console: &mut Console, said: &mut ProbeSaid) -> bool {
    let probed = any_marker_stands(state, &mut |path, error| said.skipped(console, path, error));
    probed.unwrap_or_else(|error| {
        said.failed(console, &v1::drain_dir(state), &error);
        false
    })
}

/// Whether a file of the marker folder `folder` is held, removing each one that opens; a missing folder holds none. An
/// entry that cannot be probed or removed is passed to `skipped` with why.
fn stands_in(folder: &Path, skipped: &mut dyn FnMut(&Path, &io::Error)) -> io::Result<bool> {
    let entries = match fs::read_dir(folder) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        listed => listed?,
    };
    for entry in entries {
        let path = entry?.path();
        match held(&path) {
            Ok(true) => return Ok(true),
            Ok(false) => {}
            Err(error) => skipped(&path, &error),
        }
    }
    Ok(false)
}

/// Whether the marker file at `path` is held, from one open for reading that shares everything and does not follow a
/// link: a sharing or lock violation is held; a file that opens is dead, and is removed; one already gone is not held.
fn held(path: &Path) -> io::Result<bool> {
    let opened = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path);
    match opened {
        Ok(file) => {
            drop(file);
            match fs::remove_file(path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
                _ => Ok(false),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) if matches!(error.raw_os_error(), Some(SHARING_VIOLATION | LOCK_VIOLATION)) => Ok(true),
        Err(error) => Err(error),
    }
}

/// What `line`, compared as a whole line, says of the drain; `None` for any other line, a malformed one included.
pub(crate) fn line_of(line: &str) -> Option<DrainLine> {
    if let Some(place) = line.strip_prefix(PLACE) {
        return match place {
            "none" => Some(DrainLine::Place(None)),
            digits => whole(digits).map(|place| DrainLine::Place(Some(place))),
        };
    }
    let barrier = line.strip_prefix(BARRIER)?;
    if barrier == "none" {
        return Some(DrainLine::Barrier(None));
    }
    let (raised, lapses) = barrier.split_once(' ')?;
    Some(DrainLine::Barrier(Some((whole(raised)?, whole(lapses)?))))
}

/// `digits` as a whole number, when they are one or more ASCII digits and nothing else.
fn whole<T: std::str::FromStr>(digits: &str) -> Option<T> {
    (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make, hold or remove its marker files must fail loudly")]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use gate_state::v1;

    use super::{Drain, DrainLine, Marker, any_marker_stands, line_of, marker_stands, said_in};

    /// [`marker_stands`] of a marker folder whose entries are all plain files, which the probe skips none of.
    fn stands(root: &Path, id: Option<&str>) -> bool {
        let mut skipped: Vec<PathBuf> = Vec::new();
        let stood = marker_stands(root, id, &mut |path, _| skipped.push(path.to_owned())).expect("the folder is probed");
        assert_eq!(skipped, Vec::<PathBuf>::new(), "a probe of plain marker files skips none");
        stood
    }

    /// [`any_marker_stands`] of marker folders whose entries are all plain files, which the probe skips none of.
    fn any_stands(root: &Path) -> bool {
        let mut skipped: Vec<PathBuf> = Vec::new();
        let stood = any_marker_stands(root, &mut |path, _| skipped.push(path.to_owned())).expect("the folders are probed");
        assert_eq!(skipped, Vec::<PathBuf>::new(), "a probe of plain marker files skips none");
        stood
    }

    /// A fresh state root of the test `name`.
    fn state(name: &str) -> PathBuf {
        let root = crate::test_support::temp_dir().join(format!("gate-drain-{name}-{}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root).expect("an earlier root can be removed");
        }
        root
    }

    #[test]
    fn a_dead_marker_file_is_removed_by_the_probe() {
        let root = state("dead");
        assert!(!stands(&root, None), "no folder, no marker");
        let folder = v1::drain_marker_dir(&root, None);
        fs::create_dir_all(&folder).expect("the marker folder can be made");
        let dead = folder.join("4-0123456789abcdef0123456789abcdef");
        fs::write(&dead, "").expect("a dead marker can be left");

        assert!(!stands(&root, None), "a file that opens is no marker");
        assert!(!dead.exists(), "the probe removed the dead marker");

        let held = Marker::hold(&root, None, "0123456789abcdef0123456789abcdef").expect("a marker can be held");
        assert!(stands(&root, None), "a held marker stands");
        assert!(any_stands(&root));
        assert!(!stands(&root, Some("1f0efc62")), "for its own builder alone");
        let path = held.path().to_owned();
        held.end().expect("the marker is let go");
        assert!(!path.exists(), "an ended marker leaves no file");
        assert!(!any_stands(&root));
        fs::remove_dir_all(&root).expect("the root can be removed");
    }

    #[test]
    fn a_folder_among_the_markers_is_skipped_and_a_live_marker_beside_it_still_stands() {
        let root = state("skipped");
        let folder = v1::drain_marker_dir(&root, None);
        // Named to be listed before the marker, whose name starts with the pid.
        fs::create_dir_all(folder.join("0")).expect("a folder among the markers can be made");
        let held = Marker::hold(&root, None, "0123456789abcdef0123456789abcdef").expect("a marker can be held");

        let mut skipped = Vec::new();
        let stood = marker_stands(&root, None, &mut |path, _| skipped.push(path.to_owned())).expect("the folder is probed");
        assert!(stood, "the live marker beside the folder stands");
        assert_eq!(skipped, [folder.join("0")], "the folder is skipped");
        let mut any_skipped = Vec::new();
        let any = any_marker_stands(&root, &mut |path, _| any_skipped.push(path.to_owned())).expect("the folders are probed");
        assert!(any, "for any builder too");
        assert_eq!(any_skipped, [folder.join("0")]);
        held.end().expect("the marker is let go");
        fs::remove_dir_all(&root).expect("the root can be removed");
    }

    #[test]
    fn the_drain_lines_are_read_from_among_others() {
        assert_eq!(said_in(["gate: remote version 1", "other"]), None);
        let drain = said_in(["gate-remote-refused: n drain", "gate: drain place none", "gate: drain barrier 100 1900"]);
        assert_eq!(
            drain,
            Some(Drain {
                place: None,
                raised: 100,
                lapses: 1900
            })
        );
        assert_eq!(drain.and_then(Drain::times).as_deref(), Some("raised 00:01 UTC, lapses 00:31 UTC"));
        assert_eq!(Drain::default().times(), None, "no barrier, no times");
    }

    // The lines below are the box's own, as `boxside::drain::refused_lines` says them.

    #[test]
    fn a_place_line_names_the_place_in_line() {
        assert_eq!(line_of("gate: drain place 2"), Some(DrainLine::Place(Some(2))));
        assert_eq!(line_of("gate: drain place 2 "), None, "a padded line is no drain line");
        assert_eq!(line_of("gate: drain place -2"), None);
    }

    #[test]
    fn a_place_line_of_none_names_no_place() {
        assert_eq!(line_of("gate: drain place none"), Some(DrainLine::Place(None)));
        assert_eq!(line_of("gate: drain place "), None);
    }

    #[test]
    fn a_barrier_line_names_its_raise_and_lapse_times() {
        assert_eq!(line_of("gate: drain barrier 100 1900"), Some(DrainLine::Barrier(Some((100, 1900)))));
        assert_eq!(line_of("gate: drain barrier 100"), None);
        assert_eq!(line_of("gate: drain barrier lapsed 1900"), None, "a status line, which no refusal says");
    }

    #[test]
    fn a_barrier_line_of_none_names_no_barrier() {
        assert_eq!(line_of("gate: drain barrier none"), Some(DrainLine::Barrier(None)));
        let drain = Drain::default().with(DrainLine::Place(Some(1))).with(DrainLine::Barrier(None));
        assert_eq!(
            drain,
            Drain {
                place: Some(1),
                raised: 0,
                lapses: 0
            }
        );
        let standing = drain.with(DrainLine::Barrier(Some((100, 1900))));
        assert_eq!((standing.place, standing.raised, standing.lapses), (Some(1), 100, 1900));
    }
}
