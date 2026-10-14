//! The check of this PC: the worktree root the gate's working folder is in, the repo's `.gate/toolchain.toml` and
//! `.gate/siblings.toml` read from disk through the guarded `.gate` read routing uses ([`route::read_gate_file`]), so
//! an uncommitted edit counts, the root `global.json`'s SDK pin, the manifest's tools probed as a builder probes them
//! ([`probe::probe`]) while this PC's SDK list is read, both with the worktree's own folders dropped from `PATH`, and
//! each sibling looked for by routing's rule. It runs only version commands and installs nothing.

use std::path::Path;
use std::thread;
use std::time::Duration;

use gate_args::{CheckRequest, CheckSeams};
use gate_router::{Place, repo_name, sibling_root};

use crate::answer::FAILED_STATUS;
use crate::boxside::probe;
use crate::check::report::{self, Checked, Line, Problem, Sdk, Sibling, Tool};
use crate::console::Console;
use crate::environment::Variables;
use crate::remote::toolchain::{self, Manifest, ToolNeed};
use crate::remote::{git, local_sdk_listing, outside_worktree, parity, siblings};
use crate::route::{self, PLACE_ARGS, read_place};

/// The manifest's name in `.gate`.
const TOOLCHAIN: &str = "toolchain.toml";
/// The siblings file's name in `.gate`.
const SIBLINGS: &str = "siblings.toml";
/// Why an SDK pin is skipped when the root's `global.json` holds none that reads.
const NO_READABLE_PIN: &str = "global.json has no readable sdk pin";
/// Why a `.gate` that is not a folder leaves the manifest unreadable.
const GATE_NOT_A_FOLDER: &str = ".gate is not a folder";

/// Checks this PC for the repo the working folder is in and says what it found through `console`; returns the exit
/// status: 0 when nothing is missing, 1 when something is or the check could not run.
pub(crate) fn check(request: &CheckRequest, console: &mut Console) -> u32 {
    let place = match placed() {
        Ok(Some(place)) => place,
        Ok(None) => {
            console.err("check: not in a git repository");
            return u32::from(FAILED_STATUS);
        }
        Err(why) => {
            console.err(&format!("check: {why}"));
            return u32::from(FAILED_STATUS);
        }
    };
    let said = report::said(report::HERE, &checked(&place, probe_limit(&request.seams)));
    for line in &said.lines {
        match line {
            Line::Out(text) => console.out(text),
            Line::Err(text) => console.err(text),
        }
    }
    said.status
}

/// How long the tool probes may take together: `--test-probe-limit-ms` when given, else [`probe::PROBE_LIMIT`].
fn probe_limit(seams: &CheckSeams) -> Duration {
    seams.probe_limit_ms.map_or(probe::PROBE_LIMIT, Duration::from_millis)
}

/// The place in its git worktree of the gate's working folder; `None` when it is in none; else why git could not
/// tell.
fn placed() -> Result<Option<Place>, String> {
    let folder = std::env::current_dir().map_err(|error| format!("cannot read the working directory: {error}"))?;
    let git = git::Git::find(&folder)?;
    read_place(&git, &git::Call::plain(&folder, &PLACE_ARGS, git::GIT_LIMIT))
}

/// What this PC has of what the repo at `place` needs, its tools probed within `limit`.
fn checked(place: &Place, limit: Duration) -> Checked {
    let root = Path::new(&place.root);
    let mut problems = Vec::new();
    let manifest = read_manifest(root);
    let declares_toolchain = !matches!(manifest, Ok(None));
    let toolchain_unreadable = manifest.is_err();
    let Manifest {
        tools,
        workloads,
        problems: skipped,
    } = match manifest {
        Ok(Some(manifest)) => manifest,
        Ok(None) => Manifest::default(),
        Err(why) => {
            problems.push(Problem {
                file: report::TOOLCHAIN,
                text: why,
            });
            Manifest::default()
        }
    };
    problems.extend(skipped.into_iter().map(|text| Problem {
        file: report::TOOLCHAIN,
        text,
    }));
    let (pin, pin_problem) = read_pin(root);
    let pin_unreadable = pin_problem.is_some();
    if let Some(text) = pin_problem {
        problems.push(Problem {
            file: report::GLOBAL_JSON,
            text,
        });
    }
    // The SDK list is read while the tools are probed, so the check waits for the longer of the two bounds.
    let (tools, sdk) = thread::scope(|scope| {
        let sdk = scope.spawn(|| sdk_of(pin.as_ref(), || sdk_list(root)));
        let tools = probed(tools, limit, root);
        let sdk = sdk
            .join()
            .unwrap_or_else(|panic| Some(Sdk::Skipped(format!("cannot read this PC's SDK list: {}", probe::panic_reason(&*panic)))));
        (tools, sdk)
    });
    let (siblings, siblings_unreadable) = read_siblings(&place.root, &mut problems);
    let unreadable = [
        toolchain_unreadable.then_some(report::TOOLCHAIN),
        siblings_unreadable.then_some(report::SIBLINGS),
        pin_unreadable.then_some(report::GLOBAL_JSON),
    ]
    .into_iter()
    .flatten()
    .collect();
    Checked {
        repo: repo_name(&place.checkout),
        root: place.root.clone(),
        declares_toolchain,
        unreadable,
        problems,
        tools,
        sdk,
        workloads,
        siblings,
    }
}

/// The manifest `<root>\.gate\toolchain.toml` holds as it stands on disk; `None` when there is none; else why it
/// cannot be read whole, a `.gate` that is not a folder among the reasons.
fn read_manifest(root: &Path) -> Result<Option<Manifest>, String> {
    match route::read_gate_file(root, TOOLCHAIN)? {
        Some(bytes) => toolchain::read_bytes(&bytes).map(Some),
        None if route::gate_is_not_a_folder(root) => Err(GATE_NOT_A_FOLDER.to_owned()),
        None => Ok(None),
    }
}

/// Each of `needs` with what its probe found within `limit`, in their order; an unknown tool runs nothing. The tools
/// are looked for as this PC's SDK list looks for dotnet: along `PATH` with the folders inside the worktree `root`
/// dropped, and one found that still lies inside it refused.
fn probed(needs: Vec<ToolNeed>, limit: Duration, root: &Path) -> Vec<Tool> {
    if needs.is_empty() {
        return Vec::new();
    }
    let names: Vec<String> = needs.iter().map(|need| need.name.clone()).collect();
    probe::probe(&names, limit, outside_worktree(Variables::from_process(), root), Some(root))
        .into_iter()
        .zip(needs)
        .map(|((_, probed), need)| Tool { need, probed })
        .collect()
}

/// The SDK pin of `<root>\global.json`, read without following a link and up to 64 KiB
/// ([`route::read_unfollowed`]), and why its pin cannot be read when the file names one that does not: the pin is
/// `None` when there is no such file or it names no `sdk.version` at all, which pins no SDK rather than failing.
fn read_pin(root: &Path) -> (Option<parity::Pin>, Option<String>) {
    let bytes = match route::read_unfollowed(&root.join("global.json")) {
        Ok(None) => return (None, None),
        Err(why) => return (Some(parity::Pin::unreadable()), Some(why)),
        Ok(Some(bytes)) => bytes,
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return (Some(parity::Pin::unreadable()), Some("is not UTF-8".to_owned()));
    };
    match parity::pin_in_text(text) {
        parity::PinRead::Pinned(pin) => (Some(pin), None),
        parity::PinRead::NoPin => (None, None),
        parity::PinRead::Unreadable(why) => (Some(parity::Pin::unreadable()), Some(why)),
    }
}

/// The check of `pin` against the SDKs `sdks` lists, read only for a pin that names a version; `None` with no pin.
fn sdk_of(pin: Option<&parity::Pin>, sdks: impl FnOnce() -> Result<Vec<String>, String>) -> Option<Sdk> {
    let pin = pin?;
    let Some(version) = pin.version.clone() else {
        return Some(Sdk::Skipped(NO_READABLE_PIN.to_owned()));
    };
    let have = match sdks() {
        Ok(have) => have,
        Err(why) => return Some(Sdk::Skipped(format!("cannot read this PC's SDK list: {why}"))),
    };
    // With no SDK to choose from, a pin that names a version always needs it: its words are parity's need text.
    let need = parity::need(pin, &[], || 0).into_iter().next().map_or_else(String::new, |need| need.text);
    let served = parity::need(pin, &have, || 0).is_empty();
    Some(Sdk::Checked { version, need, have, served })
}

/// The SDKs this PC's `dotnet --list-sdks` lists, run with the folders inside the worktree `root` dropped from
/// `PATH`; none when no dotnet is found; else why the list cannot be read.
fn sdk_list(root: &Path) -> Result<Vec<String>, String> {
    let listing = local_sdk_listing(Variables::from_process(), root, git::DOTNET_LIMIT, None)?;
    Ok(parity::sdk_versions(&listing).into_iter().map(str::to_owned).collect())
}

/// Each sibling `<root>\.gate\siblings.toml` declares, looked for beside the worktree root `root`, and whether that
/// file is there but cannot be read whole; every problem of the file is added to `problems`, and the names that read
/// are still looked for.
fn read_siblings(root: &str, problems: &mut Vec<Problem>) -> (Vec<Sibling>, bool) {
    let declared = route::read_gate_file(Path::new(root), SIBLINGS).and_then(|bytes| bytes.map(|bytes| siblings::read_bytes(&bytes)).transpose());
    let names = match declared {
        Ok(None) => return (Vec::new(), false),
        Err(why) => {
            problems.push(Problem {
                file: report::SIBLINGS,
                text: why,
            });
            return (Vec::new(), true);
        }
        Ok(Some(declared)) => {
            problems.extend(declared.problems.into_iter().map(|text| Problem {
                file: report::SIBLINGS,
                text,
            }));
            declared.names
        }
    };
    (names.into_iter().map(|name| sibling(root, name)).collect(), false)
}

/// The sibling `name` of the worktree root `root`: where it must be, and whether a git worktree is there.
fn sibling(root: &str, name: String) -> Sibling {
    let path = sibling_root(root, &name);
    let present = route::sibling_present(&path);
    Sibling { name, path, present }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its folders must fail loudly")]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{read_manifest, read_pin, read_siblings, sdk_of, sibling};
    use crate::check::report::Sdk;
    use crate::remote::parity::{Pin, pin_of};
    use crate::remote::toolchain::{ToolNeed, Version};
    use crate::test_support::junction;

    /// A folder of its own under the temp folder, removed when the test ends, failed or not.
    struct Folder(PathBuf);

    impl Folder {
        fn new(name: &str) -> Folder {
            let path = std::env::temp_dir().join(format!("gate-check-{name}-{}", std::process::id()));
            if path.exists() {
                fs::remove_dir_all(&path).expect("a folder left by an earlier run can be removed");
            }
            fs::create_dir_all(&path).expect("the test folder can be made");
            Folder(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn text(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
    }

    impl Drop for Folder {
        #[expect(clippy::print_stderr, reason = "a case that cannot clean up says so without failing the test it ends")]
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.0) {
                eprintln!("cannot remove the test folder {}: {error}", self.0.display());
            }
        }
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().expect("a file has a folder")).expect("the file's folder can be made");
        fs::write(path, text).expect("the file can be written");
    }

    #[test]
    fn the_manifest_is_read_from_the_file_on_disk_not_from_the_commit() {
        let root = Folder::new("manifest-on-disk");
        write(&root.path().join(".gate").join("toolchain.toml"), "[tools]\nnode = \"24\"\n");

        let manifest = read_manifest(root.path()).expect("the manifest reads").expect("there is a manifest");

        assert_eq!(
            manifest.tools,
            [ToolNeed {
                name: "node".to_owned(),
                min: Version::parse("24"),
                known: true,
            }]
        );
    }

    #[test]
    fn a_missing_manifest_declares_nothing() {
        let root = Folder::new("manifest-missing");
        assert_eq!(read_manifest(root.path()), Ok(None), "no .gate");
        fs::create_dir_all(root.path().join(".gate")).expect("the .gate folder can be made");
        assert_eq!(read_manifest(root.path()), Ok(None), "a .gate with no manifest");
    }

    #[test]
    fn a_gate_that_is_not_a_folder_leaves_the_manifest_unreadable() {
        let root = Folder::new("manifest-gate-file");
        write(&root.path().join(".gate"), "not a folder\n");

        assert_eq!(read_manifest(root.path()), Err(".gate is not a folder".to_owned()));
    }

    #[test]
    fn a_linked_or_oversized_manifest_is_refused() {
        let root = Folder::new("manifest-refused");
        let elsewhere = Folder::new("manifest-refused-elsewhere");
        let file = root.path().join(".gate").join("toolchain.toml");
        write(&file, &format!("[tools]\n#{}\n", "x".repeat(64 * 1024)));
        assert_eq!(read_manifest(root.path()), Err("is larger than 64 KiB".to_owned()));
        write(&file, "[tools\n");
        let why = read_manifest(root.path()).expect_err("a manifest that is no TOML is unreadable");
        assert!(why.starts_with("line 1, column "), "{why}");

        write(&elsewhere.path().join("toolchain.toml"), "[tools]\nnode = \"24\"\n");
        fs::remove_dir_all(root.path().join(".gate")).expect("the real .gate can be removed");
        junction(&root.path().join(".gate"), elsewhere.path());
        assert_eq!(read_manifest(root.path()), Err(".gate is a link or a junction".to_owned()));
        fs::remove_dir(root.path().join(".gate")).expect("the junction can be removed");
    }

    #[test]
    fn a_global_json_pin_is_checked_against_the_sdk_list() {
        let pin = pin_of(Some(r#"{"sdk": {"version": "10.0.401", "rollForward": "latestFeature"}}"#));
        let checked = sdk_of(Some(&pin), || Ok(vec!["8.0.404".to_owned(), "10.0.500".to_owned()]));
        let Some(Sdk::Checked { version, need, have, served }) = checked else {
            unreachable!("a pin with a version is checked, got {checked:?}");
        };
        assert_eq!(version, "10.0.401");
        assert_eq!(need, ".NET SDK 10.0.401 (rollForward latestFeature)");
        assert_eq!(have, ["8.0.404", "10.0.500"]);
        assert!(served, "a later feature band serves latestFeature");

        assert!(
            sdk_of(None, || unreachable!("no pin reads no SDK list")).is_none(),
            "no global.json, no line"
        );
        let unreadable = sdk_of(Some(&pin_of(Some("{}"))), || unreachable!("a pin with no version reads no SDK list"));
        assert!(
            matches!(&unreadable, Some(Sdk::Skipped(why)) if why == "global.json has no readable sdk pin"),
            "{unreadable:?}"
        );
        let failed = sdk_of(Some(&pin), || Err("dotnet failed".to_owned()));
        assert!(
            matches!(&failed, Some(Sdk::Skipped(why)) if why == "cannot read this PC's SDK list: dotnet failed"),
            "{failed:?}"
        );
    }

    #[test]
    fn a_global_json_naming_no_sdk_version_pins_nothing() {
        let root = Folder::new("global-json-no-version");
        let global = root.path().join("global.json");
        assert_eq!(read_pin(root.path()), (None, None), "no global.json");
        for text in [
            "{\"msbuild-sdks\": {\"Godot.NET.Sdk\": \"4.5.0\"}}\n",
            "{}\n",
            "{\"sdk\": {\"rollForward\": \"latestFeature\"}}\n",
        ] {
            write(&global, text);
            assert_eq!(
                read_pin(root.path()),
                (None, None),
                "{text:?} names no SDK version, so there is no SDK line"
            );
        }
        for text in ["{\"sdk\": \n", "{\"sdk\": {\"version\": \"10.0\"}}\n", "{\"sdk\": {\"version\": 10}}\n"] {
            write(&global, text);
            let (pin, why) = read_pin(root.path());
            assert!(
                matches!(
                    &pin,
                    Some(Pin {
                        found: true,
                        version: None,
                        ..
                    })
                ),
                "{text:?} holds no readable pin: {pin:?}"
            );
            assert!(why.is_some(), "{text:?} says why the pin cannot be read: {why:?}");
        }
    }

    #[test]
    fn a_global_json_that_is_not_utf8_says_so() {
        let root = Folder::new("global-json-not-utf8");
        fs::write(root.path().join("global.json"), [0xff, 0xfe, b'{', b'}']).expect("the file can be written");

        let (pin, why) = read_pin(root.path());

        assert_eq!(pin, Some(Pin::unreadable()), "a file that is not UTF-8 holds no readable pin");
        assert_eq!(why, Some("is not UTF-8".to_owned()));
    }

    #[test]
    fn an_oversized_or_linked_global_json_holds_no_readable_pin() {
        let root = Folder::new("global-json-refused");
        let elsewhere = Folder::new("global-json-refused-elsewhere");
        let global = root.path().join("global.json");
        let pinned = "{\"sdk\": {\"version\": \"10.0.401\"}}";
        write(&global, &format!("{pinned}{}\n", " ".repeat(64 * 1024)));
        assert_eq!(read_pin(root.path()).0, Some(Pin::unreadable()), "over 64 KiB");
        assert_eq!(read_pin(root.path()).1, Some("is larger than 64 KiB".to_owned()), "and says why");
        write(&global, &format!("{pinned}{}\n", " ".repeat(64 * 1024 - pinned.len() - 1)));
        assert_eq!(
            read_pin(root.path()).0.and_then(|pin| pin.version),
            Some("10.0.401".to_owned()),
            "64 KiB reads"
        );

        fs::remove_file(&global).expect("the file can be removed");
        fs::create_dir_all(&global).expect("a folder named global.json can be made");
        assert_eq!(read_pin(root.path()).0, Some(Pin::unreadable()), "a folder");
        fs::remove_dir(&global).expect("the folder can be removed");
        junction(&global, elsewhere.path());
        assert_eq!(read_pin(root.path()).0, Some(Pin::unreadable()), "a junction is not followed");
        fs::remove_dir(&global).expect("the junction can be removed");
    }

    #[test]
    fn a_pin_nobody_here_has_is_missing() {
        let pin = pin_of(Some(r#"{"sdk": {"version": "10.0.401"}}"#));
        let checked = sdk_of(Some(&pin), || Ok(vec!["10.0.300".to_owned(), "10.0.500".to_owned()]));
        assert!(
            matches!(&checked, Some(Sdk::Checked { need, served: false, .. }) if need == ".NET SDK 10.0.401 (rollForward patch)"),
            "patch rolls only within the band: {checked:?}"
        );
        let none = sdk_of(Some(&pin), || Ok(Vec::new()));
        assert!(
            matches!(&none, Some(Sdk::Checked { served: false, have, .. }) if have.is_empty()),
            "{none:?}"
        );
    }

    #[test]
    fn a_sibling_is_present_when_its_git_exists_beside_the_root() {
        let parent = Folder::new("sibling-beside");
        let root = parent.path().join("app");
        fs::create_dir_all(root.join(".gate")).expect("the root can be made");
        write(&root.join(".gate").join("siblings.toml"), "siblings = ['here', 'gone', '..']\n");
        fs::create_dir_all(parent.path().join("here").join(".git")).expect("the sibling's git can be made");
        fs::create_dir_all(parent.path().join("gone")).expect("a folder with no git can be made");

        let mut problems = Vec::new();
        let (found, unreadable) = read_siblings(&root.to_string_lossy(), &mut problems);
        assert!(!unreadable, "the siblings file reads");

        let seen: Vec<(&str, Result<bool, String>)> = found.iter().map(|sibling| (sibling.name.as_str(), sibling.present.clone())).collect();
        assert_eq!(seen, [("here", Ok(true)), ("gone", Ok(false))]);
        assert_eq!(found[0].path, format!("{}\\here", parent.text()));
        let texts: Vec<&str> = problems.iter().map(|problem| problem.text.as_str()).collect();
        assert_eq!(texts.len(), 1, "the bad name is a problem and the rest are looked for: {texts:?}");
        assert!(texts[0].starts_with("siblings[2] is not a folder name"), "{texts:?}");
    }

    #[test]
    fn siblings_beside_a_linked_worktree_use_the_worktree_root() {
        let dev = Folder::new("sibling-worktree");
        fs::create_dir_all(dev.path().join("app").join(".git")).expect("the main checkout can be made");
        fs::create_dir_all(dev.path().join("b").join(".git")).expect("a sibling beside the main checkout can be made");
        let worktree = dev.path().join("app.wt").join("feature");
        write(&worktree.join(".git"), "gitdir: elsewhere\n");

        let beside = sibling(&worktree.to_string_lossy().replace('\\', "/"), "b".to_owned());

        assert_eq!(beside.path, format!("{}/app.wt/b", dev.text().replace('\\', "/")));
        assert_eq!(beside.present, Ok(false), "the sibling beside the main checkout is not the worktree's");
        fs::create_dir_all(dev.path().join("app.wt").join("b").join(".git")).expect("a sibling beside the worktree can be made");
        assert_eq!(sibling(&worktree.to_string_lossy(), "b".to_owned()).present, Ok(true));
    }
}
