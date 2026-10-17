//! The caller's environment a routed run does not send: the names whose setting changes what a build does, and the line
//! the gate says for the caller's names among them.

use crate::escape::escaped;

/// The names of the caller's environment a routed run does not send: `NuGetAudit`, `RUSTFLAGS` and `RUSTDOCFLAGS`, and
/// any name starting `NUGET_`, `MSBUILD`, `CARGO_BUILD_` or `CARGO_PROFILE_`, each compared without regard to ASCII case.
///
/// A command on a builder starts in the builder's environment, never the caller's, so a caller who sets one of these
/// would otherwise get a different build from the one the same command makes on the PC.
const WATCHED_NAMES: [&str; 3] = ["NuGetAudit", "RUSTFLAGS", "RUSTDOCFLAGS"];
/// The prefixes of the names of the caller's environment a routed run does not send ([`WATCHED_NAMES`]).
const WATCHED_PREFIXES: [&str; 4] = ["NUGET_", "MSBUILD", "CARGO_BUILD_", "CARGO_PROFILE_"];

/// The names of `caller`'s environment a routed run does not send: each watched name ([`WATCHED_NAMES`]) minus the names
/// `sent` gives (`--env` sends them) and minus the names `gates` holds (the gate sets them itself, so a gate nested in
/// the command never warns), de-duplicated without regard to ASCII case, keeping the first spelling seen, and sorted by
/// their ASCII-lowercase spelling.
#[must_use]
pub fn dropped_env<'a>(caller: impl IntoIterator<Item = &'a str>, sent: &[&str], gates: &[&str]) -> Vec<String> {
    let mut kept: Vec<&str> = Vec::new();
    for name in caller {
        if !watched(name) || named_by(sent, name) || named_by(gates, name) || named_by(&kept, name) {
            continue;
        }
        kept.push(name);
    }
    kept.sort_by_key(|name| name.to_ascii_lowercase());
    kept.into_iter().map(str::to_owned).collect()
}

/// The line the gate says for the caller's names a routed run does not send (`names`, as [`dropped_env`] keeps them),
/// each escaped ([`escaped`]) so a name cannot break or forge the line; `None` when it sends them all.
#[must_use]
pub fn dropped_env_line(names: &[String]) -> Option<String> {
    (!names.is_empty()).then(|| {
        let shown: Vec<String> = names.iter().map(|name| escaped(name)).collect();
        format!("gate: a routed run does not send the caller's {}; --env NAME sends it", shown.join(", "))
    })
}

/// Whether `name` is one a routed run does not send.
fn watched(name: &str) -> bool {
    WATCHED_NAMES.iter().any(|watched| watched.eq_ignore_ascii_case(name))
        || WATCHED_PREFIXES.iter().any(|prefix| starts_with_ignoring_case(name, prefix))
}

/// Whether any name of `names` is `name`, compared without regard to ASCII case.
fn named_by(names: &[&str], name: &str) -> bool {
    names.iter().any(|other| other.eq_ignore_ascii_case(name))
}

/// Whether `name` starts with `prefix`, compared without regard to ASCII case.
fn starts_with_ignoring_case(name: &str, prefix: &str) -> bool {
    name.get(..prefix.len()).is_some_and(|start| start.eq_ignore_ascii_case(prefix))
}
