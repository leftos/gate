//! The classifiers that tell which of a sample's processes are build servers, which are compiler servers, which in a
//! job is a build client, and which command is a WSL one, from their names and command lines alone: the port of the
//! PowerShell gate's `Test-BuildServer`, `Test-CompilerServer`, `Test-BuildClient` and its WSL test. Every comparison
//! ignores case, as PowerShell's `-eq` and `-match` do.

/// Whether a process named `name`, whose command line is `command_line`, is a build server: `VBCSCompiler.exe`, or a
/// `dotnet.exe` running `VBCSCompiler.dll` or an `MSBuild` node.
#[must_use]
pub fn is_build_server(name: &str, command_line: &str) -> bool {
    if !name.eq_ignore_ascii_case("dotnet.exe") {
        return name.eq_ignore_ascii_case("VBCSCompiler.exe");
    }
    is_compiler_server(name, command_line) || (holds(command_line, "msbuild.dll") && holds(command_line, "nodemode"))
}

/// Whether a process named `name`, whose command line is `command_line`, is a compiler server: `VBCSCompiler.exe`, or one
/// running `VBCSCompiler.dll`. Only one of these counts when it was created before the command, and an `MSBuild` node is
/// never one of them. A caller asks it only of a process [`is_build_server`] already accepted, as the PowerShell gate's
/// `Test-CompilerServer` runs after its `Test-BuildServer`.
#[must_use]
pub fn is_compiler_server(name: &str, command_line: &str) -> bool {
    name.eq_ignore_ascii_case("VBCSCompiler.exe") || holds(command_line, "vbcscompiler.dll")
}

/// Whether a process named `name`, whose command line is `command_line`, is a build client, one that may hand its
/// compile to a compiler server outside the job: `MSBuild.exe`, or a `dotnet.exe` whose command line lacks
/// `--no-build`, since a `dotnet test --no-build` compiles nothing.
#[must_use]
pub fn is_build_client(name: &str, command_line: &str) -> bool {
    if name.eq_ignore_ascii_case("MSBuild.exe") {
        return true;
    }
    name.eq_ignore_ascii_case("dotnet.exe") && !holds(command_line, "--no-build")
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
            assert_eq!(is_build_server(name, command_line), want, "{name}: {command_line}");
        }
    }

    /// An `MSBuild` node that was already running is shared CPU, never a compiler server (B1).
    #[test]
    fn compiler_servers_exclude_an_old_msbuild_node() {
        let rows: [(&str, &str, bool); 5] = [
            ("VBCSCompiler.exe", "cmd.exe /d /c spin", true),
            ("dotnet.exe", r"dotnet C:\sdk\Roslyn\VBCSCompiler.dll /shared", true),
            ("DOTNET.EXE", "dotnet vbcSCompiler.DLL", true),
            ("dotnet.exe", "dotnet MSBuild.dll /nodemode:1", false),
            ("dotnet.exe", "dotnet build", false),
        ];
        for (name, command_line, want) in rows {
            assert_eq!(is_compiler_server(name, command_line), want, "{name}: {command_line}");
        }
    }

    /// Test-NoBuildClient: a `dotnet test --no-build` compiles nothing, so it is no client; a line that could not be
    /// read holds no `--no-build` either, as the PowerShell gate reads the empty string it gets then.
    #[test]
    fn build_clients_are_msbuild_or_dotnet_without_no_build() {
        let rows: [(&str, &str, bool); 5] = [
            ("MSBuild.exe", "MSBuild.exe My.sln", true),
            ("dotnet.exe", "dotnet build", true),
            ("dotnet.exe", "dotnet test tests/ --no-build", false),
            ("dotnet.exe", "", true),
            ("cargo.exe", "cargo build", false),
        ];
        for (name, command_line, want) in rows {
            assert_eq!(is_build_client(name, command_line), want, "{name}: {command_line}");
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
}
