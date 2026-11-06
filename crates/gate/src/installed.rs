//! The installed gate: where `tools/install.ps1` puts it under `LOCALAPPDATA`, and the version its `--version` says,
//! which `gate push` sends to the builders and a stale run hands its request to; and the push-pending marker beside
//! it, which the install writes, `gate push` clears and a routed run refused `version` waits on.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};

use regex::Regex;

use crate::push::{Call, End, called};
use crate::resolve::Environment;

/// How long the installed exe's `--version` may take here.
pub(crate) const VERSION_BOUND: Duration = Duration::from_secs(30);

/// `gate --version`'s line, matched in any letter case as PowerShell's `-match` does: the version is what follows
/// `gate ` and its white space, trailing white space left out.
static VERSION: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"(?i)^gate\s+(.+?)\s*$").ok());

/// The installed gate, `<LOCALAPPDATA>\gate\bin\gate.exe`, as `gate push` reads it; `None` when `LOCALAPPDATA` is
/// unset or no absolute folder. The file may be missing.
pub(crate) fn installed_gate(environment: &Environment) -> Option<PathBuf> {
    let folder = Path::new(environment.local_app_data.as_deref()?);
    folder.is_absolute().then(|| folder.join("gate").join("bin").join("gate.exe"))
}

/// The push-pending marker's name beside the installed gate: `tools/install.ps1` writes it once it installed a gate or
/// a desktop helper that the builders do not have yet, naming the installed gate's version, and `gate push` removes it.
pub(crate) const PUSH_PENDING_FILE: &str = "push-pending";
/// How long a push-pending marker counts as fresh after its last write: also the longest a routed run waits for it.
pub(crate) const PUSH_PENDING_LIFE: Duration = Duration::from_secs(600);
/// How far ahead of this PC's clock a push-pending marker's last write may be and still count as fresh.
pub(crate) const PUSH_PENDING_SKEW: Duration = Duration::from_secs(5);
/// The most bytes a push-pending marker holds; one longer is malformed.
const PUSH_PENDING_CAP: u64 = 4096;

/// The push-pending marker, [`PUSH_PENDING_FILE`] in the installed gate's folder ([`installed_gate`]); `None` when
/// there is no such folder. The file may be missing.
pub(crate) fn push_pending_path(environment: &Environment) -> Option<PathBuf> {
    installed_gate(environment).map(|exe| exe.with_file_name(PUSH_PENDING_FILE))
}

/// A push-pending marker as read: the version it names and its last write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PushPending {
    pub(crate) version: String,
    pub(crate) written: SystemTime,
}

/// The push-pending marker at `path`, read through one handle, its last write and its text from that same handle, so a
/// marker replaced between the two is never read as one: its text, one trailing `\n` or `\r\n` left out, UTF-8 with
/// no byte-order mark, one line and not empty, at most [`PUSH_PENDING_CAP`] bytes. `Ok(None)` when there is none.
///
/// # Errors
///
/// The line saying the marker is malformed, or why it could not be read.
pub(crate) fn read_push_pending(path: &Path) -> Result<Option<PushPending>, String> {
    let cannot = |error: &io::Error| format!("cannot read the push-pending marker {}: {error}", path.display());
    let malformed = || format!("the push-pending marker {} is malformed", path.display());
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(cannot(&error)),
    };
    let written = file.metadata().and_then(|metadata| metadata.modified()).map_err(|error| cannot(&error))?;
    let mut bytes = Vec::new();
    (&file)
        .take(PUSH_PENDING_CAP + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| cannot(&error))?;
    // A length no `u64` holds is over the cap too.
    let over = match u64::try_from(bytes.len()) {
        Ok(read) => read > PUSH_PENDING_CAP,
        Err(_) => true,
    };
    if over {
        return Err(malformed());
    }
    let text = String::from_utf8(bytes).map_err(|_| malformed())?;
    let line = text.strip_suffix("\r\n").or_else(|| text.strip_suffix('\n')).unwrap_or(&text);
    if line.is_empty() || line.starts_with('\u{feff}') || line.contains(['\r', '\n']) {
        return Err(malformed());
    }
    Ok(Some(PushPending {
        version: line.to_owned(),
        written,
    }))
}

/// Whether the push-pending `marker` is fresh at `now`: written no more than [`PUSH_PENDING_SKEW`] ahead of `now`, and
/// less than [`PUSH_PENDING_LIFE`] before it, a time ahead within the skew counting as written now.
pub(crate) fn fresh(marker: &PushPending, now: SystemTime) -> bool {
    if now.checked_add(PUSH_PENDING_SKEW).is_none_or(|latest| marker.written > latest) {
        return false;
    }
    now.duration_since(marker.written).unwrap_or(Duration::ZERO) < PUSH_PENDING_LIFE
}

/// The installed exe's version, from its `--version`, a call bounded as a builder's is, at [`VERSION_BOUND`], with
/// `variables` as its whole environment; else the line saying it could not be read.
pub(crate) fn local_version(exe: &Path, variables: &[(OsString, OsString)]) -> Result<String, String> {
    let heard = called(&Call {
        program: exe,
        args: vec![OsString::from("--version")],
        input: b"",
        variables,
        bound: VERSION_BOUND,
    });
    let version = match heard.end {
        End::Exited(_) => heard.first.as_deref().and_then(version_of),
        End::TimedOut | End::Failed(_) => None,
    };
    version.ok_or_else(|| format!("could not read the version of {}", exe.display()))
}

/// What `--version`'s output says after `gate `, as `tools/install.ps1`'s `Get-GateVersion` reads it: its first line
/// matched in any letter case, `None` when that line is no `gate <version>`.
pub(crate) fn version_of(output: &str) -> Option<String> {
    let first = output.lines().next()?;
    VERSION
        .as_ref()?
        .captures(first)
        .and_then(|captures| captures.get(1))
        .map(|version| version.as_str().to_owned())
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot write or read its files must fail loudly")]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    use super::{PUSH_PENDING_LIFE, PUSH_PENDING_SKEW, PushPending, fresh, installed_gate, read_push_pending};
    use crate::resolve::Environment;

    /// A marker file of this test and process holding `bytes`, in the unit tests' temp folder; its path.
    fn marker_file(test: &str, bytes: &[u8]) -> PathBuf {
        let folder = crate::test_support::temp_dir().join(format!("push-pending-{}-{test}", std::process::id()));
        fs::create_dir_all(&folder).expect("the marker's folder can be made");
        let path = folder.join("push-pending");
        fs::write(&path, bytes).expect("the marker can be written");
        path
    }

    /// A marker naming `0.1.0+abc` written `at`.
    fn marker_at(at: SystemTime) -> PushPending {
        PushPending {
            version: "0.1.0+abc".to_owned(),
            written: at,
        }
    }

    #[test]
    fn a_marker_read_strips_one_line_ending() {
        for (name, bytes) in [("lf", &b"0.1.0+abc\n"[..]), ("crlf", b"0.1.0+abc\r\n"), ("bare", b"0.1.0+abc")] {
            let path = marker_file(&format!("strips-{name}"), bytes);
            // Its last write an hour before its creation, so a read of the creation time cannot pass for it.
            let written = SystemTime::now() - Duration::from_hours(1);
            fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(written))
                .expect("the marker's last write can be set");

            let read = read_push_pending(&path);

            assert_eq!(
                read,
                Ok(Some(PushPending {
                    version: "0.1.0+abc".to_owned(),
                    written,
                })),
                "{name}"
            );
        }
    }

    #[test]
    fn a_missing_marker_reads_as_none() {
        let path = marker_file("missing", b"0.1.0+abc\n");
        fs::remove_file(&path).expect("the marker can be removed");

        assert_eq!(read_push_pending(&path), Ok(None));
    }

    #[test]
    fn a_marker_with_a_bom_two_lines_no_text_or_over_4096_bytes_is_malformed() {
        let mut long = vec![b'x'; 4097];
        long.push(b'\n');
        let cases: [(&str, &[u8]); 7] = [
            ("bom", b"\xef\xbb\xbf0.1.0+abc\n"),
            ("two-lines", b"0.1.0+abc\n0.1.0+def\n"),
            ("two-endings", b"0.1.0+abc\n\n"),
            ("lone-cr", b"0.1.0\r+abc\n"),
            ("empty", b""),
            ("ending-only", b"\r\n"),
            ("over-cap", &long),
        ];
        for (name, bytes) in cases {
            let path = marker_file(&format!("malformed-{name}"), bytes);

            let read = read_push_pending(&path);

            assert_eq!(read, Err(format!("the push-pending marker {} is malformed", path.display())), "{name}");
        }
        let path = marker_file("malformed-not-utf8", b"0.1.0+\xff\n");
        assert_eq!(
            read_push_pending(&path),
            Err(format!("the push-pending marker {} is malformed", path.display())),
            "not UTF-8"
        );
        let at_cap = marker_file("at-cap", &[b'x'; 4096]);
        assert!(matches!(read_push_pending(&at_cap), Ok(Some(_))), "4096 bytes is within the cap");
    }

    #[test]
    fn a_marker_younger_than_ten_minutes_is_fresh() {
        let now = SystemTime::now();

        assert!(fresh(&marker_at(now), now), "written now");
        assert!(fresh(&marker_at(now - Duration::from_secs(599)), now), "written 599 s ago");
    }

    #[test]
    fn a_marker_ten_minutes_old_is_stale() {
        let now = SystemTime::now();

        assert!(!fresh(&marker_at(now - PUSH_PENDING_LIFE), now), "written exactly 600 s ago");
        assert!(!fresh(&marker_at(now - Duration::from_mins(11)), now), "written 11 minutes ago");
    }

    #[test]
    fn a_marker_dated_within_the_skew_ahead_is_fresh() {
        let now = SystemTime::now();

        assert!(fresh(&marker_at(now + PUSH_PENDING_SKEW), now), "dated the whole skew ahead");
        assert!(fresh(&marker_at(now + Duration::from_secs(1)), now), "dated 1 s ahead");
    }

    #[test]
    fn a_marker_dated_past_the_skew_ahead_is_stale() {
        let now = SystemTime::now();

        assert!(
            !fresh(&marker_at(now + PUSH_PENDING_SKEW + Duration::from_millis(1)), now),
            "dated just past the skew ahead"
        );
        assert!(!fresh(&marker_at(now + Duration::from_secs(60)), now), "dated a minute ahead");
    }

    fn environment(local_app_data: Option<&str>) -> Environment {
        Environment {
            state_dir: None,
            local_app_data: local_app_data.map(OsString::from),
            git_index_file: None,
            slot_held: None,
            box_job: None,
            commit_slot: None,
            stage_blocking: Vec::new(),
            handed_over: None,
        }
    }

    #[test]
    fn the_installed_gate_is_under_local_app_data_and_none_without_it() {
        assert_eq!(
            installed_gate(&environment(Some(r"C:\Users\u\AppData\Local"))),
            Some(PathBuf::from(r"C:\Users\u\AppData\Local\gate\bin\gate.exe"))
        );
        assert_eq!(installed_gate(&environment(None)), None, "no LOCALAPPDATA, no installed gate");
        assert_eq!(
            installed_gate(&environment(Some(r"relative\folder"))),
            None,
            "a relative folder names none"
        );
    }
}
