//! The installed gate: where `tools/install.ps1` puts it under `LOCALAPPDATA`, and the version its `--version` says,
//! which `gate push` sends to the builders and a stale run hands its request to.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

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
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use super::installed_gate;
    use crate::resolve::Environment;

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
