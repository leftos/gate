//! The lines the Unix box says and the pure rules behind them: each line byte for byte as the Windows box says it where
//! the PC parses it, and the judgements (the work root's trust, the warm record, the layout record, the job record)
//! as functions of facts the verbs read, so they are tested on every target.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::Path;
use std::time::Duration;

use gate_wire::{BoxKill, PathForm, Refusal, SHA256_DIGITS, build_version, escaped, is_lower_hex, refused_line, work_root};

/// The line saying the work folder's tree is new, which the client raises a run's ceiling for.
pub const NEW_WORK_TREE: &str = "gate: remote new work tree";

/// The line a payload whose `Needs` is not an object of texts gets; nothing is probed.
pub const NEEDS_REFUSED: &str = "gate: box payload Needs is not an object of texts; probing nothing";

/// The macOS installer step that locks the builder's work root down, as the work-root refusal names it
/// ([`gate_wire::root_unsafe_line`]).
pub const WORK_ROOT_STEP: &str = "install-macos.sh --only \"Gate work root\"";

/// The status of a usage error: a request or payload the box cannot act on.
pub const USAGE_STATUS: u8 = 2;

/// The status of a step that failed with no status of its own.
pub const FAILED_STATUS: u8 = 1;

/// The layout record's text for a work folder holding one repo's tree.
pub const LAYOUT_SINGLE: &str = "single";

/// The layout record's text while a run clears the work folder for another layout, as the Windows box writes it: a
/// layout no run has, so a clear cut short is cleared again.
pub const LAYOUT_CHANGING: &str = "changing";

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

/// A marker for one print of the login shell: `__GATE_LOGIN_<16 hex>__`, the hex from the box's process id and the
/// time, so no fixed text a login profile prints can be taken for it. The login shell prints it between two NULs, then
/// its environment, each entry ended by a NUL: an environment value cannot hold a NUL, so no value can end an entry
/// early or start another.
#[must_use]
pub fn login_marker() -> String {
    // A clock before the epoch leaves the process id alone to tell one box's marker from another's.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    format!("__GATE_LOGIN_{:08x}{nanos:08x}__", std::process::id())
}

/// What the box reads of its user's login shell, from one print of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Login {
    /// Its `PATH`.
    pub path: String,
    /// Its exported `GATE_GODOT_ROOT`, the folder holding the builder's Godot installs; `None` when it is unset or
    /// empty, so Godot is missing on the builder.
    pub godot_root: Option<String>,
}

/// What the login shell's output `stdout` says in its record: the first NUL, `marker` ([`login_marker`]), NUL, then
/// its environment, each entry `NAME=value` ended by a NUL, of which the first `PATH` and the first `GATE_GODOT_ROOT`
/// are read. What the login profile prints before the record is left out, as is what follows its last NUL; marker
/// text inside a value is part of that value. The root is `None` when it is not there, empty or not UTF-8.
///
/// # Errors
///
/// Why there is no `PATH`, after the shell's name: no record, no `PATH` entry in it, an empty one, or one that is not
/// UTF-8.
pub fn marked_login(stdout: &[u8], marker: &str) -> Result<Login, String> {
    let start = [b"\0", marker.as_bytes(), b"\0"].concat();
    let Some(block) = stdout
        .windows(start.len())
        .position(|window| window == start.as_slice())
        .map(|at| &stdout[at + start.len()..])
    else {
        return Err("said no marked PATH".to_owned());
    };
    let mut entries: Vec<&[u8]> = block.split(|byte| *byte == 0).collect();
    // What follows the last NUL ends no entry: it is what the shell printed after its environment.
    entries.pop();
    let value = |name: &[u8]| entries.iter().find_map(|entry| entry.strip_prefix(name));
    let path = match value(b"PATH=").map(std::str::from_utf8) {
        None => return Err("said no marked PATH".to_owned()),
        Some(Ok("")) => return Err("said an empty PATH".to_owned()),
        Some(Ok(path)) => path.to_owned(),
        Some(Err(_)) => return Err("said a PATH that is not UTF-8".to_owned()),
    };
    let godot_root = value(b"GATE_GODOT_ROOT=")
        .and_then(|root| std::str::from_utf8(root).ok())
        .filter(|root| !root.is_empty())
        .map(str::to_owned);
    Ok(Login { path, godot_root })
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
    Ok(format!("gate: remote boot {}", round_trip(seconds, micros * 10)))
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

/// What a pair run says when its back commits were not all pushed, or were never made because the run ended before
/// them: the PC applies nothing, as the Windows box's line tells it.
pub const BACK_COMMITS_FAILED: &str = "gate: remote back commits: failed";

/// The line a pair run says once every back commit is pushed, as the Windows box builds it: `gate: remote back commits: `
/// and, for each repo of `commits`, the main one first, its name and its back commit, or `none` for a repo whose tree is
/// unchanged, comma-separated.
#[must_use]
pub fn back_commits_line(commits: &[(&str, Option<&str>)]) -> String {
    let named: Vec<String> = commits
        .iter()
        .map(|(name, commit)| format!("{name} {}", commit.unwrap_or("none")))
        .collect();
    format!("gate: remote back commits: {}", named.join(", "))
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
/// nothing after; else `None`. A run writes only digits, a group above 1 and a session above 0, so any other value,
/// a sign included, is no record of a run's.
fn recorded_lines(lines: &mut std::str::Lines<'_>) -> Option<Recorded> {
    let group = digits_above(lines.next()?, 1)?;
    let session = match lines.next() {
        None => None,
        Some(line) => Some(digits_above(line, 0)?),
    };
    lines.next().is_none().then_some(Recorded { group, session })
}

/// `line` read as a decimal number above `least`, written in digits only; else `None`.
fn digits_above(line: &str, least: i32) -> Option<i32> {
    if line.is_empty() || !line.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    line.parse::<i32>().ok().filter(|value| *value > least)
}

/// The text of the job record of the job `job` whose process group is `group`, made in the session `session`, as
/// [`job_record`] reads it: the job's name, the group, then the session in decimal, each on a line. The child writes
/// the record itself, so only the tests make its text.
#[cfg(test)]
#[must_use]
fn job_record_text(job: &str, group: i32, session: i32) -> String {
    format!("{job}\n{group}\n{session}\n")
}

/// Refuses to signal the process group `group` when it is the box's own `own`; a group of 1 or below is never read
/// from a record ([`job_record`]). Else why, as the stop line says it.
///
/// # Errors
///
/// Why the group is not signalled.
pub fn signal_guard(group: i32, own: i32) -> Result<(), String> {
    if group == own {
        Err(format!("its recorded process group {group} is the box's own"))
    } else {
        Ok(())
    }
}

/// The status of a run whose client closed the connection: the PC reads it as a lost connection.
pub const GONE_STATUS: u8 = 255;
/// The status of a run whose command could not be started.
pub const NOT_STARTED_STATUS: u8 = 127;

/// How the run's command ended, as its status says it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandEnd {
    /// It exited with this code.
    Exited(i32),
    /// A signal, this one, ended it.
    Signalled(i32),
    /// Its client is gone.
    Gone,
    /// It could not be started.
    NotStarted,
    /// The box could not see it to its end: its exit unread, its group still there, or a record not changed.
    Failed,
    /// The box killed it, for its ceiling or its backstop.
    Killed(BoxKill),
}

/// The status of a run whose command the box killed for its ceiling or its backstop, said with the kill tag, as a
/// local kill's.
pub const KILLED_STATUS: u8 = 124;

/// The status a run whose command ended as `end` exits with: the command's own code; 128 plus the signal for a command
/// a signal ended, never 124 or 255, which the PC reads as killed and as a lost connection; [`KILLED_STATUS`] for the
/// box's own kill; [`GONE_STATUS`], [`NOT_STARTED_STATUS`], or [`FAILED_STATUS`] for the box's own failure. A code or
/// signal outside a status is the box's failure.
#[must_use]
pub fn command_status(end: CommandEnd) -> u8 {
    let status = match end {
        CommandEnd::Killed(_) => Some(KILLED_STATUS),
        CommandEnd::Exited(code) => u8::try_from(code).ok(),
        // 1 to 126, so the status is 129 to 254 and never the lost connection's 255.
        CommandEnd::Signalled(signal) if (1..127).contains(&signal) => u8::try_from(128 + signal).ok(),
        CommandEnd::Signalled(_) | CommandEnd::Failed => None,
        CommandEnd::Gone => Some(GONE_STATUS),
        CommandEnd::NotStarted => Some(NOT_STARTED_STATUS),
    };
    status.unwrap_or(FAILED_STATUS)
}

/// The line a box running under Rosetta says after its `install` refusal tag: its CPU times read about 41.7 times low.
pub const ROSETTA_LINE: &str = "gate: remote gate-box runs under Rosetta on this builder, whose CPU times it cannot read: install the arm64 build";

/// What `sysctl.proc_translated` says of the box.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Translation {
    /// It runs natively.
    Native,
    /// It is taken as native, as the value could not be read: the line saying so.
    Unread(String),
    /// It runs under Rosetta: the run is refused `install`, so the PC tries the next builder.
    Rosetta,
}

/// What the read `read` of `sysctl.proc_translated` says: 1 is Rosetta; any other value, or no such name (an Intel
/// Mac), native; any other failure native too, with the line saying it could not be read.
#[must_use]
pub fn translation(read: io::Result<i32>) -> Translation {
    match read {
        Ok(1) => Translation::Rosetta,
        Ok(_) => Translation::Native,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Translation::Native,
        Err(error) => Translation::Unread(format!(
            "gate: remote could not read sysctl.proc_translated: {error}; taking the box as native"
        )),
    }
}

/// The lines a box under Rosetta refuses the run of the nonce `nonce` with: the `install` refusal tag, so the PC tries
/// the next builder and then itself, then [`ROSETTA_LINE`].
#[must_use]
pub fn rosetta_lines(nonce: &str) -> [String; 2] {
    [refused_line(nonce, Refusal::Install), ROSETTA_LINE.to_owned()]
}

/// The time `seconds` and `hundreds` of 100 ns after the Unix epoch in .NET's round-trip `o` format of a UTC time:
/// seven fractional digits and a `Z`, as the Windows box writes its times.
#[must_use]
pub fn round_trip(seconds: i64, hundreds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{hundreds:07}Z",
        of_day / 3600,
        of_day % 3600 / 60,
        of_day % 60
    )
}

/// The variable the run's command reads its job's name from, as on the Windows box (`gate`'s `environment.rs`).
pub const BOX_JOB_VARIABLE: &str = "GATE_BOX_JOB";

/// The variable the command of a run with a chosen Godot version reads the Godot install it resolved from.
pub const GODOT_PATH: &str = "GODOT_PATH";

/// The Godot of a run's command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Godot<'a> {
    /// The run chose no Godot version: its words run as written and [`GODOT_PATH`] is left as the environment has it.
    Unchosen,
    /// The program of the install of the run's chosen version: a Godot console word runs it, and [`GODOT_PATH`] names
    /// it.
    Installed(&'a str),
    /// The run's chosen version has no install: a Godot console word cannot start, and [`GODOT_PATH`] is removed.
    Missing,
}

/// What the box sets over its own environment for a run's command and its tool restore.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Along<'a> {
    /// The login shell's `PATH`, `None` to keep the box's own.
    pub path: Option<&'a str>,
    /// The run's own temp folder with a trailing `/`, the `TMPDIR`; `None` to keep the box's own.
    pub tmp: Option<&'a str>,
    /// The run's Godot, which sets or removes [`GODOT_PATH`] over the payload's `Env`.
    pub godot: Godot<'a>,
}

/// The command's environment: `start` (the box's own), its `PATH` the login shell's `along.path` and its `TMPDIR` the
/// run's folder `along.tmp` when there are, the payload's `Env` pairs `caller` set over that, each replacing its name;
/// [`GODOT_PATH`] then set to the program of `along.godot`'s install over any value it holds, or removed for a run whose
/// chosen version has none; then the build-server settings of the run `run` ([`gate_wire::build_server_settings`]) and
/// [`BOX_JOB_VARIABLE`] the job `job`, each first removing every name that differs from it only in case, so the command
/// sees one value per name, as on the Windows box.
#[must_use]
pub fn command_environment(
    start: Vec<(OsString, OsString)>,
    along: Along<'_>,
    caller: &[(String, String)],
    run: &str,
    job: &str,
) -> Vec<(OsString, OsString)> {
    let mut environment = start;
    if let Some(path) = along.path {
        set_variable(&mut environment, "PATH", path, false);
    }
    if let Some(tmp) = along.tmp {
        set_variable(&mut environment, "TMPDIR", tmp, false);
    }
    for (name, value) in caller {
        set_variable(&mut environment, name, value, false);
    }
    match along.godot {
        Godot::Unchosen => {}
        Godot::Installed(program) => set_variable(&mut environment, GODOT_PATH, program, false),
        Godot::Missing => environment.retain(|(name, _)| name.as_os_str() != OsStr::new(GODOT_PATH)),
    }
    for (name, value) in gate_wire::build_server_settings(run) {
        set_variable(&mut environment, name, &value, true);
    }
    set_variable(&mut environment, BOX_JOB_VARIABLE, job, true);
    environment
}

/// Sets `name` to `value` in `environment`, last: every value it holds for that name removed first, and with
/// `any_case` every value for a name that differs from it only in ASCII case.
fn set_variable(environment: &mut Vec<(OsString, OsString)>, name: &str, value: &str, any_case: bool) {
    environment.retain(|(set, _)| {
        let same = if any_case {
            set.to_str().is_some_and(|set| set.eq_ignore_ascii_case(name))
        } else {
            set.as_os_str() == OsStr::new(name)
        };
        !same
    });
    environment.push((OsString::from(name), OsString::from(value)));
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::io;
    use std::time::Duration;

    use super::{
        Along, BACK_COMMITS_FAILED, BOX_JOB_VARIABLE, CommandEnd, GODOT_PATH, Godot, ROSETTA_LINE, Translation, back_commits_line,
        command_environment, command_status, rosetta_lines, round_trip, translation,
    };
    use super::{
        JobRecord, Login, Recorded, RootFacts, WORK_ROOT_STEP, boot_line, created_line, folder_failure, folder_flaw, is_git_variable, is_lower_hex,
        job_record, job_record_text, layout_text, login_marker, marked_login, names_another_layout, no_answer, readies_single, record_unread,
        record_without_group, recorded_group, root_flaw, sdks_line, signal_guard, step_unran, version_line, warm_text, work_layout, write_failure,
    };
    use gate_wire::BoxKill;

    #[test]
    fn a_command_s_status_is_its_code_or_128_and_its_signal_and_the_box_s_own_ends_have_theirs() {
        assert_eq!(command_status(CommandEnd::Exited(0)), 0);
        assert_eq!(command_status(CommandEnd::Exited(3)), 3);
        assert_eq!(command_status(CommandEnd::Exited(255)), 255);
        assert_eq!(command_status(CommandEnd::Signalled(9)), 137);
        assert_eq!(command_status(CommandEnd::Signalled(15)), 143);
        assert_eq!(command_status(CommandEnd::Signalled(1)), 129);
        assert_eq!(command_status(CommandEnd::Gone), 255);
        assert_eq!(command_status(CommandEnd::NotStarted), 127);
        assert_eq!(command_status(CommandEnd::Failed), 1);
        assert_eq!(command_status(CommandEnd::Exited(256)), 1, "no status holds it");
        assert_eq!(command_status(CommandEnd::Exited(-1)), 1, "no status holds it");
        assert_eq!(command_status(CommandEnd::Signalled(128)), 1, "past a status");
    }

    #[test]
    fn the_box_s_kill_gives_124_and_a_signal_still_never_does() {
        assert_eq!(command_status(CommandEnd::Killed(BoxKill::Backstop)), 124);
        assert_eq!(command_status(CommandEnd::Killed(BoxKill::TimedOut)), 124);
        assert_eq!(command_status(CommandEnd::Signalled(15)), 143, "SIGTERM is 143, not the kill's 124");
        for signal in 1..127 {
            assert_ne!(command_status(CommandEnd::Signalled(signal)), 124, "signal {signal}");
        }
    }

    #[test]
    fn rosetta_is_refused_install_and_a_native_or_unread_box_runs() {
        assert_eq!(translation(Ok(1)), Translation::Rosetta);
        assert_eq!(translation(Ok(0)), Translation::Native);
        assert_eq!(
            translation(Err(io::Error::from(io::ErrorKind::NotFound))),
            Translation::Native,
            "an Intel Mac"
        );
        assert_eq!(
            translation(Err(io::Error::other("busy"))),
            Translation::Unread("gate: remote could not read sysctl.proc_translated: busy; taking the box as native".to_owned())
        );
        assert_eq!(
            rosetta_lines("0123456789ab"),
            [
                "gate-remote-refused: 0123456789ab install".to_owned(),
                "gate: remote gate-box runs under Rosetta on this builder, whose CPU times it cannot read: install the arm64 build".to_owned()
            ]
        );
        assert_eq!(rosetta_lines("n")[1], ROSETTA_LINE);
    }

    #[test]
    fn a_round_trip_time_has_seven_fractional_digits_and_a_z() {
        assert_eq!(round_trip(0, 0), "1970-01-01T00:00:00.0000000Z");
        assert_eq!(round_trip(1_000_000_000, 1_234_567), "2001-09-09T01:46:40.1234567Z");
    }

    /// The value `environment` gives each name that equals `name` but for case, in order.
    fn values_of<'e>(environment: &'e [(OsString, OsString)], name: &str) -> Vec<(&'e str, &'e str)> {
        environment
            .iter()
            .filter(|(set, _)| set.to_string_lossy().eq_ignore_ascii_case(name))
            .map(|(set, value)| (set.to_str().unwrap_or_default(), value.to_str().unwrap_or_default()))
            .collect()
    }

    /// What a case's command runs along: the login `PATH` `path` and no folder of the box's own for `TMPDIR`.
    fn along_path(path: Option<&str>) -> Along<'_> {
        Along {
            path,
            tmp: None,
            godot: Godot::Unchosen,
        }
    }

    #[test]
    fn godot_path_is_the_run_s_install_over_the_caller_s_and_removed_when_its_version_has_none() {
        let start = vec![(OsString::from("GODOT_PATH"), OsString::from("/ssh/godot"))];
        let theirs = [("GODOT_PATH".to_owned(), "/theirs/godot".to_owned())];
        let with = |godot: Godot<'_>, caller: &[(String, String)]| {
            let along = Along { godot, ..along_path(None) };
            command_environment(start.clone(), along, caller, "r", "j")
        };
        let program = "/Users/b/godot/4.7.2/Godot.app/Contents/MacOS/Godot";
        assert_eq!(values_of(&with(Godot::Installed(program), &theirs), GODOT_PATH), [(GODOT_PATH, program)]);
        assert_eq!(values_of(&with(Godot::Installed(program), &[]), GODOT_PATH), [(GODOT_PATH, program)]);
        assert!(
            values_of(&with(Godot::Missing, &theirs), GODOT_PATH).is_empty(),
            "no install, no GODOT_PATH"
        );
        assert!(values_of(&with(Godot::Missing, &[]), GODOT_PATH).is_empty(), "the box's own goes too");
        assert_eq!(
            values_of(&with(Godot::Unchosen, &theirs), GODOT_PATH),
            [(GODOT_PATH, "/theirs/godot")],
            "no version keeps the caller's"
        );
        assert_eq!(values_of(&with(Godot::Unchosen, &[]), GODOT_PATH), [(GODOT_PATH, "/ssh/godot")]);
    }

    #[test]
    fn the_command_s_environment_is_the_login_path_and_tmp_then_the_payload_s_env_then_the_box_s_own_names_once() {
        let start = vec![
            (OsString::from("PATH"), OsString::from("/ssh/bin")),
            (OsString::from("HOME"), OsString::from("/Users/b")),
            (OsString::from("TMPDIR"), OsString::from("/var/folders/xy/long/T/")),
            (OsString::from("msbuilduseserver"), OsString::from("1")),
        ];
        let caller = [
            ("SHAREDCOMPILATIONID".to_owned(), "theirs".to_owned()),
            ("gate_box_job".to_owned(), "theirs".to_owned()),
            ("MY_VAR".to_owned(), "v".to_owned()),
        ];
        let along = Along {
            path: Some("/login/bin:/usr/bin"),
            tmp: Some("/tmp/gate.abcdefgh/"),
            godot: Godot::Unchosen,
        };
        let environment = command_environment(start.clone(), along, &caller, "gate-box-n", "gate-remote-k-n");
        assert_eq!(values_of(&environment, "PATH"), [("PATH", "/login/bin:/usr/bin")]);
        assert_eq!(values_of(&environment, "TMPDIR"), [("TMPDIR", "/tmp/gate.abcdefgh/")]);
        assert_eq!(values_of(&environment, "HOME"), [("HOME", "/Users/b")]);
        assert_eq!(values_of(&environment, "MY_VAR"), [("MY_VAR", "v")]);
        assert_eq!(values_of(&environment, "SharedCompilationId"), [("SharedCompilationId", "gate-box-n")]);
        assert_eq!(values_of(&environment, "MSBUILDUSESERVER"), [("MSBUILDUSESERVER", "0")]);
        assert_eq!(values_of(&environment, BOX_JOB_VARIABLE), [("GATE_BOX_JOB", "gate-remote-k-n")]);
        assert_eq!(values_of(&environment, "UseSharedCompilation"), [("UseSharedCompilation", "false")]);

        let theirs = [("PATH".to_owned(), "/payload/bin".to_owned())];
        let over = command_environment(start.clone(), along_path(Some("/login/bin")), &theirs, "r", "j");
        assert_eq!(
            values_of(&over, "PATH"),
            [("PATH", "/payload/bin")],
            "the payload's Env comes after the login PATH"
        );
        let unread = command_environment(start, along_path(None), &[], "r", "j");
        assert_eq!(values_of(&unread, "PATH"), [("PATH", "/ssh/bin")], "no login PATH keeps the box's");
        assert_eq!(
            values_of(&unread, "TMPDIR"),
            [("TMPDIR", "/var/folders/xy/long/T/")],
            "no folder of the box's own keeps the box's TMPDIR"
        );
    }

    #[test]
    fn a_tmp_folder_given_is_the_command_environment_s_one_tmpdir() {
        let start = vec![
            (OsString::from("TMPDIR"), OsString::from("/var/folders/xy/long/T/")),
            (OsString::from("TMPDIR"), OsString::from("/second/")),
        ];
        let along = Along {
            path: None,
            tmp: Some("/tmp/gate.abcdefgh/"),
            godot: Godot::Unchosen,
        };
        let environment = command_environment(start, along, &[], "r", "j");
        assert_eq!(values_of(&environment, "TMPDIR"), [("TMPDIR", "/tmp/gate.abcdefgh/")]);
        let unset = command_environment(Vec::new(), along, &[], "r", "j");
        assert_eq!(
            values_of(&unset, "TMPDIR"),
            [("TMPDIR", "/tmp/gate.abcdefgh/")],
            "set where the box had none"
        );
    }

    #[test]
    fn a_caller_s_tmpdir_wins_over_the_box_s_folder_in_the_command_environment() {
        let start = vec![(OsString::from("TMPDIR"), OsString::from("/var/folders/xy/long/T/"))];
        let caller = [("TMPDIR".to_owned(), "/theirs/".to_owned())];
        let along = Along {
            path: None,
            tmp: Some("/tmp/gate.abcdefgh/"),
            godot: Godot::Unchosen,
        };
        let environment = command_environment(start, along, &caller, "r", "j");
        assert_eq!(values_of(&environment, "TMPDIR"), [("TMPDIR", "/theirs/")]);
    }

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
            gate_wire::root_unsafe_line("/g\n is not owned by b or is writable by others", WORK_ROOT_STEP),
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
    fn the_back_commits_line_is_the_windows_box_s() {
        let (main, lib) = ("a".repeat(40), "b".repeat(40));
        // The Windows box's own test of its line (`box_run_with_a_pair_names_each_repo_s_back_commit`) pins this form.
        assert_eq!(
            back_commits_line(&[("app", Some(&main)), ("lib", Some(&lib)), ("tools", None)]),
            format!("gate: remote back commits: app {main}, lib {lib}, tools none")
        );
        assert_eq!(
            back_commits_line(&[("app", None), ("lib", None)]),
            "gate: remote back commits: app none, lib none"
        );
        assert_eq!(BACK_COMMITS_FAILED, "gate: remote back commits: failed");
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
            JobRecord::Malformed,
            "a group no run writes"
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
    fn a_record_whose_group_or_session_no_run_writes_is_malformed() {
        for group in ["0", "1", "+7", "-1", "-7"] {
            let bad = format!("{JOB}\n{group}\n77\n");
            assert_eq!(job_record(bad.as_bytes(), JOB), JobRecord::Malformed, "group {group:?}");
        }
        for session in ["-1", "0", "+7"] {
            let bad = format!("{JOB}\n4242\n{session}\n");
            assert_eq!(job_record(bad.as_bytes(), JOB), JobRecord::Malformed, "session {session:?}");
        }
        assert_eq!(
            job_record(format!("{JOB}\n2\n1\n").as_bytes(), JOB),
            JobRecord::Group(Recorded { group: 2, session: Some(1) }),
            "the least group and session a run writes"
        );
    }

    #[test]
    fn init_any_group_below_it_and_the_box_s_own_group_are_never_signalled() {
        for group in [1, 0, -1, i32::MIN] {
            assert_eq!(
                job_record(format!("{JOB}\n{group}\n").as_bytes(), JOB),
                JobRecord::Malformed,
                "the record reader refuses the group {group}"
            );
        }
        assert_eq!(signal_guard(900, 900), Err("its recorded process group 900 is the box's own".to_owned()));
        assert_eq!(signal_guard(2, 900), Ok(()));
    }

    /// The marker the login-print cases use.
    const MARKER: &str = "__GATE_LOGIN_0123456789abcdef__";

    /// The login shell's output: `before`, the record of the environment `entries` as the box asks the shell to print
    /// it, then `after`.
    fn printed(before: &str, entries: &[&str], after: &str) -> Vec<u8> {
        let block: String = entries.iter().flat_map(|entry| [*entry, "\0"]).collect();
        format!("{before}\0{MARKER}\0{block}{after}").into_bytes()
    }

    /// The login `path` and `root`.
    fn login(path: &str, root: Option<&str>) -> Login {
        Login {
            path: path.to_owned(),
            godot_root: root.map(str::to_owned),
        }
    }

    #[test]
    fn each_login_marker_is_its_own() {
        let marker = login_marker();
        assert_eq!(marker.len(), "__GATE_LOGIN___".len() + 16, "{marker}");
        assert!(marker.starts_with("__GATE_LOGIN_") && marker.ends_with("__"), "{marker}");
        assert!(is_lower_hex(&marker["__GATE_LOGIN_".len()..marker.len() - 2], 16), "{marker}");
    }

    #[test]
    fn the_path_and_the_godot_root_are_read_from_one_record_whatever_the_profile_prints() {
        let both = ["HOME=/Users/b", "PATH=/usr/bin", "GATE_GODOT_ROOT=/Users/b/gate/godot", "TERM=dumb"];
        assert_eq!(
            marked_login(&printed("", &both, ""), MARKER),
            Ok(login("/usr/bin", Some("/Users/b/gate/godot")))
        );
        let noise = format!("Last login: today\nwelcome {MARKER}\n__GATE_PATH__ ");
        assert_eq!(
            marked_login(
                &printed(&noise, &["PATH=/opt/homebrew/bin:/usr/bin", "GATE_GODOT_ROOT=/g"], "logout\nPATH=/x"),
                MARKER
            ),
            Ok(login("/opt/homebrew/bin:/usr/bin", Some("/g"))),
            "noise before and after, as from .zlogout"
        );
        let fixed = "\0__GATE_LOGIN__\0PATH=/fake\0GATE_GODOT_ROOT=/fake-root\0";
        assert_eq!(
            marked_login(&printed(&format!("banner{fixed}"), &["PATH=/usr/bin", "GATE_GODOT_ROOT=/g"], ""), MARKER),
            Ok(login("/usr/bin", Some("/g"))),
            "a banner holding the fixed marker text and a fake record"
        );
        let after = format!("\0{MARKER}\0PATH=/after\0GATE_GODOT_ROOT=/after-root\0");
        assert_eq!(
            marked_login(&printed("", &["PATH=/usr/bin", "GATE_GODOT_ROOT=/g"], &after), MARKER),
            Ok(login("/usr/bin", Some("/g"))),
            "the first record is read"
        );
        assert_eq!(
            marked_login(&printed("", &["PATH=/usr/bin", "GATE_GODOT_ROOT=/Users/b/My Godot "], ""), MARKER),
            Ok(login("/usr/bin", Some("/Users/b/My Godot "))),
            "spaces are kept"
        );
        for no_root in [
            &["PATH=/usr/bin", "GATE_GODOT_ROOT="][..],
            &["PATH=/usr/bin"],
            &["PATH=/usr/bin", "XGATE_GODOT_ROOT=/g"],
        ] {
            assert_eq!(
                marked_login(&printed("", no_root, ""), MARKER),
                Ok(login("/usr/bin", None)),
                "{no_root:?}"
            );
        }
    }

    #[test]
    fn marker_text_inside_either_value_shifts_neither() {
        let root = format!("/g/__GATE_PATH__/x\u{1}{MARKER}__GATE_GODOT_ROOT__");
        assert_eq!(
            marked_login(&printed("", &["PATH=/usr/bin", &format!("GATE_GODOT_ROOT={root}")], ""), MARKER),
            Ok(login("/usr/bin", Some(&root))),
            "a root holding the old and the new marker text"
        );
        let path = format!("/a{MARKER}:/bPATH=__GATE_PATH__:/usr/bin");
        assert_eq!(
            marked_login(&printed("", &[&format!("PATH={path}"), "GATE_GODOT_ROOT=/g"], ""), MARKER),
            Ok(login(&path, Some("/g"))),
            "a PATH holding marker text"
        );
    }

    #[test]
    fn a_login_print_with_no_record_or_a_bad_path_gives_no_login() {
        let none = Err("said no marked PATH".to_owned());
        assert_eq!(marked_login(b"PATH=/usr/bin\0", MARKER), none);
        assert_eq!(
            marked_login(format!("{MARKER}PATH=/usr/bin\0").as_bytes(), MARKER),
            none,
            "no NULs around the marker"
        );
        assert_eq!(marked_login(&printed("", &["HOME=/h"], ""), MARKER), none, "no PATH entry");
        assert_eq!(marked_login(&printed("", &[], "PATH=/usr/bin"), MARKER), none, "a PATH no NUL ends");
        assert_eq!(
            marked_login(&printed("", &["PATH=/usr/bin"], ""), "__GATE_LOGIN_ffffffffffffffff__"),
            none,
            "another call's marker"
        );
        assert_eq!(marked_login(&printed("", &["PATH="], ""), MARKER), Err("said an empty PATH".to_owned()));
        let mut bad = format!("\0{MARKER}\0PATH=/u").into_bytes();
        bad.push(0xff);
        bad.extend_from_slice(b"\0GATE_GODOT_ROOT=/g\0");
        assert_eq!(marked_login(&bad, MARKER), Err("said a PATH that is not UTF-8".to_owned()));
        let mut bad_root = format!("\0{MARKER}\0PATH=/usr/bin\0GATE_GODOT_ROOT=/g").into_bytes();
        bad_root.push(0xff);
        bad_root.push(0);
        assert_eq!(marked_login(&bad_root, MARKER), Ok(login("/usr/bin", None)), "a root that is not UTF-8");
    }
}
