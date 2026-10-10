//! The command's environment: the gate's own, with the slot marker and the build-server settings set over it, and the
//! state root set under a non-default root or unset under the default one. The gate's own environment is never changed.

use std::ffi::{OsStr, OsString};
use std::io;

use gate_state::StateRoot;
use gate_win32::{EnvironmentBlock, same_variable_name};

use crate::resolve::ResolvedRoot;
use crate::slots::ThisGate;

/// The variables a command is started with.
pub(crate) struct Variables(Vec<(OsString, OsString)>);

/// A variable set over the gate's environment, or unset when its value is `None`.
type Override = (OsString, Option<OsString>);

impl Variables {
    /// The gate's environment with the overrides of a gate holding the slot `marker` ([`slot_marker`]) under `root`.
    pub(crate) fn for_command(root: &ResolvedRoot, marker: &str) -> Variables {
        Variables(applied(std::env::vars_os().collect(), overrides(root, marker)))
    }

    /// The gate's own environment, unchanged: what a program the gate starts for itself sees.
    pub(crate) fn from_process() -> Variables {
        Variables(std::env::vars_os().collect())
    }

    /// The value the command gets for `name`, names compared as Windows compares them.
    pub(crate) fn get(&self, name: &str) -> Option<&OsStr> {
        let name = OsStr::new(name);
        self.0
            .iter()
            .rev()
            .find(|(set, _)| same_variable_name(set, name))
            .map(|(_, value)| value.as_os_str())
    }

    /// Whether the command's environment sets `name`.
    pub(crate) fn is_set(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// The environment block the command is created with.
    ///
    /// # Errors
    ///
    /// An error of kind [`io::ErrorKind::InvalidInput`] naming a variable no block can carry.
    pub(crate) fn block(&self) -> io::Result<EnvironmentBlock> {
        EnvironmentBlock::new(self.0.iter().cloned())
    }
}

/// What the gate sets over its own environment for the command: the slot marker, the build-server settings that keep
/// `MSBuild`'s worker nodes and server in the command's job, and the state root when it is not the default. Under the
/// default root an inherited `GATE_STATE_DIR` is unset, so a gate nested in the command uses the same root.
fn overrides(root: &ResolvedRoot, marker: &str) -> Vec<Override> {
    let state_dir = (!root.root.is_default()).then(|| root.path.clone().into_os_string());
    vec![
        (OsString::from("GATE_SLOT_HELD"), Some(OsString::from(marker))),
        (OsString::from("MSBUILDDISABLENODEREUSE"), Some(OsString::from("1"))),
        (OsString::from("DOTNET_CLI_USE_MSBUILD_SERVER"), Some(OsString::from("0"))),
        (OsString::from("GATE_STATE_DIR"), state_dir),
    ]
}

/// `variables` with each override applied in turn: every pair of its name removed, names compared as Windows compares
/// them, then the new pair added when it has a value.
fn applied(mut variables: Vec<(OsString, OsString)>, overrides: Vec<Override>) -> Vec<(OsString, OsString)> {
    for (name, value) in overrides {
        variables.retain(|(set, _)| !same_variable_name(set, &name));
        if let Some(value) = value {
            variables.push((name, value));
        }
    }
    variables
}

/// The slot marker of `gate`, a gate of `kind` under `root`: `<kind>:<pid>:<start ms>`, and `:<h>` after it under a
/// non-default root.
pub(crate) fn slot_marker(kind: &str, gate: &ThisGate, root: &StateRoot) -> String {
    let marker = format!("{kind}:{}:{}", gate.pid, gate.start_ms);
    match root.hash() {
        Some(hash) => format!("{marker}:{hash}"),
        None => marker,
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::path::PathBuf;

    use gate_state::StateRoot;
    use gate_win32::same_variable_name;

    use super::{applied, overrides};
    use crate::resolve::ResolvedRoot;

    const DEFAULT: &str = r"C:\Users\someone\AppData\Local\gate\v1";

    fn root(path: &str) -> ResolvedRoot {
        ResolvedRoot {
            path: PathBuf::from(path),
            root: StateRoot::new(path, Some(DEFAULT)),
        }
    }

    fn state_dirs(variables: &[(OsString, OsString)]) -> Vec<&OsString> {
        variables
            .iter()
            .filter(|(name, _)| same_variable_name(name, OsStr::new("GATE_STATE_DIR")))
            .map(|(_, value)| value)
            .collect()
    }

    #[test]
    fn an_inherited_state_dir_is_unset_under_the_default_root_and_replaced_under_another() {
        let path = (OsString::from("Path"), OsString::from(r"C:\bin"));
        let inherited = vec![(OsString::from("gate_state_dir"), OsString::from(r"D:\elsewhere")), path.clone()];

        let under_default = applied(inherited.clone(), overrides(&root(DEFAULT), "heavy:1:2"));
        let under_other = applied(inherited, overrides(&root(r"D:\test-root"), "heavy:1:2:abcdef12"));

        assert_eq!(state_dirs(&under_default), Vec::<&OsString>::new());
        assert_eq!(state_dirs(&under_other), [&OsString::from(r"D:\test-root")]);
        assert!(under_default.contains(&path), "other variables pass through");
        assert!(under_default.contains(&(OsString::from("GATE_SLOT_HELD"), OsString::from("heavy:1:2"))));
    }
}
