//! The build servers outside the job and the build client inside it: the process table read once per sample, the
//! command line of each process that matters read once per pid and start and kept while it is alive, and the classifier
//! calls that fill a sample's server maps, its client flag and the classified servers whose priority the watch moves.
//! A process the gate cannot read is skipped, so no one else's process can fail a sample.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io;
use std::path::Path;

use gate_watchdog::{is_build_client, is_build_server, is_compiler_image, is_compiler_server};
use gate_win32::ProcessTimes;
use proc_table::{ProcessInfo, filetime_to_timestamp};

use crate::escaped;

/// The `dotnet` host an `MSBuild` node or a `VBCSCompiler.dll` runs under, and the one image whose command line a
/// build client's name alone does not settle.
const DOTNET: &str = "dotnet.exe";

/// What one sample found outside the job and inside it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Found {
    /// The build servers the command started, by pid, with the CPU time each has used in 100 ns ticks.
    pub(crate) new_servers: BTreeMap<u32, u64>,
    /// The compiler servers already running that the command's build client may be using, by pid, with the CPU time
    /// each has used in 100 ns ticks.
    pub(crate) shared_servers: BTreeMap<u32, u64>,
    /// Whether the job holds a build client.
    pub(crate) build_client: bool,
    /// Every build server the sample classified, in this gate's job or outside it, whose priority class the watch's
    /// pass moves ([`crate::priority`]).
    pub(crate) servers: Vec<Classified>,
}

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

/// The servers of a sample, each with whether it started after the command: the new ones first, then the shared ones,
/// each in pid order, as the diagnostics block a kill writes lists them.
pub(crate) fn counted(new_servers: &BTreeMap<u32, u64>, shared_servers: &BTreeMap<u32, u64>) -> Vec<(u32, bool)> {
    new_servers
        .keys()
        .map(|&pid| (pid, true))
        .chain(shared_servers.keys().map(|&pid| (pid, false)))
        .collect()
}

/// Whether a process whose image is `name` is a build server, as a sample and a kill both classify one, by the
/// watchdog's [`is_build_server`]: a compiler server's image by its name alone, or the `dotnet` host whose command line
/// names a compiler server or an `MSBuild` node. `line` gives the command line and is called only for the `dotnet`
/// host; a line that cannot be read is the empty string the classifier takes.
pub(crate) fn is_server(name: &str, test_image: Option<&str>, line: impl FnOnce() -> String) -> bool {
    let line = if name.eq_ignore_ascii_case(DOTNET) { line() } else { String::new() };
    is_build_server(name, &line, test_image)
}

/// What one sample's read is confined to: the pids of the job's processes, the command's start, and the folder a build
/// server must live under.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Scope<'a> {
    /// The pids of the job's processes created since the command.
    pub(crate) members: &'a BTreeSet<u32>,
    /// The command's start as a `FILETIME`.
    pub(crate) since: u64,
    /// The folder a build server's program must lie under, `None` when the scan is machine-wide
    /// (`--test-servers-under`).
    pub(crate) under: Option<&'a Path>,
}

/// One build server a sample classified outside the job: what the sample counts it as, if anything, and the server
/// itself, which a watch moves whether or not anything counts it.
#[derive(Clone, Debug)]
struct Server {
    /// Whether the sample counts it, and as what: `true` for a server started after the command and `false` for a
    /// compiler server already running, each with its CPU in 100 ns ticks; `None` for a server nothing counts, as an
    /// `MSBuild` node already running is.
    counted: Option<(bool, u64)>,
    /// The server, as the priority pass reads it.
    classified: Classified,
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

    /// The build servers, the build client and the classified servers of one sample: `processes` is the snapshot
    /// without command lines, and `scope` what the sample is confined to, the job's members, the command's start and
    /// the folder a server must live under. `times` reads a process's own times — its start, not the snapshot's,
    /// decides whether it is new — and `command_line` reads a process's command line, each called only for the
    /// processes that matter; a process either call cannot read for is skipped rather than failing the sample, as the
    /// PowerShell gate's `Get-BuildServer` skips one it cannot open.
    pub(crate) fn read(
        &mut self,
        processes: &[ProcessInfo],
        scope: &Scope<'_>,
        times: impl Fn(u32) -> io::Result<Option<ProcessTimes>>,
        command_line: impl Fn(u32) -> Option<String>,
    ) -> Found {
        let mut alive: HashSet<(u32, u64)> = HashSet::new();
        let mut found = Found::default();
        for process in processes {
            if scope.members.contains(&process.pid) {
                self.member(process, &times, &command_line, &mut alive, &mut found);
            } else if let Some(server) = self.server_of(process, scope, &times, &command_line, &mut alive) {
                if let Some((new, cpu)) = server.counted {
                    let servers = if new { &mut found.new_servers } else { &mut found.shared_servers };
                    servers.insert(process.pid, cpu);
                }
                found.servers.push(server.classified);
            }
        }
        self.command_lines.retain(|key, _| alive.contains(key));
        found
    }

    /// Reads a job member: whether it is a build client, and, when it runs as a build server's image, the server the
    /// priority pass moves. `MSBuild.exe` is a client by its name alone, so no line is read for it, and so is a
    /// `VBCSCompiler.exe` a build server by its name alone; a `dotnet.exe` is a client unless its command line names
    /// `--no-build`. A hosted member whose times cannot be read has no key, so it is neither client nor server.
    fn member(
        &mut self,
        process: &ProcessInfo,
        times: &impl Fn(u32) -> io::Result<Option<ProcessTimes>>,
        read: &impl Fn(u32) -> Option<String>,
        alive: &mut HashSet<(u32, u64)>,
        found: &mut Found,
    ) {
        let compiler = self.is_compiler_image(&process.name);
        let (line, created) = if compiler || process.name.eq_ignore_ascii_case(DOTNET) {
            let Ok(Some(times)) = times(process.pid) else {
                return;
            };
            let key = (process.pid, times.created);
            alive.insert(key);
            let line = if compiler { String::new() } else { self.line(key, process.pid, read) };
            (line, Some(times.created))
        } else {
            // The empty line is what the classifier takes for a name it reads no line for, as for an unreadable one.
            (String::new(), None)
        };
        found.build_client |= is_build_client(&process.name, &line);
        if let Some(created) = created
            && is_server(&process.name, self.test_image.as_deref(), || line.clone())
        {
            found.servers.push(Classified {
                pid: process.pid,
                name: process.name.clone(),
                created,
                inside: true,
            });
        }
    }

    /// The build server `process` is outside the job, if the sample classifies it: `None` for a process that is no
    /// server, one whose times cannot be read, and one whose program lies outside the scoped folder. An `MSBuild` node
    /// already running is a build server nothing counts, so it comes back with `counted` unset. Its command line's key
    /// is kept alive once a line was read for it.
    fn server_of(
        &mut self,
        process: &ProcessInfo,
        scope: &Scope<'_>,
        times: &impl Fn(u32) -> io::Result<Option<ProcessTimes>>,
        read: &impl Fn(u32) -> Option<String>,
        alive: &mut HashSet<(u32, u64)>,
    ) -> Option<Server> {
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
        let counted = if times.created >= scope.since {
            Some((true, times.cpu))
        } else {
            is_compiler_server(&process.name, &line, test_image).then_some((false, times.cpu))
        };
        Some(Server {
            counted,
            classified: Classified {
                pid: process.pid,
                name: process.name.clone(),
                created: times.created,
                inside: false,
            },
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

    /// Whether any of `escaped`, the processes outside the job under its members a sample recorded, is a build client,
    /// as a job member is one ([`is_build_client`]): `MSBuild.exe` by its name alone, and a `dotnet.exe` unless its
    /// command line names `--no-build`, on the line a sample of this watch read for its pid and start, else the one
    /// `read` reads; a line that cannot be read is the empty string the classifier takes, as for a member.
    pub(crate) fn holds_client(&self, escaped: &[&ProcessInfo], read: impl Fn(u32) -> Option<String>) -> bool {
        escaped.iter().any(|process| {
            let line = if process.name.eq_ignore_ascii_case(DOTNET) {
                self.known_line(process).cloned().unwrap_or_else(|| read(process.pid)).unwrap_or_default()
            } else {
                String::new()
            };
            is_build_client(&process.name, &line)
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
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    use gate_win32::ProcessTimes;
    use proc_table::ProcessInfo;

    use super::{Scope, Servers};

    /// `ERROR_ACCESS_DENIED`, as a raw OS error.
    const ACCESS_DENIED: i32 = 5;

    /// The `FILETIME` ticks of `seconds` after the Unix epoch, as the watch's `since` and a process's own start time
    /// are held.
    fn ticks(seconds: i64) -> i64 {
        seconds * 10_000_000 + 116_444_736_000_000_000
    }

    #[test]
    fn an_escaped_build_client_holds_the_servers_as_one_in_the_job_does() {
        let servers = Servers::new(None);
        let escaped = |name: &str, pid: u32| ProcessInfo {
            pid,
            parent_pid: 1,
            name: name.to_owned(),
            command_line: None,
            created: None,
        };
        let line = |pid: u32| match pid {
            2 => Some("dotnet build".to_owned()),
            3 => Some("dotnet test --no-build".to_owned()),
            _ => None,
        };

        assert!(servers.holds_client(&[&escaped("dotnet.exe", 2)], line), "a dotnet that builds");
        assert!(
            servers.holds_client(&[&escaped("pwsh.exe", 4), &escaped("MSBuild.exe", 5)], line),
            "MSBuild by its name alone"
        );
        assert!(
            servers.holds_client(&[&escaped("dotnet.exe", 6)], line),
            "a dotnet whose line cannot be read"
        );
        assert!(
            !servers.holds_client(&[&escaped("dotnet.exe", 3), &escaped("pwsh.exe", 4)], line),
            "a dotnet with --no-build, and no build tool, hold none"
        );
    }

    /// The `since` of a command that started `seconds` after the epoch.
    fn since(seconds: i64) -> u64 {
        ticks(seconds).cast_unsigned()
    }

    /// A process of the injected snapshot, named `name`: what decides whether it is new is its own times, not the
    /// start a command-line-free snapshot reports.
    fn process(pid: u32, name: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid: 0,
            name: name.to_owned(),
            command_line: None,
            created: None,
        }
    }

    /// The times of a process that started `seconds` after the epoch and has used `cpu` ticks.
    fn times_at(seconds: i64, cpu: u64) -> ProcessTimes {
        ProcessTimes {
            created: since(seconds),
            cpu,
        }
    }

    /// The times of a test's processes, `cpu` ticks each of them: the pid's own CPU is its pid times ten. The `Result`
    /// is the shape the reader the watch passes has, and this double never fails.
    #[expect(clippy::unnecessary_wraps, reason = "the reader the watch passes is fallible; this double always answers")]
    fn times(pid: u32) -> std::io::Result<Option<ProcessTimes>> {
        Ok(Some(times_at(20, u64::from(pid) * 10)))
    }

    /// The scope of a read of a command that started 10 s after the epoch, whose job holds `members`, its server scan
    /// confined to the folder `under`.
    fn scope<'a>(members: &'a BTreeSet<u32>, under: Option<&'a Path>) -> Scope<'a> {
        Scope {
            members,
            since: since(10),
            under,
        }
    }

    #[test]
    fn build_servers_are_split_into_the_new_and_the_shared_by_their_start_time() {
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
        // Pid 1 started before the command, every other process after it.
        let times = |pid: u32| Ok(Some(times_at(if pid == 1 { 5 } else { 20 }, u64::from(pid) * 10)));

        let found = servers.read(&processes, &scope(&BTreeSet::new(), None), times, lines);

        assert_eq!(
            found.new_servers,
            BTreeMap::from([(2, 20), (3, 30)]),
            "the servers started after the command"
        );
        assert_eq!(found.shared_servers, BTreeMap::from([(1, 10)]), "the compiler server already running");
        assert!(!found.build_client, "no member is a client");
    }

    #[test]
    fn an_old_msbuild_node_is_left_out_while_a_new_one_counts() {
        let mut servers = Servers::new(None);
        let processes = [process(1, "dotnet.exe"), process(2, "dotnet.exe")];
        let lines = |_pid: u32| Some("dotnet MSBuild.dll /nodemode:1 /nodeReuse:false".to_owned());
        let times = |pid: u32| Ok(Some(times_at(if pid == 1 { 5 } else { 20 }, u64::from(pid) * 10)));

        let found = servers.read(&processes, &scope(&BTreeSet::new(), None), times, lines);

        assert_eq!(found.new_servers, BTreeMap::from([(2, 20)]), "the node the command started");
        assert!(
            found.shared_servers.is_empty(),
            "an MSBuild node already running is no compiler server (B1)"
        );
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

        assert_eq!(
            found.new_servers,
            BTreeMap::new(),
            "a denied server and one that has gone are both skipped"
        );
        assert_eq!(found.shared_servers, BTreeMap::new());
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

        assert_eq!(found.new_servers, BTreeMap::new(), "a process whose start cannot be read is no server");
        assert_eq!(found.shared_servers, BTreeMap::new());
        assert_eq!(reads.get(), 0, "and no command line is read for it");
    }

    #[test]
    fn a_server_that_started_exactly_when_the_command_did_is_new() {
        let mut servers = Servers::new(None);
        let lines = |_pid: u32| None;
        let times = |_pid: u32| Ok(Some(times_at(10, 15)));

        let found = servers.read(&[process(1, "VBCSCompiler.exe")], &scope(&BTreeSet::new(), None), times, lines);

        assert_eq!(found.new_servers, BTreeMap::from([(1, 15)]), "a start equal to `since` counts as new");
        assert_eq!(found.shared_servers, BTreeMap::new());
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

        servers.read(&processes, &scope(&BTreeSet::new(), None), |_pid| Ok(Some(times_at(20, 10))), lines);
        assert_eq!(reads.get(), 1);
        // The same pid, now started later: another process holds it, and its own line is read for it.
        servers.read(&processes, &scope(&BTreeSet::new(), None), |_pid| Ok(Some(times_at(25, 20))), lines);
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
    fn a_build_client_is_msbuild_or_a_dotnet_host_without_no_build() {
        let mut servers = Servers::new(None);
        let processes = [process(1, "MSBuild.exe"), process(2, "dotnet.exe"), process(3, "dotnet.exe")];
        let lines = |pid: u32| match pid {
            1 => Some("MSBuild.exe My.sln".to_owned()),
            2 => Some("dotnet test tests/ --no-build".to_owned()),
            _ => None,
        };

        for (members, want) in [(BTreeSet::from([1]), true), (BTreeSet::from([2]), false), (BTreeSet::from([3]), true)] {
            let found = servers.read(&processes, &scope(&members, None), times, lines);
            assert_eq!(found.build_client, want, "the client of {members:?}");
        }
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

        assert!(found.build_client, "MSBuild.exe in the job is a client by its name");
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

        assert_eq!(found.new_servers, BTreeMap::from([(1, 10)]), "only the program under the folder counts");
        assert_eq!(found.shared_servers, BTreeMap::new());
    }

    #[test]
    fn a_job_member_is_a_client_wherever_its_program_lives() {
        let mut servers = Servers::new(None);
        let processes = [process(3, "dotnet.exe")];
        let lines = |_pid: u32| Some(r"dotnet C:\work\elsewhere\My.sln".to_owned());
        let members = BTreeSet::from([3]);

        let scope = scope(&members, Some(Path::new(r"C:\work\case")));
        let found = servers.read(&processes, &scope, times, lines);

        assert!(found.build_client, "the folder narrows the server scan, not the build-client check");
    }
}
