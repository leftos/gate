//! A [`Facts`] of fixed answers that records every question asked, and small builders for the decision's inputs.

use std::cell::RefCell;

use gate_router::{Config, ConfigProblem, Facts, Inputs, Place, Slot};

/// The worktree root the fixtures use.
pub const ROOT: &str = "X:/dev/yaat.wt/a";

/// The sha of [`ROOT`]'s worktree in the fixtures.
pub const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

/// A routing file listing the repo `yaat`.
pub fn config() -> Config {
    Config::parse(
        r#"{"host": "builder-1", "slots": 2,
            "repos": {"yaat": ["dotnet *", "cargo test*", "build.cmd*", "tool *", "./tool *", "* --x"], "empty": []},
            "outputs": {"other": ["out/**"]}}"#,
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
    pub asked: RefCell<Vec<String>>,
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
            place: Ok(Some(Place {
                root: ROOT.to_owned(),
                prefix: prefix.to_owned(),
                checkout: "yaat".to_owned(),
            })),
            has_files: Ok(true),
            head: Ok(Some(HEAD.to_owned())),
            files: Vec::new(),
            devices: Vec::new(),
            asked: RefCell::new(Vec::new()),
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
}
