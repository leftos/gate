//! The lines the Unix box says and the pure rules behind them: each line byte for byte as the Windows box says it where
//! the PC parses it, and the judgements (the work root's trust, the warm record, the layout record, the job record)
//! as functions of facts the verbs read, so they are tested on every target.

use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::time::Duration;

use gate_wire::{PathForm, SHA256_DIGITS, build_version, escaped, is_lower_hex, work_root};

/// The line saying the work folder's tree is new, which the client raises a run's ceiling for.
pub const NEW_WORK_TREE: &str = "gate: remote new work tree";

/// The line a preflight with a `Pair` says: this builder lays out no pair, and the run refuses one.
pub const PAIR_NOT_BUILT: &str = "gate: remote pair layout is not built on this builder";

/// The line a payload whose `Needs` is not an object of texts gets; nothing is probed.
pub const NEEDS_REFUSED: &str = "gate: box payload Needs is not an object of texts; probing nothing";

/// What the preflight says on stderr, before why, when another account may write in the builder's work root, as the
/// Windows box says it; the client reads it, with status [`ROOT_STATUS`], as a `root` fallback.
pub const ROOT_UNSAFE: &str = "gate: the builder's work root is not locked down: ";

/// The status of a preflight that refused its work root with [`ROOT_UNSAFE`].
pub const ROOT_STATUS: u8 = 1;

/// The status of a usage error: a request or payload the box cannot act on.
pub const USAGE_STATUS: u8 = 2;

/// The status of a step that failed with no status of its own.
pub const FAILED_STATUS: u8 = 1;

/// The layout record's text for a work folder holding one repo's tree.
pub const LAYOUT_SINGLE: &str = "single";

/// `gate: remote version <v>`, the version as `gate --version` says it after `gate `.
#[must_use]
pub fn version_line() -> String {
    format!("gate: remote version {}", build_version())
}

/// `gate: remote warm <hash>`, or `gate: remote warm none` without one.
#[must_use]
pub fn warm_line(hash: Option<&str>) -> String {
    format!("gate: remote warm {}", hash.unwrap_or("none"))
}

/// `gate: remote created <bare>`, said before the bare repository is made, the path escaped.
#[must_use]
pub fn created_line(bare: &str) -> String {
    format!("gate: remote created {}", escaped(bare))
}

/// The line refusing the work root for `why`, escaped ([`ROOT_UNSAFE`]), naming the macOS installer step that locks it
/// down.
#[must_use]
pub fn root_unsafe_line(why: &str) -> String {
    format!(
        "{ROOT_UNSAFE}{}; run the installer's \"Gate work root\" step (install-macos.sh --only \"Gate work root\")",
        escaped(why)
    )
}

/// What the login shell prints on each side of its `PATH`, so the box can tell it from what the login profile prints.
pub const PATH_MARKER: &str = "__GATE_PATH__";

/// The `PATH` in the login shell's output `stdout`: the text between the last two [`PATH_MARKER`]s.
///
/// # Errors
///
/// Why there is none, after the shell's name: no two markers, an empty `PATH`, or one that is not UTF-8.
pub fn marked_path(stdout: &[u8]) -> Result<String, String> {
    let text = String::from_utf8_lossy(stdout);
    let marked = text
        .rfind(PATH_MARKER)
        .and_then(|end| text[..end].rfind(PATH_MARKER).map(|start| &text[start + PATH_MARKER.len()..end]));
    match marked {
        None => Err("said no marked PATH".to_owned()),
        Some("") => Err("said an empty PATH".to_owned()),
        Some(path) if path.contains('\u{FFFD}') => Err("said a PATH that is not UTF-8".to_owned()),
        Some(path) => Ok(path.to_owned()),
    }
}

/// The note said when the login shell's `PATH` cannot be read, for `why`.
#[must_use]
pub fn login_path_line(why: &str) -> String {
    format!("gate: remote path: could not read the login shell's PATH: {why}; using the ssh PATH")
}

/// The boot line for a boot time `seconds` and `micros` after the Unix epoch: `gate: remote boot <time>`, the time in
/// .NET's round-trip `o` format of a UTC time, seven fractional digits and a `Z`, as the Windows box formats it.
///
/// # Errors
///
/// Why the time is no time: `micros` outside a second.
pub fn boot_line(seconds: i64, micros: i64) -> Result<String, String> {
    if !(0..1_000_000).contains(&micros) {
        return Err(format!("the boot time's microseconds {micros} are not within a second"));
    }
    let days = seconds.div_euclid(86_400);
    let of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    Ok(format!(
        "gate: remote boot {year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:07}Z",
        of_day / 3600,
        of_day % 3600 / 60,
        of_day % 60,
        micros * 10
    ))
}

/// The proleptic Gregorian year, month and day of the day `days` after 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let of_era = shifted.rem_euclid(146_097);
    let year_of_era = (of_era - of_era / 1460 + of_era / 36_524 - of_era / 146_096) / 365;
    let day_of_year = of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// `gate: remote could not read the boot time: <message>`.
#[must_use]
pub fn boot_failure(message: &str) -> String {
    format!("gate: remote could not read the boot time: {message}")
}

/// The SDK list of `dotnet --list-sdks`'s output `listing`: the first word of each non-empty line, joined by `,`;
/// `none` when there is none.
#[must_use]
pub fn sdk_list(listing: &str) -> String {
    let versions: Vec<&str> = listing.lines().filter_map(|line| line.split_whitespace().next()).collect();
    if versions.is_empty() { "none".to_owned() } else { versions.join(",") }
}

/// `gate: remote sdks <list>`, the list escaped, as `dotnet` wrote it.
#[must_use]
pub fn sdks_line(list: &str) -> String {
    format!("gate: remote sdks {}", escaped(list))
}

/// Whether the environment variable `name` is git's own, `GIT_` in any ASCII case: none is passed on to a git step,
/// so no inherited one points git at another repository, config, template or program.
#[must_use]
pub fn is_git_variable(name: &OsStr) -> bool {
    name.as_encoded_bytes()
        .get(..GIT_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(GIT_PREFIX))
}

/// What every git variable's name starts with, in some ASCII case.
const GIT_PREFIX: &[u8] = b"GIT_";

/// `gate: remote could not read the SDK list: <message>`.
#[must_use]
pub fn sdks_failure(message: &str) -> String {
    format!("gate: remote could not read the SDK list: {message}")
}

/// What a probe found of one tool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probed {
    /// It ran, and its version reads as this text.
    Version(String),
    /// It ran, and no version reads in what it wrote, or it exited non-zero.
    Present,
    /// It is not found.
    Missing,
    /// It was not probed to an answer, for this reason.
    Skipped(String),
}

/// What a probe whose program ran to its exit found: present when it exited non-zero (`succeeded` false) or its first
/// line holds no version ([`gate_wire::version_of`]); else that version.
#[must_use]
pub fn probed(succeeded: bool, stdout: &[u8], stderr: &[u8]) -> Probed {
    if !succeeded {
        return Probed::Present;
    }
    gate_wire::version_of(&String::from_utf8_lossy(stdout), &String::from_utf8_lossy(stderr)).map_or(Probed::Present, Probed::Version)
}

/// Why a probe cut by its deadline `limit` was skipped: `no answer in <n> s`, or `<n> ms` when the limit is not whole
/// seconds.
#[must_use]
pub fn no_answer(limit: Duration) -> String {
    if limit.subsec_millis() == 0 {
        format!("no answer in {} s", limit.as_secs())
    } else {
        format!("no answer in {} ms", limit.as_millis())
    }
}

/// The line the preflight says for the tool `name`, as its probe found it: `gate: remote tool <name> <version>`,
/// `present`, `missing` or `skipped: <why>`, the name and the reason escaped, as either may come from outside.
#[must_use]
pub fn tool_line(name: &str, probed: &Probed) -> String {
    let name = escaped(name);
    match probed {
        Probed::Version(version) => format!("gate: remote tool {name} {version}"),
        Probed::Present => format!("gate: remote tool {name} present"),
        Probed::Missing => format!("gate: remote tool {name} missing"),
        Probed::Skipped(why) => format!("gate: remote tool {name} skipped: {}", escaped(why)),
    }
}

/// What the box read of its work root, as itself and never through a link.
#[derive(Clone, Copy, Debug)]
pub struct RootFacts {
    /// The user id that owns it.
    pub owner: u32,
    /// The box's effective user id.
    pub euid: u32,
    /// Its mode bits.
    pub mode: u32,
    /// Whether it is a symbolic link.
    pub is_symlink: bool,
    /// Whether it is a folder.
    pub is_dir: bool,
}

/// The work root of the work folder `work`, `<root>/work/<key>` ([`gate_wire::work_root`]), and the folder holding
/// the work folders, `<root>/work`; else the usage line. A root holding a `:` is refused: the git steps set
/// `GIT_CEILING_DIRECTORIES` to the root, a list that git splits at every `:` with no escape.
///
/// # Errors
///
/// The usage line of a `work` not laid out so, or of a root holding a `:`.
pub fn work_layout(work: &str) -> Result<(String, &Path), String> {
    match (work_root(work, PathForm::Unix), Path::new(work).parent()) {
        (Some(root), Some(_)) if root.contains(':') => Err(format!(
            "gate: box payload Work must be <root>/work/<key> with no ':' in <root>, which GIT_CEILING_DIRECTORIES cannot hold: {}",
            escaped(work)
        )),
        (Some(root), Some(parent)) => Ok((root, parent)),
        _ => Err(format!(
            "gate: box payload Work must be <root>/work/<key> to lock its root down: {}",
            escaped(work)
        )),
    }
}

/// The group and other write bits of a mode.
const WRITABLE_BY_OTHERS: u32 = 0o022;

/// Why the work root `root`, as `facts` say it is, is not trusted, the box's user named `user`; `None` when it is: a
/// folder, no link, owned by the box's effective user and writable by no group or other.
#[must_use]
pub fn root_flaw(facts: RootFacts, root: &str, user: &str) -> Option<String> {
    if facts.is_symlink {
        Some(format!("{root} is a symbolic link"))
    } else if !facts.is_dir {
        Some(format!("{root} is not a folder"))
    } else if facts.owner != facts.euid || facts.mode & WRITABLE_BY_OTHERS != 0 {
        Some(format!("{root} is not owned by {user} or is writable by others"))
    } else {
        None
    }
}

/// Why a folder of the work root (`<root>/work`, a work folder, a bare repository), as `facts` say it is, is not one
/// the box uses, its user named `user`; `None` when it is a folder, no link, owned by the box's effective user.
#[must_use]
pub fn folder_flaw(facts: RootFacts, user: &str) -> Option<String> {
    if facts.is_symlink {
        Some("it is a symbolic link".to_owned())
    } else if !facts.is_dir {
        Some("it is not a folder".to_owned())
    } else if facts.owner != facts.euid {
        Some(format!("it is not owned by {user}"))
    } else {
        None
    }
}

/// `gate: remote could not create the folder <path>: <why>`, both escaped.
#[must_use]
pub fn folder_failure(path: &str, why: &str) -> String {
    format!("gate: remote could not create the folder {}: {}", escaped(path), escaped(why))
}

/// `gate: remote could not write <path>: <error>`, both escaped.
#[must_use]
pub fn write_failure(path: &str, error: &str) -> String {
    format!("gate: remote could not write {}: {}", escaped(path), escaped(error))
}

/// `gate: remote <path> was written but its folder could not be synced: <error>`, both escaped: the new text is in
/// place, its rename not yet on the drive.
#[must_use]
pub fn unsynced_failure(path: &str, error: &str) -> String {
    format!(
        "gate: remote {} was written but its folder could not be synced: {}",
        escaped(path),
        escaped(error)
    )
}

/// `gate: remote <step> failed: <why>`, the line of a git step that did not run to an exit, `why` escaped.
#[must_use]
pub fn step_unran(step: &str, why: &str) -> String {
    format!("gate: remote {step} failed: {}", escaped(why))
}

/// Why a stop could not read the job record `record`, for `error`: `cannot read <record>: <error>`, both escaped.
#[must_use]
pub fn record_unread(record: &str, error: &str) -> String {
    format!("cannot read {}: {}", escaped(record), escaped(error))
}

/// Why a stop did not signal the job whose record `record` holds no process group: `<record> names no process group`,
/// the path escaped.
#[must_use]
pub fn record_without_group(record: &str) -> String {
    format!("{} names no process group", escaped(record))
}

/// The hash a warm record holding `bytes` names: exactly 64 lower-case hex digits and nothing else, a line end
/// included, as the Windows box reads one; `None` for anything else.
#[must_use]
pub fn warm_text(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    is_lower_hex(text, SHA256_DIGITS).then(|| text.to_owned())
}

/// The layout record's text for a run of one repo (`pair` none), or for the pair of the leaf and the siblings, the
/// siblings ordered ignoring ASCII case, as the Windows box writes it.
#[must_use]
pub fn layout_text(pair: Option<(&str, Vec<&str>)>) -> String {
    let Some((leaf, mut siblings)) = pair else {
        return LAYOUT_SINGLE.to_owned();
    };
    siblings.sort_by_key(|name| name.to_ascii_lowercase());
    format!("pair {leaf} {}", siblings.join(" "))
}

/// Whether the layout record as `read` names another layout than `wanted`: a missing record is a single layout's, a
/// record is compared ignoring ASCII case, and one that cannot be read counts as another.
#[must_use]
pub fn names_another_layout(read: &io::Result<Option<String>>, wanted: &str) -> bool {
    match read {
        Ok(None) => wanted != LAYOUT_SINGLE,
        Ok(Some(text)) => !text.eq_ignore_ascii_case(wanted),
        Err(_) => true,
    }
}

/// Whether the layout record as `read` lets the preflight ready the work folder's single tree: missing, or saying a
/// single layout.
#[must_use]
pub fn readies_single(read: &io::Result<Option<String>>) -> bool {
    match read {
        Ok(None) => true,
        Ok(Some(text)) => text.eq_ignore_ascii_case(LAYOUT_SINGLE),
        Err(_) => false,
    }
}

/// What a job record, `<Work>.pgid`, says of the job a stop names.
#[derive(Debug, PartialEq, Eq)]
pub enum JobRecord {
    /// The record is another job's, or names no job: the stop finds no such job.
    Other,
    /// The record is the job's, and records this.
    Group(Recorded),
    /// The record is the job's, and its second line is no process group, or its third no session.
    Malformed,
}

/// What a job record records of its job's process group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Recorded {
    /// The process group.
    pub group: i32,
    /// The session the group's leader was in when it wrote the record; `None` for a two-line record of an older box.
    pub session: Option<i32>,
}

/// What the job record holding `bytes` says of the job `job`: the job's name, then its process group in decimal, then
/// optionally its session in decimal, each on a line, a final line end optional.
#[must_use]
pub fn job_record(bytes: &[u8], job: &str) -> JobRecord {
    match record_lines(bytes) {
        Some((named, recorded)) if named == job => recorded.map_or(JobRecord::Malformed, JobRecord::Group),
        _ => JobRecord::Other,
    }
}

/// What a job record holding `bytes` records, whatever its job, read as [`job_record`] reads it; `None` for a record
/// of any other form.
#[must_use]
pub fn recorded_group(bytes: &[u8]) -> Option<Recorded> {
    record_lines(bytes).filter(|(job, _)| !job.is_empty())?.1
}

/// A job record holding `bytes` read as its lines: the job it names, its first line, and what it records
/// ([`recorded_lines`]). `None` for bytes that are not UTF-8 or hold no line.
fn record_lines(bytes: &[u8]) -> Option<(&str, Option<Recorded>)> {
    let mut lines = std::str::from_utf8(bytes).ok()?.lines();
    let job = lines.next()?;
    Some((job, recorded_lines(&mut lines)))
}

/// The lines after a job record's first: its process group in decimal, then optionally its session in decimal, and
/// nothing after; else `None`.
fn recorded_lines(lines: &mut std::str::Lines<'_>) -> Option<Recorded> {
    let group = lines.next()?.parse::<i32>().ok()?;
    let session = match lines.next() {
        None => None,
        Some(line) => Some(line.parse::<i32>().ok()?),
    };
    lines.next().is_none().then_some(Recorded { group, session })
}

/// The text of the job record of the job `job` whose process group is `group`, made in the session `session`, as
/// [`job_record`] reads it: the job's name, the group, then the session in decimal, each on a line.
#[must_use]
pub fn job_record_text(job: &str, group: i32, session: i32) -> String {
    format!("{job}\n{group}\n{session}\n")
}

/// Refuses to signal the process group `group` when it is no run's: 1 or below (init, or every process the box may
/// signal) or the box's own `own`. Else why, as the stop line says it.
///
/// # Errors
///
/// Why the group is not signalled.
pub fn signal_guard(group: i32, own: i32) -> Result<(), String> {
    if group <= 1 {
        Err(format!("its recorded process group {group} is not a run's"))
    } else if group == own {
        Err(format!("its recorded process group {group} is the box's own"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::io;
    use std::time::Duration;

    use super::{
        JobRecord, PATH_MARKER, Probed, Recorded, RootFacts, boot_line, created_line, folder_failure, folder_flaw, is_git_variable, job_record,
        job_record_text, layout_text, marked_path, names_another_layout, no_answer, probed, readies_single, record_unread, record_without_group,
        recorded_group, root_flaw, root_unsafe_line, sdk_list, sdks_line, signal_guard, step_unran, tool_line, version_line, warm_text, work_layout,
        write_failure,
    };

    #[test]
    fn a_recorded_group_is_read_whatever_job_the_record_names() {
        assert_eq!(
            recorded_group(job_record_text("gate-remote-k-0123456789ab", 4242, 99).as_bytes()),
            Some(Recorded {
                group: 4242,
                session: Some(99)
            })
        );
        assert_eq!(
            recorded_group(b"another-job\n17"),
            Some(Recorded { group: 17, session: None }),
            "a two-line record of an older box, a final line end optional"
        );
        for malformed in [
            &b""[..],
            b"job\n",
            b"job\nx\n",
            b"\n17\n",
            b"job\n17\nmore\n",
            b"job\n17\n5\n6\n",
            &[0xff, b'\n', b'1'],
        ] {
            assert_eq!(recorded_group(malformed), None, "{malformed:?}");
        }
    }

    #[test]
    fn a_work_root_holding_a_colon_is_a_usage_error() {
        let refused = |shown: &str| {
            Err(format!(
                "gate: box payload Work must be <root>/work/<key> with no ':' in <root>, which GIT_CEILING_DIRECTORIES cannot hold: {shown}"
            ))
        };
        assert_eq!(work_layout("/Volumes/Build:1/work/k"), refused("/Volumes/Build:1/work/k"));
        assert_eq!(work_layout("/g/a:b\n/work/k"), refused(r"/g/a:b\n/work/k"), "the path is escaped");
        assert_eq!(
            work_layout("/g/work/k").map(|(root, parent)| (root, parent.to_owned())),
            Ok(("/g".to_owned(), std::path::PathBuf::from("/g/work")))
        );
    }

    #[test]
    fn the_boot_time_has_seven_fractional_digits_and_a_z() {
        assert_eq!(boot_line(0, 0).as_deref(), Ok("gate: remote boot 1970-01-01T00:00:00.0000000Z"));
        assert_eq!(
            boot_line(1_000_000_000, 123_456).as_deref(),
            Ok("gate: remote boot 2001-09-09T01:46:40.1234560Z")
        );
        assert_eq!(
            boot_line(951_782_400, 999_999).as_deref(),
            Ok("gate: remote boot 2000-02-29T00:00:00.9999990Z"),
            "a leap day"
        );
        assert_eq!(
            boot_line(-1, 0).as_deref(),
            Ok("gate: remote boot 1969-12-31T23:59:59.0000000Z"),
            "before the epoch"
        );
        assert!(boot_line(0, 1_000_000).is_err(), "a microsecond count past the second");
        assert!(boot_line(0, -1).is_err());
    }

    #[test]
    fn the_version_line_names_the_build_version() {
        assert_eq!(version_line(), format!("gate: remote version {}", gate_wire::build_version()));
    }

    #[test]
    fn the_sdk_list_is_the_first_word_of_each_line_joined() {
        assert_eq!(
            sdk_list("8.0.415 [/usr/local/share/dotnet/sdk]\n10.0.100 [/usr/local/share/dotnet/sdk]\n"),
            "8.0.415,10.0.100"
        );
        assert_eq!(sdk_list(""), "none");
        assert_eq!(sdk_list("\n  \n"), "none", "blank lines name nothing");
    }

    #[test]
    fn a_tool_line_says_what_the_probe_found_escaped() {
        assert_eq!(tool_line("rust", &Probed::Version("1.99.0".to_owned())), "gate: remote tool rust 1.99.0");
        assert_eq!(tool_line("node", &Probed::Present), "gate: remote tool node present");
        assert_eq!(tool_line("godot", &Probed::Missing), "gate: remote tool godot missing");
        assert_eq!(
            tool_line("x\u{1b}", &Probed::Skipped("unknown\ntool".to_owned())),
            r"gate: remote tool x\u{1b} skipped: unknown\ntool"
        );
    }

    #[test]
    fn every_git_variable_is_scrubbed_in_any_case_and_nothing_else() {
        let environment = [
            "GIT_CONFIG",
            "GIT_TEMPLATE_DIR",
            "GIT_EXEC_PATH",
            "git_dir",
            "Git_Ceiling_Directories",
            "PATH",
            "HOME",
            "GIT",
            "LOGIT_X",
        ];
        let kept: Vec<&str> = environment.into_iter().filter(|name| !is_git_variable(OsStr::new(name))).collect();
        assert_eq!(kept, ["PATH", "HOME", "GIT", "LOGIT_X"]);
    }

    #[test]
    fn the_created_and_sdks_lines_escape_outside_text() {
        assert_eq!(created_line("/r/a\nb.git"), r"gate: remote created /r/a\nb.git");
        assert_eq!(sdks_line("8.0.415,9\u{1b}"), r"gate: remote sdks 8.0.415,9\u{1b}");
        assert_eq!(
            write_failure("/w/k\n/.git/objects/info/alternates", "Is a\u{1b} FIFO"),
            r"gate: remote could not write /w/k\n/.git/objects/info/alternates: Is a\u{1b} FIFO"
        );
        assert_eq!(step_unran("git init", "no such\nfile"), r"gate: remote git init failed: no such\nfile");
        assert_eq!(record_unread("/w/k\n.pgid", "denied\u{1b}"), r"cannot read /w/k\n.pgid: denied\u{1b}");
        assert_eq!(record_without_group("/w/k\n.pgid"), r"/w/k\n.pgid names no process group");
    }

    #[test]
    fn a_probe_that_ran_reads_its_version_or_is_present() {
        assert_eq!(
            probed(true, b"rustc 1.99.0 (abc 2026-09-28)\n", b""),
            Probed::Version("1.99.0".to_owned())
        );
        assert_eq!(
            probed(true, b"git-lfs/3.8.0 (GitHub; darwin arm64; go 1.24.4)\n", b""),
            Probed::Version("3.8.0".to_owned())
        );
        assert_eq!(
            probed(true, b"", b"PowerShell 7.6.6\n"),
            Probed::Version("7.6.6".to_owned()),
            "stderr when stdout is blank"
        );
        assert_eq!(probed(true, b"no version\n", b""), Probed::Present);
        assert_eq!(probed(false, b"v24.19.0\n", b""), Probed::Present, "a non-zero exit is present");
    }

    #[test]
    fn no_answer_names_the_limit_in_seconds_or_milliseconds() {
        assert_eq!(no_answer(Duration::from_secs(10)), "no answer in 10 s");
        assert_eq!(no_answer(Duration::from_millis(250)), "no answer in 250 ms");
    }

    fn facts(owner: u32, mode: u32, is_symlink: bool, is_dir: bool) -> RootFacts {
        RootFacts {
            owner,
            euid: 501,
            mode,
            is_symlink,
            is_dir,
        }
    }

    #[test]
    fn a_root_owned_by_the_box_user_and_writable_by_no_one_else_is_trusted() {
        for mode in [0o700, 0o755, 0o40_755, 0o711] {
            assert_eq!(root_flaw(facts(501, mode, false, true), "/g", "b"), None, "{mode:o}");
        }
    }

    #[test]
    fn a_root_another_account_may_write_in_is_refused() {
        let refused = Some("/g is not owned by b or is writable by others".to_owned());
        for mode in [0o777, 0o775, 0o757, 0o720, 0o702] {
            assert_eq!(root_flaw(facts(501, mode, false, true), "/g", "b"), refused, "{mode:o}");
        }
        assert_eq!(root_flaw(facts(0, 0o700, false, true), "/g", "b"), refused, "owned by root");
        assert_eq!(root_flaw(facts(502, 0o700, false, true), "/g", "b"), refused, "owned by another user");
    }

    #[test]
    fn a_root_that_is_a_link_or_no_folder_is_refused() {
        assert_eq!(
            root_flaw(facts(501, 0o755, true, false), "/g", "b"),
            Some("/g is a symbolic link".to_owned())
        );
        assert_eq!(
            root_flaw(facts(501, 0o644, false, false), "/g", "b"),
            Some("/g is not a folder".to_owned())
        );
    }

    #[test]
    fn a_folder_in_the_root_must_be_the_box_user_s_own_plain_folder() {
        assert_eq!(folder_flaw(facts(501, 0o40_755, false, true), "b"), None);
        assert_eq!(folder_flaw(facts(501, 0o777, true, false), "b"), Some("it is a symbolic link".to_owned()));
        assert_eq!(folder_flaw(facts(501, 0o644, false, false), "b"), Some("it is not a folder".to_owned()));
        assert_eq!(folder_flaw(facts(0, 0o700, false, true), "b"), Some("it is not owned by b".to_owned()));
        assert_eq!(
            folder_failure("/g/work/k\n", "it is a symbolic link"),
            r"gate: remote could not create the folder /g/work/k\n: it is a symbolic link"
        );
    }

    #[test]
    fn the_root_line_escapes_its_why_and_names_the_mac_installer_step() {
        assert_eq!(
            root_unsafe_line("/g\n is not owned by b or is writable by others"),
            concat!(
                r"gate: the builder's work root is not locked down: /g\n is not owned by b or is writable by others; ",
                r#"run the installer's "Gate work root" step (install-macos.sh --only "Gate work root")"#
            )
        );
    }

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn a_warm_record_is_exactly_64_lower_case_hex_digits() {
        assert_eq!(warm_text(HASH.as_bytes()), Some(HASH.to_owned()));
        for bad in [
            format!("{HASH}\n"),
            format!("{HASH}\r\n"),
            HASH.to_uppercase(),
            HASH[1..].to_owned(),
            format!("{HASH}0"),
            format!("{HASH}\n\n"),
            format!(" {HASH}"),
            String::new(),
        ] {
            assert_eq!(warm_text(bad.as_bytes()), None, "{bad:?}");
        }
        assert_eq!(warm_text(&[0xff; 64]), None, "not UTF-8");
    }

    #[test]
    fn the_layout_text_orders_the_siblings_ignoring_case() {
        assert_eq!(layout_text(None), "single");
        assert_eq!(layout_text(Some(("app", vec!["lib", "Core", "b"]))), "pair app b Core lib");
    }

    #[test]
    fn a_layout_record_names_another_layout_unless_it_is_the_wanted_one() {
        assert!(!names_another_layout(&Ok(None), "single"), "no record is a single layout's");
        assert!(names_another_layout(&Ok(None), "pair app lib"), "a pair's first run");
        assert!(!names_another_layout(&Ok(Some("SINGLE".to_owned())), "single"), "any case");
        assert!(names_another_layout(&Ok(Some("pair app lib".to_owned())), "single"));
        assert!(names_another_layout(&Err(io::Error::other("unreadable")), "single"));
    }

    #[test]
    fn only_a_missing_or_single_layout_record_readies_the_tree() {
        assert!(readies_single(&Ok(None)));
        assert!(readies_single(&Ok(Some("Single".to_owned()))));
        assert!(!readies_single(&Ok(Some("pair app lib".to_owned()))));
        assert!(!readies_single(&Ok(Some("changing".to_owned()))));
        assert!(!readies_single(&Err(io::Error::other("unreadable"))));
    }

    const JOB: &str = "gate-remote-repo-0123abcd-0123456789ab";

    /// What a two-line record of `group` records.
    fn two_line(group: i32) -> JobRecord {
        JobRecord::Group(Recorded { group, session: None })
    }

    #[test]
    fn a_job_record_names_its_job_then_its_process_group() {
        assert_eq!(job_record(format!("{JOB}\n4242\n").as_bytes(), JOB), two_line(4242));
        assert_eq!(
            job_record(format!("{JOB}\r\n4242").as_bytes(), JOB),
            two_line(4242),
            "CRLF, no final line end"
        );
        assert_eq!(
            job_record(format!("{JOB}\n4242\n77").as_bytes(), JOB),
            JobRecord::Group(Recorded {
                group: 4242,
                session: Some(77)
            }),
            "then its session"
        );
    }

    #[test]
    fn a_job_record_s_text_reads_back_as_its_job_group_and_session() {
        assert_eq!(job_record_text(JOB, 4242, 77), format!("{JOB}\n4242\n77\n"));
        assert_eq!(
            job_record(job_record_text(JOB, 4242, 77).as_bytes(), JOB),
            JobRecord::Group(Recorded {
                group: 4242,
                session: Some(77)
            })
        );
        assert_eq!(
            job_record(job_record_text(JOB, -7, 77).as_bytes(), JOB),
            JobRecord::Group(Recorded {
                group: -7,
                session: Some(77)
            }),
            "read back, refused by the guard"
        );
    }

    #[test]
    fn a_record_of_another_job_is_no_such_job() {
        assert_eq!(job_record(b"gate-remote-repo-0123abcd-ffffffffffff\n4242\n", JOB), JobRecord::Other);
        assert_eq!(job_record(b"", JOB), JobRecord::Other);
        assert_eq!(job_record(&[0xff, b'\n', b'1'], JOB), JobRecord::Other, "not UTF-8");
    }

    #[test]
    fn a_record_of_the_job_without_a_process_group_is_malformed() {
        for bad in [
            format!("{JOB}\n"),
            format!("{JOB}\nabc\n"),
            format!("{JOB}\n42\nmore\n"),
            format!("{JOB}\n42\n7\n8\n"),
            format!("{JOB}\n 42\n"),
        ] {
            assert_eq!(job_record(bad.as_bytes(), JOB), JobRecord::Malformed, "{bad:?}");
        }
    }

    #[test]
    fn init_any_group_below_it_and_the_box_s_own_group_are_never_signalled() {
        for group in [1, 0, -1, i32::MIN] {
            assert_eq!(
                signal_guard(group, 900),
                Err(format!("its recorded process group {group} is not a run's")),
                "{group}"
            );
        }
        assert_eq!(signal_guard(900, 900), Err("its recorded process group 900 is the box's own".to_owned()));
        assert_eq!(signal_guard(2, 900), Ok(()));
    }

    #[test]
    fn the_path_is_the_text_between_the_markers_whatever_the_profile_prints() {
        let marked = |path: &str| format!("\n{PATH_MARKER}{path}{PATH_MARKER}\n");
        assert_eq!(marked_path(marked("/usr/bin:/bin").as_bytes()), Ok("/usr/bin:/bin".to_owned()));
        let noisy = format!("Last login: today\nwelcome {PATH_MARKER}\n{}", marked("/opt/homebrew/bin:/usr/bin"));
        assert_eq!(marked_path(noisy.as_bytes()), Ok("/opt/homebrew/bin:/usr/bin".to_owned()), "noise before");
        assert_eq!(marked_path(b"/usr/bin:/bin"), Err("said no marked PATH".to_owned()));
        assert_eq!(
            marked_path(format!("{PATH_MARKER}/usr/bin").as_bytes()),
            Err("said no marked PATH".to_owned())
        );
        assert_eq!(marked_path(marked("").as_bytes()), Err("said an empty PATH".to_owned()));
        let mut bad = format!("\n{PATH_MARKER}/u").into_bytes();
        bad.push(0xff);
        bad.extend_from_slice(format!("{PATH_MARKER}\n").as_bytes());
        assert_eq!(marked_path(&bad), Err("said a PATH that is not UTF-8".to_owned()));
    }
}
