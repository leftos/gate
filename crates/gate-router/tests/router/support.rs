//! A [`Facts`] of fixed answers that records every question asked, and small builders for the decision's inputs.

use std::cell::{Cell, RefCell};

use gate_router::{CarriedSibling, Config, ConfigProblem, Facts, FolderId, Inputs, Place, SiblingFolder, SiblingRoot, Slot};

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
    }
}

pub struct FakeFacts {
    pub config: Result<Option<Config>, ConfigProblem>,
    pub place: Result<Option<Place>, String>,
    pub has_files: Result<bool, String>,
    pub head: Result<Option<String>, String>,
    pub files: Vec<String>,
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
    /// Each root asked whether a git worktree is there, in order, kept apart from [`FakeFacts::asked`].
    pub presence_asked: RefCell<Vec<String>>,
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
            devices: Vec::new(),
            siblings: Ok(Vec::new()),
            present: Vec::new(),
            folders: Vec::new(),
            root_id: Ok(fake_id(ROOT)),
            asked: RefCell::new(Vec::new()),
            siblings_read: Cell::new(0),
            presence_asked: RefCell::new(Vec::new()),
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

    fn file_in_folder(&self, name: &str) -> bool {
        self.ask(format!("file_in_folder {name}"));
        self.files.iter().any(|file| file == name)
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

    fn root_id(&self, _root: &str) -> Result<FolderId, String> {
        self.root_id.clone()
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
