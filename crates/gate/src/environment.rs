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
    /// The gate's environment with the caller's `--env` pairs `caller` set over it, then the overrides of `gate`,
    /// holding the slot `marker` ([`slot_marker`]) under `root`, so no pair can undo one ([`composed`]).
    pub(crate) fn for_command(root: &ResolvedRoot, gate: &ThisGate, marker: &str, caller: &[(String, String)]) -> Variables {
        let run = run_id(gate, &root.root);
        Variables(composed(std::env::vars_os().collect(), caller, overrides(root, &run, marker)))
    }

    /// The gate's own environment, unchanged: what a program the gate starts for itself sees.
    pub(crate) fn from_process() -> Variables {
        Variables(std::env::vars_os().collect())
    }

    /// This environment with `name` set to `value`, replacing every value it holds for that name, names compared as
    /// Windows compares them, so a name set under another letter case is replaced rather than joined.
    pub(crate) fn with(self, name: &str, value: &str) -> Variables {
        Variables(applied(self.0, vec![(OsString::from(name), Some(OsString::from(value)))]))
    }

    /// This environment with the build-server settings of the run `run` ([`build_servers`]) set over it, replacing
    /// every value it holds for each name, names compared as Windows compares them.
    pub(crate) fn with_build_servers(self, run: &str) -> Variables {
        Variables(applied(self.0, build_servers(run)))
    }

    /// This environment with the caller's `Env` pairs set over it, each pair replacing every value the environment holds
    /// for its name, names compared as Windows compares them. Applied before the gate's own settings, so no pair can
    /// undo one.
    pub(crate) fn with_caller(self, caller: &[(String, String)]) -> Variables {
        Variables(applied(self.0, caller_overrides(caller)))
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

/// What the gate sets over its own environment for the command: the slot marker, the build-server settings of the run
/// `run` ([`build_servers`]), and the state root when it is not the default. Under the default root an inherited
/// `GATE_STATE_DIR` is unset, so a gate nested in the command uses the same root.
fn overrides(root: &ResolvedRoot, run: &str, marker: &str) -> Vec<Override> {
    let state_dir = (!root.root.is_default()).then(|| root.path.clone().into_os_string());
    let mut overrides = vec![(OsString::from("GATE_SLOT_HELD"), Some(OsString::from(marker)))];
    overrides.extend(build_servers(run));
    overrides.push((OsString::from("GATE_STATE_DIR"), state_dir));
    overrides
}

/// The build-server settings of the run `run`: the compiler server's pipe and `MSBuild`'s node handshake are named by
/// the run id, so no other run's command uses the servers this one starts; the `MSBuild` server is off, an inherited
/// `MSBUILDUSESERVER` included; and `MSBuild`'s node reuse is off, so no node idles in the command's job after its
/// build, keeping alive whatever in the command waits for its whole tree.
fn build_servers(run: &str) -> Vec<Override> {
    vec![
        (OsString::from("SharedCompilationId"), Some(OsString::from(run))),
        (OsString::from("MSBUILDNODEHANDSHAKESALT"), Some(OsString::from(run))),
        (OsString::from("MSBUILDUSESERVER"), Some(OsString::from("0"))),
        (OsString::from("DOTNET_CLI_USE_MSBUILD_SERVER"), Some(OsString::from("0"))),
        (OsString::from("MSBUILDDISABLENODEREUSE"), Some(OsString::from("1"))),
    ]
}

/// The run id of `gate` under `root`: `gate-<pid>-<start ms>`, and `-<h>` after it under a non-default root, so a run
/// under another root never shares an id with a run of the machine's own.
fn run_id(gate: &ThisGate, root: &StateRoot) -> String {
    let id = format!("gate-{}-{}", gate.pid, gate.start_ms);
    match root.hash() {
        Some(hash) => format!("{id}-{hash}"),
        None => id,
    }
}

/// The environment a command starts in: `start`, then the caller's pairs `caller` over it, then the gate's own
/// `overrides` last, names compared as Windows compares them at each step.
fn composed(start: Vec<(OsString, OsString)>, caller: &[(String, String)], overrides: Vec<Override>) -> Vec<(OsString, OsString)> {
    applied(applied(start, caller_overrides(caller)), overrides)
}

/// The caller's pairs as overrides, each setting its name.
fn caller_overrides(caller: &[(String, String)]) -> Vec<Override> {
    caller
        .iter()
        .map(|(name, value)| (OsString::from(name), Some(OsString::from(value))))
        .collect()
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

    use super::{Variables, applied, composed, overrides};
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

        let under_default = applied(inherited.clone(), overrides(&root(DEFAULT), "gate-1-2", "heavy:1:2"));
        let under_other = applied(inherited, overrides(&root(r"D:\test-root"), "gate-1-2-abcdef12", "heavy:1:2:abcdef12"));

        assert_eq!(state_dirs(&under_default), Vec::<&OsString>::new());
        assert_eq!(state_dirs(&under_other), [&OsString::from(r"D:\test-root")]);
        assert!(under_default.contains(&path), "other variables pass through");
        assert!(under_default.contains(&(OsString::from("GATE_SLOT_HELD"), OsString::from("heavy:1:2"))));
    }

    #[test]
    fn the_command_gets_its_run_s_compiler_server_id_over_an_inherited_one() {
        let inherited = vec![
            (OsString::from("sharedcompilationid"), OsString::from("someone-else")),
            (OsString::from("MSBuildNodeHandshakeSalt"), OsString::from("work-folder-key")),
        ];

        let variables = Variables(applied(
            inherited,
            overrides(&root(r"D:\test-root"), "gate-1-2-abcdef12", "heavy:1:2:abcdef12"),
        ));

        assert_eq!(variables.get("SharedCompilationId"), Some(OsStr::new("gate-1-2-abcdef12")));
        assert_eq!(variables.get("MSBUILDNODEHANDSHAKESALT"), Some(OsStr::new("gate-1-2-abcdef12")));
        assert_eq!(variables.get("MSBUILDDISABLENODEREUSE"), Some(OsStr::new("1")), "node reuse is off");
        let named = |wanted: &str| {
            variables
                .0
                .iter()
                .filter(|(name, _)| same_variable_name(name, OsStr::new(wanted)))
                .count()
        };
        assert_eq!(
            (named("SharedCompilationId"), named("MSBUILDNODEHANDSHAKESALT")),
            (1, 1),
            "the inherited values are gone"
        );
    }

    #[test]
    fn with_build_servers_replaces_inherited_values_under_any_case() {
        let inherited = vec![
            (OsString::from("sharedcompilationid"), OsString::from("x")),
            (OsString::from("MSBuildUseServer"), OsString::from("1")),
            (OsString::from("MSBuildDisableNodeReuse"), OsString::from("0")),
        ];

        let variables = Variables(inherited).with_build_servers("gate-box-ab");

        let named = |wanted: &str| {
            variables
                .0
                .iter()
                .filter(|(name, _)| same_variable_name(name, OsStr::new(wanted)))
                .map(|(_, value)| value.as_os_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(named("SharedCompilationId"), [OsStr::new("gate-box-ab")]);
        assert_eq!(named("MSBUILDNODEHANDSHAKESALT"), [OsStr::new("gate-box-ab")]);
        assert_eq!(named("MSBUILDUSESERVER"), [OsStr::new("0")]);
        assert_eq!(named("DOTNET_CLI_USE_MSBUILD_SERVER"), [OsStr::new("0")]);
        assert_eq!(named("MSBUILDDISABLENODEREUSE"), [OsStr::new("1")]);
    }

    #[test]
    fn with_replaces_a_set_variable_whatever_its_letter_case_and_adds_an_unset_one() {
        let path = (OsString::from("Path"), OsString::from(r"C:\bin"));
        let inherited = vec![(OsString::from("msbuildnodehandshakesalt"), OsString::from("other")), path.clone()];
        let set = (OsString::from("MSBUILDNODEHANDSHAKESALT"), OsString::from("k"));

        let replaced = Variables(inherited).with("MSBUILDNODEHANDSHAKESALT", "k");
        let added = Variables(vec![path.clone()]).with("MSBUILDNODEHANDSHAKESALT", "k");

        assert_eq!(replaced.0, [path.clone(), set.clone()], "the value set under another letter case is gone");
        assert_eq!(added.0, [path, set], "an unset name is added");
    }

    fn pairs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect()
    }

    #[test]
    fn the_caller_s_pairs_come_over_the_start_environment_and_under_the_gate_s_own() {
        let start = vec![
            (OsString::from("nugetaudit"), OsString::from("true")),
            (OsString::from("Path"), OsString::from(r"C:\bin")),
        ];
        let caller = pairs(&[("NuGetAudit", "false"), ("GATE_SLOT_HELD", "forged"), ("EMPTY", "")]);

        let variables = Variables(composed(start, &caller, overrides(&root(DEFAULT), "gate-1-2", "heavy:1:2")));

        assert_eq!(
            variables.get("NUGETAUDIT"),
            Some(OsStr::new("false")),
            "the caller's value replaces the inherited one"
        );
        assert_eq!(
            variables.get("GATE_SLOT_HELD"),
            Some(OsStr::new("heavy:1:2")),
            "the gate's own setting comes last"
        );
        assert_eq!(variables.get("EMPTY"), Some(OsStr::new("")), "an empty value is set");
        assert_eq!(variables.get("Path"), Some(OsStr::new(r"C:\bin")), "other variables pass through");
    }
}
