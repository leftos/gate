//! What the binary adds to a parsed request: the state root chosen from `--state-dir`, `GATE_STATE_DIR` or
//! `%LOCALAPPDATA%\gate\v1`, in that order, and paths made absolute against the working directory. Nothing is created.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use gate_state::{StateRoot, default_root};

use crate::environment::{BOX_JOB_VARIABLE, SLOT_HELD_VARIABLE};

/// The environment variables the gate reads.
pub(crate) struct Environment {
    pub state_dir: Option<OsString>,
    pub local_app_data: Option<OsString>,
    /// `GIT_INDEX_FILE`, which git sets for a hook: a gate inside one routes only when `remote.json`'s `hooks` names its
    /// repo.
    pub git_index_file: Option<OsString>,
    /// `GATE_SLOT_HELD`, the slot marker of the gate this one runs under, if any.
    pub slot_held: Option<OsString>,
    /// `GATE_BOX_JOB`, the job of the box run this gate may run inside, if any.
    pub box_job: Option<OsString>,
}

impl Environment {
    pub(crate) fn from_process() -> Environment {
        Environment {
            state_dir: std::env::var_os("GATE_STATE_DIR"),
            local_app_data: std::env::var_os("LOCALAPPDATA"),
            git_index_file: std::env::var_os("GIT_INDEX_FILE"),
            slot_held: std::env::var_os(SLOT_HELD_VARIABLE),
            box_job: std::env::var_os(BOX_JOB_VARIABLE),
        }
    }
}

/// A state root: its absolute path as the gate uses it, and its identity.
pub(crate) struct ResolvedRoot {
    pub path: PathBuf,
    pub root: StateRoot,
}

/// The state root for `flag` (`--state-dir`) and `environment`, or the `gate: ` line saying why there is none.
///
/// The default root, `%LOCALAPPDATA%\gate\v1`, is made absolute the same way as the chosen root before the two are
/// compared. An unreadable LOCALAPPDATA is an error only when no root was given; a given root is then not the default.
pub(crate) fn root(flag: Option<&Path>, environment: &Environment) -> Result<ResolvedRoot, String> {
    let local_app_data = set(environment.local_app_data.as_deref()).map(OsStr::to_str);
    let default = match local_app_data.flatten() {
        Some(value) => Some(absolute(Path::new(&default_root(value)), "the default state root")?),
        None => None,
    };
    let path = match (flag, set(environment.state_dir.as_deref()), &default) {
        (Some(flag), _, _) => absolute(flag, "the state root")?,
        (None, Some(variable), _) => absolute(Path::new(variable), "the state root")?,
        (None, None, Some(default)) => default.clone(),
        (None, None, None) if local_app_data.is_some() => return Err("gate: cannot read the LOCALAPPDATA variable".to_owned()),
        (None, None, None) => return Err("gate: LOCALAPPDATA is not set; give the state root with --state-dir or GATE_STATE_DIR".to_owned()),
    };
    let text = path
        .to_str()
        .ok_or_else(|| format!("gate: cannot read the state root {}", path.display()))?;
    let root = StateRoot::new(text, default.as_deref().and_then(Path::to_str));
    Ok(ResolvedRoot { path, root })
}

/// The log path, absolute against the working directory.
pub(crate) fn log(log: &Path) -> Result<PathBuf, String> {
    absolute(log, "the log path")
}

/// A variable's value, with an empty value read as unset.
fn set(value: Option<&OsStr>) -> Option<&OsStr> {
    value.filter(|value| !value.is_empty())
}

fn absolute(path: &Path, what: &str) -> Result<PathBuf, String> {
    std::path::absolute(path).map_err(|error| format!("gate: cannot make {what} {} absolute: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;

    use super::{Environment, root};

    /// A LOCALAPPDATA value that is not valid Unicode.
    #[cfg(windows)]
    fn unreadable() -> OsString {
        use std::os::windows::ffi::OsStringExt;
        OsString::from_wide(&[u16::from(b'c'), 0xD800])
    }

    #[cfg(unix)]
    fn unreadable() -> OsString {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(vec![b'c', 0xFF])
    }

    #[test]
    fn unreadable_local_app_data_fails_only_without_a_given_root() {
        let given = crate::test_support::temp_dir().join("gate-resolve-test");
        let no_root = Environment {
            state_dir: None,
            local_app_data: Some(unreadable()),
            git_index_file: None,
            slot_held: None,
            box_job: None,
        };
        let variable = Environment {
            state_dir: Some(given.clone().into_os_string()),
            local_app_data: Some(unreadable()),
            git_index_file: None,
            slot_held: None,
            box_job: None,
        };

        assert_eq!(root(None, &no_root).err().as_deref(), Some("gate: cannot read the LOCALAPPDATA variable"));
        assert!(root(Some(Path::new(&given)), &no_root).is_ok_and(|resolved| !resolved.root.is_default()));
        assert!(root(None, &variable).is_ok_and(|resolved| resolved.path == given && !resolved.root.is_default()));
    }
}
