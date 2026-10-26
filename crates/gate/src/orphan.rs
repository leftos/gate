//! The refusal of a gate started under an orphaned script: at start, a gate with no live parent gate walks its own
//! ancestors to the first one whose parent is gone, stepping over Git Bash's fork stub to a live MSYS parent as the
//! MSYS runtime records it, and refuses when that one is a shell running a script or a command, whose caller was killed
//! and left the script stepping on alone. An elevated gate is not judged: an agent's killed sequence never runs
//! elevated, and `sudo`'s elevated side exits once it has started its command.

use std::io;

use gate_win32::MsysParent;
use jiff::Timestamp;
use proc_table::{ProcessInfo, ProcessTree, Win32ProcessTable, command_line_of};

/// The shell running a script or a command that a gate's ancestor walk ended at, its parent gone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Orphan {
    /// The shell's executable name, as the process table gives it.
    pub image: String,
    /// The shell's pid.
    pub pid: u32,
    /// The pid of the parent it records, which is gone.
    pub parent_pid: u32,
}

impl Orphan {
    /// The line a gate refused under this orphan says.
    pub(crate) fn line(&self) -> String {
        let Orphan { image, pid, parent_pid } = self;
        format!(
            "gate: refused: the script that started this gate lost its caller ({image} {pid}, whose parent {parent_pid} is gone); \
             stop the sequence with gate.ps1 stop-tree {pid}"
        )
    }
}

/// The most live MSYS parents a walk steps to; one that would take more refuses nothing.
const MSYS_STEPS: usize = 16;

/// The orphaned script this process was started under, read from the machine's process table and the MSYS runtime's
/// records: `None` when the walk ends anywhere else, or when the snapshot, this process's place in it, an MSYS record
/// or the orphan's command line cannot be read. An elevated gate is not judged, since an agent's killed sequence never
/// runs elevated and `sudo`'s elevated side exits once it has started its command.
pub(crate) fn orphaned_caller() -> Option<Orphan> {
    orphan_unless_elevated(gate_win32::token_facts().map(|facts| facts.elevated), || {
        let processes = Win32ProcessTable.try_snapshot_without_command_lines().ok()?;
        orphan_of(
            processes,
            std::process::id(),
            |process| {
                let line = command_line_of(process.pid)?;
                Some(gate_state::split_words(&line).into_iter().skip(1).collect())
            },
            gate_win32::msys_parent,
        )
    })
}

/// What `walk` finds, unless `elevated`, this process's own token's elevation as it reads it, says the gate runs
/// elevated: an elevated gate walks nothing, and one whose elevation cannot be read is judged as an unelevated one is.
fn orphan_unless_elevated(elevated: io::Result<bool>, walk: impl FnOnce() -> Option<Orphan>) -> Option<Orphan> {
    if elevated.is_ok_and(|is_elevated| is_elevated) {
        return None;
    }
    walk()
}

/// The orphan `own`'s ancestor walk over `processes` ends at: the last process of [`ProcessTree::ancestors`], when its
/// parent is missing from the snapshot or started after it, and it is a shell that `arguments` (its words after the
/// program, `None` when they cannot be read) show running a script or a command. `arguments` is called only for a
/// process whose image is a listed shell.
///
/// A last process whose parent is gone is first asked of `msys`, the MSYS runtime's record of its parent by Windows
/// pid: a live MSYS parent in the snapshot, started no later than it, continues the walk from that parent (at most
/// [`MSYS_STEPS`] times, past which nothing is refused), as Git Bash's fork stub leaves every script's bash with a gone
/// Windows parent; a record that cannot be read refuses nothing; no record, a parent from outside MSYS, or a gone MSYS
/// parent judge the process as above. `msys` is called only for a process whose parent is gone.
pub(crate) fn orphan_of(
    processes: Vec<ProcessInfo>,
    own: u32,
    arguments: impl FnOnce(&ProcessInfo) -> Option<Vec<String>>,
    msys: impl Fn(u32) -> MsysParent,
) -> Option<Orphan> {
    let tree = ProcessTree::new(processes);
    let mut last = tree.ancestors(own).last().or_else(|| tree.get(own))?;
    let mut steps = 0;
    while parent_gone(&tree, last) {
        let parent = match msys(last.pid) {
            MsysParent::NotMsys | MsysParent::FromWindows | MsysParent::Gone => None,
            MsysParent::Unreadable => return None,
            MsysParent::Alive(windows_pid) => tree.get(windows_pid).filter(|parent| !started_after(parent.created, last.created)),
        };
        let Some(parent) = parent else {
            return judged(last, arguments);
        };
        if steps == MSYS_STEPS {
            return None;
        }
        steps += 1;
        last = tree.ancestors(parent.pid).last().unwrap_or(parent);
    }
    None
}

/// Whether `process`'s parent is missing from `tree` or started after it.
fn parent_gone(tree: &ProcessTree, process: &ProcessInfo) -> bool {
    tree.get(process.parent_pid)
        .is_none_or(|parent| started_after(parent.created, process.created))
}

/// The orphan `last`, whose parent is gone, is: `None` unless it is a shell that `arguments` show running a script or a
/// command.
fn judged(last: &ProcessInfo, arguments: impl FnOnce(&ProcessInfo) -> Option<Vec<String>>) -> Option<Orphan> {
    let shell = Shell::of(&last.name)?;
    let words = arguments(last)?;
    shell.runs_a_script(&words).then(|| Orphan {
        image: last.name.clone(),
        pid: last.pid,
        parent_pid: last.parent_pid,
    })
}

/// Whether both start times are known and the parent's is later: a parent that started after its child took the pid
/// of the child's real parent, which is gone.
fn started_after(parent: Option<Timestamp>, child: Option<Timestamp>) -> bool {
    match (parent, child) {
        (Some(parent), Some(child)) => parent > child,
        (None, _) | (_, None) => false,
    }
}

/// A shell whose arguments say whether it runs a script or a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shell {
    /// `bash.exe`, `sh.exe`, `zsh.exe` or `dash.exe`.
    Posix,
    /// `pwsh.exe` or `powershell.exe`.
    Pwsh,
    /// `cmd.exe`.
    Cmd,
}

/// The PowerShell switches that run a script or a command, matched in any case.
const POWERSHELL_RUNS: [&str; 7] = ["-File", "-f", "-Command", "-c", "-EncodedCommand", "-e", "-ec"];
/// The PowerShell switches that keep the session open after its script or command, so it is interactive whatever else
/// it carries, matched in any case.
const POWERSHELL_STAYS: [&str; 2] = ["-NoExit", "-noe"];

impl Shell {
    /// The shell an executable name is, in any case, or `None` for any other program.
    fn of(image: &str) -> Option<Shell> {
        let image = image.to_ascii_lowercase();
        match image.as_str() {
            "bash.exe" | "sh.exe" | "zsh.exe" | "dash.exe" => Some(Shell::Posix),
            "pwsh.exe" | "powershell.exe" => Some(Shell::Pwsh),
            "cmd.exe" => Some(Shell::Cmd),
            _ => None,
        }
    }

    /// Whether the shell's `words`, after its program, run a script or a command rather than an interactive session.
    fn runs_a_script(self, words: &[String]) -> bool {
        match self {
            Shell::Posix => words.iter().any(|word| word == "-c" || !word.starts_with('-')),
            Shell::Pwsh
                if words
                    .iter()
                    .any(|word| POWERSHELL_STAYS.iter().any(|switch| word.eq_ignore_ascii_case(switch))) =>
            {
                false
            }
            Shell::Pwsh => {
                words
                    .iter()
                    .any(|word| POWERSHELL_RUNS.iter().any(|switch| word.eq_ignore_ascii_case(switch)))
                    || words.first().is_some_and(|word| !word.starts_with('-'))
            }
            Shell::Cmd => words
                .iter()
                .any(|word| word.eq_ignore_ascii_case("/c") || word.eq_ignore_ascii_case("/r")),
        }
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test timestamp that does not parse must fail loudly")]
mod tests {
    use std::io;

    use gate_win32::MsysParent;
    use jiff::Timestamp;
    use proc_table::ProcessInfo;

    use super::{Orphan, orphan_of, orphan_unless_elevated};

    /// The gate's pid in every chain.
    const GATE: u32 = 100;
    /// The launcher's pid between the gate and the shell.
    const LAUNCHER: u32 = 90;
    /// The shell's pid.
    const SHELL: u32 = 80;
    /// The pid of the shell's parent, which most chains leave out of the table.
    const CALLER: u32 = 70;

    fn ts(text: &str) -> Timestamp {
        text.parse().expect("a test timestamp parses")
    }

    fn process(pid: u32, parent_pid: u32, name: &str, created: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid,
            name: name.to_owned(),
            command_line: None,
            created: Some(ts(created)),
        }
    }

    /// The gate under a launcher pwsh under `shell`, whose parent `CALLER` is not in the table.
    fn chain(shell: &str) -> Vec<ProcessInfo> {
        vec![
            process(SHELL, CALLER, shell, "2026-10-01T10:00:00Z"),
            process(LAUNCHER, SHELL, "pwsh.exe", "2026-10-01T10:00:01Z"),
            process(GATE, LAUNCHER, "gate.exe", "2026-10-01T10:00:02Z"),
        ]
    }

    /// The words an argument reader that could read the command line gives.
    #[expect(
        clippy::unnecessary_wraps,
        reason = "the type an argument reader returns, so a closure can return it as it is"
    )]
    fn words(text: &[&str]) -> Option<Vec<String>> {
        Some(text.iter().map(|word| (*word).to_owned()).collect())
    }

    /// What `orphan_of` finds when the MSYS runtime has no record of a process whose parent is gone, asserted to be
    /// what it finds when the record says that process was started from outside MSYS.
    fn walk(processes: Vec<ProcessInfo>, own: u32, mut arguments: impl FnMut(&ProcessInfo) -> Option<Vec<String>>) -> Option<Orphan> {
        let not_msys = orphan_of(processes.clone(), own, &mut arguments, |_| MsysParent::NotMsys);
        let from_windows = orphan_of(processes, own, &mut arguments, |_| MsysParent::FromWindows);
        assert_eq!(
            not_msys, from_windows,
            "a process started from outside MSYS is judged as one MSYS has no record of"
        );
        not_msys
    }

    /// The pid of the script bash's MSYS parent, the outer bash, in the MSYS chains.
    const OUTER: u32 = 60;
    /// The pid of the outer bash's own parent.
    const TOP: u32 = 50;

    /// A [`chain`] whose `bash.exe` shell has the MSYS parent `OUTER`, a bash started at `outer_created` whose Windows
    /// parent is `TOP`, a `top` started before both, whose own parent is not in the table.
    fn msys_chain(outer_created: &str, top: &str) -> Vec<ProcessInfo> {
        let mut processes = chain("bash.exe");
        processes.push(process(OUTER, TOP, "bash.exe", outer_created));
        processes.push(process(TOP, 1, top, "2026-10-01T08:00:00Z"));
        processes
    }

    /// An MSYS reader that gives `parent` for the pid `child` and no record for any other.
    fn msys_of(child: u32, parent: MsysParent) -> impl Fn(u32) -> MsysParent {
        move |pid| if pid == child { parent } else { MsysParent::NotMsys }
    }

    #[test]
    fn a_live_msys_parent_continues_the_walk() {
        assert_eq!(
            orphan_of(
                msys_chain("2026-10-01T09:00:00Z", "claude.exe"),
                GATE,
                |_| words(&["ci.sh"]),
                msys_of(SHELL, MsysParent::Alive(OUTER))
            ),
            None,
            "a script whose outer shell and its parent live has its caller"
        );
    }

    #[test]
    fn a_live_msys_parent_started_after_the_child_counts_as_gone() {
        assert_eq!(
            orphan_of(
                msys_chain("2026-10-01T11:00:00Z", "claude.exe"),
                GATE,
                |_| words(&["ci.sh"]),
                msys_of(SHELL, MsysParent::Alive(OUTER))
            ),
            orphan("bash.exe")
        );
        assert_eq!(
            orphan_of(chain("bash.exe"), GATE, |_| words(&["ci.sh"]), msys_of(SHELL, MsysParent::Alive(OUTER))),
            orphan("bash.exe"),
            "a live MSYS parent missing from the snapshot counts as gone"
        );
    }

    #[test]
    fn a_gone_msys_parent_judges_the_shell() {
        let msys = || msys_of(SHELL, MsysParent::Gone);
        assert_eq!(orphan_of(chain("bash.exe"), GATE, |_| words(&["step.sh"]), msys()), orphan("bash.exe"));
        assert_eq!(orphan_of(chain("bash.exe"), GATE, |_| words(&["--login", "-i"]), msys()), None);
    }

    #[test]
    fn an_unreadable_msys_record_is_not_an_orphan() {
        assert_eq!(
            orphan_of(chain("bash.exe"), GATE, |_| words(&["step.sh"]), msys_of(SHELL, MsysParent::Unreadable)),
            None
        );
    }

    #[test]
    fn a_chain_of_live_msys_parents_is_followed() {
        // The outer bash's own Windows parent, `TOP`, is gone too, and its MSYS parent is a third bash that lost its
        // caller: the orphan found is that third bash.
        let mut processes = chain("bash.exe");
        processes.push(process(OUTER, TOP, "bash.exe", "2026-10-01T09:00:00Z"));
        processes.push(process(40, 30, "bash.exe", "2026-10-01T08:00:00Z"));
        let msys = |pid| match pid {
            SHELL => MsysParent::Alive(OUTER),
            OUTER => MsysParent::Alive(40),
            40 => MsysParent::Gone,
            _ => MsysParent::NotMsys,
        };
        assert_eq!(
            orphan_of(processes, GATE, |_| words(&["ci.sh"]), msys),
            Some(Orphan {
                image: "bash.exe".to_owned(),
                pid: 40,
                parent_pid: 30,
            })
        );
    }

    /// A [`chain`] whose shell steps to `steps` live MSYS parents, each a bash with a gone Windows parent started before
    /// the one before it, the last of which lost its caller, with the reader that says so.
    fn msys_steps(steps: u32) -> (Vec<ProcessInfo>, impl Fn(u32) -> MsysParent) {
        let mut processes = chain("bash.exe");
        for step in 1..=steps {
            processes.push(process(
                200 + step,
                1000 + step,
                "bash.exe",
                &format!("2026-10-01T09:{:02}:00Z", 59 - step),
            ));
        }
        let pid_of = |step: u32| if step == 0 { SHELL } else { 200 + step };
        let msys = move |pid: u32| {
            (0..steps)
                .find(|step| pid_of(*step) == pid)
                .map_or(MsysParent::Gone, |step| MsysParent::Alive(pid_of(step + 1)))
        };
        (processes, msys)
    }

    #[test]
    fn more_than_sixteen_live_msys_parents_refuse_nothing() {
        let (processes, msys) = msys_steps(16);
        assert_eq!(
            orphan_of(processes, GATE, |_| words(&["ci.sh"]), msys),
            Some(Orphan {
                image: "bash.exe".to_owned(),
                pid: 216,
                parent_pid: 1016,
            }),
            "sixteen steps are followed"
        );
        let (processes, msys) = msys_steps(17);
        assert_eq!(orphan_of(processes, GATE, |_| words(&["ci.sh"]), msys), None);
    }

    /// The orphan `orphan_of` finds at `shell` in a [`chain`].
    #[expect(clippy::unnecessary_wraps, reason = "the type `orphan_of` returns, so an assertion compares it as it is")]
    fn orphan(shell: &str) -> Option<Orphan> {
        Some(Orphan {
            image: shell.to_owned(),
            pid: SHELL,
            parent_pid: CALLER,
        })
    }

    #[test]
    fn a_bash_script_whose_parent_is_gone_is_an_orphan() {
        assert_eq!(walk(chain("bash.exe"), GATE, |_| words(&["ci-routed.sh"])), orphan("bash.exe"));
        assert_eq!(walk(chain("bash.exe"), GATE, |_| words(&["-c", "./ci.sh"])), orphan("bash.exe"));
        assert_eq!(walk(chain("SH.EXE"), GATE, |_| words(&["--norc", "x.sh"])), orphan("SH.EXE"));
    }

    #[test]
    fn a_pwsh_file_whose_parent_is_gone_is_an_orphan() {
        assert_eq!(
            walk(chain("pwsh.exe"), GATE, |_| words(&["-NoProfile", "-file", "tools/test-all.ps1"])),
            orphan("pwsh.exe")
        );
        assert_eq!(walk(chain("powershell.exe"), GATE, |_| words(&["script.ps1"])), orphan("powershell.exe"));
        assert_eq!(
            walk(chain("pwsh.exe"), GATE, |_| words(&["-NoLogo", "-EC", "ZQBjAGgAbwA="])),
            orphan("pwsh.exe")
        );
    }

    #[test]
    fn a_cmd_c_whose_parent_is_gone_is_an_orphan() {
        let found = walk(chain("cmd.exe"), GATE, |_| words(&["/c", r"X:\case\step.cmd"]));
        assert_eq!(found, orphan("cmd.exe"));
        assert_eq!(
            found.map(|orphan| orphan.line()).as_deref(),
            Some(
                "gate: refused: the script that started this gate lost its caller (cmd.exe 80, whose parent 70 is gone); \
                 stop the sequence with gate.ps1 stop-tree 80"
            )
        );
        assert_eq!(walk(chain("Cmd.exe"), GATE, |_| words(&["/D", "/R", "x.cmd"])), orphan("Cmd.exe"));
    }

    #[test]
    fn an_interactive_shell_whose_parent_is_gone_is_not() {
        assert_eq!(walk(chain("cmd.exe"), GATE, |_| words(&[])), None);
        assert_eq!(walk(chain("pwsh.exe"), GATE, |_| words(&["-NoLogo"])), None);
        assert_eq!(walk(chain("bash.exe"), GATE, |_| words(&["--login", "-i"])), None);
    }

    #[test]
    fn a_chain_that_ends_at_a_non_shell_is_not() {
        let mut asked = false;
        let found = walk(chain("explorer.exe"), GATE, |_| {
            asked = true;
            words(&["/c", "x"])
        });
        assert_eq!(found, None);
        assert!(!asked, "the command line of a process that is no listed shell is never read");
    }

    #[test]
    fn a_parent_started_after_its_child_counts_as_gone() {
        let mut processes = chain("bash.exe");
        processes.push(process(CALLER, 1, "claude.exe", "2026-10-01T11:00:00Z"));
        assert_eq!(walk(processes, GATE, |_| words(&["ci.sh"])), orphan("bash.exe"));

        let mut alive = chain("bash.exe");
        alive.push(process(CALLER, 1, "claude.exe", "2026-10-01T09:00:00Z"));
        assert_eq!(
            walk(alive, GATE, |_| words(&["ci.sh"])),
            None,
            "a shell whose parent started before it still has its caller"
        );
    }

    #[test]
    fn a_pwsh_noexit_command_is_interactive() {
        assert_eq!(
            walk(chain("pwsh.exe"), GATE, |_| words(&["-NoExit", "-Command", r"Set-Location X:\dev"])),
            None
        );
        assert_eq!(walk(chain("powershell.exe"), GATE, |_| words(&["-noe", "-c", "x"])), None);
        assert_eq!(walk(chain("pwsh.exe"), GATE, |_| words(&["-File", "x.ps1", "-NOEXIT"])), None);
        assert_eq!(walk(chain("pwsh.exe"), GATE, |_| words(&["script.ps1", "-NoE"])), None);
    }

    #[test]
    fn a_parent_whose_start_time_is_unknown_is_not_gone() {
        // The shell's parent is present with no start time, so the walk goes past the shell to that parent, itself a
        // script whose own parent is missing: the orphan found is the parent, never the shell.
        let mut processes = chain("bash.exe");
        processes.push(ProcessInfo {
            created: None,
            ..process(CALLER, 1, "bash.exe", "2026-10-01T11:00:00Z")
        });
        assert_eq!(
            walk(processes, GATE, |_| words(&["ci.sh"])),
            Some(Orphan {
                image: "bash.exe".to_owned(),
                pid: CALLER,
                parent_pid: 1,
            })
        );
    }

    #[test]
    fn an_unreadable_command_line_is_not_an_orphan() {
        assert_eq!(walk(chain("bash.exe"), GATE, |_| None), None);
        assert_eq!(
            walk(chain("cmd.exe"), 4242, |_| words(&["/c", "x"])),
            None,
            "a gate missing from its own snapshot"
        );
    }

    /// The orphan an unelevated gate's walk finds, its shell and pids unlike any in a [`chain`].
    fn found_orphan() -> Orphan {
        Orphan {
            image: "pwsh.exe".into(),
            pid: 2,
            parent_pid: 1,
        }
    }

    #[expect(clippy::panic, reason = "a gate that walks despite its elevation fails the case")]
    #[test]
    fn an_elevated_gate_is_never_refused() {
        assert_eq!(orphan_unless_elevated(Ok(true), || panic!("an elevated gate walks nothing")), None);
    }

    #[test]
    fn an_unelevated_gate_is_judged_by_its_walk() {
        assert_eq!(orphan_unless_elevated(Ok(false), || Some(found_orphan())), Some(found_orphan()));
    }

    #[test]
    fn a_gate_whose_token_cannot_be_read_is_judged_by_its_walk() {
        assert_eq!(
            orphan_unless_elevated(Err(io::Error::other("no token")), || Some(found_orphan())),
            Some(found_orphan())
        );
    }
}
