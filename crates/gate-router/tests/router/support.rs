//! A [`Facts`] of fixed answers that records every question asked, and small builders for the decision's inputs.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};

use gate_router::windows::WindowsChange;
use gate_router::{CarriedSibling, Config, ConfigProblem, Facts, FolderId, Inputs, Pattern, Place, SiblingFolder, SiblingRoot, Slot, Target};

/// The worktree root the fixtures use.
pub const ROOT: &str = "X:/dev/yaat.wt/a";

/// The folder [`ROOT`] is in, where its siblings are.
pub const PARENT: &str = "X:/dev/yaat.wt";

/// The root of [`ROOT`]'s sibling `name`.
pub fn sibling_root(name: &str) -> String {
    format!("{PARENT}/{name}")
}

/// The place of a gate at `prefix` below [`ROOT`] in a worktree of `yaat`, its folder spelled as git spells the root.
pub fn place_at(prefix: &str) -> Place {
    Place {
        root: ROOT.to_owned(),
        prefix: prefix.to_owned(),
        checkout: "yaat".to_owned(),
        folder: format!("{ROOT}/{prefix}").trim_end_matches('/').to_owned(),
    }
}

/// The sibling `name` of [`ROOT`] as the decision hands it to the rewrite.
pub fn sibling(name: &str) -> SiblingRoot {
    SiblingRoot {
        name: name.to_owned(),
        root: sibling_root(name),
    }
}

/// The sibling `name` of [`ROOT`] as a routed run carries it, present with its [`fake_id`].
pub fn carried(name: &str) -> CarriedSibling {
    let root = sibling_root(name);
    CarriedSibling {
        name: name.to_owned(),
        id: fake_id(&root),
        root,
    }
}

/// The sha of [`ROOT`]'s worktree in the fixtures.
pub const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

/// A routing file listing the repo `yaat`, which may carry the siblings `b`, `c`, `yaat-server` and `A`.
pub fn config() -> Config {
    Config::parse(
        r#"{"host": "builder-1", "slots": 2,
            "repos": {"yaat": ["dotnet *", "cargo test*", "build.cmd*", "tool *", "./tool *", "* --x"], "empty": []},
            "outputs": {"other": ["out/**"]},
            "siblings": {"yaat": ["b", "c", "yaat-server", "A"]}}"#,
    )
    .expect("the fixture is usable")
}

pub fn words(list: &[&str]) -> Vec<String> {
    list.iter().map(|word| (*word).to_owned()).collect()
}

/// A heavy gate, not `--local`, under no parent, outside a git hook.
pub fn heavy(words: &[String]) -> Inputs<'_> {
    Inputs {
        slot: Slot::Heavy,
        covering_parent: false,
        local: false,
        git_hook: false,
        words,
        pin: None,
    }
}

pub struct FakeFacts {
    pub config: Result<Option<Config>, ConfigProblem>,
    pub place: Result<Option<Place>, String>,
    pub has_files: Result<bool, String>,
    pub head: Result<Option<String>, String>,
    pub files: Vec<String>,
    /// The files the snapshot carries, `/`-separated from the root; a folder above one is carried too.
    pub snapshot: Vec<String>,
    pub devices: Vec<(char, String)>,
    /// What `.gate/siblings.toml` gives.
    pub siblings: Result<Vec<String>, String>,
    /// The roots that hold a git worktree, each present with its [`fake_id`].
    pub present: Vec<String>,
    /// What the folder check gives for these roots, in place of what [`FakeFacts::present`] says.
    pub folders: Vec<(String, Result<SiblingFolder, String>)>,
    /// The worktree root's folder id.
    pub root_id: Result<FolderId, String>,
    pub asked: RefCell<Vec<String>>,
    /// How many times the siblings file was read, kept apart from [`FakeFacts::asked`].
    pub siblings_read: Cell<u32>,
    /// What `.gate/desktop.toml` gives.
    pub desktop: Result<Vec<Pattern>, String>,
    /// How many times the desktop file was read, kept apart from [`FakeFacts::asked`].
    pub desktop_read: Cell<u32>,
    /// What `.gate/local.toml` gives.
    pub local: Result<Vec<Pattern>, String>,
    /// How many times the local file was read, kept apart from [`FakeFacts::asked`].
    pub local_read: Cell<u32>,
    /// Each root asked whether a git worktree is there, in order, kept apart from [`FakeFacts::asked`].
    pub presence_asked: RefCell<Vec<String>>,
    /// The folders that hold a `.git`: the nearest of them a path lies in is its worktree.
    pub trees: Vec<PathBuf>,
    /// Each path asked which worktree it lies in, in order, kept apart from [`FakeFacts::asked`].
    pub trees_asked: RefCell<Vec<PathBuf>>,
    /// The main checkout's folder name of each of these trees; any other has none that can be read.
    pub checkouts: Vec<(PathBuf, String)>,
    /// What the folder id check gives for these folders as written, in place of the rule [`FakeFacts::root_id`] says.
    pub ids: Vec<(String, Result<FolderId, String>)>,
    /// The builder's root the work folders lie under, `None` when it cannot be told: then no box tree is given.
    pub builder_root: Option<String>,
    /// The box trees and PC roots given as they stand, in place of the layout under [`FakeFacts::builder_root`].
    pub given_trees: Option<Vec<(String, String)>>,
    /// Whether a `dotnet test`'s test projects are Windows-only.
    pub windows_only: bool,
    /// How many times that was asked, kept apart from [`FakeFacts::asked`].
    pub windows_only_asked: Cell<u32>,
    /// What the change is to `.gate/windows.toml`'s paths.
    pub windows_change: WindowsChange,
    /// How many times that was asked, kept apart from [`FakeFacts::asked`].
    pub windows_change_asked: Cell<u32>,
}

impl FakeFacts {
    /// A gate at the root of a worktree of `yaat`, with the routing file [`config`], whose folder holds files and no
    /// file named like a command, at the sha [`HEAD`].
    pub fn at_root() -> Self {
        Self::at("")
    }

    /// A gate at `prefix` below the root.
    pub fn at(prefix: &str) -> Self {
        Self {
            config: Ok(Some(config())),
            place: Ok(Some(place_at(prefix))),
            has_files: Ok(true),
            head: Ok(Some(HEAD.to_owned())),
            files: Vec::new(),
            snapshot: Vec::new(),
            devices: Vec::new(),
            siblings: Ok(Vec::new()),
            present: Vec::new(),
            folders: Vec::new(),
            root_id: Ok(fake_id(ROOT)),
            asked: RefCell::new(Vec::new()),
            siblings_read: Cell::new(0),
            desktop: Ok(Vec::new()),
            desktop_read: Cell::new(0),
            local: Ok(Vec::new()),
            local_read: Cell::new(0),
            presence_asked: RefCell::new(Vec::new()),
            trees: Vec::new(),
            trees_asked: RefCell::new(Vec::new()),
            checkouts: Vec::new(),
            ids: Vec::new(),
            builder_root: None,
            given_trees: None,
            windows_only: false,
            windows_only_asked: Cell::new(0),
            windows_change: WindowsChange::Untouched,
            windows_change_asked: Cell::new(0),
        }
    }

    /// A gate at `prefix` below the root declaring `names` as siblings, the ones in `present` beside the root.
    pub fn with_siblings(prefix: &str, names: &[&str], present: &[&str]) -> Self {
        Self {
            siblings: Ok(words(names)),
            present: present.iter().map(|name| sibling_root(name)).collect(),
            ..Self::at(prefix)
        }
    }

    /// A gate at the root with the routing file `config`.
    pub fn with_config(config: Config) -> Self {
        Self {
            config: Ok(Some(config)),
            ..Self::at_root()
        }
    }

    pub fn asked(&self) -> Vec<String> {
        self.asked.borrow().clone()
    }

    fn ask(&self, question: String) {
        self.asked.borrow_mut().push(question);
    }
}

impl Facts for FakeFacts {
    fn config(&self) -> Result<Option<Config>, ConfigProblem> {
        self.ask("config".to_owned());
        self.config.clone()
    }

    fn place(&self) -> Result<Option<Place>, String> {
        self.ask("place".to_owned());
        self.place.clone()
    }

    fn folder_has_files(&self, place: &Place) -> Result<bool, String> {
        self.ask(format!("folder_has_files {}", place.prefix));
        self.has_files.clone()
    }

    fn head_sha(&self, _place: &Place) -> Result<Option<String>, String> {
        self.ask("head_sha".to_owned());
        self.head.clone()
    }

    /// No build file moves the output.
    fn moves_build_output(&self, _place: &Place, _folder: &str, _file: Option<&str>) -> bool {
        self.ask("moves_build_output".to_owned());
        false
    }

    /// What [`FakeFacts::windows_only`] says.
    fn windows_only_tests(&self, _place: &Place, _words: &[String]) -> bool {
        self.windows_only_asked.set(self.windows_only_asked.get() + 1);
        self.windows_only
    }

    /// What [`FakeFacts::windows_change`] says.
    fn windows_change(&self, _place: &Place, _siblings: &[SiblingRoot], _siblings_problem: Option<&str>) -> WindowsChange {
        self.windows_change_asked.set(self.windows_change_asked.get() + 1);
        self.windows_change.clone()
    }

    /// The box's layout under [`FakeFacts::builder_root`]: the work folder `{root}\work\{key}` itself with no sibling,
    /// else `{work}\{leaf}` and `{work}\{name}` for each sibling; each PC root with `\` separators, as the gate spells it.
    fn box_trees(&self, target: &Target) -> Vec<(String, String)> {
        if let Some(given) = &self.given_trees {
            return given.clone();
        }
        let Some(root) = &self.builder_root else {
            return Vec::new();
        };
        let work = format!(r"{root}\work\{}", target.key);
        let spelled = |root: &str| root.replace('/', "\\");
        if target.siblings.is_empty() {
            return vec![(work, spelled(&target.place.root))];
        }
        std::iter::once((format!(r"{work}\{}", target.leaf), spelled(&target.place.root)))
            .chain(
                target
                    .siblings
                    .iter()
                    .map(|sibling| (format!(r"{work}\{}", sibling.name), spelled(&sibling.root))),
            )
            .collect()
    }

    fn file_in_folder(&self, name: &str) -> bool {
        self.ask(format!("file_in_folder {name}"));
        self.files.iter().any(|file| file == name)
    }

    /// Each path that is a file of [`FakeFacts::snapshot`] or a folder above one.
    fn snapshot_carries(&self, _place: &Place, paths: &[String]) -> Vec<String> {
        self.ask(format!("snapshot_carries {}", paths.join(" ")));
        let carried = |path: &str| {
            self.snapshot
                .iter()
                .any(|file| file == path || file.strip_prefix(path).is_some_and(|rest| rest.starts_with('/')))
        };
        paths.iter().filter(|path| carried(path)).cloned().collect()
    }

    fn subst_target(&self, letter: char) -> Option<String> {
        self.ask(format!("subst_target {letter}"));
        self.devices
            .iter()
            .find(|(device, _)| *device == letter)
            .map(|(_, target)| target.clone())
    }

    fn siblings(&self, _place: &Place) -> Result<Vec<String>, String> {
        self.siblings_read.set(self.siblings_read.get() + 1);
        self.siblings.clone()
    }

    fn desktop(&self, _place: &Place) -> Result<Vec<Pattern>, String> {
        self.desktop_read.set(self.desktop_read.get() + 1);
        self.desktop.clone()
    }

    fn local(&self, _place: &Place) -> Result<Vec<Pattern>, String> {
        self.local_read.set(self.local_read.get() + 1);
        self.local.clone()
    }

    fn declaration_said(&self, _place: &Place, name: &str) -> String {
        format!(".gate/{name}")
    }

    fn sibling_folder(&self, root: &str) -> Result<SiblingFolder, String> {
        self.presence_asked.borrow_mut().push(root.to_owned());
        if let Some((_, folder)) = self.folders.iter().find(|(folder_root, _)| folder_root == root) {
            return folder.clone();
        }
        Ok(if self.present.iter().any(|present| present == root) {
            SiblingFolder::Present(fake_id(root))
        } else {
            SiblingFolder::Missing
        })
    }

    /// The folder's id as [`FakeFacts::ids`] scripts it; else, for the place's root in any spelling of its separators
    /// and trailing ones, [`FakeFacts::root_id`]; else the [`fake_id`] of the folder with `/` separators, as a carried
    /// sibling's.
    fn root_id(&self, root: &str) -> Result<FolderId, String> {
        if let Some((_, id)) = self.ids.iter().find(|(folder, _)| folder == root) {
            return id.clone();
        }
        let folded = |path: &str| path.replace('\\', "/").trim_end_matches('/').to_owned();
        let place_root = self.place.as_ref().ok().and_then(Option::as_ref).map(|place| folded(&place.root));
        if place_root.as_deref() == Some(folded(root).as_str()) {
            return self.root_id.clone();
        }
        Ok(fake_id(&folded(root)))
    }

    /// The scripted tree of [`FakeFacts::trees`], as written, that is the nearest folder `path` lies in.
    fn worktree_of(&self, path: &Path) -> Option<PathBuf> {
        self.trees_asked.borrow_mut().push(path.to_path_buf());
        path.ancestors()
            .find_map(|folder| self.trees.iter().find(|tree| tree.as_path() == folder))
            .cloned()
    }

    fn checkout_of(&self, tree: &Path) -> Option<String> {
        self.checkouts
            .iter()
            .find(|(folder, _)| folder == tree)
            .map(|(_, checkout)| checkout.clone())
    }
}

/// A folder id of the folder at `root` that differs from every other root's: its text's bytes, the last 16 of them.
pub fn fake_id(root: &str) -> FolderId {
    let mut file = [0; 16];
    for (slot, byte) in file.iter_mut().rev().zip(root.bytes().rev()) {
        *slot = byte;
    }
    FolderId { volume: 7, file }
}
