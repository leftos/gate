//! Git's MSYS keeper: a `cat.exe` of Git's own MSYS runtime the box keeps running for a whole run or warm-up, started
//! once the run's heartbeat beats and before any git call of the checkout. Every MSYS program of one user in one
//! session shares one section whose first word is both its set-up lock and its ready mark; the first program to create
//! it holds the lock for about 2 ms while it sets the section up. A program killed inside that window, while another
//! keeps the section open, leaves the lock held, and every later MSYS start waits out the runtime's 15 s spinlock and
//! dies with `add_item ... errno 1`. With the MSYS keeper up and answering first, the section is already set up when
//! anything else of the run starts an MSYS program, so no kill can leave the lock held.
//!
//! The MSYS keeper is the `cat.exe` of the first `usr\bin` holding it and the MSYS runtime within three folders above
//! the `git.exe` the box finds, used only when that folder is owned by Administrators, SYSTEM or `TrustedInstaller`,
//! which keeps out a Git the user installed in a folder of their own. It checks the folder's owner alone, not who else
//! may write there: an elevated install outside `Program Files` can still be writable by another local user. It runs on
//! a hidden console, on the command's restricted token, in a job of its own that kills on close, never the command's; its
//! standard streams are pipes the box holds. The box writes it one line and waits for the line back, looking at the run's
//! stop signal while it waits: the echo shows the runtime has finished its set-up. Dropping the keeper ends it. A keeper
//! that cannot be started, does not answer in time, or is waited for when the run is stopped is said once, with the first
//! line of its stderr when it ended, and the run goes on without it: it is a guard, not a step.

use std::ffi::OsString;
use std::fs::File;
use std::io::Write as _;
use std::io::{self, Read};
use std::os::windows::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use gate_router::escaped;
use gate_win32::{Job, PipedConsole, PipedFed, PipedLaunch, RestrictedToken, Running, TRUSTED_OWNERS};
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID};

use crate::boxside::preflight::Git;
use crate::environment::Variables;
use crate::run::{Written, is_within, resolved, written};

/// The MSYS keeper, in Git's `usr\bin` folder: it echoes what it reads and stays up until its input ends.
const KEEPER: &str = "cat.exe";
/// The MSYS runtime, which marks a `usr\bin` holding [`KEEPER`] as Git's own.
const RUNTIME: &str = "msys-2.0.dll";
/// The shell Git's `bin` holds, which a routed command's recipe may name bare: with that folder ahead of the machine
/// `PATH`, the name resolves to Git's own rather than WSL's `bash.exe` in the system folder.
const BASH: &str = "bash.exe";
/// The line a preflight says when no trusted Git `bin` was found for a routed command's `PATH`.
pub(super) const NO_GIT_BIN: &str = "gate: remote path: Git's bin is not trusted here; bash resolves by the machine PATH";
/// How many folders above `git.exe` the box looks in for Git's root: those of `cmd\git.exe`, `bin\git.exe` and
/// `<environment>\bin\git.exe`, such as `ucrt64\bin\git.exe`.
const ROOT_DEPTH: usize = 3;
/// The line the box writes to the keeper and waits to read back.
const ECHO: &[u8] = b"gate: msys keeper\n";
/// How long the box waits for the keeper's echo before it goes on without it.
const ECHO_LIMIT: Duration = Duration::from_secs(30);
/// How long one look for the echo lasts before the box looks again whether the run was stopped.
const STOP_SLICE: Duration = Duration::from_millis(100);
/// The most bytes of the keeper's answer, or of its stderr, the box reads looking for the end of its first line.
const ANSWER_CAP: usize = 4096;
/// The most characters of the keeper's stderr its failure line carries.
const SAID_CAP: usize = 200;
/// The test seam that starts a stand-in in the keeper's place: its program, then each of its words, all separated by
/// `|`, which no Windows path holds.
#[cfg(feature = "test-seams")]
const STAND_IN: &str = "GATE_TEST_BOX_MSYS_KEEPER";
/// The test seam that names the folder in place of the trusted Git `bin` a routed command's `PATH` starts with, as the
/// suite's boxes run from a folder this user owns.
#[cfg(feature = "test-seams")]
const GIT_BIN_STAND_IN: &str = "GATE_TEST_BOX_GIT_BIN";
/// The test seam that sets how long the box waits for the keeper's echo, in milliseconds, in place of 30 s.
#[cfg(feature = "test-seams")]
const LIMIT_MS: &str = "GATE_TEST_BOX_MSYS_KEEPER_MS";

/// A running MSYS keeper and everything the box holds of it. Its job is dropped first, which ends the keeper.
pub(super) struct MsysKeeper {
    _job: Job,
    _running: Running,
    _stdin: File,
    _stdout: File,
    _stderr: File,
}

/// What the box starts as the keeper and how long it waits for its echo.
struct Planned {
    program: PathBuf,
    words: Vec<OsString>,
    limit: Duration,
}

impl MsysKeeper {
    /// Starts the MSYS keeper beside `git`, writes it [`ECHO`] and waits for the line back, at most [`ECHO_LIMIT`],
    /// looking at `stopped` about every [`STOP_SLICE`]; else why it could not, the keeper then ended.
    pub(super) fn start(git: &Git, stopped: &dyn Fn() -> bool) -> Result<MsysKeeper, String> {
        let planned = planned(git)?;
        let launch = launch(&planned.program, &planned.words)?;
        let job = Job::anonymous_kill_on_close().map_err(|error| format!("cannot create its job: {error}"))?;
        let PipedFed {
            suspended,
            mut stdin,
            stdout,
            stderr,
        } = launch
            .start_suspended_within_fed(&job)
            .map_err(|error| format!("cannot start {}: {error}", planned.program.display()))?;
        let running = suspended
            .assign_and_resume(&job)
            .map_err(|error| format!("cannot run {}: {error}", planned.program.display()))?;
        // A keeper that ended before it could take the line is said by the wait below as one that ended without
        // answering, with what it wrote on stderr, which says more than the broken pipe this write would give.
        let _ = stdin.write_all(ECHO);
        let (stdout, stderr) = echoed(&read_answer(stdout, stderr)?, planned.limit, stopped)?;
        Ok(MsysKeeper {
            _job: job,
            _running: running,
            _stdin: stdin,
            _stdout: stdout,
            _stderr: stderr,
        })
    }
}

/// The MSYS keeper started for a run, held by the caller until the run returns; else the line saying why not, for the
/// caller to say once, which fails nothing. `stopped` says whether the run was stopped, looked at while the box waits.
pub(super) fn kept(stopped: &dyn Fn() -> bool) -> Result<MsysKeeper, String> {
    MsysKeeper::start(&Git::find(), stopped).map_err(|why| {
        format!(
            "gate: box could not start Git's MSYS keeper: {}; a Git shell killed as it starts can stall later ones",
            escaped(&why)
        )
    })
}

/// Git's `usr\bin` folder for `git`: the first above it that holds the keeper and the runtime ([`usr_bin_above`]),
/// owned by one of [`TRUSTED_OWNERS`]; else why there is none.
fn usr_bin(git: &Git) -> Result<PathBuf, String> {
    let program = git.program.as_ref().map_err(|why| format!("cannot find git: {why}"))?;
    let folder = usr_bin_above(program).ok_or_else(|| format!(r"no Git root with usr\bin\{KEEPER} above {}", program.display()))?;
    let owner = owner_of(&folder).map_err(|error| format!("cannot read the owner of {}: {error}", folder.display()))?;
    if !is_trusted_owner(&owner) {
        return Err(format!("{} is not owned by Administrators, SYSTEM or TrustedInstaller", folder.display()));
    }
    Ok(folder)
}

/// The first `usr\bin` holding both [`KEEPER`] and [`RUNTIME`] under the folders up to [`ROOT_DEPTH`] above `program`,
/// the nearest first; `None` when there is none, as above a shim outside Git's install.
fn usr_bin_above(program: &Path) -> Option<PathBuf> {
    program
        .ancestors()
        .skip(1)
        .take(ROOT_DEPTH)
        .map(|root| root.join("usr").join("bin"))
        .find(|folder| folder.join(KEEPER).is_file() && folder.join(RUNTIME).is_file())
}

/// The folder a routed command's `PATH` starts with: `<Git root>\bin` above the `git.exe` the box finds, when the
/// folder lies outside the run's own folders `refused` — which no folder a command runs from may lie in — and every
/// file it puts ahead of the machine `PATH` ([`exposed`]) passes [`gate_win32::check_installed_file`], the check the
/// box's own image gets, which refuses a link or junction, a network share, and a file or folder whose owner or access
/// list lets anyone but SYSTEM, Administrators or `TrustedInstaller` change what runs from it. The folder returned is
/// the final path's, in its usual form, never the name as found; the test seam's folder stands in for both checks.
/// `None` otherwise, and the command keeps the machine `PATH` it would have had.
pub(super) fn git_bin(git: &Git, refused: &[PathBuf]) -> Option<PathBuf> {
    #[cfg(feature = "test-seams")]
    {
        if let Ok(folder) = std::env::var(GIT_BIN_STAND_IN) {
            let folder = PathBuf::from(folder);
            return outside(&folder, refused).then_some(folder);
        }
    }
    let program = git.program.as_ref().ok()?;
    let bin = git_bin_above(program)?;
    if !outside(&bin, refused) {
        return None;
    }
    let bash = bin.join(BASH);
    // The launcher first, which judges `bin` itself among the folders above it; only once `bin` passes are its entries
    // listed, so no file of a folder that failed is read.
    gate_win32::check_installed_file(&bash).ok()?;
    let root = bin.parent()?;
    if !trusted(&exposed(&bin, root)?) {
        return None;
    }
    let bash = usual(&std::fs::canonicalize(&bash).ok()?)?;
    bash.parent().map(Path::to_path_buf)
}

/// The files a command's `PATH` starting with `bin` puts ahead of the machine `PATH`: every `.exe` directly in `bin`,
/// and the files Git's shell runs from the same install, `usr\bin`'s [`BASH`] and [`RUNTIME`] and `mingw64\bin\git.exe`
/// when that one exists; `None` when `bin` cannot be listed.
fn exposed(bin: &Path, root: &Path) -> Option<Vec<PathBuf>> {
    let usr_bin = root.join("usr").join("bin");
    let mut files = vec![usr_bin.join(BASH), usr_bin.join(RUNTIME)];
    let mingw_git = root.join("mingw64").join("bin").join("git.exe");
    if mingw_git.is_file() {
        files.push(mingw_git);
    }
    for entry in std::fs::read_dir(bin).ok()? {
        let path = entry.ok()?.path();
        if path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("exe")) {
            files.push(path);
        }
    }
    Some(files)
}

/// Whether every file of `files` passes [`gate_win32::check_installed_file`], the check the box's own image gets: a
/// link or junction, a network share, or a file or folder whose owner or access list lets anyone but SYSTEM,
/// Administrators or `TrustedInstaller` change what runs from it fails it.
fn trusted(files: &[PathBuf]) -> bool {
    files.iter().all(|file| gate_win32::check_installed_file(file).is_ok())
}

/// Whether the Git `bin` `bin` lies outside every one of the run's own folders `refused`.
fn outside(bin: &Path, refused: &[PathBuf]) -> bool {
    !refused.iter().any(|folder| is_within(&resolved(bin), &resolved(folder)))
}

/// `path` in its usual form, as the system gives a final path: a `\\?\UNC\` prefix written `\\` and a `\\?\` one left
/// out; `None` for a path that is not Unicode, which no `PATH` value can carry.
fn usual(path: &Path) -> Option<PathBuf> {
    let text = path.to_str()?;
    match text.strip_prefix(r"\\?\UNC\") {
        Some(rest) => Some(PathBuf::from(format!(r"\\{rest}"))),
        None => Some(PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(text))),
    }
}

/// The `bin` of the first Git root above `program` whose `usr\bin` holds [`KEEPER`] and [`RUNTIME`] ([`usr_bin_above`])
/// and whose `bin` holds [`BASH`], the nearest first; `None` when there is none, unchecked ([`git_bin`]).
fn git_bin_above(program: &Path) -> Option<PathBuf> {
    let usr_bin = usr_bin_above(program)?;
    let root = usr_bin.parent()?.parent()?;
    let bin = root.join("bin");
    bin.join(BASH).is_file().then_some(bin)
}

/// Whether the SID `owner`, in its string form, is one of [`TRUSTED_OWNERS`].
fn is_trusted_owner(owner: &str) -> bool {
    TRUSTED_OWNERS.contains(&owner)
}

/// The string form of the SID owning `path`.
fn owner_of(path: &Path) -> io::Result<String> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut owner: PSID = std::ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `wide` is NUL-terminated and outlives the call; `owner` and `descriptor` are writable, and no other part
    // of the descriptor is asked for.
    let error = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &raw mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    if error != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(error.cast_signed()));
    }
    // SAFETY: `owner` points into the descriptor, which lives until it is freed below.
    let text = unsafe { sid_text(owner) };
    // SAFETY: the system allocated the descriptor with `LocalAlloc`; it is freed once, after its last use.
    unsafe { LocalFree(descriptor) };
    text
}

/// The string form of `sid` (`S-1-5-18`).
///
/// # Safety
///
/// `sid` points to a valid SID for the whole call.
unsafe fn sid_text(sid: PSID) -> io::Result<String> {
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: the caller guarantees `sid`; `text` is writable.
    if unsafe { ConvertSidToStringSidW(sid, &raw mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut length = 0;
    // SAFETY: a success leaves `text` a NUL-terminated string, so every read up to its NUL is within it.
    while unsafe { *text.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: `length` characters precede the NUL.
    let decoded = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) });
    // SAFETY: the system allocated `text` with `LocalAlloc`; it is freed once, after its last read.
    unsafe { LocalFree(text.cast()) };
    Ok(decoded)
}

/// Git's MSYS keeper, waited for [`ECHO_LIMIT`].
fn git_keeper(git: &Git) -> Result<Planned, String> {
    Ok(Planned {
        program: usr_bin(git)?.join(KEEPER),
        words: Vec::new(),
        limit: ECHO_LIMIT,
    })
}

#[cfg(not(feature = "test-seams"))]
fn planned(git: &Git) -> Result<Planned, String> {
    git_keeper(git)
}

/// Git's MSYS keeper, or the stand-in [`STAND_IN`] names; waited for [`ECHO_LIMIT`], or the milliseconds
/// [`LIMIT_MS`] gives.
#[cfg(feature = "test-seams")]
fn planned(git: &Git) -> Result<Planned, String> {
    let mut planned = match std::env::var(STAND_IN) {
        Ok(value) => {
            let (program, words) = value.split_once('|').unwrap_or((value.as_str(), ""));
            Planned {
                program: PathBuf::from(program),
                words: words.split('|').filter(|word| !word.is_empty()).map(OsString::from).collect(),
                limit: ECHO_LIMIT,
            }
        }
        Err(_) => git_keeper(git)?,
    };
    if let Ok(text) = std::env::var(LIMIT_MS) {
        let ms: u64 = text.parse().map_err(|_| format!("{LIMIT_MS} is not a count of milliseconds: {text}"))?;
        planned.limit = Duration::from_millis(ms);
    }
    Ok(planned)
}

/// The keeper's launch: `program` with `words`, the box's own environment, the command's restricted token, on a
/// hidden console of its own, in the system folder; else why it cannot be launched.
fn launch(program: &Path, words: &[OsString]) -> Result<PipedLaunch, String> {
    let token = RestrictedToken::of_this_process().map_err(|error| format!("cannot make its restricted token: {error}"))?;
    let variables = Variables::from_process();
    let environment = variables.block().map_err(|error| format!("cannot build its environment: {error}"))?;
    let directory = gate_win32::system_directory().map_err(|error| format!("cannot find the system folder: {error}"))?;
    match written(program, words, &variables) {
        Written::Line(application, command_line) => Ok(PipedLaunch {
            application,
            command_line,
            directory,
            environment,
            console: PipedConsole::Hidden,
            token: Some(token),
        }),
        Written::Refused(line) | Written::NoShell(line) => Err(line),
    }
}

/// What the keeper answered: its first line on stdout, and the first line of its stderr when stdout ended without
/// one, with both read ends back.
struct Answer {
    stdout: File,
    stderr: File,
    line: io::Result<Vec<u8>>,
    said: Option<io::Result<Vec<u8>>>,
}

impl Answer {
    /// Both read ends once the line is [`ECHO`]; else why not.
    fn into_echo(self) -> Result<(File, File), String> {
        match self.line {
            Ok(line) if line == ECHO => Ok((self.stdout, self.stderr)),
            Ok(line) if line.last() != Some(&b'\n') => Err(ended(self.said)),
            Ok(line) => Err(format!(
                "it answered '{}' rather than the line written to it",
                String::from_utf8_lossy(&line).trim_end()
            )),
            Err(error) => Err(format!("cannot read its answer: {error}")),
        }
    }
}

/// Why a keeper whose stdout ended gave no echo, with `said`, the first line of its stderr, at most [`SAID_CAP`]
/// characters of it.
fn ended(said: Option<io::Result<Vec<u8>>>) -> String {
    match said {
        Some(Ok(bytes)) => {
            let text = String::from_utf8_lossy(&bytes);
            let line = text.trim_end();
            if line.is_empty() {
                "it ended without answering".to_owned()
            } else {
                format!("it ended without answering: {}", line.chars().take(SAID_CAP).collect::<String>())
            }
        }
        Some(Err(error)) => format!("it ended without answering, and its stderr cannot be read: {error}"),
        None => "it ended without answering".to_owned(),
    }
}

/// Reads the keeper's answer on a thread of its own and sends it on the returned receiver: its first line on `stdout`,
/// then, when stdout ended without one, the first line of `stderr`. A reader left waiting once the box gave up ends
/// when the keeper does, as the box's dropped job ends it.
fn read_answer(mut stdout: File, mut stderr: File) -> Result<Receiver<Answer>, String> {
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("msys-keeper-echo".to_owned())
        .spawn(move || {
            let line = first_line(&mut stdout);
            let said = match &line {
                Ok(line) if line.len() < ANSWER_CAP && line.last() != Some(&b'\n') => Some(first_line(&mut stderr)),
                _ => None,
            };
            // Once the wait has given up no one receives this, and the keeper is ended.
            let _ = sender.send(Answer { stdout, stderr, line, said });
        })
        .map_err(|error| format!("cannot read its answer: {error}"))?;
    Ok(receiver)
}

/// Both read ends once the answer `receiver` gets is the echo, within `limit`, looking at `stopped` about every
/// [`STOP_SLICE`]; else why not, `the run was stopped` once `stopped` says so.
fn echoed(receiver: &Receiver<Answer>, limit: Duration, stopped: &dyn Fn() -> bool) -> Result<(File, File), String> {
    let deadline = Instant::now() + limit;
    loop {
        let slice = deadline.saturating_duration_since(Instant::now()).min(STOP_SLICE);
        match receiver.recv_timeout(slice) {
            Ok(answer) => return answer.into_echo(),
            Err(RecvTimeoutError::Timeout) if Instant::now() >= deadline => {
                return Err(format!("it did not answer within {} s", limit.as_secs_f64()));
            }
            Err(RecvTimeoutError::Timeout) if stopped() => return Err("the run was stopped".to_owned()),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err("its answer's reader ended without one".to_owned()),
        }
    }
}

/// The bytes of `stream` up to and with its first line feed, at most [`ANSWER_CAP`] of them, or fewer when it ends.
fn first_line(stream: &mut File) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    while line.len() < ANSWER_CAP && line.last() != Some(&b'\n') {
        if stream.read(&mut byte)? == 0 {
            break;
        }
        line.push(byte[0]);
    }
    Ok(line)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a case whose folders or processes cannot be made must fail loudly")]
mod tests {
    use std::fs;
    use std::io::Write as _;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{NtOpenDirectoryObject, NtQueryDirectoryObject};
    use windows_sys::Win32::Foundation::{HANDLE, STATUS_NO_MORE_ENTRIES, UNICODE_STRING};
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Memory::{FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile};
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId, OpenProcessToken};

    use super::{
        ANSWER_CAP, BASH, KEEPER, MsysKeeper, RUNTIME, TRUSTED_OWNERS, echoed, exposed, git_bin, git_bin_above, is_trusted_owner, outside, owner_of,
        read_answer, sid_text, trusted, usr_bin, usr_bin_above, usual,
    };
    use crate::boxside::preflight::Git;
    use crate::test_support::temp_dir;

    /// How many lone start-ups the poisoning tries before it gives up catching one inside the lock.
    const ATTEMPTS: u32 = 50;
    /// How long the poisoning looks for a start-up's section before it lets that start-up go.
    const ATTACH_LIMIT: Duration = Duration::from_secs(2);
    /// How long the case looks for the folder a throwaway sh makes before it fails: a hang guard only.
    const FOLDER_LIMIT: Duration = Duration::from_secs(600);
    /// The prefix of the object folder Git's MSYS runtime keeps its sections in, its install key after it.
    const FOLDER_PREFIX: &str = "msys-2.0S5-";
    /// `DIRECTORY_QUERY | DIRECTORY_TRAVERSE`.
    const DIRECTORY_READ: u32 = 0x1 | 0x2;
    /// `OBJ_CASE_INSENSITIVE`.
    const CASE_INSENSITIVE: u32 = 0x40;
    /// The version word of a section a start-up holds the set-up lock of.
    const LOCKED: i32 = -1;

    /// A fresh folder `name` under the unit tests' temp folder, holding `files`, each a path relative to it.
    fn tree(name: &str, files: &[&str]) -> PathBuf {
        let root = temp_dir().join(format!("msys-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for file in files {
            let path = root.join(file);
            fs::create_dir_all(path.parent().expect("a file has a folder")).expect("the folder is made");
            fs::write(&path, "").expect("the file is written");
        }
        root
    }

    /// A Git install's `usr\bin`, as files relative to its root.
    fn git_files(git: &'static str) -> Vec<&'static str> {
        vec![git, r"usr\bin\cat.exe", r"usr\bin\msys-2.0.dll"]
    }

    /// A Git install's `usr\bin` and its `bin`, as files relative to its root.
    fn git_files_with_bin(git: &'static str) -> Vec<&'static str> {
        let mut files = git_files(git);
        files.push(r"bin\bash.exe");
        files
    }

    #[test]
    fn git_s_usr_bin_is_found_above_each_layout_of_git_exe() {
        for (name, git) in [
            ("cmd", r"cmd\git.exe"),
            ("bin", r"bin\git.exe"),
            ("mingw64", r"mingw64\bin\git.exe"),
            ("ucrt64", r"ucrt64\bin\git.exe"),
        ] {
            let root = tree(name, &git_files(git));
            assert_eq!(usr_bin_above(&root.join(git)), Some(root.join(r"usr\bin")), "{git}");
        }
    }

    #[test]
    fn a_shim_outside_git_s_install_has_no_usr_bin() {
        let root = tree("shim", &[r"scoop\shims\git.exe"]);
        assert_eq!(usr_bin_above(&root.join(r"scoop\shims\git.exe")), None);
    }

    #[test]
    fn a_usr_bin_without_the_msys_runtime_is_not_git_s() {
        let root = tree("no-runtime", &[r"cmd\git.exe", r"usr\bin\cat.exe"]);
        assert_eq!(usr_bin_above(&root.join(r"cmd\git.exe")), None, "{KEEPER} without {RUNTIME}");
    }

    #[test]
    fn a_root_more_than_three_folders_above_git_exe_is_not_looked_in() {
        let git = r"a\b\c\git.exe";
        let root = tree("deep", &git_files(git));
        assert_eq!(usr_bin_above(&root.join(git)), None);
    }

    #[test]
    fn git_s_bin_is_found_above_each_layout_of_git_exe() {
        for (name, git) in [
            ("bin-cmd", r"cmd\git.exe"),
            ("bin-direct", r"bin\git.exe"),
            ("bin-mingw64", r"mingw64\bin\git.exe"),
            ("bin-ucrt64", r"ucrt64\bin\git.exe"),
        ] {
            let root = tree(name, &git_files_with_bin(git));
            assert_eq!(git_bin_above(&root.join(git)), Some(root.join(r"bin")), "{git}");
        }
    }

    #[test]
    fn a_git_root_without_a_bash_in_its_bin_has_no_git_bin() {
        let root = tree("no-bash", &git_files(r"cmd\git.exe"));
        assert_eq!(git_bin_above(&root.join(r"cmd\git.exe")), None, "no {BASH}");
    }

    #[test]
    fn a_git_bin_this_user_owns_is_refused() {
        let root = tree("user-owned-bin", &git_files_with_bin(r"cmd\git.exe"));

        let found = git_bin(
            &Git {
                program: Ok(root.join(r"cmd\git.exe")),
            },
            &[],
        );

        assert_eq!(found, None, "a folder this user owns is not put ahead of the machine PATH");
    }

    #[test]
    fn a_git_bin_inside_a_run_folder_is_refused() {
        let root = tree("refused-bin", &git_files_with_bin(r"cmd\git.exe"));
        let git = Git {
            program: Ok(root.join(r"cmd\git.exe")),
        };

        assert_eq!(
            git_bin(&git, std::slice::from_ref(&root)),
            None,
            "a folder the run writes is not put first"
        );
    }

    #[test]
    fn the_files_a_git_bin_exposes_are_the_ones_checked() {
        let root = tree(
            "exposed",
            &[
                r"cmd\git.exe",
                r"usr\bin\cat.exe",
                r"usr\bin\msys-2.0.dll",
                r"usr\bin\bash.exe",
                r"bin\bash.exe",
                r"bin\git.exe",
                r"bin\sh.exe",
                r"bin\notes.txt",
                r"mingw64\bin\git.exe",
            ],
        );
        let mut files = exposed(&root.join(r"bin"), &root).expect("the bin folder is listed");
        files.sort();
        let mut want = vec![
            root.join(r"usr\bin\bash.exe"),
            root.join(r"usr\bin\msys-2.0.dll"),
            root.join(r"mingw64\bin\git.exe"),
            root.join(r"bin\bash.exe"),
            root.join(r"bin\git.exe"),
            root.join(r"bin\sh.exe"),
        ];
        want.sort();

        assert_eq!(files, want, "every .exe in bin and the shell's own files, and no other entry");
    }

    #[test]
    fn a_git_root_without_a_mingw64_git_exposes_no_such_file() {
        let root = tree(
            "exposed-no-mingw",
            &[r"cmd\git.exe", r"usr\bin\cat.exe", r"usr\bin\msys-2.0.dll", r"bin\bash.exe"],
        );

        let files = exposed(&root.join(r"bin"), &root).expect("the bin folder is listed");

        assert!(
            !files.iter().any(|file| file.ends_with(r"mingw64\bin\git.exe")),
            "no mingw64 git: {files:?}"
        );
    }

    #[test]
    fn a_file_this_user_owns_fails_a_list_of_trusted_files() {
        let root = tree("trusted-files", &[r"bin\bash.exe", r"bin\extra.exe"]);
        let system = gate_win32::system_directory().expect("the system folder is found").join("cmd.exe");

        assert!(trusted(std::slice::from_ref(&system)), "a system file passes");
        assert!(!trusted(&[root.join(r"bin\extra.exe")]), "a file this user owns fails");
        assert!(
            !trusted(&[system, root.join(r"bin\extra.exe")]),
            "one file this user owns fails the whole list"
        );
    }

    #[test]
    fn a_bin_inside_a_refused_folder_is_refused() {
        let root = tree("outside-inside", &[r"work\bin\bash.exe"]);

        assert!(!outside(&root.join(r"work\bin"), &[root.join("work")]), "a run folder holds the bin");
    }

    #[test]
    fn a_bin_outside_every_refused_folder_is_kept() {
        let root = tree("outside-kept", &[r"git\bin\bash.exe"]);

        assert!(outside(&root.join(r"git\bin"), &[root.join("work")]), "no run folder holds the bin");
    }

    #[test]
    fn a_bin_reached_through_a_junction_into_a_refused_folder_is_refused() {
        let root = tree("outside-junction", &[r"work\bin\bash.exe"]);
        let link = root.join("link");
        crate::test_support::junction(&link, &root.join("work"));

        assert!(!outside(&link.join("bin"), &[root.join("work")]), "the link leads into a run folder");
    }

    #[test]
    fn a_final_path_is_given_in_its_usual_form() {
        assert_eq!(usual(Path::new(r"\\?\C:\Git\bin")), Some(PathBuf::from(r"C:\Git\bin")), "a drive path");
        assert_eq!(
            usual(Path::new(r"\\?\UNC\host\share\bin")),
            Some(PathBuf::from(r"\\host\share\bin")),
            "a network path"
        );
        assert_eq!(
            usual(Path::new(r"C:\Git\bin")),
            Some(PathBuf::from(r"C:\Git\bin")),
            "a path already usual"
        );
    }

    #[test]
    fn only_system_administrators_and_trusted_installer_are_trusted_owners() {
        for owner in TRUSTED_OWNERS {
            assert!(is_trusted_owner(owner), "{owner}");
        }
        assert!(!is_trusted_owner(&user_sid()), "this process's user");
        assert!(!is_trusted_owner("S-1-1-0"), "Everyone");
        assert!(!is_trusted_owner("S-1-5-32-545"), "Users");
    }

    #[test]
    fn the_owner_of_a_folder_is_read_in_its_string_form() {
        let system = gate_win32::system_directory().expect("the system folder is found");
        let owner = owner_of(&system).expect("the system folder's owner is read");
        assert!(is_trusted_owner(&owner), "{owner}");
        let missing = temp_dir().join(format!("msys-no-such-folder-{}", std::process::id()));
        assert!(owner_of(&missing).is_err(), "a missing folder has no owner");
    }

    #[test]
    fn a_usr_bin_this_user_owns_is_refused() {
        let root = tree("user-owned", &git_files(r"cmd\git.exe"));
        let folder = root.join(r"usr\bin");
        let owner = owner_of(&folder).expect("the test's own folder has an owner");
        assert!(!is_trusted_owner(&owner), "the test's own folder is owned by {owner}");

        let why = usr_bin(&Git {
            program: Ok(root.join(r"cmd\git.exe")),
        })
        .expect_err("a folder this user owns is not used for the keeper");

        assert_eq!(
            why,
            format!("{} is not owned by Administrators, SYSTEM or TrustedInstaller", folder.display())
        );
    }

    /// The channel never sends, so only the stop check can end the wait before the generous limit, which a version
    /// without the check would spend waiting and then fail on the outcome.
    #[test]
    fn a_stop_ends_the_wait_for_the_echo() {
        let (_sender, receiver) = mpsc::channel();
        let why = echoed(&receiver, Duration::from_secs(60), &|| true).expect_err("a stopped run does not wait for the echo");
        assert_eq!(why, "the run was stopped");
    }

    /// An answer that fills the cap without a line feed is refused as one that ended, without its stderr read: on a
    /// live keeper that read would block until the limit even though stdout already said enough.
    #[test]
    fn an_answer_at_the_cap_without_a_line_feed_is_refused_without_reading_stderr() {
        let root = tree("full-cap", &["stdout"]);
        let answer = root.join("stdout");
        fs::write(&answer, vec![b'x'; ANSWER_CAP]).expect("the answer is written");
        let said = root.join("stderr");
        fs::write(&said, "stderr is not read\n").expect("the stderr is written");
        let receiver = read_answer(
            fs::File::open(&answer).expect("the answer opens"),
            fs::File::open(&said).expect("the stderr opens"),
        )
        .expect("the reader starts");

        let why = echoed(&receiver, Duration::from_secs(60), &|| false).expect_err("a capped answer is not an echo");

        assert_eq!(why, "it ended without answering");
    }

    /// An MSYS start-up killed inside the per-user section's init lock leaves its version word at -1, and every later
    /// start waits out the runtime's 15 s spinlock and dies with `add_item ... errno 1`. With the box's MSYS keeper up
    /// first, the word is already set when anything else starts, so no kill can leave it held.
    ///
    /// It needs no other MSYS process alive, so it runs over a plain ssh session on a builder (as `.tmp/g387/rps.sh`
    /// reaches one), never as a routed run: a routed run's own box keeps a keeper up, under which the case passes with
    /// the keeper line below removed too.
    #[test]
    #[ignore = "needs no other MSYS process alive (a direct ssh session, not a routed run): run on a builder"]
    fn a_start_up_killed_beside_the_keeper_leaves_the_next_sh_working() {
        let git = Git::find();
        let sh = usr_bin(&git).expect("Git's usr\\bin is found").join("sh.exe");
        let names = Names::learned(&sh);
        let _keeper = MsysKeeper::start(&git, &|| false).expect("the keeper starts and echoes");
        // Without the keeper this catches the lock on the first try and leaves the word at -1, measured 4 of 4 on the
        // builder; the handles it returns stand for another MSYS process keeping the section open meanwhile.
        let held = poison_if_unkept(&sh, &names);
        let caught = held.is_some();
        let ran = gate_win32::spawn(
            Command::new(&sh)
                .args(["-c", "true"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped()),
        )
        .expect("sh starts")
        .wait_with_output()
        .expect("sh runs");
        drop(held);
        let said = String::from_utf8_lossy(&ran.stderr);
        assert!(ran.status.success() && !said.contains("add_item"), "{}: {said}", ran.status);
        assert!(!caught, "with the keeper up no start-up takes the lock");
    }

    /// The port of `KillInit.Chain`: starts lone `sh -c true`s, each watched through the per-user section's first word
    /// with the section and its object folder held open, and kills the first one seen holding the set-up lock (the
    /// word gone from 0 to -1). The handles that saw it, which the caller holds until its next start has run; `None`
    /// when no attempt caught the lock, as with the section already set up.
    fn poison_if_unkept(sh: &Path, names: &Names) -> Option<Held> {
        let mut seen = false;
        for _ in 0..ATTEMPTS {
            let mut child = gate_win32::spawn(
                Command::new(sh)
                    .args(["-c", "true"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null()),
            )
            .expect("sh starts");
            let Some(view) = attach(&mut child, &names.section) else {
                child.wait().expect("sh ends");
                continue;
            };
            seen = true;
            let folder = open_folder(&names.folder);
            while view.word() == 0 && is_running(&mut child) {}
            if view.word() != LOCKED || folder.is_none() {
                child.wait().expect("sh ends");
                continue;
            }
            child.kill().expect("sh is killed");
            child.wait().expect("sh ends");
            return folder.map(|folder| Held {
                _view: view,
                _folder: folder,
            });
        }
        assert!(seen, "the section {} was never seen: are its names right?", names.section);
        None
    }

    /// The handles to a section and its object folder that keep both alive.
    struct Held {
        _view: View,
        _folder: OwnedHandle,
    }

    /// A read-only view of a section's first word, with the section's handle.
    struct View {
        address: MEMORY_MAPPED_VIEW_ADDRESS,
        _section: OwnedHandle,
    }

    impl View {
        fn word(&self) -> i32 {
            // SAFETY: the view maps at least the section's first four bytes, read-only, until it is dropped.
            unsafe { std::ptr::read_volatile(self.address.Value.cast::<i32>()) }
        }
    }

    impl Drop for View {
        fn drop(&mut self) {
            // SAFETY: the address is the view this value mapped, unmapped once here.
            unsafe { UnmapViewOfFile(self.address) };
        }
    }

    /// The names of this user's MSYS section, as `OpenFileMappingW` takes it, and of its object folder in the object
    /// manager's own namespace.
    struct Names {
        section: String,
        folder: String,
    }

    impl Names {
        /// The names for this process's user and session, the install key read from the folder a throwaway `sh -c
        /// read` makes, held up on its piped stdin while the folder is looked for and then let go. A runtime that may
        /// create global objects keeps its folder among the global named objects, any other among its session's; in
        /// session 0 the two are one. When some MSYS folder is already there the case's precondition is broken, and
        /// the first one found is taken.
        fn learned(sh: &Path) -> Names {
            let places = named_places();
            let before = msys_folders(&places);
            let mut child = gate_win32::spawn(
                Command::new(sh)
                    .args(["-c", "read line"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null()),
            )
            .expect("the throwaway sh starts");
            let began = Instant::now();
            let found = loop {
                if let Some(found) = msys_folders(&places).into_iter().find(|found| !before.contains(found)) {
                    break found;
                }
                if let Some(found) = before.first() {
                    break found.clone();
                }
                assert!(is_running(&mut child), "the throwaway sh ended before its MSYS folder was seen");
                assert!(began.elapsed() < FOLDER_LIMIT, "no MSYS folder appeared for the throwaway sh");
            };
            let mut stdin = child.stdin.take().expect("the throwaway sh's stdin is piped");
            // Its read ends at the line or at the end of its input; either lets it go.
            let _ = stdin.write_all(b"\n");
            drop(stdin);
            child.wait().expect("the throwaway sh ends");
            let (named, prefix, key) = found;
            Names {
                section: format!(r"{prefix}{key}\{}.1", user_sid()),
                folder: format!(r"{named}\{key}"),
            }
        }
    }

    /// Each folder of named objects an MSYS runtime of this process's session may use, with the prefix
    /// `OpenFileMappingW` takes for it: the global ones, then the session's own unless this is session 0.
    fn named_places() -> Vec<(String, &'static str)> {
        let mut session = 0;
        // SAFETY: `session` is writable.
        let found = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &raw mut session) };
        assert_ne!(found, 0, "this process's session is read");
        let global = (r"\BaseNamedObjects".to_owned(), r"Global\");
        if session == 0 {
            vec![global]
        } else {
            vec![global, (format!(r"\Sessions\{session}\BaseNamedObjects"), "")]
        }
    }

    /// Every MSYS folder in `places`: the folder of named objects, its prefix and the MSYS folder's name.
    fn msys_folders(places: &[(String, &'static str)]) -> Vec<(String, &'static str, String)> {
        places
            .iter()
            .flat_map(|(named, prefix)| prefixed(named, FOLDER_PREFIX).into_iter().map(|key| (named.clone(), *prefix, key)))
            .collect()
    }

    /// The name of every object in the folder `named` whose name starts with `prefix`.
    fn prefixed(named: &str, prefix: &str) -> Vec<String> {
        let folder = open_folder(named).expect("the named objects open");
        let mut buffer = vec![0_u64; 1024];
        let mut context = 0_u32;
        let mut names = Vec::new();
        loop {
            // SAFETY: the folder handle is open; the buffer is writable for its whole length and aligned for the
            // entry it receives; `context` is writable.
            let status = unsafe {
                NtQueryDirectoryObject(
                    folder.as_raw_handle(),
                    buffer.as_mut_ptr().cast(),
                    u32::try_from(buffer.len() * 8).expect("the buffer's size fits"),
                    true,
                    false,
                    &raw mut context,
                    std::ptr::null_mut(),
                )
            };
            if status == STATUS_NO_MORE_ENTRIES {
                return names;
            }
            assert!(status >= 0, "{named} is listed: status {status:#x}");
            // SAFETY: a success leaves one entry at the buffer's start, its name within the buffer.
            let name = unsafe { text_of(&buffer.as_ptr().cast::<UNICODE_STRING>().read()) };
            if name.starts_with(prefix) {
                names.push(name);
            }
        }
    }

    /// The text of `string`.
    ///
    /// # Safety
    ///
    /// `string` points to `Length` readable bytes.
    unsafe fn text_of(string: &UNICODE_STRING) -> String {
        // SAFETY: the caller guarantees the buffer.
        let units = unsafe { std::slice::from_raw_parts(string.Buffer, usize::from(string.Length / 2)) };
        String::from_utf16_lossy(units)
    }

    /// The object folder `name`, opened to list and pass through; `None` when it cannot be.
    fn open_folder(name: &str) -> Option<OwnedHandle> {
        let mut wide: Vec<u16> = name.encode_utf16().collect();
        let length = u16::try_from(wide.len() * 2).expect("the name's length fits");
        wide.push(0);
        let string = UNICODE_STRING {
            Length: length,
            MaximumLength: length + 2,
            Buffer: wide.as_mut_ptr(),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: u32::try_from(size_of::<OBJECT_ATTRIBUTES>()).expect("the size fits"),
            RootDirectory: std::ptr::null_mut(),
            ObjectName: &raw const string,
            Attributes: CASE_INSENSITIVE,
            SecurityDescriptor: std::ptr::null(),
            SecurityQualityOfService: std::ptr::null(),
        };
        let mut handle: HANDLE = std::ptr::null_mut();
        // SAFETY: `handle` is writable; `attributes` and the name it points to outlive the call.
        let status = unsafe { NtOpenDirectoryObject(&raw mut handle, DIRECTORY_READ, &raw const attributes) };
        // SAFETY: a success is a fresh handle nothing else owns.
        (status >= 0).then(|| unsafe { OwnedHandle::from_raw_handle(handle) })
    }

    /// A view of the section `name`'s first word, once `child` has made it; `None` when `child` ended first or the
    /// section did not appear within [`ATTACH_LIMIT`].
    fn attach(child: &mut Child, name: &str) -> Option<View> {
        let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
        let began = Instant::now();
        while is_running(child) && began.elapsed() < ATTACH_LIMIT {
            // SAFETY: `wide` is NUL-terminated and outlives the call.
            let section = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, wide.as_ptr()) };
            if section.is_null() {
                continue;
            }
            // SAFETY: a success is a fresh handle nothing else owns.
            let section = unsafe { OwnedHandle::from_raw_handle(section) };
            // SAFETY: the section handle is open with read access; four bytes lie within any section MSYS makes.
            let address = unsafe { MapViewOfFile(section.as_raw_handle(), FILE_MAP_READ, 0, 0, 4) };
            assert!(!address.Value.is_null(), "the section {name} maps");
            return Some(View { address, _section: section });
        }
        None
    }

    fn is_running(child: &mut Child) -> bool {
        child.try_wait().expect("sh can be looked at").is_none()
    }

    /// The string form of this process's user's SID.
    fn user_sid() -> String {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: the pseudo-handle of this process is always open; `token` is writable.
        assert_ne!(
            unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) },
            0,
            "the token opens"
        );
        // SAFETY: a success is a fresh handle nothing else owns.
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let handle = token.as_raw_handle();
        let mut size = 0_u32;
        // SAFETY: a null buffer of size 0 asks only for the size, into the writable `size`.
        unsafe { GetTokenInformation(handle, TokenUser, std::ptr::null_mut(), 0, &raw mut size) };
        let mut buffer = vec![0_u64; (size as usize).div_ceil(8)];
        // SAFETY: the buffer is writable for `size` bytes and aligned for a `TOKEN_USER`.
        let read = unsafe { GetTokenInformation(handle, TokenUser, buffer.as_mut_ptr().cast(), size, &raw mut size) };
        assert_ne!(read, 0, "the token's user is read");
        // SAFETY: a success leaves a `TOKEN_USER` at the buffer's start, its SID within the buffer.
        let sid = unsafe { buffer.as_ptr().cast::<TOKEN_USER>().read() }.User.Sid;
        // SAFETY: `sid` points into the buffer, which lives past the call.
        unsafe { sid_text(sid) }.expect("the SID converts")
    }
}
