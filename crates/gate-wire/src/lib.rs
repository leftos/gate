//! The box request payload both the PC client and every box parse: the payload of a `gate box` call, one base64 word
//! (standard alphabet, padded) of UTF-8 JSON holding an object, as the remote client sends it as the first line of the
//! call's standard input. Keys are read by their exact name; keys no verb reads are ignored. Its path rules follow the
//! box's [`PathForm`]; nothing here touches the operating system, so it builds for every box.
#![expect(
    clippy::missing_errors_doc,
    reason = "each reader's own doc names the usage line it refuses with, as the parser's docs always have"
)]

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use gate_args::{MAX_WHOLE_NUMBER, MOST_ENV_BYTES, MOST_ENV_NAMES, env_refusal, over_cap, shown_name};
pub use gate_router::escaped;
use gate_router::warm::MOST_COMMANDS;
use gate_router::{is_layout_name, repo_name};
use serde_json::{Map, Value};

/// The commit this build was made from, from `GATE_BUILD_ID` at compile time; `None` for a build made without one, or
/// with an empty one.
const BUILD_ID: Option<&str> = option_env!("GATE_BUILD_ID");

/// The version this build names itself by: `<package version>+<build id>` for a build `tools/install.ps1` made, else
/// `<package version>+dev`. Two builds of one package version are told apart by the id, so a builder running another
/// build of it is refused as one running another version, and a hand-built `+dev` compares equal only to another `+dev`.
/// The gate, its desktop helper and every box name themselves by it.
#[must_use]
pub fn build_version() -> String {
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

/// The version a tool probe's program wrote: the first version of the first non-blank line of its standard output, or
/// of its standard error when its standard output holds nothing but blanks ([`first_version`]). Only that line is read:
/// a tool's later lines may name another program's version, as ffmpeg's `built with gcc 14.2.0` does.
#[must_use]
pub fn version_of(stdout: &str, stderr: &str) -> Option<String> {
    let chosen = if stdout.trim().is_empty() { stderr } else { stdout };
    chosen.lines().find(|line| !line.trim().is_empty()).and_then(first_version)
}

/// The first version `text` spells: the first run of two or more dot-separated runs of ASCII digits, after an optional
/// `v`, that starts no word or number already begun (so not after a letter, a digit or a `.`), cut to its first three
/// numbers; what follows it, such as a `-` build label, is no part of it. `None` when there is no such run, or when a
/// number of the first one has more than [`MAX_PART_DIGITS`] digits.
#[must_use]
pub fn first_version(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let parts = (0..bytes.len())
        .filter(|&at| starts_number(bytes, at))
        .find_map(|at| dotted(&text[at..]))?;
    let readable = parts.iter().all(|part| part.len() <= MAX_PART_DIGITS);
    readable.then(|| parts[..parts.len().min(3)].join("."))
}

/// Whether a version may start at `at` in `bytes`: a digit not after a letter, a digit or a `.`, or after a `v` that
/// is itself not after one of them.
fn starts_number(bytes: &[u8], at: usize) -> bool {
    let free = |before: Option<&u8>| before.is_none_or(|byte| !byte.is_ascii_alphanumeric() && *byte != b'.');
    let before = at.checked_sub(1).and_then(|index| bytes.get(index));
    bytes[at].is_ascii_digit()
        && match before {
            Some(b'v' | b'V') => free(at.checked_sub(2).and_then(|index| bytes.get(index))),
            _ => free(before),
        }
}

/// The dot-separated runs of digits `text` starts with, up to the first empty one, when there are two or more; `None`
/// when there are fewer.
fn dotted(text: &str) -> Option<Vec<&str>> {
    let end = text
        .find(|character: char| !character.is_ascii_digit() && character != '.')
        .unwrap_or(text.len());
    let parts: Vec<&str> = text[..end].split('.').take_while(|part| !part.is_empty()).collect();
    (parts.len() >= 2).then_some(parts)
}

/// The most digits a part of a version may have: a longer one is no version the client could compare.
pub const MAX_PART_DIGITS: usize = 10;

/// The extension of an outputs archive's name.
pub const SUFFIX: &str = ".tar.zst";

/// The hex digits of a SHA-256 as the gate writes it.
pub const SHA256_DIGITS: usize = 64;

/// The most siblings a repo may declare.
pub const MOST_SIBLINGS: usize = 8;

/// Whether `text` is exactly `digits` lower-case hex digits, with nothing before or after them.
#[must_use]
pub fn is_lower_hex(text: &str, digits: usize) -> bool {
    text.len() == digits && text.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// The path rules a payload is read under, those of the box that reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathForm {
    /// A Windows box: a path is absolute on a drive ([`is_drive_path`]), two paths are compared as full paths without
    /// regard to case, a path is joined with `\` and a job name starts `Local\`.
    Windows,
    /// A Unix box: a path is absolute in its one spelling ([`is_unix_path`]), two paths are compared byte for byte, a
    /// path is joined with `/` and a job name has no `Local\`.
    Unix,
}

impl PathForm {
    /// The separator a path of this form is joined with.
    fn separator(self) -> char {
        match self {
            PathForm::Windows => '\\',
            PathForm::Unix => '/',
        }
    }

    /// What a run's job name starts with before its key.
    fn job_prefix(self) -> &'static str {
        match self {
            PathForm::Windows => r"Local\gate-remote-",
            PathForm::Unix => "gate-remote-",
        }
    }
}

/// The decoded JSON object, and the path rules it is read under.
pub struct Payload {
    object: Map<String, Value>,
    form: PathForm,
}

/// The key naming the variables a run's commands get set to the chosen Unreal Engine install's root.
const ENGINE_ENV: &str = "EngineEnv";

/// The one value an [`ENGINE_ENV`] entry may have.
const ENGINE_ENV_UNREAL: &str = "unreal";

/// The prefix of the names git reads, none of which a manifest `[env]` may set, compared without regard to ASCII case.
const GIT_NAME_PREFIX: &str = "GIT_";

/// Whether `name` cannot be a name a repo's toolchain manifest `[env]` sets, as the PC reads the manifest and the box
/// reads `EngineEnv`: a name that is not an ASCII letter or `_` followed by ASCII letters, digits and `_`, a name
/// starting with `GIT_` in any ASCII case, and a name `--env` refuses ([`env_refusal`]).
#[must_use]
pub fn engine_env_name_refused(name: &str) -> bool {
    let mut bytes = name.bytes();
    let well_formed = bytes.next().is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
    let git = name
        .get(..GIT_NAME_PREFIX.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(GIT_NAME_PREFIX));
    !well_formed || git || env_refusal(name, "").is_some()
}

impl Payload {
    /// Decodes the payload word, its paths to be read under `form`; else the usage line saying why it cannot be read.
    pub fn decode(word: &str, form: PathForm) -> Result<Payload, String> {
        object(word)
            .map(|object| Payload { object, form })
            .map_err(|reason| format!("gate: box payload is not base64 JSON: {reason}"))
    }

    /// The text of `key`; else the usage line saying the payload lacks it. A key that is absent, empty or not a string
    /// is lacking.
    pub fn text(&self, key: &str) -> Result<&str, String> {
        self.object
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| format!("gate: box payload lacks {key}"))
    }

    /// The path of `key`: under [`PathForm::Windows`] absolute on a drive (`X:\…` or `X:/…`), with no character below
    /// U+0020; under [`PathForm::Unix`] as [`is_unix_path`] says. Else the usage line saying why, the value shown with
    /// its control characters escaped. A relative, UNC or `\\?\` path, or one that starts with `-`, is refused, so no
    /// such value reaches git as a path or an option.
    pub fn path(&self, key: &str) -> Result<&str, String> {
        let value = self.text(key)?;
        match self.form {
            PathForm::Windows if is_drive_path(value) => Ok(value),
            PathForm::Windows => Err(format!("gate: box payload {key} is not an absolute path on a drive: {value:?}")),
            PathForm::Unix if is_unix_path(value) => Ok(value),
            PathForm::Unix => Err(format!("gate: box payload {key} is not an absolute path: {value:?}")),
        }
    }

    /// The words of `key`, a list of texts; else the usage line saying the payload lacks it (absent or empty) or that
    /// it is not a list of texts.
    pub fn words(&self, key: &str) -> Result<Vec<String>, String> {
        let not_words = || format!("gate: box payload {key} is not a list of texts");
        let list = match self.object.get(key) {
            None | Some(Value::Null) => return Err(format!("gate: box payload lacks {key}")),
            Some(value) => value.as_array().ok_or_else(not_words)?,
        };
        if list.is_empty() {
            return Err(format!("gate: box payload lacks {key}"));
        }
        list.iter().map(|word| word.as_str().map(str::to_owned).ok_or_else(not_words)).collect()
    }

    /// The text of `key`, empty when the key is absent or null; else the usage line saying it is not a text.
    pub fn optional_text(&self, key: &str) -> Result<&str, String> {
        match self.object.get(key) {
            None | Some(Value::Null) => Ok(""),
            Some(value) => value.as_str().ok_or_else(|| format!("gate: box payload {key} is not a text")),
        }
    }

    /// The name and text of each entry of the object at `key`, none when the key is absent or null; else the usage
    /// line saying it is not an object of texts.
    pub fn pairs(&self, key: &str) -> Result<Vec<(String, String)>, String> {
        let Some(object) = self.object_at(key)? else {
            return Ok(Vec::new());
        };
        object
            .iter()
            .map(|(name, value)| {
                value
                    .as_str()
                    .map(|text| (name.clone(), text.to_owned()))
                    .ok_or_else(|| not_an_object(key))
            })
            .collect()
    }

    /// How many entries the object at `key` holds and how many bytes its names and values take together, counted over
    /// the object `serde_json` parsed (`serde_json` has already built it, and the 16 MiB request line limit bounds it)
    /// so [`Payload::pairs`] need not copy an object over the box's cap; `None` when the key is absent or null. Else the
    /// usage line saying it is not an object of texts.
    pub fn pair_size(&self, key: &str) -> Result<Option<(usize, usize)>, String> {
        let Some(object) = self.object_at(key)? else {
            return Ok(None);
        };
        let bytes = object.iter().map(|(name, value)| name.len() + value.as_str().map_or(0, str::len)).sum();
        Ok(Some((object.len(), bytes)))
    }

    /// The object at `key`, none when the key is absent or null; else the usage line saying it is not an object of texts.
    fn object_at(&self, key: &str) -> Result<Option<&Map<String, Value>>, String> {
        match self.object.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Object(object)) => Ok(Some(object)),
            Some(_) => Err(not_an_object(key)),
        }
    }

    /// The run's output globs, `Outputs`: a list of non-empty texts holding no `[` or `]`, as the routing file allows
    /// them; none when the key is absent or null. Else the usage line, a glob shown with its control characters escaped.
    pub fn output_globs(&self) -> Result<Vec<String>, String> {
        let not_globs = || "gate: box payload Outputs is not a list of texts".to_owned();
        let list = match self.object.get("Outputs") {
            None | Some(Value::Null) => return Ok(Vec::new()),
            Some(value) => value.as_array().ok_or_else(not_globs)?,
        };
        list.iter()
            .map(|glob| match glob.as_str() {
                None => Err(not_globs()),
                Some("") => Err("gate: box payload Outputs holds an empty glob".to_owned()),
                Some(glob) if glob.contains(['[', ']']) => Err(format!("gate: box payload Outputs glob '{}' holds '[' or ']'", escaped(glob))),
                Some(glob) => Ok(glob.to_owned()),
            })
            .collect()
    }

    /// Whether `OutputsFull` is set, as the client sets it when the PC's last outputs step did not complete; false when
    /// the key is absent or null. Else the usage line saying it is not true or false.
    pub fn outputs_full(&self) -> Result<bool, String> {
        match self.object.get("OutputsFull") {
            None | Some(Value::Null) => Ok(false),
            Some(Value::Bool(full)) => Ok(*full),
            Some(_) => Err("gate: box payload OutputsFull is not true or false".to_owned()),
        }
    }

    /// The run's outputs `Nonce`: 32 lower-case hex digits, as the client draws it; else the usage line, the value
    /// shown with its control characters escaped.
    pub fn nonce(&self) -> Result<&str, String> {
        let nonce = self.text("Nonce")?;
        if is_lower_hex(nonce, OUTPUTS_NONCE_DIGITS) {
            Ok(nonce)
        } else {
            Err(format!("gate: box payload Nonce is not 32 lower-case hex digits: {}", escaped(nonce)))
        }
    }

    /// The run's ceiling, `Timeout`: whole seconds from 1 to 2147483647, the load-adjusted time the box gives the
    /// command; `None` when the key is absent or null, a run with no ceiling on the box. Else the usage line.
    pub fn timeout(&self) -> Result<Option<u64>, String> {
        match self.object.get("Timeout") {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_u64()
                .filter(|seconds| (1..=MAX_WHOLE_NUMBER).contains(seconds))
                .map(Some)
                .ok_or_else(|| "gate: box payload Timeout is not a whole number of seconds".to_owned()),
        }
    }

    /// Whether the run asks for the builder's desktop, `Desktop`: true or false, false when the key is absent or null;
    /// else the usage line saying it is not true or false.
    pub fn desktop(&self) -> Result<bool, String> {
        match self.object.get("Desktop") {
            None | Some(Value::Null) => Ok(false),
            Some(Value::Bool(desktop)) => Ok(*desktop),
            Some(_) => Err("gate: box payload Desktop is not true or false".to_owned()),
        }
    }

    /// The run's `DesktopWait`: whole seconds from 1 to 2147483647, how long the box may wait for the builder's
    /// desktop; `None` when the key is absent or null. Else the usage line.
    pub fn desktop_wait(&self) -> Result<Option<u64>, String> {
        match self.object.get("DesktopWait") {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_u64()
                .filter(|seconds| (1..=MAX_WHOLE_NUMBER).contains(seconds))
                .map(Some)
                .ok_or_else(|| "gate: box payload DesktopWait is not a whole number of seconds".to_owned()),
        }
    }

    /// The run's `InputsDigest`, the SHA-256 of the inputs manifest line the client sent the work folder's inputs
    /// with: 64 lower-case hex digits; `None` when the key is absent or null. Else the usage line, the value shown with
    /// its control characters escaped.
    pub fn inputs_digest(&self) -> Result<Option<&str>, String> {
        self.digest("InputsDigest")
    }

    /// The run's `SentDigest`, the SHA-256 of the manifest line of the PC's build the client sent the work folder,
    /// without its newline: 64 lower-case hex digits; `None` when the key is absent or null. Else the usage line, as
    /// [`Payload::inputs_digest`] gives it.
    pub fn sent_digest(&self) -> Result<Option<&str>, String> {
        self.digest("SentDigest")
    }

    /// The run's `LfsDigest`, the SHA-256 of the LFS manifest line of the objects the snapshot's pointers name, without
    /// its newline: 64 lower-case hex digits; `None` when the key is absent or null. Else the usage line, as
    /// [`Payload::inputs_digest`] gives it.
    pub fn lfs_digest(&self) -> Result<Option<&str>, String> {
        self.digest("LfsDigest")
    }

    /// The SHA-256 under the key `key`: 64 lower-case hex digits; `None` when the key is absent or null. Else the usage
    /// line, the value shown with its control characters escaped.
    fn digest(&self, key: &str) -> Result<Option<&str>, String> {
        match self.object.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(digest)) if is_lower_hex(digest, SHA256_DIGITS) => Ok(Some(digest)),
            Some(other) => Err(format!(
                "gate: box payload {key} is not 64 lower-case hex digits: {}",
                escaped(&other.as_str().map_or_else(|| other.to_string(), str::to_owned))
            )),
        }
    }

    /// The version of the builder's Godot install the run uses, `GodotVersion` ([`Payload::install_version`]).
    pub fn godot_version(&self) -> Result<Option<&str>, String> {
        self.install_version("GodotVersion")
    }

    /// The version of the builder's Unreal Engine install the run uses, `UnrealVersion` ([`Payload::install_version`]).
    pub fn unreal_version(&self) -> Result<Option<&str>, String> {
        self.install_version("UnrealVersion")
    }

    /// The version of an install of the builder the run uses, under the key `key`: a canonical `major.minor.patch`
    /// ([`is_canonical_version`]), as it is joined into an install's path; `None` when the key is absent or null. Else
    /// the usage line, the value shortened and shown with its control characters escaped.
    fn install_version(&self, key: &str) -> Result<Option<&str>, String> {
        match self.object.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(version)) if is_canonical_version(version) => Ok(Some(version)),
            Some(other) => Err(format!(
                "gate: box payload {key} is not a version: '{}'",
                escaped(&shown_name(&other.as_str().map_or_else(|| other.to_string(), str::to_owned)))
            )),
        }
    }

    /// The names the run's commands get set to the root of the Unreal Engine install it uses, `EngineEnv`: an object of
    /// each name and `"unreal"`, as the repo's toolchain manifest `[env]` names them; none when the key is absent or
    /// null. Else the usage line: not an object of texts, over the cap `Env` and `EngineEnv` share
    /// (`gate_args::MOST_ENV_NAMES`, `gate_args::MOST_ENV_BYTES`), a name the gate refuses ([`engine_env_name_refused`]),
    /// a value other than `"unreal"`, or a name given twice in any ASCII case, the name shortened and escaped. The cap is
    /// counted over the raw objects before any pair is copied.
    pub fn engine_env(&self) -> Result<Vec<String>, String> {
        let (names, bytes) = self.pair_size(ENGINE_ENV)?.unwrap_or_default();
        let (env_names, env_bytes) = self.pair_size("Env")?.unwrap_or_default();
        if over_cap(names + env_names, bytes + env_bytes) {
            return Err(format!(
                "gate: box payload {ENGINE_ENV} is over its cap of {MOST_ENV_NAMES} names or {} KiB",
                MOST_ENV_BYTES / 1024
            ));
        }
        let pairs = self.pairs(ENGINE_ENV)?;
        let mut names: Vec<String> = Vec::with_capacity(pairs.len());
        for (name, value) in pairs {
            let shown = escaped(&shown_name(&name));
            if engine_env_name_refused(&name) {
                return Err(format!("gate: box payload {ENGINE_ENV} sets {shown}, a name the gate sets or refuses"));
            }
            if value != ENGINE_ENV_UNREAL {
                return Err(format!(
                    "gate: box payload {ENGINE_ENV} gives {shown} a value other than \"{ENGINE_ENV_UNREAL}\": '{}'",
                    escaped(&shown_name(&value))
                ));
            }
            if names.iter().any(|earlier| earlier.eq_ignore_ascii_case(&name)) {
                return Err(format!("gate: box payload {ENGINE_ENV} names {shown} twice"));
            }
            names.push(name);
        }
        Ok(names)
    }

    /// What the run runs ([`Commands`]): the one `Command` of a payload that sets no `WarmCommands`, else the warm-up's
    /// `WarmCommands` and its `WarmHash`, in a payload that sets no `Command`, asks for no outputs (no `Outputs` glob,
    /// `OutputsFull` not true) and gives a `Timeout`. Else the usage line.
    pub fn commands(&self) -> Result<Commands<'_>, String> {
        if !self.sets("WarmCommands") {
            return Ok(Commands {
                words: vec![self.words("Command")?],
                warm_hash: None,
            });
        }
        if self.sets("Command") {
            return Err("gate: box payload sets both Command and WarmCommands".to_owned());
        }
        let words = self.warm_commands()?;
        let warm_hash = self.warm_hash()?;
        if !self.output_globs()?.is_empty() || self.outputs_full()? {
            return Err("gate: box payload asks a warm-up for outputs".to_owned());
        }
        if self.sets("Env") {
            return Err("gate: box payload asks a warm-up for Env".to_owned());
        }
        // The budget bounds the warm-up's commands, their wall time included, though not the layout, checkout and
        // restores before them; a warm-up never goes without one.
        if self.timeout()?.is_none() {
            return Err("gate: box payload lacks Timeout".to_owned());
        }
        Ok(Commands {
            words,
            warm_hash: Some(warm_hash),
        })
    }

    /// The warm-up's `WarmCommands`: a list of 1 to [`MOST_COMMANDS`] commands, each a non-empty list of non-empty
    /// texts; else the usage line.
    fn warm_commands(&self) -> Result<Vec<Vec<String>>, String> {
        let malformed =
            || format!("gate: box payload WarmCommands is not a list of 1 to {MOST_COMMANDS} commands, each a non-empty list of non-empty texts");
        let commands = self
            .object
            .get("WarmCommands")
            .and_then(Value::as_array)
            .filter(|commands| (1..=MOST_COMMANDS).contains(&commands.len()))
            .ok_or_else(malformed)?;
        commands
            .iter()
            .map(|command| {
                let words = command.as_array().filter(|words| !words.is_empty()).ok_or_else(malformed)?;
                words
                    .iter()
                    .map(|word| word.as_str().filter(|word| !word.is_empty()).map(str::to_owned).ok_or_else(malformed))
                    .collect()
            })
            .collect()
    }

    /// The warm-up's `WarmHash`, the SHA-256 of the warm-up file the commands come from: 64 lower-case hex digits.
    /// Else the usage line saying the payload lacks it (absent or null) or, the value shown with its control characters
    /// escaped, that it is not one.
    fn warm_hash(&self) -> Result<&str, String> {
        match self.object.get("WarmHash") {
            None | Some(Value::Null) => Err("gate: box payload lacks WarmHash".to_owned()),
            Some(Value::String(hash)) if is_lower_hex(hash, SHA256_DIGITS) => Ok(hash),
            Some(other) => Err(format!(
                "gate: box payload WarmHash is not 64 lower-case hex digits: {}",
                escaped(&other.as_str().map_or_else(|| other.to_string(), str::to_owned))
            )),
        }
    }

    /// Whether the payload sets `key` to anything but null.
    #[must_use]
    pub fn sets(&self, key: &str) -> bool {
        !matches!(self.object.get(key), None | Some(Value::Null))
    }

    /// The run's `Snapshot`: the payload's commit, exactly 40 lowercase hex digits, as the client sends it; else the
    /// line saying it is not one. This is PowerShell's `-cmatch '^[0-9a-f]{40}$'` without .NET's allowance for a
    /// trailing newline. A missing key and a JSON null are shown as the empty string, any other non-string value as its
    /// compact JSON text, each with its control characters escaped, so a value cannot break or forge the line.
    pub fn snapshot(&self) -> Result<&str, String> {
        commit(self.object.get("Snapshot")).map_err(|shown| not_a_snapshot("Snapshot", &shown))
    }
}

/// What a run runs, as [`Payload::commands`] reads it.
pub struct Commands<'a> {
    /// Each command's words, its program first: the one command of a run, or each of a warm-up's in order.
    pub words: Vec<Vec<String>>,
    /// The warm-up file's hash the box records once every command passed; `None` for a run.
    pub warm_hash: Option<&'a str>,
}

/// A sibling pair as the box lays it out: the main checkout's folder name, `Leaf`, and each sibling with the bare
/// repository its snapshot is pushed to.
pub struct Pair {
    leaf: String,
    siblings: Vec<PairSibling>,
}

impl Pair {
    /// The main checkout's folder name.
    #[must_use]
    pub fn leaf(&self) -> &str {
        &self.leaf
    }

    /// The siblings, in the payload's order.
    #[must_use]
    pub fn siblings(&self) -> &[PairSibling] {
        &self.siblings
    }
}

/// Why the box refuses a run's `Pair`: the usage line, and the sibling it names when one sibling alone is refused.
#[derive(Debug)]
pub struct PairRefused {
    pub line: String,
    pub sibling: Option<String>,
}

impl From<String> for PairRefused {
    fn from(line: String) -> PairRefused {
        PairRefused { line, sibling: None }
    }
}

/// One sibling of a [`Pair`]: its folder name and its bare repository, `{root}\{repo_name(name)}.git` beside the main's,
/// derived on the box and never read from the payload.
pub struct PairSibling {
    name: String,
    bare: String,
}

impl PairSibling {
    /// The sibling's folder name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The sibling's bare repository on the box.
    #[must_use]
    pub fn bare(&self) -> &str {
        &self.bare
    }
}

/// The sibling pair checks: the names the box lays out, the key they share and the bare repositories beside `Work`.
impl Payload {
    /// The run's sibling `Pair`, `{"Leaf": "<name>", "Siblings": ["<name>", …]}`; `None` when the key is absent or null,
    /// a run of one repo. Else the usage line: a name the box cannot lay out ([`is_layout_name`]), no sibling or more
    /// than [`MOST_SIBLINGS`], two names (the leaf included) equal ignoring ASCII case, a `Key` that is not the leaf's
    /// (`<repo_name(leaf)>-<8 hex>`), a `Bare` that is not `<root>\<name>.git` for the work folder `<root>\work\<key>`
    /// (`/` for `\` under [`PathForm::Unix`]), or two bare repositories (the main's and each sibling's
    /// `<repo_name(name)>.git`) equal ignoring ASCII case.
    pub fn pair(&self) -> Result<Option<Pair>, String> {
        self.laid_out().map_err(|refused| refused.line)
    }

    /// The run's sibling `Pair` as [`Payload::pair`] reads it; else its usage line and, when it refuses one sibling's
    /// bare repository, that sibling's name.
    pub fn laid_out(&self) -> Result<Option<Pair>, PairRefused> {
        let (leaf, names) = match self.object.get("Pair") {
            None | Some(Value::Null) => return Ok(None),
            Some(value) => pair_names(value)?,
        };
        check_pair_names(&leaf, &names)?;
        let (key, work) = self.work_folder()?;
        if !is_key_of(key, &leaf) {
            return Err(format!("gate: box payload Key {key} is not the work-folder key of the Pair's Leaf {leaf}").into());
        }
        let bare = self.path("Bare")?;
        let separator = self.form.separator();
        let Some(root) = grandparent(work) else {
            return Err(format!(
                "gate: box payload Work must be <root>{separator}work{separator}<key> with a Pair: {}",
                escaped(work)
            )
            .into());
        };
        let Some(main_stem) = bare_stem(bare, root, self.form) else {
            return Err(format!(
                "gate: box payload Bare must be {root}{separator}<name>.git with a Pair: {}",
                escaped(bare)
            )
            .into());
        };
        let mut stems = vec![main_stem.to_owned()];
        let mut siblings = Vec::with_capacity(names.len());
        for name in names {
            let stem = repo_name(&name);
            if stems.iter().any(|taken| taken.eq_ignore_ascii_case(&stem)) {
                return Err(PairRefused {
                    line: format!("gate: box payload Pair sibling {name} would share the bare repository {stem}.git"),
                    sibling: Some(name),
                });
            }
            siblings.push(PairSibling {
                bare: format!("{root}{separator}{stem}.git"),
                name,
            });
            stems.push(stem);
        }
        Ok(Some(Pair { leaf, siblings }))
    }

    /// The commit of each sibling of `pair`, in its order, from the run's `SiblingSnapshots`, an object whose names are
    /// exactly the siblings' and whose values are each 40 lowercase hex digits, as [`Payload::snapshot`] reads them;
    /// else the line saying why.
    pub fn sibling_snapshots(&self, pair: &Pair) -> Result<Vec<&str>, String> {
        let object = match self.object.get("SiblingSnapshots") {
            None | Some(Value::Null) => None,
            Some(Value::Object(object)) => Some(object),
            Some(_) => return Err("gate: box payload SiblingSnapshots is not an object".to_owned()),
        };
        // A pair has a sibling, so no SiblingSnapshots never names them all.
        let named =
            object.filter(|object| object.len() == pair.siblings.len() && pair.siblings.iter().all(|sibling| object.contains_key(&sibling.name)));
        let Some(object) = named else {
            let names: Vec<&str> = pair.siblings.iter().map(PairSibling::name).collect();
            return Err(format!(
                "gate: box payload SiblingSnapshots must name exactly the Pair's siblings: {}",
                names.join(", ")
            ));
        };
        pair.siblings
            .iter()
            .map(|sibling| commit(object.get(&sibling.name)).map_err(|shown| not_a_snapshot(&format!("SiblingSnapshots {}", sibling.name), &shown)))
            .collect()
    }
}

/// The box path of every tree a run checks out in the work folder `work`: the main repo's, then each sibling's in the
/// order given. With no `pair`, `work` itself and no sibling; with the pair of the main checkout's folder name and the
/// siblings' folder names, `{work}\{leaf}` and `{work}\{name}` for each sibling, joined with `form`'s separator. The box
/// lays its trees out at these paths and the client rewrites recorded box paths from them, so the two never compute
/// them apart.
#[must_use]
pub fn trees(work: &str, pair: Option<(&str, &[&str])>, form: PathForm) -> (String, Vec<String>) {
    let Some((leaf, siblings)) = pair else {
        return (work.to_owned(), Vec::new());
    };
    let separator = form.separator();
    let tree = |name: &str| format!("{work}{separator}{name}");
    (tree(leaf), siblings.iter().map(|name| tree(name)).collect())
}

/// The leaf and the sibling names of the `Pair` value `value`; else the usage line saying it is not one.
fn pair_names(value: &Value) -> Result<(String, Vec<String>), String> {
    let not_a_pair = || "gate: box payload Pair is not an object of a Leaf text and a Siblings list of texts".to_owned();
    let object = value.as_object().ok_or_else(not_a_pair)?;
    let leaf = object.get("Leaf").and_then(Value::as_str).ok_or_else(not_a_pair)?;
    let names = object.get("Siblings").and_then(Value::as_array).ok_or_else(not_a_pair)?;
    let names = names
        .iter()
        .map(|name| name.as_str().map(str::to_owned).ok_or_else(not_a_pair))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((leaf.to_owned(), names))
}

/// Checks the pair's names: each one the box can lay out, 1 to [`MOST_SIBLINGS`] siblings, and no two, the leaf
/// included, equal ignoring ASCII case; else the usage line.
fn check_pair_names(leaf: &str, names: &[String]) -> Result<(), String> {
    if let Some(name) = std::iter::once(leaf)
        .chain(names.iter().map(String::as_str))
        .find(|name| !is_layout_name(name))
    {
        return Err(format!(
            "gate: box payload Pair name '{}' is not a folder name of letters, digits, '.', '_' and '-' not starting or ending \
             with a dot, not a device name",
            escaped(name)
        ));
    }
    if names.is_empty() {
        return Err("gate: box payload Pair names no sibling".to_owned());
    }
    if names.len() > MOST_SIBLINGS {
        return Err(format!("gate: box payload Pair names more than {MOST_SIBLINGS} siblings"));
    }
    for (index, name) in names.iter().enumerate() {
        let mut earlier = std::iter::once(leaf).chain(names[..index].iter().map(String::as_str));
        if let Some(earlier) = earlier.find(|earlier| earlier.eq_ignore_ascii_case(name)) {
            return Err(format!("gate: box payload Pair names {name} and {earlier}, equal ignoring case"));
        }
    }
    Ok(())
}

/// Whether `key` is the work-folder key of a root whose folder name is `leaf`: `<repo_name(leaf)>-<8 lower-case hex>`.
fn is_key_of(key: &str, leaf: &str) -> bool {
    let name = repo_name(leaf);
    key.strip_prefix(name.as_str())
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|digits| is_lower_hex(digits, KEY_HASH_DIGITS))
}

/// The stem of the bare repository `bare` when it is `<root>\<stem>.git`, `<stem>` one or more ASCII letters, digits,
/// `-` and `_`, its folder compared with `root` as `form` compares paths ([`same_path`]); else `None`.
#[must_use]
pub fn bare_stem<'a>(bare: &'a str, root: &str, form: PathForm) -> Option<&'a str> {
    let split = bare.rfind(['\\', '/'])?;
    let (folder, file) = (&bare[..split], &bare[split + 1..]);
    let dot = file.len().checked_sub(".git".len())?;
    let (stem, suffix) = (file.get(..dot)?, file.get(dot..)?);
    (suffix.eq_ignore_ascii_case(".git") && is_key(stem) && same_path(folder, root, form)).then_some(stem)
}

/// The commit `value` names: exactly 40 lowercase hex digits; else the value as a refusal line shows it, a missing key
/// and a JSON null as the empty string and any other non-string value as its compact JSON text.
fn commit(value: Option<&Value>) -> Result<&str, String> {
    match value {
        Some(Value::String(text)) if is_lower_hex(text, COMMIT_SHA_DIGITS) => Ok(text),
        Some(Value::String(text)) => Err(text.clone()),
        Some(Value::Null) | None => Err(String::new()),
        Some(other) => Err(other.to_string()),
    }
}

/// The client's work-folder checks: the key, the work folder named for it, and the run's job and files beside it.
impl Payload {
    /// The work-folder key: ASCII letters, digits, `-` and `_`, as the client builds it from the repo's name; else the
    /// usage line, the key shown with its control characters escaped.
    pub fn key(&self) -> Result<&str, String> {
        let key = self.text("Key")?;
        if is_key(key) { Ok(key) } else { Err(not_a_key(key)) }
    }

    /// The key and the work folder: `Work` an absolute path ([`Payload::path`]), with no trailing separator, whose last
    /// name is the key, without regard to ASCII case under [`PathForm::Windows`] and exactly under [`PathForm::Unix`];
    /// else the usage line.
    pub fn work_folder(&self) -> Result<(&str, &str), String> {
        let work = self.path("Work")?;
        if work.ends_with(['\\', '/']) {
            return Err(format!("gate: box payload Work must not end with a separator: {}", escaped(work)));
        }
        let key = self.key()?;
        let last = work.rsplit(['\\', '/']).next();
        let named = match self.form {
            PathForm::Windows => last.is_some_and(|last| last.eq_ignore_ascii_case(key)),
            PathForm::Unix => last == Some(key),
        };
        if named { Ok((key, work)) } else { Err(not_a_key(key)) }
    }

    /// The path of `key`, which must be `<work>.<extension>`, as the client builds the lock and the index; compared as
    /// the payload's form compares paths ([`same_path`]). Else the usage line.
    pub fn beside_work(&self, key: &str, work: &str, extension: &str) -> Result<&str, String> {
        let value = self.path(key)?;
        let expected = format!("{work}.{extension}");
        if same_path(value, &expected, self.form) {
            Ok(value)
        } else {
            Err(format!("gate: box payload {key} must be {expected}"))
        }
    }

    /// The `OutputFolder`, which must be `<root>\outputs` for the work folder `work`, `<root>\work\<key>` (`/` for `\`
    /// under [`PathForm::Unix`]), as the client builds it; compared as the payload's form compares paths
    /// ([`same_path`]). Else the usage line.
    pub fn output_folder(&self, work: &str) -> Result<&str, String> {
        let value = self.path("OutputFolder")?;
        let separator = self.form.separator();
        let Some(root) = grandparent(work) else {
            return Err(format!(
                "gate: box payload Work must be <root>{separator}work{separator}<key> to place OutputFolder: {}",
                escaped(work)
            ));
        };
        let expected = format!("{root}{separator}outputs");
        if same_path(value, &expected, self.form) {
            Ok(value)
        } else {
            Err(format!("gate: box payload OutputFolder must be {expected}"))
        }
    }

    /// The outputs archive's `Name`: `<key>-<nonce>.tar.zst` with the payload's own key and an outputs nonce, as the box
    /// names a run's archive; else the line refusing it, the name shown with its control characters escaped, or the
    /// usage line saying the payload lacks it. The outputs verb can so reach no file in `OutputFolder` but an archive of
    /// this work folder.
    pub fn archive_name(&self) -> Result<&str, String> {
        let key = self.key()?;
        let name = self.text("Name")?;
        if archive_key(name) == Some(key) {
            Ok(name)
        } else {
            Err(format!(
                "gate: remote outputs archive name '{}' is not an outputs archive of this work folder",
                escaped(name)
            ))
        }
    }

    /// The run's job name, `Local\gate-remote-<key>-<nonce>` (`gate-remote-<key>-<nonce>` under [`PathForm::Unix`]) with
    /// the payload's own key and a nonce of 12 hex digits, as the client names it; else the usage line. A stop can so
    /// reach no job but a run's of this work folder.
    pub fn job(&self, key: &str) -> Result<&str, String> {
        let job = self.text("Job")?;
        if is_job_name(job, key, self.form) {
            Ok(job)
        } else {
            Err(format!("gate: box payload Job is not this work folder's job name: {}", escaped(job)))
        }
    }
}

/// Whether `job` is a run's job name of the work folder `key` under `form`: `Local\gate-remote-<key>-<nonce>` under
/// [`PathForm::Windows`], `gate-remote-<key>-<nonce>` under [`PathForm::Unix`], the nonce 12 hex digits, as the client
/// names it.
#[must_use]
pub fn is_job_name(job: &str, key: &str, form: PathForm) -> bool {
    is_nonce(job.strip_prefix(form.job_prefix()).and_then(|rest| rest.strip_prefix(key)))
}

/// Whether `job`, read from an interrupted-command record, is a run's job name of the work folder `key` as
/// [`is_job_name`] says, but for the key's case, which a record written under another case of the key may differ in.
#[must_use]
pub fn is_recorded_job_name(job: &str, key: &str, form: PathForm) -> bool {
    let rest = job.strip_prefix(form.job_prefix());
    is_nonce(rest.and_then(|rest| {
        let (named, after) = rest.split_at_checked(key.len())?;
        named.eq_ignore_ascii_case(key).then_some(after)
    }))
}

/// Whether `after_key`, what follows the key in a job name, is `-` and a nonce of 12 hex digits.
fn is_nonce(after_key: Option<&str>) -> bool {
    let nonce = after_key.and_then(|rest| rest.strip_prefix('-'));
    nonce.is_some_and(|nonce| nonce.len() == NONCE_DIGITS && nonce.chars().all(|digit| digit.is_ascii_hexdigit()))
}

/// The hex digits of a run's nonce, as the client draws it.
const NONCE_DIGITS: usize = 12;
/// The hex digits of a run's outputs nonce, as the client draws it.
pub const OUTPUTS_NONCE_DIGITS: usize = 32;
/// The hex digits of a commit's sha.
const COMMIT_SHA_DIGITS: usize = 40;
/// The hex digits that end a work-folder key, after the repo name and a `-`.
const KEY_HASH_DIGITS: usize = 8;

/// Whether `text` is a work-folder key: one or more ASCII letters, digits, `-` and `_`.
#[must_use]
pub fn is_key(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

/// The name of the outputs archive of the work folder `key` for the outputs nonce `nonce`: `<key>-<nonce>.tar.zst`.
/// [`archive_key`] reads it back.
#[must_use]
pub fn archive_file_name(key: &str, nonce: &str) -> String {
    format!("{key}-{nonce}{SUFFIX}")
}

/// The work-folder key of `name` when it is an outputs archive's name as [`archive_file_name`] makes it, with a
/// work-folder key and an outputs nonce of 32 lower-case hex digits; else `None`.
#[must_use]
pub fn archive_key(name: &str) -> Option<&str> {
    let stem = name.strip_suffix(SUFFIX)?;
    let split = stem.len().checked_sub(OUTPUTS_NONCE_DIGITS + 1)?;
    let key = stem.get(..split)?;
    let nonce = stem.get(split..)?.strip_prefix('-')?;
    (is_key(key) && is_lower_hex(nonce, OUTPUTS_NONCE_DIGITS)).then_some(key)
}

/// The `<root>` of the work folder `path`, `<root>\work\<key>` with `work` in any case, without a trailing separator;
/// `None` when `path` is not one.
#[must_use]
pub fn grandparent(path: &str) -> Option<&str> {
    let parent = &path[..path.rfind(['\\', '/'])?];
    let split = parent.rfind(['\\', '/'])?;
    let (root, middle) = (&parent[..split], &parent[split + 1..]);
    (!root.is_empty() && middle.eq_ignore_ascii_case("work")).then_some(root)
}

/// The builder's work root of the work folder `work`, `<root>\work\<key>`, as a path to open: [`grandparent`]'s
/// `<root>` without a trailing separator, a doubled one included, and under [`PathForm::Windows`] a drive's own root
/// with its separator (`D:\`); `None` when `work` is not laid out so, or its `<root>` is only separators, as a Unix
/// `/work/<key>`'s is.
#[must_use]
pub fn work_root(work: &str, form: PathForm) -> Option<String> {
    let root = grandparent(work)?.trim_end_matches(['\\', '/']);
    if root.is_empty() {
        None
    } else if form == PathForm::Windows && root.ends_with(':') {
        Some(format!(r"{root}\"))
    } else {
        Some(root.to_owned())
    }
}

/// Whether `first` and `second` are the same path as `form` compares them: under [`PathForm::Windows`] as
/// [`same_full_path`] does, under [`PathForm::Unix`] byte for byte, since [`is_unix_path`] admits one spelling of each
/// path.
fn same_path(first: &str, second: &str, form: PathForm) -> bool {
    match form {
        PathForm::Windows => same_full_path(first, second),
        PathForm::Unix => first == second,
    }
}

/// Whether `first` and `second` are the same path once made full, without regard to case. Never when a part of
/// `first` other than `.` or `..` ends in `.` or a space, which making the path full would drop, so the path that
/// passes is the one opened.
fn same_full_path(first: &str, second: &str) -> bool {
    let renamed = first
        .split(['\\', '/'])
        .any(|part| !matches!(part, "." | "..") && part.ends_with(['.', ' ']));
    let full = |path: &str| std::path::absolute(path).map(|full| ordinal_upper(&full.to_string_lossy())).ok();
    !renamed && matches!((full(first), full(second)), (Some(first_full), Some(second_full)) if first_full == second_full)
}

/// The line saying the value at the payload key `key` is not an object of texts.
fn not_an_object(key: &str) -> String {
    format!("gate: box payload {key} is not an object of texts")
}

fn not_a_key(key: &str) -> String {
    format!("gate: box payload Key is not a work-folder key: {}", escaped(key))
}

/// The line refusing a run whose payload named no 40-hex commit as the snapshot `what`, showing `value` as it is.
fn not_a_snapshot(what: &str, value: &str) -> String {
    format!("gate: remote run payload's {what} '{}' is not a 40-hex commit sha", escaped(value))
}

/// `text` with each character mapped to its simple uppercase, as .NET's `OrdinalIgnoreCase` compares: a character
/// whose uppercase is more than one character is kept.
#[must_use]
pub fn ordinal_upper(text: &str) -> String {
    text.chars()
        .map(|character| {
            let mut upper = character.to_uppercase();
            match (upper.next(), upper.next()) {
                (Some(single), None) => single,
                _ => character,
            }
        })
        .collect()
}

/// Whether `text` is a version in the one spelling a Godot or Unreal Engine install's folder has: three dot-separated
/// parts, each of one to [`MAX_PART_DIGITS`] ASCII digits with no leading zero unless the part is `0`, as the godot and
/// unreal probes spell the version an install reports.
#[must_use]
pub fn is_canonical_version(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    let canonical = |part: &&str| {
        (1..=MAX_PART_DIGITS).contains(&part.len()) && part.bytes().all(|byte| byte.is_ascii_digit()) && (*part == "0" || !part.starts_with('0'))
    };
    parts.len() == 3 && parts.iter().all(canonical)
}

/// Whether `value` is a path absolute on a drive, `X:\…` or `X:/…`, with no character below U+0020.
#[must_use]
pub fn is_drive_path(value: &str) -> bool {
    let rooted = matches!(value.as_bytes(), [drive, b':', b'\\' | b'/', ..] if drive.is_ascii_alphabetic());
    rooted && !value.chars().any(|character| character < ' ')
}

/// Whether `value` is an absolute Unix path in its one spelling: `/` and one or more names, with no `\`, no character
/// below U+0020, and no name that is empty (`//`), `.` or `..`. A single trailing `/` is allowed here, as a trailing
/// separator is on a drive; [`Payload::work_folder`] refuses it.
#[must_use]
pub fn is_unix_path(value: &str) -> bool {
    let Some(names) = value.strip_prefix('/') else {
        return false;
    };
    let names = names.strip_suffix('/').unwrap_or(names);
    !names.is_empty()
        && !value.chars().any(|character| character < ' ' || character == '\\')
        && names.split('/').all(|name| !matches!(name, "" | "." | ".."))
}

fn object(word: &str) -> Result<Map<String, Value>, String> {
    let bytes = STANDARD.decode(word).map_err(|error| error.to_string())?;
    let json = String::from_utf8(bytes).map_err(|error| format!("the decoded bytes are not UTF-8: {error}"))?;
    match serde_json::from_str(&json).map_err(|error| error.to_string())? {
        Value::Object(map) => Ok(map),
        _ => Err("the JSON is not an object".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde_json::json;

    use super::{PathForm, Payload, escaped, first_version, version_of, version_text, work_root};

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

    /// `git-lfs version` answers `git-lfs/<x.y.z> (...)`; the version is the one after the slash, never the go or git
    /// version in the parentheses.
    #[test]
    fn git_lfs_version_is_read_from_its_slash_line() {
        assert_eq!(
            version_of("git-lfs/3.8.0 (GitHub; windows amd64; go 1.24.4; git 2d2b1a8c)\n", ""),
            Some("3.8.0".to_owned())
        );
        assert_eq!(
            version_of("git-lfs/3.7.1-rc1 (GitHub; windows arm64; go 1.24.2)\r\n", ""),
            Some("3.7.1".to_owned())
        );
    }

    #[test]
    fn the_version_is_the_first_dotted_number_after_an_optional_v() {
        for (text, version) in [
            ("rustc 1.99.0 (b940084d7 2026-09-28)", "1.99.0"),
            ("v24.19.0", "24.19.0"),
            ("12.5.1", "12.5.1"),
            ("PowerShell 7.6.6", "7.6.6"),
            ("ffmpeg version 9.0.2-full_build-www.gyan.dev Copyright (c) 2000-2026", "9.0.2"),
            ("cargo-nextest 0.9.100 (abc 2026-01-01)", "0.9.100"),
            ("cargo-nextest 0.9.148 (cd1d6d546 2026-10-07)", "0.9.148"),
            ("10.12", "10.12"),
            ("tool 1.2.3.4", "1.2.3"),
            ("tool 1.2.", "1.2"),
            ("tool 1234567890.1", "1234567890.1"),
        ] {
            assert_eq!(first_version(text).as_deref(), Some(version), "{text:?}");
        }
    }

    #[test]
    fn text_with_no_dotted_number_has_no_version() {
        for text in [
            "ffmpeg version N-112345-gabcdef",
            "",
            "24",
            "x1.2 rev2.3",
            "abc1.2.3",
            "1..2",
            "release 7.",
        ] {
            assert_eq!(first_version(text), None, "{text:?}");
        }
    }

    #[test]
    fn a_number_of_more_than_ten_digits_is_no_readable_version() {
        for text in [
            "tool 12345678901.2",
            "tool 1.12345678901",
            "tool 1.2.3.12345678901",
            "v99999999999.0 then 1.2.3",
        ] {
            assert_eq!(first_version(text), None, "{text:?}");
        }
    }

    #[test]
    fn only_the_first_line_of_the_chosen_stream_is_read() {
        let gyan = "ffmpeg version 9.0.2-full_build-www.gyan.dev Copyright (c) 2000-2026\r\nbuilt with gcc 14.2.0\r\n";
        assert_eq!(version_of(gyan, "").as_deref(), Some("9.0.2"));
        let git_build = "ffmpeg version N-117456-gabc Copyright (c) 2000-2026\r\nbuilt with gcc 14.2.0\r\n";
        assert_eq!(version_of(git_build, ""), None, "the compiler's version is not the tool's");
        assert_eq!(version_of("\r\n  \r\nv24.19.0\r\n", "").as_deref(), Some("24.19.0"), "blank lines first");
        assert_eq!(
            version_of(" \r\n", "PowerShell 7.6.6\r\n").as_deref(),
            Some("7.6.6"),
            "stderr when stdout is blank"
        );
        assert_eq!(
            version_of("no version here\r\n", "7.6.6\r\n"),
            None,
            "stderr is not read when stdout has text"
        );
    }

    fn decoded(json: &str) -> Result<Payload, String> {
        Payload::decode(&STANDARD.encode(json), PathForm::Windows)
    }

    fn text(json: &str, key: &str) -> Result<String, String> {
        decoded(json).and_then(|payload| payload.text(key).map(str::to_owned))
    }

    /// What the `GodotVersion` reader makes of the payload whose `GodotVersion` is `version`.
    fn godot_version_of(version: &str) -> Result<Option<String>, String> {
        decoded(&json!({ "GodotVersion": version }).to_string()).and_then(|payload| payload.godot_version().map(|read| read.map(str::to_owned)))
    }

    /// `GodotVersion` is three parts of at most ten digits each, none with a leading zero but a part that is `0`, the
    /// one spelling an install's folder is named.
    #[test]
    fn a_godot_version_is_three_canonical_parts_of_at_most_ten_digits() {
        for version in ["4.7.2", "0.0.0", "4.0.10", "9999999999.0.1"] {
            assert_eq!(godot_version_of(version), Ok(Some(version.to_owned())), "{version}");
        }
        for version in ["04.7.2", "4.07.2", "4.7.00", "4.7.02", "99999999999.0.1", "4.7.12345678901"] {
            assert_eq!(
                godot_version_of(version),
                Err(format!("gate: box payload GodotVersion is not a version: '{version}'")),
                "{version}"
            );
        }
    }

    /// `LfsDigest` is 64 lower-case hex digits, as `InputsDigest` is; absent or null is none.
    #[test]
    fn an_lfs_digest_is_64_lower_case_hex_digits() {
        let lfs_digest_of = |value: serde_json::Value| {
            decoded(&json!({ "LfsDigest": value }).to_string()).and_then(|payload| payload.lfs_digest().map(|read| read.map(str::to_owned)))
        };
        let good = "0a".repeat(32);
        assert_eq!(lfs_digest_of(json!(good)), Ok(Some(good.clone())));
        assert_eq!(lfs_digest_of(serde_json::Value::Null), Ok(None), "null is none");
        assert_eq!(
            decoded("{}").and_then(|payload| payload.lfs_digest().map(|read| read.map(str::to_owned))),
            Ok(None)
        );
        for bad in [json!(good.to_uppercase()), json!(&good[..63]), json!(format!("{good}0")), json!(12)] {
            let shown = bad.as_str().map_or_else(|| bad.to_string(), str::to_owned);
            assert_eq!(
                lfs_digest_of(bad),
                Err(format!("gate: box payload LfsDigest is not 64 lower-case hex digits: {shown}")),
                "{shown}"
            );
        }
    }

    /// What the `UnrealVersion` reader makes of the payload whose `UnrealVersion` is `version`.
    fn unreal_version_of(version: &serde_json::Value) -> Result<Option<String>, String> {
        decoded(&json!({ "UnrealVersion": version }).to_string()).and_then(|payload| payload.unreal_version().map(|read| read.map(str::to_owned)))
    }

    /// `UnrealVersion` is read as `GodotVersion` is, with its own key in the refusal, and a null one is none.
    #[test]
    fn an_unreal_version_is_three_canonical_parts_of_at_most_ten_digits() {
        for version in ["5.8.0", "0.0.0", "5.10.3", "9999999999.0.1"] {
            assert_eq!(unreal_version_of(&json!(version)), Ok(Some(version.to_owned())), "{version}");
        }
        for version in ["05.8.0", "5.08.0", "5.8.00", "5.8", "99999999999.0.1", "5.8.12345678901"] {
            assert_eq!(
                unreal_version_of(&json!(version)),
                Err(format!("gate: box payload UnrealVersion is not a version: '{version}'")),
                "{version}"
            );
        }
        assert_eq!(unreal_version_of(&serde_json::Value::Null), Ok(None));
        assert_eq!(
            decoded("{}").and_then(|payload| payload.unreal_version().map(|read| read.is_none())),
            Ok(true)
        );
    }

    /// What the `EngineEnv` reader makes of the payload whose `EngineEnv` is `engine_env`.
    fn engine_env_of(engine_env: &serde_json::Value) -> Result<Vec<String>, String> {
        decoded(&json!({ "EngineEnv": engine_env }).to_string()).and_then(|payload| payload.engine_env())
    }

    #[test]
    fn engine_env_names_are_read_in_order_and_none_when_absent_or_null() {
        assert_eq!(
            engine_env_of(&json!({ "UE_MCP_TEST_ENGINE_ROOT": "unreal", "OTHER_ROOT": "unreal" })),
            Ok(vec!["UE_MCP_TEST_ENGINE_ROOT".to_owned(), "OTHER_ROOT".to_owned()])
        );
        assert_eq!(engine_env_of(&serde_json::Value::Null), Ok(Vec::new()));
        assert_eq!(decoded("{}").and_then(|payload| payload.engine_env()), Ok(Vec::new()));
    }

    #[test]
    fn a_malformed_engine_env_is_refused() {
        for bad in [json!(["UE_MCP_TEST_ENGINE_ROOT"]), json!("unreal"), json!({ "A": 1 })] {
            assert_eq!(
                engine_env_of(&bad),
                Err("gate: box payload EngineEnv is not an object of texts".to_owned()),
                "{bad}"
            );
        }
        assert_eq!(
            engine_env_of(&json!({ "A": "godot\n" })),
            Err("gate: box payload EngineEnv gives A a value other than \"unreal\": 'godot\\n'".to_owned())
        );
        assert_eq!(
            engine_env_of(&json!({ "Engine_Root": "unreal", "ENGINE_ROOT": "unreal" })),
            Err("gate: box payload EngineEnv names ENGINE_ROOT twice".to_owned())
        );
        assert_eq!(
            engine_env_of(&json!({ "UE_ROOT": "godot" })),
            Err("gate: box payload EngineEnv sets UE_ROOT, a name the gate sets or refuses".to_owned()),
            "a refused name is named before its value, as the PC does"
        );
    }

    /// `EngineEnv` and `Env` share `Env`'s cap of 64 names or 32 KiB, counted before any pair is read.
    #[test]
    fn engine_env_over_the_cap_it_shares_with_env_is_refused() {
        let over = "gate: box payload EngineEnv is over its cap of 64 names or 32 KiB".to_owned();
        let named =
            |count: usize| -> serde_json::Map<String, serde_json::Value> { (0..count).map(|at| (format!("N{at}"), json!("unreal"))).collect() };
        let read = |json: serde_json::Value| decoded(&json.to_string()).and_then(|payload| payload.engine_env());

        assert_eq!(read(json!({ "EngineEnv": named(64) })).map(|names| names.len()), Ok(64));
        assert_eq!(read(json!({ "EngineEnv": named(65) })), Err(over.clone()));
        assert_eq!(
            read(json!({ "EngineEnv": named(60), "Env": { "A": "1", "B": "2", "C": "3", "D": "4", "E": "5" } })),
            Err(over.clone())
        );
        let long = "N".repeat(32 * 1024);
        assert_eq!(read(json!({ "EngineEnv": { long: "unreal" } })), Err(over), "over the bytes");
    }

    #[test]
    fn an_engine_env_name_the_gate_refuses_is_refused() {
        for name in [
            "UE_ROOT",
            "GODOT_PATH",
            "GATE_X",
            "PATH",
            "A=B",
            "GIT_WORK_TREE",
            "git_dir",
            "A-B",
            "1A",
            "",
        ] {
            assert_eq!(
                engine_env_of(&json!({ name: "unreal" })),
                Err(format!("gate: box payload EngineEnv sets {name}, a name the gate sets or refuses")),
                "{name}"
            );
        }
        assert_eq!(
            engine_env_of(&json!({ "BAD\u{1}": "unreal" })),
            Err("gate: box payload EngineEnv sets BAD\\u{1}, a name the gate sets or refuses".to_owned()),
            "a forged name is escaped"
        );
    }

    const NOT_BASE64_JSON: &str = "gate: box payload is not base64 JSON: ";

    /// Checks `refused` is the not-base64-JSON line with a reason after its prefix; the reason's wording is the
    /// decoder's own.
    fn assert_names_a_reason(refused: Result<Payload, String>, what: &str) {
        let line = refused.err();
        assert!(
            line.as_deref()
                .is_some_and(|line| line.len() > NOT_BASE64_JSON.len() && line.starts_with(NOT_BASE64_JSON)),
            "{what}: a reason follows the prefix, got {line:?}"
        );
    }

    #[test]
    fn a_word_that_is_not_base64_json_names_why() {
        assert_names_a_reason(Payload::decode("not base64", PathForm::Windows), "not base64");
        assert_names_a_reason(Payload::decode(&STANDARD.encode([0xFF, 0xFE]), PathForm::Windows), "not UTF-8");
        assert_names_a_reason(decoded("{\"Work\":"), "not JSON");
        let unpadded = STANDARD.encode("{}").trim_end_matches('=').to_owned();
        assert_names_a_reason(Payload::decode(&unpadded, PathForm::Windows), "the standard engine wants the padding");
        assert_eq!(
            decoded("[\"Work\"]").err(),
            Some("gate: box payload is not base64 JSON: the JSON is not an object".to_owned())
        );
    }

    fn path(value: &str) -> Result<String, String> {
        decoded(&json!({ "Work": value }).to_string()).and_then(|payload| payload.path("Work").map(str::to_owned))
    }

    #[test]
    fn a_path_is_absolute_on_a_drive_with_no_control_character() {
        for good in [r"C:\w", "c:/w", r"X:\dev\a b\k.git"] {
            assert_eq!(path(good), Ok(good.to_owned()), "{good}");
        }
        for bad in [
            "w",
            r"remote\k",
            r"\w",
            "C:w",
            "C:",
            r"\\server\share\w",
            r"\\?\C:\w",
            "//server/share/w",
            "-bare",
            "--upload-pack=C:/x",
            "C:\\w\nx",
            "C:\\w\u{1}",
            "C:\\w\u{1f}",
            "1:\\w",
        ] {
            assert_eq!(
                path(bad),
                Err(format!("gate: box payload Work is not an absolute path on a drive: {bad:?}")),
                "{bad:?}"
            );
        }
        assert_eq!(path(""), Err("gate: box payload lacks Work".to_owned()), "an empty path is lacking");
    }

    #[test]
    fn a_missing_empty_or_non_text_key_is_lacking_and_other_keys_are_ignored() {
        let json = r#"{"Bare":"C:\\b.git","Work":"C:\\w","Job":"j","Unknown":[1]}"#;
        assert_eq!(text(json, "Bare"), Ok(r"C:\b.git".to_owned()));
        assert_eq!(text(json, "Work"), Ok(r"C:\w".to_owned()));
        assert_eq!(text(r#"{"Bare":"b"}"#, "Work"), Err("gate: box payload lacks Work".to_owned()));
        assert_eq!(text(r#"{"Work":"w"}"#, "Bare"), Err("gate: box payload lacks Bare".to_owned()));
        assert_eq!(text(r#"{"Work":""}"#, "Work"), Err("gate: box payload lacks Work".to_owned()));
        assert_eq!(text(r#"{"Work":3}"#, "Work"), Err("gate: box payload lacks Work".to_owned()));
        assert_eq!(
            text(r#"{"work":"w"}"#, "Work"),
            Err("gate: box payload lacks Work".to_owned()),
            "names are exact"
        );
    }

    /// A good warm-up hash: 64 lower-case hex digits.
    const WARM_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// The commands of the JSON `json` and its warm hash, or the line refusing them.
    fn commands(json: &serde_json::Value) -> Result<(Vec<Vec<String>>, Option<String>), String> {
        decoded(&json.to_string()).and_then(|payload| payload.commands().map(|commands| (commands.words, commands.warm_hash.map(str::to_owned))))
    }

    /// The words of `commands` as owned lists.
    fn owned(commands: &[&[&str]]) -> Vec<Vec<String>> {
        commands
            .iter()
            .map(|words| words.iter().map(|word| (*word).to_owned()).collect())
            .collect()
    }

    #[test]
    fn a_run_payload_runs_its_one_command() {
        assert_eq!(
            commands(&json!({"Command": ["dotnet", "build"], "WarmHash": WARM_HASH})),
            Ok((owned(&[&["dotnet", "build"]]), None)),
            "a WarmHash beside a Command is ignored"
        );
        assert_eq!(commands(&json!({})), Err("gate: box payload lacks Command".to_owned()));
        assert_eq!(
            commands(&json!({"WarmCommands": null})),
            Err("gate: box payload lacks Command".to_owned()),
            "a null WarmCommands is not set"
        );
    }

    #[test]
    fn a_warm_payload_reads_its_commands_and_hash() {
        let json = json!({
            "WarmCommands": [["dotnet", "restore"], ["dotnet", "build", "-c", "Release"]],
            "WarmHash": WARM_HASH,
            "Command": null,
            "Outputs": [],
            "OutputsFull": false,
            "Timeout": 1800,
        });
        assert_eq!(
            commands(&json),
            Ok((
                owned(&[&["dotnet", "restore"], &["dotnet", "build", "-c", "Release"]]),
                Some(WARM_HASH.to_owned())
            ))
        );
        let eight: Vec<Vec<&str>> = (0..8).map(|_| vec!["x"]).collect();
        assert!(
            commands(&json!({"WarmCommands": eight, "WarmHash": WARM_HASH, "Timeout": 1})).is_ok(),
            "8 commands are allowed"
        );
    }

    #[test]
    fn a_warm_payload_without_a_timeout_is_refused() {
        for json in [
            json!({"WarmCommands": [["x"]], "WarmHash": WARM_HASH}),
            json!({"WarmCommands": [["x"]], "WarmHash": WARM_HASH, "Timeout": null}),
        ] {
            assert_eq!(commands(&json), Err("gate: box payload lacks Timeout".to_owned()), "{json}");
        }
        assert_eq!(
            commands(&json!({"WarmCommands": [["x"]], "WarmHash": WARM_HASH, "Timeout": 0})),
            Err("gate: box payload Timeout is not a whole number of seconds".to_owned()),
            "a bad Timeout keeps its own line"
        );
    }

    #[test]
    fn a_warm_payload_that_also_sets_command_is_refused() {
        for command in [json!(["x"]), json!([]), json!("x")] {
            assert_eq!(
                commands(&json!({"Command": command, "WarmCommands": [["x"]], "WarmHash": WARM_HASH})),
                Err("gate: box payload sets both Command and WarmCommands".to_owned()),
                "{command}"
            );
        }
    }

    #[test]
    fn a_warm_payload_without_a_warm_hash_is_refused() {
        for json in [json!({"WarmCommands": [["x"]]}), json!({"WarmCommands": [["x"]], "WarmHash": null})] {
            assert_eq!(commands(&json), Err("gate: box payload lacks WarmHash".to_owned()), "{json}");
        }
    }

    #[test]
    fn a_warm_payload_that_asks_for_outputs_is_refused() {
        for json in [
            json!({"WarmCommands": [["x"]], "WarmHash": WARM_HASH, "Outputs": ["bin/**"]}),
            json!({"WarmCommands": [["x"]], "WarmHash": WARM_HASH, "OutputsFull": true}),
        ] {
            assert_eq!(commands(&json), Err("gate: box payload asks a warm-up for outputs".to_owned()), "{json}");
        }
    }

    #[test]
    fn a_warm_payload_that_asks_for_env_is_refused() {
        let warmed = |env: serde_json::Value| json!({"WarmCommands": [["x"]], "WarmHash": WARM_HASH, "Timeout": 1, "Env": env});
        for env in [json!({"NuGetAudit": "false"}), json!({})] {
            assert_eq!(commands(&warmed(env)), Err("gate: box payload asks a warm-up for Env".to_owned()));
        }
        assert!(commands(&warmed(json!(null))).is_ok(), "a null Env is not set");
    }

    #[test]
    fn warm_commands_must_be_1_to_8_non_empty_lists_of_non_empty_texts() {
        let nine: Vec<Vec<&str>> = (0..9).map(|_| vec!["x"]).collect();
        for bad in [
            json!([]),
            json!(nine),
            json!([[]]),
            json!([["x"], []]),
            json!([[""]]),
            json!([["x", 3]]),
            json!(["x"]),
            json!("x"),
            json!({"a": ["x"]}),
        ] {
            assert_eq!(
                commands(&json!({"WarmCommands": bad, "WarmHash": WARM_HASH})),
                Err("gate: box payload WarmCommands is not a list of 1 to 8 commands, each a non-empty list of non-empty texts".to_owned()),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_warm_hash_must_be_64_lower_case_hex_digits() {
        let upper = WARM_HASH.to_uppercase();
        let short = &WARM_HASH[..63];
        let newline = format!("{WARM_HASH}\n");
        for (bad, shown) in [
            (json!(upper), upper.clone()),
            (json!(short), short.to_owned()),
            (json!(newline), format!("{WARM_HASH}\\n")),
            (json!(""), String::new()),
            (json!(7), "7".to_owned()),
        ] {
            assert_eq!(
                commands(&json!({"WarmCommands": [["x"]], "WarmHash": bad})),
                Err(format!("gate: box payload WarmHash is not 64 lower-case hex digits: {shown}")),
                "{bad}"
            );
        }
    }

    /// The run's snapshot of the JSON `json`: its text, or the line refusing it.
    fn snapshot(json: &str) -> Result<String, String> {
        decoded(json).and_then(|payload| payload.snapshot().map(str::to_owned))
    }

    /// The refusal line a snapshot value is shown in.
    fn refusing(value: &str) -> String {
        format!("gate: remote run payload's Snapshot '{value}' is not a 40-hex commit sha")
    }

    #[test]
    fn a_snapshot_is_exactly_40_lowercase_hex_digits() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            snapshot(&format!(r#"{{"Snapshot":"{sha}"}}"#)),
            Ok(sha.to_owned()),
            "40 lowercase hex digits"
        );
        assert_eq!(
            snapshot(&format!(r#"{{"Snapshot":"{sha}\n"}}"#)),
            Err(refusing(&format!("{sha}\\n"))),
            "a line end after the sha is not allowed"
        );
        assert_eq!(
            snapshot(&format!(r#"{{"Snapshot":"{sha}0"}}"#)),
            Err(refusing(&format!("{sha}0"))),
            "41 of them"
        );
        let dashes = "-".repeat(40);
        assert_eq!(snapshot(&format!(r#"{{"Snapshot":"{dashes}"}}"#)), Err(refusing(&dashes)), "40 non-hex");
        let gees = "g".repeat(40);
        assert_eq!(snapshot(&format!(r#"{{"Snapshot":"{gees}"}}"#)), Err(refusing(&gees)), "a non-hex letter");
        assert_eq!(snapshot(r#"{"Snapshot":""}"#), Err(refusing("")), "the empty string");
        assert_eq!(snapshot(r#"{"Snapshot":null}"#), Err(refusing("")), "null shows nothing");
    }

    #[test]
    fn a_snapshot_that_is_not_a_string_is_shown_as_its_compact_json() {
        assert_eq!(snapshot(r#"{"Snapshot":3}"#), Err(refusing("3")), "a number");
        assert_eq!(snapshot(r#"{"Snapshot":true}"#), Err(refusing("true")), "a boolean");
        assert_eq!(snapshot(r#"{"Snapshot":["x"]}"#), Err(refusing(r#"["x"]"#)), "an array");
        assert_eq!(snapshot(r#"{"Snapshot":{"a":1}}"#), Err(refusing(r#"{"a":1}"#)), "an object");
    }

    #[test]
    fn a_snapshot_line_escapes_every_control_character() {
        // ESC, CR, LF, DEL and U+009B, in a string value and in a non-string one.
        let controls = "\u{1b}\r\n\u{7f}\u{9b}";
        assert_eq!(
            snapshot(&json!({ "Snapshot": controls }).to_string()),
            Err(refusing(r"\u{1b}\r\n\u{7f}\u{9b}")),
            "a string value"
        );
        assert_eq!(
            snapshot(&json!({ "Snapshot": ["\u{1b}[31m"] }).to_string()),
            Err(refusing(r#"["\u001b[31m"]"#)),
            "a non-string value is shown as its compact JSON text"
        );
    }

    fn output_globs(json: &serde_json::Value) -> Result<Vec<String>, String> {
        decoded(&json.to_string()).and_then(|payload| payload.output_globs())
    }

    #[test]
    fn output_globs_are_a_list_of_texts_none_when_absent() {
        assert_eq!(output_globs(&json!({})), Ok(vec![]), "absent");
        assert_eq!(output_globs(&json!({ "Outputs": null })), Ok(vec![]), "null");
        assert_eq!(output_globs(&json!({ "Outputs": [] })), Ok(vec![]), "an empty list");
        assert_eq!(
            output_globs(&json!({ "Outputs": ["**/bin/**", "*.dll"] })),
            Ok(vec!["**/bin/**".to_owned(), "*.dll".to_owned()])
        );
        let not_a_list = Err("gate: box payload Outputs is not a list of texts".to_owned());
        assert_eq!(output_globs(&json!({ "Outputs": "**/bin/**" })), not_a_list, "a text");
        assert_eq!(output_globs(&json!({ "Outputs": ["a", 1] })), not_a_list, "a number in the list");
        assert_eq!(
            output_globs(&json!({ "Outputs": ["a", ""] })),
            Err("gate: box payload Outputs holds an empty glob".to_owned())
        );
        assert_eq!(
            output_globs(&json!({ "Outputs": ["bin/[ab].dll"] })),
            Err("gate: box payload Outputs glob 'bin/[ab].dll' holds '[' or ']'".to_owned())
        );
        assert_eq!(
            output_globs(&json!({ "Outputs": ["\u{1b}]"] })),
            Err(r"gate: box payload Outputs glob '\u{1b}]' holds '[' or ']'".to_owned()),
            "a glob shown with its control characters escaped"
        );
    }

    #[test]
    fn outputs_full_is_true_or_false_and_false_when_absent() {
        let full = |json: serde_json::Value| decoded(&json.to_string()).and_then(|payload| payload.outputs_full());
        assert_eq!(full(json!({})), Ok(false), "absent");
        assert_eq!(full(json!({ "OutputsFull": null })), Ok(false), "null");
        assert_eq!(full(json!({ "OutputsFull": true })), Ok(true));
        assert_eq!(full(json!({ "OutputsFull": false })), Ok(false));
        for bad in [json!("true"), json!(1), json!([true])] {
            assert_eq!(
                full(json!({ "OutputsFull": bad })),
                Err("gate: box payload OutputsFull is not true or false".to_owned()),
                "{bad}"
            );
        }
    }

    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn desktop_is_true_or_false_and_false_when_absent() {
        let desktop = |json: serde_json::Value| decoded(&json.to_string()).and_then(|payload| payload.desktop());
        assert_eq!(desktop(json!({})), Ok(false), "absent");
        assert_eq!(desktop(json!({ "Desktop": null })), Ok(false), "null");
        assert_eq!(desktop(json!({ "Desktop": true })), Ok(true));
        assert_eq!(desktop(json!({ "Desktop": false })), Ok(false));
        for bad in [json!("true"), json!(1), json!([true])] {
            assert_eq!(
                desktop(json!({ "Desktop": bad })),
                Err("gate: box payload Desktop is not true or false".to_owned()),
                "{bad}"
            );
        }
    }

    #[test]
    fn desktop_wait_is_whole_seconds_like_timeout() {
        let wait = |json: serde_json::Value| decoded(&json.to_string()).and_then(|payload| payload.desktop_wait());
        assert_eq!(wait(json!({})), Ok(None), "absent");
        assert_eq!(wait(json!({ "DesktopWait": null })), Ok(None), "null");
        assert_eq!(wait(json!({ "DesktopWait": 1 })), Ok(Some(1)), "a second");
        assert_eq!(wait(json!({ "DesktopWait": 2_147_483_647 })), Ok(Some(2_147_483_647)), "the ceiling");
        for bad in [json!(0), json!("30"), json!(1.5), json!([30]), json!(-1), json!(2_147_483_648u64)] {
            assert_eq!(
                wait(json!({ "DesktopWait": bad })),
                Err("gate: box payload DesktopWait is not a whole number of seconds".to_owned()),
                "{bad}"
            );
        }
    }

    #[test]
    fn an_outputs_nonce_is_32_lower_case_hex_digits() {
        let nonce = |json: serde_json::Value| decoded(&json.to_string()).and_then(|payload| payload.nonce().map(str::to_owned));
        assert_eq!(nonce(json!({ "Nonce": NONCE })), Ok(NONCE.to_owned()));
        assert_eq!(nonce(json!({})), Err("gate: box payload lacks Nonce".to_owned()));
        for bad in [
            NONCE.to_uppercase(),
            NONCE[1..].to_owned(),
            format!("{NONCE}0"),
            format!("{NONCE}\n"),
            "n1".to_owned(),
        ] {
            assert_eq!(
                nonce(json!({ "Nonce": bad })),
                Err(format!("gate: box payload Nonce is not 32 lower-case hex digits: {}", escaped(&bad))),
                "{bad:?}"
            );
        }
    }

    const WORK: &str = r"D:\gate-remote\work\app-1a2b3c4d";

    fn output_folder(work: &str, folder: &str) -> Result<String, String> {
        decoded(&json!({ "OutputFolder": folder }).to_string()).and_then(|payload| payload.output_folder(work).map(str::to_owned))
    }

    // Tests drive paths through std::path::absolute, which reads them as drive paths only on Windows.
    #[cfg(windows)]
    #[test]
    fn the_output_folder_is_outputs_beside_the_work_folders_parent() {
        for good in [r"D:\gate-remote\outputs", "d:/GATE-REMOTE/outputs", r"D:\gate-remote\work\..\outputs"] {
            assert_eq!(output_folder(WORK, good), Ok(good.to_owned()), "{good}");
        }
        assert_eq!(
            output_folder(r"C:\work\k", r"C:\outputs"),
            Ok(r"C:\outputs".to_owned()),
            "a root on the drive"
        );
        let refused = Err(r"gate: box payload OutputFolder must be D:\gate-remote\outputs".to_owned());
        for bad in [
            r"D:\gate-remote\work\outputs",
            r"D:\other\outputs",
            r"D:\gate-remote\outputs\x",
            r"D:\gate-remote",
        ] {
            assert_eq!(output_folder(WORK, bad), refused, "{bad}");
        }
        assert_eq!(
            output_folder(WORK, "outputs"),
            Err("gate: box payload OutputFolder is not an absolute path on a drive: \"outputs\"".to_owned())
        );
        assert_eq!(output_folder(WORK, ""), Err("gate: box payload lacks OutputFolder".to_owned()));
        assert_eq!(
            output_folder(r"D:\app-1a2b3c4d", r"D:\outputs"),
            Err(r"gate: box payload Work must be <root>\work\<key> to place OutputFolder: D:\app-1a2b3c4d".to_owned())
        );
    }

    fn archive_name(name: &str) -> Result<String, String> {
        decoded(&json!({ "Key": "app-1a2b3c4d", "Name": name }).to_string()).and_then(|payload| payload.archive_name().map(str::to_owned))
    }

    #[test]
    fn the_work_root_of_a_drive_s_own_root_keeps_its_separator() {
        assert_eq!(work_root(r"D:\work\app-1a2b3c4d", PathForm::Windows), Some(r"D:\".to_owned()));
        assert_eq!(
            work_root(r"D:\\work\app-1a2b3c4d", PathForm::Windows),
            Some(r"D:\".to_owned()),
            "a doubled separator"
        );
        assert_eq!(work_root(r"\\work\app-1a2b3c4d", PathForm::Windows), None, "a root of separators alone");
        assert_eq!(work_root(r"D:\gate-remote\app-1a2b3c4d", PathForm::Windows), None, "no work folder");
    }

    #[test]
    fn the_work_root_has_no_trailing_separator() {
        assert_eq!(
            work_root(r"D:\gate-remote\work\app-1a2b3c4d", PathForm::Windows),
            Some(r"D:\gate-remote".to_owned())
        );
        assert_eq!(
            work_root(r"D:\gate-remote\\work\app-1a2b3c4d", PathForm::Windows),
            Some(r"D:\gate-remote".to_owned()),
            "a doubled separator"
        );
        assert_eq!(
            work_root("D:/gate-remote//work/app-1a2b3c4d", PathForm::Windows),
            Some("D:/gate-remote".to_owned()),
            "forward ones"
        );
    }

    #[test]
    fn grandparent_needs_a_work_folder() {
        let outputs = r"D:\gate-remote\outputs";
        assert_eq!(
            output_folder(r"D:\gate-remote\WORK\app-1a2b3c4d", outputs),
            Ok(outputs.to_owned()),
            "work in any case"
        );
        for work in [
            r"D:\gate-remote\other\app-1a2b3c4d",
            r"D:\gate-remote\works\app-1a2b3c4d",
            r"D:\gate-remote\work.\app-1a2b3c4d",
        ] {
            assert_eq!(
                output_folder(work, outputs),
                Err(format!(r"gate: box payload Work must be <root>\work\<key> to place OutputFolder: {work}")),
                "{work}"
            );
        }
    }

    #[test]
    fn a_path_with_a_part_ending_in_a_dot_or_space_is_not_the_one_expected() {
        let refused = Err(r"gate: box payload OutputFolder must be D:\gate-remote\outputs".to_owned());
        for bad in [
            r"D:\gate-remote\outputs.",
            r"D:\gate-remote\outputs ",
            r"D:\gate-remote.\outputs",
            r"D:\gate-remote \outputs",
            "D:/gate-remote/outputs..",
        ] {
            assert_eq!(output_folder(WORK, bad), refused, "{bad:?}");
        }
    }

    #[test]
    fn an_archive_name_needs_a_work_folder_key() {
        let name = format!("app-1a2b3c4d-{NONCE}.tar.zst");
        let named = |json: serde_json::Value| decoded(&json.to_string()).and_then(|payload| payload.archive_name().map(str::to_owned));
        assert_eq!(named(json!({ "Name": name })), Err("gate: box payload lacks Key".to_owned()));
        assert_eq!(
            named(json!({ "Key": "app 1", "Name": name })),
            Err("gate: box payload Key is not a work-folder key: app 1".to_owned())
        );
    }

    #[test]
    fn an_archive_name_is_the_work_folders_key_and_an_outputs_nonce() {
        let good = format!("app-1a2b3c4d-{NONCE}.tar.zst");
        assert_eq!(archive_name(&good), Ok(good.clone()));
        for bad in [
            format!("app-1a2b3c4d-{NONCE}.tar.xz"),
            format!("other-{NONCE}.tar.zst"),
            format!("APP-1A2B3C4D-{NONCE}.tar.zst"),
            format!("app-1a2b3c4d-{}.tar.zst", NONCE.to_uppercase()),
            format!("app-1a2b3c4d-{}.tar.zst", &NONCE[1..]),
            format!(r"..\app-1a2b3c4d-{NONCE}.tar.zst"),
            format!("app-1a2b3c4d-{NONCE}.tar.zst\n"),
            format!("app-1a2b3c4d{NONCE}.tar.zst"),
        ] {
            assert_eq!(
                archive_name(&bad),
                Err(format!(
                    "gate: remote outputs archive name '{}' is not an outputs archive of this work folder",
                    escaped(&bad)
                )),
                "{bad:?}"
            );
        }
        assert_eq!(archive_name(""), Err("gate: box payload lacks Name".to_owned()));
    }

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    /// A run payload of the work folder `app-1a2b3c4d` under `D:\gate-remote`, its bare repository `app.git`, with
    /// `pair` as its `Pair`.
    fn with_pair(pair: serde_json::Value) -> serde_json::Value {
        let mut payload = json!({
            "Key": "app-1a2b3c4d",
            "Work": WORK,
            "Bare": r"D:\gate-remote\app.git",
        });
        payload["Pair"] = pair;
        payload
    }

    /// A pair's leaf, and each sibling's name and bare repository.
    type LaidOut = (String, Vec<(String, String)>);

    /// The leaf and each sibling's name and bare repository the payload `json` lays out, `None` for no pair; else the
    /// line refusing it.
    fn laid_out(json: &serde_json::Value) -> Result<Option<LaidOut>, String> {
        let pair = decoded(&json.to_string())?.pair()?;
        Ok(pair.map(|pair| {
            let siblings = pair
                .siblings()
                .iter()
                .map(|sibling| (sibling.name().to_owned(), sibling.bare().to_owned()))
                .collect();
            (pair.leaf().to_owned(), siblings)
        }))
    }

    /// The refusal line of the payload `json`'s pair.
    fn refused_pair(json: &serde_json::Value) -> String {
        laid_out(json).err().unwrap_or_else(|| format!("not refused: {json}"))
    }

    #[test]
    fn a_pair_absent_or_null_is_a_run_of_one_repo() {
        let mut payload = with_pair(json!(null));
        assert_eq!(laid_out(&payload), Ok(None), "null");
        if let Some(object) = payload.as_object_mut() {
            object.remove("Pair");
        }
        assert_eq!(laid_out(&payload), Ok(None), "absent");
    }

    #[test]
    fn a_pair_puts_each_siblings_bare_repository_beside_the_main_ones() {
        let payload = with_pair(json!({"Leaf": "app", "Siblings": ["yaat-server", "a.b", "Lib_2"]}));
        let siblings = [
            ("yaat-server", r"D:\gate-remote\yaat-server.git"),
            ("a.b", r"D:\gate-remote\a-b.git"),
            ("Lib_2", r"D:\gate-remote\Lib_2.git"),
        ]
        .map(|(name, bare)| (name.to_owned(), bare.to_owned()));
        assert_eq!(laid_out(&payload), Ok(Some(("app".to_owned(), siblings.to_vec()))));
    }

    #[test]
    fn a_pair_that_is_not_an_object_of_a_leaf_and_a_list_of_siblings_is_refused() {
        let refused = "gate: box payload Pair is not an object of a Leaf text and a Siblings list of texts";
        for bad in [
            json!("app"),
            json!(["app"]),
            json!({"Siblings": ["lib"]}),
            json!({"Leaf": 1, "Siblings": ["lib"]}),
            json!({"Leaf": "app"}),
            json!({"Leaf": "app", "Siblings": "lib"}),
            json!({"Leaf": "app", "Siblings": ["lib", 1]}),
        ] {
            assert_eq!(refused_pair(&with_pair(bad.clone())), refused, "{bad}");
        }
    }

    #[test]
    fn a_pair_refuses_a_name_the_box_cannot_lay_out() {
        for bad in ["..", r"a\b", "nul.txt", ".x", "x.", ""] {
            let line = format!(
                "gate: box payload Pair name '{}' is not a folder name of letters, digits, '.', '_' and '-' not starting or \
                 ending with a dot, not a device name",
                escaped(bad)
            );
            assert_eq!(
                refused_pair(&with_pair(json!({"Leaf": "app", "Siblings": ["lib", bad]}))),
                line,
                "sibling {bad:?}"
            );
            assert_eq!(refused_pair(&with_pair(json!({"Leaf": bad, "Siblings": ["lib"]}))), line, "leaf {bad:?}");
        }
    }

    #[test]
    fn a_pair_names_one_to_eight_siblings() {
        let names = |count: usize| (1..=count).map(|number| format!("s{number}")).collect::<Vec<_>>();
        assert_eq!(
            refused_pair(&with_pair(json!({"Leaf": "app", "Siblings": []}))),
            "gate: box payload Pair names no sibling"
        );
        assert!(
            laid_out(&with_pair(json!({"Leaf": "app", "Siblings": names(8)}))).is_ok_and(|pair| pair.is_some()),
            "eight"
        );
        assert_eq!(
            refused_pair(&with_pair(json!({"Leaf": "app", "Siblings": names(9)}))),
            "gate: box payload Pair names more than 8 siblings"
        );
    }

    #[test]
    fn a_pair_refuses_names_equal_ignoring_case() {
        assert_eq!(
            refused_pair(&with_pair(json!({"Leaf": "main", "Siblings": ["App", "app"]}))),
            "gate: box payload Pair names app and App, equal ignoring case"
        );
        assert_eq!(
            refused_pair(&with_pair(json!({"Leaf": "app", "Siblings": ["lib", "APP"]}))),
            "gate: box payload Pair names APP and app, equal ignoring case",
            "a sibling named as the leaf"
        );
    }

    #[test]
    fn a_pair_refuses_siblings_sharing_a_bare_repository() {
        assert_eq!(
            refused_pair(&with_pair(json!({"Leaf": "app", "Siblings": ["a.b", "a-b"]}))),
            "gate: box payload Pair sibling a-b would share the bare repository a-b.git"
        );
        let mut worktree = with_pair(json!({"Leaf": "app-wt", "Siblings": ["App"]}));
        worktree["Key"] = json!("app-wt-1a2b3c4d");
        worktree["Work"] = json!(r"D:\gate-remote\work\app-wt-1a2b3c4d");
        assert_eq!(
            refused_pair(&worktree),
            "gate: box payload Pair sibling App would share the bare repository App.git",
            "a sibling named as the main checkout, whose bare is app.git"
        );
    }

    #[test]
    fn a_pair_needs_the_key_of_its_leaf() {
        assert!(laid_out(&with_pair(json!({"Leaf": "app", "Siblings": ["lib"]}))).is_ok_and(|pair| pair.is_some()));
        let refusing = |key: &str, leaf: &str| format!("gate: box payload Key {key} is not the work-folder key of the Pair's Leaf {leaf}");
        assert_eq!(
            refused_pair(&with_pair(json!({"Leaf": "other", "Siblings": ["lib"]}))),
            refusing("app-1a2b3c4d", "other")
        );
        assert_eq!(
            refused_pair(&with_pair(json!({"Leaf": "App", "Siblings": ["lib"]}))),
            refusing("app-1a2b3c4d", "App"),
            "the key keeps the leaf's case"
        );
        for key in ["app-1A2B3C4D", "app-1a2b3c4", "app-1a2b3c4d0", "app1a2b3c4d"] {
            let mut payload = with_pair(json!({"Leaf": "app", "Siblings": ["lib"]}));
            payload["Key"] = json!(key);
            payload["Work"] = json!(format!(r"D:\gate-remote\work\{key}"));
            assert_eq!(refused_pair(&payload), refusing(key, "app"), "{key}");
        }
    }

    // Tests drive paths through std::path::absolute, which reads them as drive paths only on Windows.
    #[cfg(windows)]
    #[test]
    fn a_pair_needs_a_bare_repository_beside_the_work_folders() {
        assert!(
            laid_out(&json!({"Key": "app-1a2b3c4d", "Work": WORK, "Bare": "d:/GATE-REMOTE/App.GIT", "Pair": {"Leaf": "app", "Siblings": ["lib"]}}))
                .is_ok_and(|pair| pair.is_some()),
            "compared without regard to case"
        );
        for bad in [
            r"D:\other\app.git",
            r"D:\gate-remote\app",
            r"D:\gate-remote\a b.git",
            r"D:\gate-remote\work\app.git",
            r"D:\gate-remote\.git",
            r"D:\gate-remote\a.b.git",
        ] {
            let mut payload = with_pair(json!({"Leaf": "app", "Siblings": ["lib"]}));
            payload["Bare"] = json!(bad);
            assert_eq!(
                refused_pair(&payload),
                format!(r"gate: box payload Bare must be D:\gate-remote\<name>.git with a Pair: {bad}"),
                "{bad}"
            );
        }
        let mut outside = with_pair(json!({"Leaf": "app", "Siblings": ["lib"]}));
        outside["Work"] = json!(r"D:\gate-remote\app-1a2b3c4d");
        assert_eq!(
            refused_pair(&outside),
            r"gate: box payload Work must be <root>\work\<key> with a Pair: D:\gate-remote\app-1a2b3c4d"
        );
    }

    /// The sibling commits of `snapshots` for a pair of `lib` and `tools`; else the line refusing them.
    fn sibling_snapshots(snapshots: &serde_json::Value) -> Result<Vec<String>, String> {
        let mut json = with_pair(json!({"Leaf": "app", "Siblings": ["lib", "tools"]}));
        json["SiblingSnapshots"] = snapshots.clone();
        let payload = decoded(&json.to_string())?;
        let pair = payload.pair()?.ok_or("a pair is given")?;
        payload
            .sibling_snapshots(&pair)
            .map(|commits| commits.into_iter().map(str::to_owned).collect())
    }

    #[test]
    fn sibling_snapshots_name_exactly_the_pairs_siblings() {
        let other = "fedcba9876543210fedcba9876543210fedcba98";
        assert_eq!(
            sibling_snapshots(&json!({"tools": other, "lib": SHA})),
            Ok(vec![SHA.to_owned(), other.to_owned()]),
            "in the pair's order"
        );
        let refused = Err("gate: box payload SiblingSnapshots must name exactly the Pair's siblings: lib, tools".to_owned());
        for bad in [
            json!(null),
            json!({}),
            json!({"lib": SHA}),
            json!({"lib": SHA, "tools": SHA, "app": SHA}),
            json!({"lib": SHA, "Tools": SHA}),
            json!({"lib": SHA, "other": SHA}),
        ] {
            assert_eq!(sibling_snapshots(&bad), refused, "{bad}");
        }
        assert_eq!(
            sibling_snapshots(&json!([SHA, SHA])),
            Err("gate: box payload SiblingSnapshots is not an object".to_owned())
        );
    }

    #[test]
    fn a_sibling_snapshot_is_exactly_40_lowercase_hex_digits() {
        let refusing = |value: &str| {
            Err(format!(
                "gate: remote run payload's SiblingSnapshots tools '{value}' is not a 40-hex commit sha"
            ))
        };
        let upper = SHA.to_uppercase();
        assert_eq!(sibling_snapshots(&json!({"lib": SHA, "tools": &upper})), refusing(&upper));
        assert_eq!(sibling_snapshots(&json!({"lib": SHA, "tools": &SHA[1..]})), refusing(&SHA[1..]));
        assert_eq!(sibling_snapshots(&json!({"lib": SHA, "tools": null})), refusing(""));
        assert_eq!(sibling_snapshots(&json!({"lib": SHA, "tools": 3})), refusing("3"));
        assert_eq!(sibling_snapshots(&json!({"lib": SHA, "tools": "\u{1b}"})), refusing(r"\u{1b}"));
    }

    #[test]
    fn the_trees_of_a_run_without_a_pair_are_its_work_folder() {
        assert_eq!(
            super::trees(r"D:\gate-remote\work\app-1a2b3c4d", None, PathForm::Windows),
            (r"D:\gate-remote\work\app-1a2b3c4d".to_owned(), Vec::<String>::new())
        );
    }

    #[test]
    fn the_trees_of_a_pair_put_the_main_and_each_sibling_in_the_work_folder() {
        assert_eq!(
            super::trees(r"D:\gate-remote\work\app-1a2b3c4d", Some(("app", &["tools", "lib"])), PathForm::Windows),
            (
                r"D:\gate-remote\work\app-1a2b3c4d\app".to_owned(),
                vec![
                    r"D:\gate-remote\work\app-1a2b3c4d\tools".to_owned(),
                    r"D:\gate-remote\work\app-1a2b3c4d\lib".to_owned(),
                ]
            )
        );
    }

    /// The payload `json` read under the Unix form.
    fn decoded_unix(json: &serde_json::Value) -> Result<Payload, String> {
        Payload::decode(&STANDARD.encode(json.to_string()), PathForm::Unix)
    }

    /// A work folder on a Unix box.
    const UNIX_WORK: &str = "/Users/b/gate-remote/work/repo-0123abcd";

    fn unix_path(value: &str) -> Result<String, String> {
        decoded_unix(&json!({ "Work": value })).and_then(|payload| payload.path("Work").map(str::to_owned))
    }

    #[test]
    fn a_unix_path_is_absolute_with_no_backslash_control_character_or_dot_name() {
        for good in [UNIX_WORK, "/w", "/Users/b/a b/k.git"] {
            assert_eq!(unix_path(good), Ok(good.to_owned()), "{good}");
        }
        assert_eq!(
            unix_path("/Users/b/"),
            Ok("/Users/b/".to_owned()),
            "a trailing separator is the work folder's to refuse"
        );
        for bad in [
            "w",
            "Users/b/w",
            "./w",
            "-bare",
            r"C:\w",
            "c:/w",
            r"/Users/b\w",
            "/Users/../w",
            "/Users/b/..",
            "/Users/./w",
            "/Users//b",
            "//Users/b",
            "/Users/b//",
            "/",
            "//",
            "/Users/b\nx",
            "/Users/b\u{1}",
            "/Users/b\u{1f}",
        ] {
            assert_eq!(
                unix_path(bad),
                Err(format!("gate: box payload Work is not an absolute path: {bad:?}")),
                "{bad:?}"
            );
        }
        assert_eq!(unix_path(""), Err("gate: box payload lacks Work".to_owned()), "an empty path is lacking");
    }

    #[test]
    fn the_windows_form_refuses_a_slash_rooted_path() {
        for rooted in [UNIX_WORK, "/w"] {
            assert_eq!(
                path(rooted),
                Err(format!("gate: box payload Work is not an absolute path on a drive: {rooted:?}")),
                "{rooted}"
            );
        }
    }

    /// The key and work folder of a Unix payload of `key` and `work`; else the line refusing them.
    fn unix_work_folder(key: &str, work: &str) -> Result<(String, String), String> {
        decoded_unix(&json!({ "Key": key, "Work": work }))
            .and_then(|payload| payload.work_folder().map(|(key, work)| (key.to_owned(), work.to_owned())))
    }

    #[test]
    fn a_unix_work_folder_ends_in_its_key_exactly() {
        assert_eq!(
            unix_work_folder("repo-0123abcd", UNIX_WORK),
            Ok(("repo-0123abcd".to_owned(), UNIX_WORK.to_owned()))
        );
        let trailing = format!("{UNIX_WORK}/");
        assert_eq!(
            unix_work_folder("repo-0123abcd", &trailing),
            Err(format!("gate: box payload Work must not end with a separator: {trailing}"))
        );
        for key in ["REPO-0123ABCD", "Repo-0123abcd"] {
            assert_eq!(
                unix_work_folder(key, UNIX_WORK),
                Err(format!("gate: box payload Key is not a work-folder key: {key}")),
                "{key}"
            );
        }
        assert_eq!(
            unix_work_folder("repo-0123abcd", "/Users/b/gate-remote/work/REPO-0123ABCD"),
            Err("gate: box payload Key is not a work-folder key: repo-0123abcd".to_owned())
        );
    }

    #[test]
    fn a_unix_lock_is_the_work_folder_and_lock_byte_for_byte() {
        let lock = |value: &str| {
            decoded_unix(&json!({ "Lock": value })).and_then(|payload| payload.beside_work("Lock", UNIX_WORK, "lock").map(str::to_owned))
        };
        let good = format!("{UNIX_WORK}.lock");
        assert_eq!(lock(&good), Ok(good.clone()));
        let refused = Err(format!("gate: box payload Lock must be {UNIX_WORK}.lock"));
        for bad in [
            "/Users/b/gate-remote/work/REPO-0123abcd.lock",
            "/users/b/gate-remote/work/repo-0123abcd.lock",
            "/Users/b/gate-remote/work/repo-0123abcd.LOCK",
            "/Users/b/gate-remote/work/other.lock",
            "/Users/b/gate-remote/work/repo-0123abcd.index",
        ] {
            assert_eq!(lock(bad), refused, "{bad}");
        }
        let dotted = "/Users/b/gate-remote/work/../work/repo-0123abcd.lock";
        assert_eq!(
            lock(dotted),
            Err(format!("gate: box payload Lock is not an absolute path: {dotted:?}")),
            "a '..' name never reaches the compare"
        );
    }

    #[test]
    fn a_unix_output_folder_is_outputs_beside_the_work_folders_parent() {
        let folder = |work: &str, value: &str| {
            decoded_unix(&json!({ "OutputFolder": value })).and_then(|payload| payload.output_folder(work).map(str::to_owned))
        };
        assert_eq!(
            folder(UNIX_WORK, "/Users/b/gate-remote/outputs"),
            Ok("/Users/b/gate-remote/outputs".to_owned())
        );
        let refused = Err("gate: box payload OutputFolder must be /Users/b/gate-remote/outputs".to_owned());
        for bad in [
            "/Users/b/gate-remote/OUTPUTS",
            "/Users/b/gate-remote/outputs/",
            "/Users/b/gate-remote/work/outputs",
            "/Users/b/other/outputs",
        ] {
            assert_eq!(folder(UNIX_WORK, bad), refused, "{bad}");
        }
        assert_eq!(
            folder("/Users/b/repo-0123abcd", "/Users/outputs"),
            Err("gate: box payload Work must be <root>/work/<key> to place OutputFolder: /Users/b/repo-0123abcd".to_owned())
        );
    }

    #[test]
    fn a_unix_work_root_is_the_folder_above_work_and_none_for_the_root() {
        assert_eq!(work_root("/x/work/k", PathForm::Unix), Some("/x".to_owned()));
        assert_eq!(work_root(UNIX_WORK, PathForm::Unix), Some("/Users/b/gate-remote".to_owned()));
        assert_eq!(work_root("/work/k", PathForm::Unix), None, "no root above /work");
        assert_eq!(work_root("/x/other/k", PathForm::Unix), None, "no work folder");
        assert_eq!(
            work_root("/x:/work/k", PathForm::Unix),
            Some("/x:".to_owned()),
            "a name ending in ':' is no drive"
        );
    }

    #[test]
    fn a_unix_job_name_has_no_local_prefix() {
        let key = "repo-0123abcd";
        let job = |value: &str| decoded_unix(&json!({ "Job": value })).and_then(|payload| payload.job(key).map(str::to_owned));
        let good = format!("gate-remote-{key}-0123456789ab");
        assert_eq!(job(&good), Ok(good.clone()));
        let windows = format!(r"Local\gate-remote-{key}-0123456789ab");
        assert_eq!(
            job(&windows),
            Err(format!("gate: box payload Job is not this work folder's job name: {windows}"))
        );
        assert!(!super::is_job_name(&windows, key, PathForm::Unix));
        assert!(super::is_job_name(&windows, key, PathForm::Windows), "the Windows form keeps its prefix");
        assert!(
            super::is_recorded_job_name("gate-remote-REPO-0123ABCD-0123456789ab", key, PathForm::Unix),
            "a record written under another case of the key"
        );
        assert!(!super::is_recorded_job_name(&windows, key, PathForm::Unix));
    }

    #[test]
    fn the_trees_of_a_unix_pair_are_joined_with_a_slash() {
        assert_eq!(
            super::trees(UNIX_WORK, Some(("app", &["tools", "lib"])), PathForm::Unix),
            (format!("{UNIX_WORK}/app"), vec![format!("{UNIX_WORK}/tools"), format!("{UNIX_WORK}/lib")])
        );
        assert_eq!(
            super::trees(UNIX_WORK, None, PathForm::Unix),
            (UNIX_WORK.to_owned(), Vec::<String>::new())
        );
    }

    /// Each sibling's bare repository a Unix pair payload with the bare repository `bare` and the work folder `work`
    /// lays out; else the line refusing it.
    fn unix_pair_bares(bare: &str, work: &str) -> Result<Option<Vec<String>>, String> {
        let payload = decoded_unix(&json!({
            "Key": "app-1a2b3c4d",
            "Work": work,
            "Bare": bare,
            "Pair": {"Leaf": "app", "Siblings": ["lib"]},
        }))?;
        let pair = payload.pair()?;
        Ok(pair.map(|pair| pair.siblings().iter().map(|sibling| sibling.bare().to_owned()).collect()))
    }

    #[test]
    fn a_unix_pair_puts_each_bare_repository_beside_the_main_one_byte_for_byte() {
        let work = "/g/work/app-1a2b3c4d";
        assert_eq!(unix_pair_bares("/g/app.git", work), Ok(Some(vec!["/g/lib.git".to_owned()])));
        for bad in ["/G/app.git", "/g/work/app.git", "/other/app.git", "/g/app"] {
            assert_eq!(
                unix_pair_bares(bad, work),
                Err(format!("gate: box payload Bare must be /g/<name>.git with a Pair: {bad}")),
                "{bad}"
            );
        }
        assert_eq!(
            unix_pair_bares("/g/app.git", "/g/app-1a2b3c4d"),
            Err("gate: box payload Work must be <root>/work/<key> with a Pair: /g/app-1a2b3c4d".to_owned())
        );
        assert_eq!(super::bare_stem("/g/app.git", "/g", PathForm::Unix), Some("app"));
        assert_eq!(
            super::bare_stem("/g/app.git", "/G", PathForm::Unix),
            None,
            "the folder is compared byte for byte"
        );
    }
}
