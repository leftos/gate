//! The siblings a routed run's call carries: made only from the route's decision ([`carried`]) and read through
//! accessors, so no other part of the client can write one or change what it holds. Making one re-checks only its name
//! and its root, as text; it reads no folder, so whoever sends a sibling must open its folder again at use.

use gate_router::{FolderId, RemoteRun, is_layout_name, sibling_root};

/// A sibling a routed run carries: its folder name, its root and its folder's id, borrowed from the route's decision,
/// never a fresh read of the siblings file or the routing file.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Sibling<'a> {
    name: &'a str,
    root: &'a str,
    id: FolderId,
}

impl<'a> Sibling<'a> {
    /// The folder name as `.gate/siblings.toml` declares it.
    pub(crate) fn name(&self) -> &'a str {
        self.name
    }

    /// The sibling's root: the worktree root's parent and the name, joined with the root's separator.
    pub(crate) fn root(&self) -> &'a str {
        self.root
    }

    /// The id the decision's folder check gave the sibling's folder.
    pub(crate) fn id(&self) -> FolderId {
        self.id
    }
}

/// The siblings the routed run `run` carries, in declared order, each checked again: its name a folder name the
/// builder can lay out ([`is_layout_name`]) and its root the one [`sibling_root`] makes of the run's root and that name.
///
/// # Errors
///
/// The first sibling that fails either check, named; the run is then not sent, and no sibling is ever dropped from it.
pub(crate) fn carried(run: &RemoteRun) -> Result<Vec<Sibling<'_>>, String> {
    run.siblings
        .iter()
        .map(|sibling| {
            if !is_layout_name(&sibling.name) {
                return Err(format!("the sibling {:?} is not a folder name the builder can lay out", sibling.name));
            }
            if sibling.root != sibling_root(&run.root, &sibling.name) {
                return Err(format!(
                    "the sibling {:?} has the root {:?}, not one beside the worktree",
                    sibling.name, sibling.root
                ));
            }
            Ok(Sibling {
                name: &sibling.name,
                root: &sibling.root,
                id: sibling.id,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use gate_router::{CarriedSibling, FolderId, RemoteRun};

    use super::carried;

    const ID: FolderId = FolderId { volume: 1, file: [2; 16] };

    /// A routed run from the root `X:/dev/app` carrying `siblings`, each `(name, root)`.
    fn run(siblings: &[(&str, &str)]) -> RemoteRun {
        RemoteRun {
            host: "builder".to_owned(),
            slots: 1,
            root: "X:/dev/app".to_owned(),
            root_id: ID,
            leaf: "app".to_owned(),
            siblings: siblings
                .iter()
                .map(|(name, root)| CarriedSibling {
                    name: (*name).to_owned(),
                    root: (*root).to_owned(),
                    id: ID,
                })
                .collect(),
            repo: "app".to_owned(),
            key: "app-1a2b3c4d".to_owned(),
            subfolder: String::new(),
            pattern: "*".to_owned(),
            words: Vec::new(),
            fallback: Vec::new(),
            outputs: Vec::new(),
            inputs: Vec::new(),
            dropped_no_build: false,
            outputs_withheld: false,
        }
    }

    #[test]
    fn each_sibling_is_borrowed_as_the_decision_made_it() {
        let run = run(&[("b", "X:/dev/b"), ("c.d", "X:/dev/c.d")]);
        let siblings = carried(&run);
        let seen: Option<Vec<(&str, &str, FolderId)>> = siblings
            .ok()
            .map(|siblings| siblings.iter().map(|sibling| (sibling.name(), sibling.root(), sibling.id())).collect());
        assert_eq!(seen, Some(vec![("b", "X:/dev/b", ID), ("c.d", "X:/dev/c.d", ID)]));
    }

    #[test]
    fn a_sibling_whose_name_or_root_does_not_check_refuses_the_call() {
        for (name, root, why) in [
            (r"x\y", r"X:/dev/x\y", "a name holding a separator"),
            ("..", "X:/dev/..", "a name that climbs"),
            ("b", "X:/elsewhere/b", "a root not beside the worktree"),
            ("b", "X:/dev/B", "a root naming another folder"),
        ] {
            let run = run(&[("c", "X:/dev/c"), (name, root)]);
            let refused = carried(&run).map(|siblings| siblings.len());
            assert!(
                refused.as_ref().is_err_and(|why| why.contains(&format!("{name:?}"))),
                "{why}: {refused:?}"
            );
        }
    }
}
