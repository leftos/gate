//! The classifiers that tell which of a sample's processes are build servers and which command is a WSL one, from their
//! names and command lines alone: the port of the PowerShell gate's `Test-BuildServer` and its WSL test. Here too is the
//! rule a watch moves a build server's priority class by, the port of its `Get-ServerTarget` and `Test-ServerLeft`.
//! Every comparison ignores case, as PowerShell's `-eq` and `-match` do.

/// Whether a process named `name`, whose command line is `command_line`, is a build server: `VBCSCompiler.exe`, or a
/// `dotnet.exe` running `VBCSCompiler.dll` or an `MSBuild` node. `test_image` is one more image file name a test gate
/// reads as a compiler server's (`--test-server-image`), `None` outside the suite.
#[must_use]
pub fn is_build_server(name: &str, command_line: &str, test_image: Option<&str>) -> bool {
    if !name.eq_ignore_ascii_case("dotnet.exe") {
        return is_compiler_image(name, test_image);
    }
    is_compiler_server(name, command_line, test_image) || (holds(command_line, "msbuild.dll") && holds(command_line, "nodemode"))
}

/// Whether a process named `name`, whose command line is `command_line`, is a compiler server: `VBCSCompiler.exe`, or one
/// running `VBCSCompiler.dll`. An `MSBuild` node is never one of them. `test_image` is read as [`is_build_server`] reads
/// it.
fn is_compiler_server(name: &str, command_line: &str, test_image: Option<&str>) -> bool {
    is_compiler_image(name, test_image) || holds(command_line, "vbcscompiler.dll")
}

/// Whether `name` is a compiler server's image by its name alone: `VBCSCompiler.exe`, or `test_image` when a test gate
/// gives one, each ignoring case.
#[must_use]
pub fn is_compiler_image(name: &str, test_image: Option<&str>) -> bool {
    name.eq_ignore_ascii_case("VBCSCompiler.exe") || test_image.is_some_and(|image| name.eq_ignore_ascii_case(image))
}

/// A process's priority class, as the Windows calls read and set it and as no one of them names it here: what a
/// watch moves a build server between.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PriorityClass {
    /// The idle class, below every other.
    Idle,
    /// The below-normal class.
    BelowNormal,
    /// The normal class.
    Normal,
    /// The above-normal class.
    AboveNormal,
    /// The high class.
    High,
    /// The realtime class, above every other.
    RealTime,
}

/// The class a build server now in `class` is set to under a gate of the priority `high`, or `None` to leave it: a High
/// gate — a critical gate's, or one under it — raises one below High to High, and any other gate lowers one above
/// below-normal to below-normal, as the PowerShell gate's `Get-ServerTarget` reads it. A class that could not be read
/// is `None`, unknown: a High gate raises it as one below High, and any other gate leaves it, as PowerShell reads the
/// empty class .NET's failed getter leaves.
#[must_use]
pub fn server_target(high: bool, class: Option<PriorityClass>) -> Option<PriorityClass> {
    if high {
        return (!matches!(class, Some(PriorityClass::High | PriorityClass::RealTime))).then_some(PriorityClass::High);
    }
    matches!(
        class,
        Some(PriorityClass::Normal | PriorityClass::AboveNormal | PriorityClass::High | PriorityClass::RealTime)
    )
    .then_some(PriorityClass::BelowNormal)
}

/// Whether a build server of `class`, `None` when unknown, stays as it is this pass: one in a job the guard lists, or
/// — under a gate that is not High — one at High while a critical gate holds the machine, whose own watch raised it.
/// `guarded_job` is another live gate's job under a High gate, and the job of a critical gate holding the machine under
/// any other, as the PowerShell gate's `Test-ServerLeft` reads the two.
#[must_use]
pub fn server_left(high: bool, class: Option<PriorityClass>, guarded_job: bool, holds: bool) -> bool {
    if high {
        return guarded_job;
    }
    guarded_job || (class == Some(PriorityClass::High) && holds)
}

/// Whether the command's first word `first_word` names `wsl`: its file name with a trailing `.exe` stripped is `wsl`,
/// ignoring case. A path is allowed, and a file name starts after a drive's `:`, as `Path.GetFileName` reads it.
#[must_use]
pub fn is_wsl_command(first_word: &str) -> bool {
    let file = first_word.rsplit(['\\', '/', ':']).next().unwrap_or_default();
    strip_exe(file).eq_ignore_ascii_case("wsl")
}

/// `file` without a trailing `.exe`, ignoring case.
fn strip_exe(file: &str) -> &str {
    if file.len() >= 4 && file.as_bytes()[file.len() - 4..].eq_ignore_ascii_case(b".exe") {
        return &file[..file.len() - 4];
    }
    file
}

/// Whether `text` holds the lowercase ASCII `needle`, ignoring case.
fn holds(text: &str, needle: &str) -> bool {
    text.to_ascii_lowercase().contains(needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rows `gate.ps1`'s `Test-BuildServer` reads, and the ones it does not.
    #[test]
    fn build_servers_are_read_from_the_name_and_the_command_line() {
        let rows: [(&str, &str, bool); 8] = [
            ("VBCSCompiler.exe", "", true),
            ("vbcscompiler.exe", r"C:\Program Files\dotnet\sdk\9.0\Roslyn\VBCSCompiler.exe", true),
            (
                "dotnet.exe",
                r"dotnet C:\Program Files\dotnet\sdk\9.0\Roslyn\VBCSCompiler.dll /shared",
                true,
            ),
            ("dotnet.exe", "dotnet MSBuild.dll /nodemode:1 /nodeReuse:false", true),
            ("dotnet.exe", "dotnet build MSBuild.dll", false),
            ("dotnet.exe", "dotnet test tests/", false),
            ("MSBuild.exe", "MSBuild.exe My.sln", false),
            ("cargo.exe", "cargo build -- VBCSCompiler.dll", false),
        ];
        for (name, command_line, want) in rows {
            assert_eq!(is_build_server(name, command_line, None), want, "{name}: {command_line}");
        }
    }

    /// The image a test gate names (`--test-server-image`) is a compiler server by its name alone, ignoring case, as
    /// `VBCSCompiler.exe` is; with none named, that name is no server.
    #[test]
    fn a_test_server_image_is_a_compiler_server_only_when_named() {
        let named = Some("gate-test-server.exe");
        assert!(is_build_server("gate-test-server.exe", "", named));
        assert!(is_build_server("GATE-TEST-SERVER.EXE", r"X:\case\gate-test-server.exe wait go", named));
        assert!(is_compiler_server("gate-test-server.exe", "", named));
        assert!(!is_build_server("gate-test-server.exe", "", None), "unnamed, it is no server");
        assert!(!is_compiler_server("gate-test-server.exe", "", None), "nor a compiler server");
        assert!(!is_build_server("other.exe", "", named), "another image is still no server");
        assert!(
            is_build_server("VBCSCompiler.exe", "", named),
            "and the real compiler server still is one"
        );
        assert!(
            is_build_server("dotnet.exe", "dotnet MSBuild.dll /nodemode:1", named),
            "and so is an MSBuild node"
        );
    }

    /// An `MSBuild` node is never a compiler server (B1).
    #[test]
    fn compiler_servers_are_told_from_an_msbuild_node() {
        let rows: [(&str, &str, bool); 5] = [
            ("VBCSCompiler.exe", "cmd.exe /d /c spin", true),
            ("dotnet.exe", r"dotnet C:\sdk\Roslyn\VBCSCompiler.dll /shared", true),
            ("DOTNET.EXE", "dotnet vbcSCompiler.DLL", true),
            ("dotnet.exe", "dotnet MSBuild.dll /nodemode:1", false),
            ("dotnet.exe", "dotnet build", false),
        ];
        for (name, command_line, want) in rows {
            assert_eq!(is_compiler_server(name, command_line, None), want, "{name}: {command_line}");
        }
    }

    /// The command's first word names `wsl` by its file name with a trailing `.exe` stripped, a path allowed.
    #[test]
    fn wsl_commands_are_read_from_the_file_name() {
        let rows: [(&str, bool); 9] = [
            ("wsl", true),
            ("wsl.exe", true),
            ("WSL.EXE", true),
            (r"C:\Windows\System32\wsl.exe", true),
            ("C:wsl.exe", true),
            ("/usr/bin/wsl", true),
            ("wslconfig", false),
            ("bash.exe", false),
            ("", false),
        ];
        for (word, want) in rows {
            assert_eq!(is_wsl_command(word), want, "{word}");
        }
    }

    /// The rows `gate.ps1`'s `Get-ServerTarget` reads: which class a build server of a class takes under an ordinary
    /// gate and under a High one, and which it is left at.
    #[test]
    fn the_priority_target_of_a_server_is_read_from_its_class_and_the_gate() {
        let rows: [(bool, PriorityClass, Option<PriorityClass>); 12] = [
            (false, PriorityClass::Idle, None),
            (false, PriorityClass::BelowNormal, None),
            (false, PriorityClass::Normal, Some(PriorityClass::BelowNormal)),
            (false, PriorityClass::AboveNormal, Some(PriorityClass::BelowNormal)),
            (false, PriorityClass::High, Some(PriorityClass::BelowNormal)),
            (false, PriorityClass::RealTime, Some(PriorityClass::BelowNormal)),
            (true, PriorityClass::Idle, Some(PriorityClass::High)),
            (true, PriorityClass::BelowNormal, Some(PriorityClass::High)),
            (true, PriorityClass::Normal, Some(PriorityClass::High)),
            (true, PriorityClass::AboveNormal, Some(PriorityClass::High)),
            (true, PriorityClass::High, None),
            (true, PriorityClass::RealTime, None),
        ];
        for (high, class, want) in rows {
            assert_eq!(server_target(high, Some(class)), want, "high {high}, class {class:?}");
        }
    }

    /// A class that could not be read: an ordinary gate leaves the server, and a High gate raises it, as PowerShell
    /// reads the empty class of .NET's failed getter.
    #[test]
    fn the_priority_target_of_a_server_whose_class_is_unknown_is_a_raise_only_under_a_high_gate() {
        assert_eq!(server_target(false, None), None, "an ordinary gate leaves an unknown class");
        assert_eq!(
            server_target(true, None),
            Some(PriorityClass::High),
            "a High gate raises an unknown class"
        );
        assert!(
            !server_left(false, None, false, true),
            "an unknown class is not High while a critical gate holds"
        );
        assert!(server_left(true, None, true, false), "a High gate still leaves another gate's job");
    }

    /// The rows `gate.ps1`'s `Test-ServerLeft` reads: a server in a listed job, and one at High while a critical gate
    /// holds the machine, each under an ordinary gate and under a High one.
    #[test]
    fn the_priority_guard_leaves_a_listed_job_s_server_and_a_high_one_while_a_critical_gate_holds() {
        let rows: [(bool, PriorityClass, bool, bool, bool); 8] = [
            (false, PriorityClass::Normal, true, false, true),
            (false, PriorityClass::High, true, false, true),
            (false, PriorityClass::High, false, true, true),
            (false, PriorityClass::Normal, false, true, false),
            (false, PriorityClass::High, false, false, false),
            (false, PriorityClass::BelowNormal, false, true, false),
            (true, PriorityClass::Normal, true, false, true),
            (true, PriorityClass::High, false, true, false),
        ];
        for (high, class, guarded_job, holds, want) in rows {
            assert_eq!(server_left(high, Some(class), guarded_job, holds), want, "high {high}, class {class:?}");
        }
    }
}
