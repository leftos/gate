//! The build servers in the job and outside it: the process table read once per sample, the command line of each
//! process that matters read once per pid and start and kept while it is alive, and the classifier calls that give the
//! classified servers whose priority the watch moves. A process the gate cannot read is skipped, so no one else's
//! process can fail a sample.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io;
use std::path::Path;

use gate_watchdog::{is_build_server, is_compiler_image};
use gate_win32::ProcessTimes;
use proc_table::{ProcessInfo, filetime_to_timestamp};

use crate::escaped;

/// The `dotnet` host an `MSBuild` node or a `VBCSCompiler.dll` runs under, and the one image whose command line a
/// build server's name alone does not settle.
const DOTNET: &str = "dotnet.exe";

/// One build server a sample classified, whichever job it runs in: what the watchdog's `server_target` raises or
/// lowers, and the pid and start that name it to the failure line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Classified {
    /// Its process id.
    pub(crate) pid: u32,
    /// The image it runs as, as a failure line names it.
    pub(crate) name: String,
    /// Its start as a `FILETIME`, the key that tells a pid another process has taken since.
    pub(crate) created: u64,
    /// Whether it runs in this gate's own job.
    pub(crate) inside: bool,
}

/// Whether a process whose image is `name` is a build server, as a sample and a kill both classify one, by the
/// watchdog's [`is_build_server`]: a compiler server's image by its name alone, or the `dotnet` host whose command line
/// names a compiler server or an `MSBuild` node. `line` gives the command line and is called only for the `dotnet`
/// host; a line that cannot be read is the empty string the classifier takes.
pub(crate) fn is_server(name: &str, test_image: Option<&str>, line: impl FnOnce() -> String) -> bool {
    let line = if name.eq_ignore_ascii_case(DOTNET) { line() } else { String::new() };
    is_build_server(name, &line, test_image)
}

/// What one sample's read is confined to: the pids of the job's processes and the folder a build server outside the
/// job must live under.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Scope<'a> {
    /// The pids of the job's processes created since the command.
    pub(crate) members: &'a BTreeSet<u32>,
    /// The folder a build server's program must lie under, `None` when the scan is machine-wide
    /// (`--test-servers-under`).
    pub(crate) under: Option<&'a Path>,
}

/// The build servers of a watch and the command lines it has read: one line per process id and start time, kept while
/// that process is still in the snapshot, so no sample reads the same process's PEB twice.
#[derive(Clone, Debug, Default)]
pub(crate) struct Servers {
    /// The command line of each process read so far, keyed by its pid and the start time its own times give it in
    /// 100 ns ticks, `None` when the line could not be read; the walk for the processes outside the job reads it too.
    command_lines: BTreeMap<(u32, u64), Option<String>>,
    /// One more image file name read as a compiler server's (`--test-server-image`), `None` outside the suite.
    test_image: Option<String>,
}

impl Servers {
    /// A reader that has read nothing yet, reading `test_image` as a compiler server's image too.
    pub(crate) fn new(test_image: Option<String>) -> Self {
        Servers {
            command_lines: BTreeMap::new(),
            test_image,
        }
    }

    /// Whether `name` is a compiler server's image by its name alone, so no command line is read for it.
    fn is_compiler_image(&self, name: &str) -> bool {
        is_compiler_image(name, self.test_image.as_deref())
    }

    /// The classified build servers of one sample, in the job and outside it: `processes` is the snapshot without
    /// command lines, and `scope` what the sample is confined to, the job's members and the folder a server outside
    /// the job must live under. `times` reads a process's own times and `command_line` reads a process's command line,
    /// each called only for the processes that matter; a process either call cannot read for is skipped rather than
    /// failing the sample, as the PowerShell gate's `Get-BuildServer` skips one it cannot open.
    pub(crate) fn read(
        &mut self,
        processes: &[ProcessInfo],
        scope: &Scope<'_>,
        times: impl Fn(u32) -> io::Result<Option<ProcessTimes>>,
        command_line: impl Fn(u32) -> Option<String>,
    ) -> Vec<Classified> {
        let mut alive: HashSet<(u32, u64)> = HashSet::new();
        let mut found = Vec::new();
        for process in processes {
            let server = if scope.members.contains(&process.pid) {
                self.member(process, &times, &command_line, &mut alive)
            } else {
                self.server_of(process, scope, &times, &command_line, &mut alive)
            };
            found.extend(server);
        }
        self.command_lines.retain(|key, _| alive.contains(key));
        found
    }

    /// The build server a job member is, if it runs as one: a `VBCSCompiler.exe` by its name alone, so no line is read
    /// for it, and a `dotnet.exe` by its command line. A member whose times cannot be read has no key, so it is no
    /// server.
    fn member(
        &mut self,
        process: &ProcessInfo,
        times: &impl Fn(u32) -> io::Result<Option<ProcessTimes>>,
        read: &impl Fn(u32) -> Option<String>,
        alive: &mut HashSet<(u32, u64)>,
    ) -> Option<Classified> {
        let compiler = self.is_compiler_image(&process.name);
        if !compiler && !process.name.eq_ignore_ascii_case(DOTNET) {
            return None;
        }
        let Ok(Some(times)) = times(process.pid) else {
            return None;
        };
        let key = (process.pid, times.created);
        alive.insert(key);
        let line = if compiler { String::new() } else { self.line(key, process.pid, read) };
        is_server(&process.name, self.test_image.as_deref(), || line).then(|| Classified {
            pid: process.pid,
            name: process.name.clone(),
            created: times.created,
            inside: true,
        })
    }

    /// The build server `process` is outside the job, if the sample classifies it: `None` for a process that is no
    /// server, one whose times cannot be read, and one whose program lies outside the scoped folder. Its command line's
    /// key is kept alive once a line was read for it.
    fn server_of(
        &mut self,
        process: &ProcessInfo,
        scope: &Scope<'_>,
        times: &impl Fn(u32) -> io::Result<Option<ProcessTimes>>,
        read: &impl Fn(u32) -> Option<String>,
        alive: &mut HashSet<(u32, u64)>,
    ) -> Option<Classified> {
        // Any image but a compiler server's and the `dotnet` host is no server, and no command line is read for it.
        if !self.is_compiler_image(&process.name) && !process.name.eq_ignore_ascii_case(DOTNET) {
            return None;
        }
        let Ok(Some(times)) = times(process.pid) else {
            return None;
        };
        let key = (process.pid, times.created);
        alive.insert(key);
        let line = self.line(key, process.pid, read);
        let test_image = self.test_image.as_deref();
        if !is_server(&process.name, test_image, || line.clone()) || scope.under.is_some_and(|under| !under_folder(&line, under)) {
            return None;
        }
        Some(Classified {
            pid: process.pid,
            name: process.name.clone(),
            created: times.created,
            inside: false,
        })
    }

    /// Whether the walk for the processes outside the job leaves `process` running with everything under it, as the
    /// kill's [`escaped::spared`] judges it, on the command line a sample of this watch read for its pid and start, so
    /// one judgement holds for the process's life and its PEB is not read again; `read` reads the line of a process no
    /// sample read one for.
    pub(crate) fn spares(&self, process: &ProcessInfo, read: impl FnOnce(u32) -> Option<String>) -> bool {
        let known = self.known_line(process);
        escaped::spared(process, self.test_image.as_deref(), |pid| match known {
            Some(line) => line.clone(),
            None => read(pid),
        })
    }

    /// The command line a sample of this watch read for `process`'s pid and start, `None` when none did.
    fn known_line(&self, process: &ProcessInfo) -> Option<&Option<String>> {
        let started = process.created?;
        // The snapshot gives the start in whole microseconds, the key in 100 ns ticks.
        self.command_lines
            .range((process.pid, 0)..=(process.pid, u64::MAX))
            .find(|((_, created), _)| i64::try_from(*created).ok().and_then(filetime_to_timestamp) == Some(started))
            .map(|(_, line)| line)
    }

    /// The command line of the process behind `key`, read now and kept for later samples unless it was read before; an
    /// unreadable line is the empty string the classifiers take, as the PowerShell gate reads what it cannot open.
    fn line(&mut self, key: (u32, u64), pid: u32, read: &impl Fn(u32) -> Option<String>) -> String {
        self.command_lines.entry(key).or_insert_with(|| read(pid)).clone().unwrap_or_default()
    }
}

/// Whether the program a command line starts with lies in `folder`: the first word with its surrounding quotes
/// stripped, its components compared ignoring case, as Windows compares a path, and a whole component at a time, so
/// `C:\build\two` is not under `C:\build\t`.
fn under_folder(line: &str, folder: &Path) -> bool {
    let program = components(Path::new(first_word(line)));
    let under = components(folder);
    !under.is_empty() && program.len() > under.len() && program[..under.len()] == under[..]
}

/// The first word of a command line, its surrounding double quotes stripped: the program a Win32 command line starts
/// with, which a path holding a space is wrapped in.
fn first_word(line: &str) -> &str {
    match line.trim_start().strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or(rest),
        None => line.split_whitespace().next().unwrap_or_default(),
    }
}

/// `path`'s components as lowercase text, as Windows compares them.
fn components(path: &Path) -> Vec<String> {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::BTreeSet;
    use std::path::Path;

    use gate_win32::ProcessTimes;
    use proc_table::ProcessInfo;

    use super::{Classified, Scope, Servers};

    /// `ERROR_ACCESS_DENIED`, as a raw OS error.
    const ACCESS_DENIED: i32 = 5;

    /// The `FILETIME` ticks of `seconds` after the Unix epoch, as a process's own start time is held.
    fn ticks(seconds: i64) -> u64 {
        (seconds * 10_000_000 + 116_444_736_000_000_000).cast_unsigned()
    }

    /// A process of the injected snapshot, named `name`: its start is read from its own times, not from the start a
    /// command-line-free snapshot reports.
    fn process(pid: u32, name: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid: 0,
            name: name.to_owned(),
            command_line: None,
            created: None,
        }
    }

    /// The times of a process that started `seconds` after the epoch; the read takes nothing else from them.
    fn times_at(seconds: i64) -> ProcessTimes {
        ProcessTimes {
            created: ticks(seconds),
            cpu: 0,
        }
    }

    /// The times of a test's processes, each started 20 s after the epoch. The `Result` is the shape the reader the
    /// watch passes has, and this double never fails.
    #[expect(clippy::unnecessary_wraps, reason = "the reader the watch passes is fallible; this double always answers")]
    fn times(_pid: u32) -> std::io::Result<Option<ProcessTimes>> {
        Ok(Some(times_at(20)))
    }

    /// The scope of a read whose job holds `members`, its server scan confined to the folder `under`.
    fn scope<'a>(members: &'a BTreeSet<u32>, under: Option<&'a Path>) -> Scope<'a> {
        Scope { members, under }
    }

    /// The pids of `found`, in the order the read gave them.
    fn pids(found: &[Classified]) -> Vec<u32> {
        found.iter().map(|server| server.pid).collect()
    }

    #[test]
    fn build_servers_outside_the_job_are_classified_whatever_their_start() {
        let mut servers = Servers::new(None);
        let processes = [
            process(1, "VBCSCompiler.exe"),
            process(2, "VBCSCompiler.exe"),
            process(3, "dotnet.exe"),
            process(4, "dotnet.exe"),
            process(5, "MSBuild.exe"),
            process(6, "cargo.exe"),
            process(7, "dotnet.exe"),
        ];
        let lines = |pid: u32| match pid {
            3 => Some(r"dotnet C:\sdk\Roslyn\VBCSCompiler.dll /shared".to_owned()),
            7 => None,
            _ => Some("dotnet run".to_owned()),
        };
        // Pid 1 started 15 s before every other process.
        let times = |pid: u32| Ok(Some(times_at(if pid == 1 { 5 } else { 20 })));

        let found = servers.read(&processes, &scope(&BTreeSet::new(), None), times, lines);

        assert_eq!(pids(&found), [1, 2, 3], "the compiler servers by name and by command line");
        assert!(found.iter().all(|server| !server.inside), "none of them runs in the job");
        assert_eq!(found[0].created, ticks(5), "each keeps its own start");
    }

    #[test]
    fn a_server_whose_times_cannot_be_read_is_skipped_rather_than_failing_the_sample() {
        let mut servers = Servers::new(None);
        let processes = [process(1, "VBCSCompiler.exe"), process(2, "VBCSCompiler.exe")];
        let lines = |_pid: u32| None;
        let times = |pid: u32| match pid {
            1 => Err(std::io::Error::from_raw_os_error(ACCESS_DENIED)),
            _ => Ok(None),
        };

        let found = servers.read(&processes, &scope(&BTreeSet::new(), None), times, lines);

        assert_eq!(found, [], "a denied server and one that has gone are both skipped");
    }

    #[test]
    fn a_process_whose_start_cannot_be_read_is_skipped() {
        let mut servers = Servers::new(None);
        let reads = Cell::new(0);
        let lines = |_pid: u32| {
            reads.set(reads.get() + 1);
            Some("VBCSCompiler.exe".to_owned())
        };

        let found = servers.read(&[process(1, "VBCSCompiler.exe")], &scope(&BTreeSet::new(), None), |_pid| Ok(None), lines);

        assert_eq!(found, [], "a process whose start cannot be read is no server");
        assert_eq!(reads.get(), 0, "and no command line is read for it");
    }

    #[test]
    fn a_command_line_is_read_once_per_pid_and_start_time_across_samples() {
        let mut servers = Servers::new(None);
        let reads = Cell::new(0);
        let lines = |_pid: u32| {
            reads.set(reads.get() + 1);
            Some("dotnet VBCSCompiler.dll".to_owned())
        };
        let processes = [process(7, "dotnet.exe")];

        servers.read(&processes, &scope(&BTreeSet::new(), None), times, lines);
        assert_eq!(reads.get(), 1);
        servers.read(&processes, &scope(&BTreeSet::new(), None), times, lines);
        assert_eq!(reads.get(), 1, "the second sample answers from the cache");
    }

    #[test]
    fn a_pid_that_reappears_with_a_later_start_reads_its_command_line_again() {
        let mut servers = Servers::new(None);
        let reads = Cell::new(0);
        let lines = |_pid: u32| {
            reads.set(reads.get() + 1);
            Some("dotnet VBCSCompiler.dll".to_owned())
        };
        let processes = [process(7, "dotnet.exe")];

        servers.read(&processes, &scope(&BTreeSet::new(), None), |_pid| Ok(Some(times_at(20))), lines);
        assert_eq!(reads.get(), 1);
        // The same pid, now started later: another process holds it, and its own line is read for it.
        servers.read(&processes, &scope(&BTreeSet::new(), None), |_pid| Ok(Some(times_at(25))), lines);
        assert_eq!(reads.get(), 2, "a later start is another process");
    }

    #[test]
    fn a_command_line_no_live_process_holds_is_pruned() {
        let mut servers = Servers::new(None);
        let lines = |_pid: u32| Some("dotnet VBCSCompiler.dll".to_owned());

        servers.read(&[process(7, "dotnet.exe")], &scope(&BTreeSet::new(), None), times, lines);
        assert_eq!(servers.command_lines.len(), 1);
        servers.read(&[], &scope(&BTreeSet::new(), None), times, lines);
        assert_eq!(servers.command_lines.len(), 0, "a key the snapshot no longer holds is dropped");
    }

    #[test]
    fn a_member_that_is_no_dotnet_host_has_no_command_line_read() {
        let mut servers = Servers::new(None);
        let reads = Cell::new(0);
        let lines = |_pid: u32| {
            reads.set(reads.get() + 1);
            Some("MSBuild.exe My.sln".to_owned())
        };
        let processes = [process(1, "MSBuild.exe"), process(2, "VBCSCompiler.exe")];
        let members = BTreeSet::from([1, 2]);

        let found = servers.read(&processes, &scope(&members, None), times, lines);

        assert_eq!(pids(&found), [2], "the compiler server in the job, by its name");
        assert!(found[0].inside, "it runs in the job");
        assert_eq!(reads.get(), 0, "no line is read for a member that is no dotnet host");
    }

    #[test]
    fn only_a_server_whose_program_lies_under_the_scoped_folder_counts() {
        let mut servers = Servers::new(None);
        let folder = Path::new(r"C:\work\case");
        let processes = [
            process(1, "VBCSCompiler.exe"),
            process(2, "VBCSCompiler.exe"),
            process(3, "VBCSCompiler.exe"),
            process(4, "dotnet.exe"),
        ];
        let lines = |pid: u32| match pid {
            1 => Some(r#""C:\work\case\VBCSCompiler.exe" /shared"#.to_owned()),
            2 => Some(r"C:\work\elsewhere\VBCSCompiler.exe /shared".to_owned()),
            3 => Some(r"C:\work\cas\VBCSCompiler.exe /shared".to_owned()),
            4 => Some(r"dotnet C:\work\case\VBCSCompiler.dll /shared".to_owned()),
            _ => None,
        };

        let found = servers.read(&processes, &scope(&BTreeSet::new(), Some(folder)), times, lines);

        assert_eq!(pids(&found), [1], "only the program under the folder counts");
    }
}
