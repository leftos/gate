//! The version this gate names itself by: the package version and the build's own id.

/// The commit this build was made from, from `GATE_BUILD_ID` at compile time; `None` for a build made without one, or
/// with an empty one.
const BUILD_ID: Option<&str> = option_env!("GATE_BUILD_ID");

/// The version this gate names itself by: `<package version>+<build id>` for a build `tools/install.ps1` made, else
/// `<package version>+dev`. Two builds of one package version are told apart by the id, so a builder running another
/// build of it is refused as one running another version, and a hand-built `+dev` compares equal only to another `+dev`.
pub(crate) fn build_version() -> String {
    version_text(BUILD_ID)
}

/// The version a build with the id `id` names itself by: `<package version>+<id>`, or `<package version>+dev` for no
/// id or an empty one.
fn version_text(id: Option<&str>) -> String {
    match id {
        Some(id) if !id.is_empty() => format!("{}+{id}", env!("CARGO_PKG_VERSION")),
        _ => format!("{}+dev", env!("CARGO_PKG_VERSION")),
    }
}

#[cfg(test)]
mod tests {
    use super::version_text;

    #[test]
    fn a_build_id_follows_the_package_version() {
        assert_eq!(
            version_text(Some("0123456789ab-dirty.89abcdef")),
            format!("{}+0123456789ab-dirty.89abcdef", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn an_empty_build_id_is_dev() {
        assert_eq!(version_text(Some("")), format!("{}+dev", env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn no_build_id_is_dev() {
        assert_eq!(version_text(None), format!("{}+dev", env!("CARGO_PKG_VERSION")));
    }
}
