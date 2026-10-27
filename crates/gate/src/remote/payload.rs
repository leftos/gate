//! What the client sends `gate box`: the work folder's place on the builder, the payload word of each verb (base64,
//! standard alphabet and padded, of compact UTF-8 JSON, as `gate box` reads it), the fixed line ssh carries, and the
//! call's standard input, which carries the word.

use std::hash::{BuildHasher, RandomState};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use gate_args::BoxVerb;
use gate_wire::PathForm;
use serde_json::{Map, Value, json};

use crate::remote::snapshot::IDENTITY;
use crate::remote::transport::Profile;
use crate::wait::REMOTE_LIMIT;

/// The gate a builder runs, from its admin-owned `C:\Program Files\gate\bin`, named literally.
pub(crate) const BOX_EXE: &str = r#""C:\Program Files\gate\bin\gate.exe""#;
/// The hex digits of a run's job nonce, as the box checks them, and of its outputs nonce.
const JOB_NONCE_DIGITS: usize = 12;
const RUN_NONCE_DIGITS: usize = 32;

/// Where one work folder's state lives on the builder: the bare repository, the work folder, its lock and sync-back
/// index files, the folder outputs wait in, the job of this run, named with a nonce new to it, the sibling pair the
/// box lays out in the work folder, `None` for a run of one repo, and the path rules the builder's box reads them under.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Location {
    pub key: String,
    pub bare: String,
    pub work: String,
    pub lock: String,
    pub index: String,
    pub output_folder: String,
    pub job: String,
    pub pair: Option<Pair>,
    pub form: PathForm,
    /// The run's drain session (`Session`), sent only when there is one.
    pub session: Option<String>,
    /// The PC's name (`Pc`), `COMPUTERNAME`, sent only when it is set and not empty.
    pub pc: Option<String>,
    /// The repo the run is of (`Repo`).
    pub repo: String,
    /// Whether the call takes no ticket in the drain's line (`NoTicket`): a warm-up's.
    pub no_ticket: bool,
}

/// A sibling pair as the payload's `Pair` carries it: the main checkout's folder name and each sibling's, in order.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Pair {
    pub leaf: String,
    pub siblings: Vec<String>,
}

impl Location {
    /// The work folder `key` of `repo` on the builder of `profile`, laid out as its profile lays it out, its job named
    /// with `job_nonce`, with no pair, no drain session, the PC's name ([`pc_name`]) and a ticket asked for.
    pub(crate) fn under(profile: &Profile, repo: &str, key: &str, job_nonce: &str) -> Location {
        let work = profile.work(key);
        Location {
            key: key.to_owned(),
            bare: profile.bare(repo),
            lock: format!("{work}.lock"),
            index: format!("{work}.index"),
            work,
            output_folder: profile.output_folder(),
            job: profile.job(key, job_nonce),
            pair: None,
            form: profile.form(),
            session: None,
            pc: pc_name(),
            repo: repo.to_owned(),
            no_ticket: false,
        }
    }

    /// This location with the run's drain `session`, taking no ticket when `no_ticket`.
    pub(crate) fn with_session(self, session: &str, no_ticket: bool) -> Location {
        Location {
            session: Some(session.to_owned()),
            no_ticket,
            ..self
        }
    }

    /// This location with the sibling pair of the main checkout's folder `leaf` and the folders `siblings`.
    pub(crate) fn with_pair(self, leaf: &str, siblings: &[String]) -> Location {
        Location {
            pair: Some(Pair {
                leaf: leaf.to_owned(),
                siblings: siblings.to_vec(),
            }),
            ..self
        }
    }

    /// The payload keys every verb reads, `Pair`, `Session` and `Pc` only when there is one.
    fn object(&self) -> Map<String, Value> {
        let mut object = Map::new();
        for (name, value) in [
            ("Key", &self.key),
            ("Bare", &self.bare),
            ("Work", &self.work),
            ("Lock", &self.lock),
            ("Index", &self.index),
            ("OutputFolder", &self.output_folder),
            ("Job", &self.job),
            ("Repo", &self.repo),
        ] {
            object.insert(name.to_owned(), Value::String(value.clone()));
        }
        if let Some(pair) = &self.pair {
            object.insert("Pair".to_owned(), json!({"Leaf": pair.leaf, "Siblings": pair.siblings}));
        }
        if let Some(session) = &self.session {
            object.insert("Session".to_owned(), json!(session));
        }
        if let Some(pc) = &self.pc {
            object.insert("Pc".to_owned(), json!(pc));
        }
        object.insert("NoTicket".to_owned(), json!(self.no_ticket));
        object
    }
}

/// The PC's name, `COMPUTERNAME`; `None` when it is unset, empty or not Unicode.
fn pc_name() -> Option<String> {
    std::env::var("COMPUTERNAME").ok().filter(|name| !name.is_empty())
}

/// What a run call carries beside the location: the words the builder runs, the folder below the work folder they run
/// in, the run's outputs nonce, the snapshot the box checks out, the output globs the run brings back (none asks for no
/// outputs), and whether every file under them counts as changed (`OutputsFull`), as after an outputs step that did not
/// complete; the run's ceiling in whole seconds, which the box counts in load-adjusted time (`Timeout`), and the PC's
/// name for the builder, which the box's kill line names (`Host`); the run's variables, the caller's `--env` pairs and
/// the gate's `SourceRevisionId` (`Env`, sent only when there is one); and each sibling's name and the commit its snapshot is (`SiblingSnapshots`, sent only when there is
/// one); the versions of the builder's Godot install and Unreal Engine install the run uses (`GodotVersion` and
/// `UnrealVersion`, each sent only when one is chosen); and the names set to the Unreal Engine install's root
/// (`EngineEnv`, sent only when there is one).
pub(crate) struct RunCall<'a> {
    pub words: &'a [String],
    pub subfolder: &'a str,
    pub nonce: &'a str,
    pub snapshot: &'a str,
    pub outputs: &'a [String],
    pub full: bool,
    pub timeout: u64,
    pub host: &'a str,
    pub sibling_snapshots: &'a [(String, String)],
    /// The SHA-256 of the inputs manifest line the inputs calls sent (`InputsDigest`), `None` for a run with no inputs.
    pub inputs_digest: Option<&'a str>,
    /// The SHA-256 of the LFS manifest line of the objects the snapshot's pointers name, without its newline
    /// (`LfsDigest`), `None` for a run whose snapshot holds no pointer.
    pub lfs_digest: Option<&'a str>,
    /// The SHA-256 of the manifest line of the PC's build the send sent the work folder, without its newline
    /// (`SentDigest`), `None` for a run whose send did not land or that has none.
    pub sent_digest: Option<&'a str>,
    /// The run's variables, the caller's `--env` pairs and the gate's `SourceRevisionId` (`Env`, sent only when there
    /// is one); a warm-up never sends them.
    pub env: &'a [(String, String)],
    /// The version, `major.minor.patch`, of the builder's Godot install the run uses (`GodotVersion`, sent only when
    /// one is chosen), which a warm-up sends too.
    pub godot_version: Option<&'a str>,
    /// The version, `major.minor.patch`, of the builder's Unreal Engine install the run uses (`UnrealVersion`, sent
    /// only when one is chosen), which a warm-up sends too.
    pub unreal_version: Option<&'a str>,
    /// The names the repo's toolchain manifest `[env]` sets to the root of the chosen Unreal Engine install
    /// (`EngineEnv`, an object of each name and `"unreal"`, sent only when there is one, whether or not a version is
    /// chosen, so the box removes a value it inherited), which a warm-up sends too.
    pub engine_env: &'a [String],
    /// Whether the run asks for the builder's desktop (`Desktop: true` and `DesktopWait`, the usual remote wait in
    /// whole seconds, sent only then); a warm-up never sends them, as the box refuses a warm-up on the desktop.
    pub desktop: bool,
}

/// The step an inputs call asks the box for: which files the work folder needs (`diff`), or installing them (`put`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputsStep {
    Diff,
    Put,
}

/// The payload word of an inputs call: the location, the run's `Nonce` and the `Step`, and, for a send of the PC's
/// build of one project, its bin folder `sent_bin` as `SentBin`, `{project}/bin/{configuration}`.
pub(crate) fn inputs_word(location: &Location, nonce: &str, step: InputsStep, sent_bin: Option<&str>) -> String {
    let mut object = location.object();
    let step = match step {
        InputsStep::Diff => "diff",
        InputsStep::Put => "put",
    };
    object.insert("Nonce".to_owned(), json!(nonce));
    object.insert("Step".to_owned(), json!(step));
    if let Some(bin) = sent_bin {
        object.insert("SentBin".to_owned(), json!(bin));
    }
    word(&object)
}

/// The payload word of the preflight, and of every verb that reads only the location.
pub(crate) fn location_word(location: &Location) -> String {
    word(&location.object())
}

/// The payload word of a preflight that asks the builder to probe tools: the location, the call's `Nonce`, which a
/// refusal tag names, and `Needs`, an object of each tool's name and the minimum version the run needs of it (empty
/// for any version), in the order of `needs`. With no needs there is no `Needs`.
pub(crate) fn preflight_word(location: &Location, needs: &[(String, String)], nonce: &str) -> String {
    let mut object = location.object();
    object.insert("Nonce".to_owned(), json!(nonce));
    if !needs.is_empty() {
        let needs: Map<String, Value> = needs
            .iter()
            .map(|(name, minimum)| (name.clone(), Value::String(minimum.clone())))
            .collect();
        object.insert("Needs".to_owned(), Value::Object(needs));
    }
    word(&object)
}

/// The payload word of the outputs call: the location and the outputs archive's `Name`, `name`.
pub(crate) fn outputs_word(location: &Location, name: &str) -> String {
    let mut object = location.object();
    object.insert("Name".to_owned(), Value::String(name.to_owned()));
    word(&object)
}

/// The payload word of the run call, with the run's variables (the caller's `--env` pairs and the gate's
/// `SourceRevisionId`) as `Env` when there is one, and `Desktop: true`
/// with `DesktopWait` ([`REMOTE_LIMIT`] in whole seconds, which the box caps at the run's `Timeout`) for a run that asks
/// for the builder's desktop.
pub(crate) fn run_word(location: &Location, run: &RunCall<'_>) -> String {
    let mut object = location.object();
    for (name, value) in [
        ("Command", json!(run.words)),
        ("Subfolder", json!(run.subfolder)),
        ("Identity", identity()),
        ("Outputs", json!(run.outputs)),
        ("Nonce", json!(run.nonce)),
        ("OutputsFull", json!(run.full)),
        ("Snapshot", json!(run.snapshot)),
        ("Timeout", json!(run.timeout)),
        ("Host", json!(run.host)),
    ] {
        object.insert(name.to_owned(), value);
    }
    if !run.env.is_empty() {
        let env: Map<String, Value> = run.env.iter().map(|(name, value)| (name.clone(), Value::String(value.clone()))).collect();
        object.insert("Env".to_owned(), Value::Object(env));
    }
    if run.desktop {
        object.insert("Desktop".to_owned(), json!(true));
        object.insert("DesktopWait".to_owned(), json!(REMOTE_LIMIT.as_secs()));
    }
    insert_siblings_and_inputs(&mut object, run);
    word(&object)
}

/// What a warm-up's run call carries in place of a command: the commands of the warm-up file, in order, and the
/// SHA-256 of its bytes, which the box confirms once every command passed.
pub(crate) struct WarmCall<'a> {
    pub commands: &'a [Vec<String>],
    pub hash: &'a str,
}

/// The payload word of a warm-up's run call: the keys of `run`'s word, its `Timeout` always among them, with the
/// warm-up's `WarmCommands` and `WarmHash` in place of `Command`, and no `Outputs` or `OutputsFull`, as the box takes a
/// warm-up only without them; `run`'s words, output globs and `full` are not read.
pub(crate) fn warm_word(location: &Location, run: &RunCall<'_>, warm: &WarmCall<'_>) -> String {
    let mut object = location.object();
    for (name, value) in [
        ("WarmCommands", json!(warm.commands)),
        ("WarmHash", json!(warm.hash)),
        ("Subfolder", json!(run.subfolder)),
        ("Identity", identity()),
        ("Nonce", json!(run.nonce)),
        ("Snapshot", json!(run.snapshot)),
        ("Timeout", json!(run.timeout)),
        ("Host", json!(run.host)),
    ] {
        object.insert(name.to_owned(), value);
    }
    insert_siblings_and_inputs(&mut object, run);
    word(&object)
}

/// The git identity the box commits the run's changes as (`Identity`).
fn identity() -> Value {
    Value::Object(
        IDENTITY
            .iter()
            .map(|(name, value)| ((*name).to_owned(), Value::String((*value).to_owned())))
            .collect(),
    )
}

/// Adds `run`'s `SiblingSnapshots`, when it has any, its `InputsDigest`, when it has one, its `GodotVersion` and
/// `UnrealVersion`, each when one is chosen, and its `EngineEnv`, when it names any, to `object`.
fn insert_siblings_and_inputs(object: &mut Map<String, Value>, run: &RunCall<'_>) {
    if let Some(version) = run.godot_version {
        object.insert("GodotVersion".to_owned(), json!(version));
    }
    if let Some(version) = run.unreal_version {
        object.insert("UnrealVersion".to_owned(), json!(version));
    }
    if !run.engine_env.is_empty() {
        let names: Map<String, Value> = run
            .engine_env
            .iter()
            .map(|name| (name.clone(), Value::String("unreal".to_owned())))
            .collect();
        object.insert("EngineEnv".to_owned(), Value::Object(names));
    }
    if !run.sibling_snapshots.is_empty() {
        let snapshots: Map<String, Value> = run
            .sibling_snapshots
            .iter()
            .map(|(name, commit)| (name.clone(), Value::String(commit.clone())))
            .collect();
        object.insert("SiblingSnapshots".to_owned(), Value::Object(snapshots));
    }
    if let Some(digest) = run.inputs_digest {
        object.insert("InputsDigest".to_owned(), json!(digest));
    }
    if let Some(digest) = run.lfs_digest {
        object.insert("LfsDigest".to_owned(), json!(digest));
    }
    if let Some(digest) = run.sent_digest {
        object.insert("SentDigest".to_owned(), json!(digest));
    }
}

/// `object` as one word: base64 of its compact UTF-8 JSON.
fn word(object: &Map<String, Value>) -> String {
    STANDARD.encode(Value::Object(object.clone()).to_string())
}

/// The line ssh carries for `verb`, the same whatever the call sends: the builder's gate `gate` ([`Profile::line`]),
/// `box`, the verb and, for a verb that reads a request, `--stdin`, as the request comes on the call's standard input
/// ([`input`]).
pub(crate) fn line(gate: &str, verb: BoxVerb) -> String {
    if verb.takes_request() {
        format!("{gate} box {} --stdin", verb.as_str())
    } else {
        format!("{gate} box {}", verb.as_str())
    }
}

/// What a call writes on the box's standard input: the payload `word` as a line, then the run call's outputs
/// `manifest` line, which ends with its own newline, when it sends one.
pub(crate) fn input(word: &str, manifest: Option<&str>) -> Arc<str> {
    Arc::from(format!("{word}\n{}", manifest.unwrap_or_default()))
}

/// A job nonce new to the run: 12 lower-case hex digits.
pub(crate) fn job_nonce() -> String {
    nonce(JOB_NONCE_DIGITS)
}

/// An outputs nonce new to the run: 32 lower-case hex digits.
pub(crate) fn run_nonce() -> String {
    nonce(RUN_NONCE_DIGITS)
}

/// `digits` (at most 32) lower-case hex digits drawn from the process's randomly seeded hasher, the time and the process id.
fn nonce(digits: usize) -> String {
    let state = RandomState::new();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_nanos());
    let hash = |round: u32| state.hash_one((now, std::process::id(), round));
    let mut text = format!("{:016x}{:016x}", hash(0), hash(1));
    text.truncate(digits);
    text
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose payload cannot be decoded must fail loudly")]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use gate_args::BoxVerb;
    use serde_json::{Value, json};

    use super::{
        InputsStep, Location, Pair, RunCall, WarmCall, input, inputs_word, job_nonce, location_word, outputs_word, pc_name, preflight_word,
        run_nonce, run_word, warm_word,
    };
    use crate::remote::transport::Profile;
    use gate_router::{Builder, BuilderOs};
    use gate_wire::{PathForm, Payload};

    fn decoded(word: &str) -> Value {
        let bytes = STANDARD.decode(word).expect("the word is base64");
        serde_json::from_str(&String::from_utf8(bytes).expect("the JSON is UTF-8")).expect("the word is JSON")
    }

    /// `host`, a Windows builder, under its root with a trailing separator.
    fn host() -> Profile {
        Profile::new("builder", BuilderOs::Windows, r"D:\gate-remote\")
    }

    fn location() -> Location {
        Location::under(&host(), "app", "app-1a2b3c4d", "0123456789ab")
    }

    /// The nonce of the preflights these tests word.
    const NONCE: &str = "0123456789abcdef0123456789abcdef";
    /// The drain session of the routed calls these tests word.
    const SESSION: &str = "fedcba9876543210fedcba9876543210";

    #[test]
    fn every_routed_call_carries_session_pc_repo_and_no_ticket() {
        let routed = location().with_session(SESSION, false);
        let commands = warm_commands();
        let warm = WarmCall {
            commands: &commands,
            hash: SHA,
        };
        let words = [
            ("preflight", preflight_word(&routed, &[], NONCE)),
            ("inputs", inputs_word(&routed, NONCE, InputsStep::Diff, None)),
            ("outputs", outputs_word(&routed, "app.zip")),
            ("run", run_word(&routed, &warm_run(&[], None))),
        ];
        for (verb, word) in words {
            let payload = on_the_box(&word);
            assert_eq!(payload.session(), Ok(Some(SESSION)), "{verb}");
            assert_eq!(payload.pc(), Ok(pc_name().as_deref()), "{verb}");
            assert_eq!(payload.repo(), Ok(Some("app")), "{verb}");
            assert_eq!(payload.no_ticket(), Ok(false), "{verb}");
        }
        let warm_up = location().with_session(SESSION, true);
        let payload = on_the_box(&warm_word(&warm_up, &warm_run(&[], None), &warm));
        assert_eq!(
            (payload.session(), payload.no_ticket()),
            (Ok(Some(SESSION)), Ok(true)),
            "a warm-up takes no ticket"
        );
        let unrouted = on_the_box(&location_word(&location()));
        assert_eq!(unrouted.session(), Ok(None), "a location with no session sends none");
        assert_eq!(unrouted.repo(), Ok(Some("app")));
    }

    /// The macOS builder `mac-1` under `/Users/b/gate-remote`, reached over the ssh transport.
    fn mac() -> Profile {
        let builder = Builder {
            alias: "mac-1".to_owned(),
            os: BuilderOs::Macos,
            root: Some("/Users/b/gate-remote".to_owned()),
            slots: 1,
            id: Some("00000000".to_owned()),
        };
        Profile::of(&builder, r"D:\gate-remote")
    }

    #[test]
    fn a_macos_builder_s_location_is_laid_out_with_slashes_under_its_root() {
        let location = Location::under(&mac(), "duel", "duel-0a1b2c3d", "0123456789ab");
        let work = "/Users/b/gate-remote/work/duel-0a1b2c3d";
        assert_eq!(
            location,
            Location {
                key: "duel-0a1b2c3d".to_owned(),
                bare: "/Users/b/gate-remote/duel.git".to_owned(),
                work: work.to_owned(),
                lock: format!("{work}.lock"),
                index: format!("{work}.index"),
                output_folder: "/Users/b/gate-remote/outputs".to_owned(),
                job: "gate-remote-duel-0a1b2c3d-0123456789ab".to_owned(),
                pair: None,
                form: PathForm::Unix,
                session: None,
                pc: pc_name(),
                repo: "duel".to_owned(),
                no_ticket: false,
            }
        );
        assert!(gate_wire::is_job_name(&location.job, "duel-0a1b2c3d", PathForm::Unix), "{}", location.job);
        assert!(!gate_wire::is_job_name(&location.job, "duel-0a1b2c3d", PathForm::Windows), "no Local\\");
    }

    #[test]
    fn a_macos_builder_s_payload_is_read_back_by_the_unix_box() {
        let location = Location::under(&mac(), "duel", "duel-0a1b2c3d", &job_nonce());
        let payload = Payload::decode(&location_word(&location), PathForm::Unix).expect("the Unix box decodes the word");
        let (key, work) = payload.work_folder().expect("the Unix box takes the work folder");
        assert_eq!((key, work), ("duel-0a1b2c3d", "/Users/b/gate-remote/work/duel-0a1b2c3d"));
        assert_eq!(payload.path("Bare"), Ok("/Users/b/gate-remote/duel.git"));
        assert_eq!(payload.output_folder(work), Ok("/Users/b/gate-remote/outputs"));
        assert_eq!(payload.job(key), Ok(location.job.as_str()));
        for path in ["Bare", "Work", "Lock", "Index", "OutputFolder"] {
            assert!(payload.path(path).is_ok(), "{path} is a Unix path: {:?}", payload.path(path));
        }
        let paired = Location::under(&mac(), "duel", "duel-0a1b2c3d", &job_nonce()).with_pair("duel", &["lib".to_owned()]);
        let pair = Payload::decode(&location_word(&paired), PathForm::Unix)
            .expect("the Unix box decodes the word")
            .laid_out()
            .expect("the Unix box lays the pair out")
            .expect("the pair is there");
        let bares: Vec<&str> = pair.siblings().iter().map(gate_wire::PairSibling::bare).collect();
        assert_eq!(bares, ["/Users/b/gate-remote/lib.git"]);
    }

    #[test]
    fn the_location_lays_the_work_folder_out_under_the_root() {
        assert_eq!(
            location(),
            Location {
                key: "app-1a2b3c4d".to_owned(),
                bare: r"D:\gate-remote\app.git".to_owned(),
                work: r"D:\gate-remote\work\app-1a2b3c4d".to_owned(),
                lock: r"D:\gate-remote\work\app-1a2b3c4d.lock".to_owned(),
                index: r"D:\gate-remote\work\app-1a2b3c4d.index".to_owned(),
                output_folder: r"D:\gate-remote\outputs".to_owned(),
                job: r"Local\gate-remote-app-1a2b3c4d-0123456789ab".to_owned(),
                pair: None,
                form: PathForm::Windows,
                session: None,
                pc: pc_name(),
                repo: "app".to_owned(),
                no_ticket: false,
            }
        );
    }

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    /// The run call of `./tool.exe` with `sibling_snapshots`.
    fn run_call(sibling_snapshots: &[(String, String)]) -> String {
        let words = ["./tool.exe".to_owned()];
        let run = RunCall {
            words: &words,
            subfolder: "",
            nonce: "0123456789abcdef0123456789abcdef",
            snapshot: SHA,
            outputs: &[],
            full: false,
            timeout: 1,
            host: "builder",
            sibling_snapshots,
            inputs_digest: None,
            lfs_digest: None,
            sent_digest: None,
            env: &[],
            godot_version: None,
            unreal_version: None,
            engine_env: &[],
            desktop: false,
        };
        run_word(&location().with_pair("app", &["lib".to_owned(), "tools".to_owned()]), &run)
    }

    /// The box's reading of the payload word `word`.
    fn on_the_box(word: &str) -> Payload {
        Payload::decode(word, gate_wire::PathForm::Windows).expect("the box decodes the client's word")
    }

    #[test]
    fn a_location_with_a_pair_sends_it_and_the_box_reads_it_back() {
        let siblings = ["yaat-server".to_owned(), "a.b".to_owned()];
        let paired = location().with_pair("app", &siblings);
        assert_eq!(
            paired.pair,
            Some(Pair {
                leaf: "app".to_owned(),
                siblings: siblings.to_vec()
            })
        );
        let sent = decoded(&location_word(&paired));
        let mut expected = decoded(&location_word(&location()));
        expected["Pair"] = json!({"Leaf": "app", "Siblings": ["yaat-server", "a.b"]});
        assert_eq!(sent, expected, "the location with the Pair added");

        let pair = on_the_box(&location_word(&paired))
            .pair()
            .expect("the box takes the pair")
            .expect("the box finds a pair");
        assert_eq!(pair.leaf(), "app");
        let read: Vec<(&str, &str)> = pair.siblings().iter().map(|sibling| (sibling.name(), sibling.bare())).collect();
        assert_eq!(
            read,
            [("yaat-server", r"D:\gate-remote\yaat-server.git"), ("a.b", r"D:\gate-remote\a-b.git")]
        );
    }

    #[test]
    fn a_location_without_a_pair_sends_none_and_the_box_reads_none() {
        assert!(decoded(&location_word(&location())).get("Pair").is_none(), "no Pair key");
        assert!(
            on_the_box(&location_word(&location())).pair().expect("no pair is no refusal").is_none(),
            "a run of one repo"
        );
    }

    #[test]
    fn a_run_with_sibling_snapshots_sends_them_and_the_box_reads_them_back() {
        let other = "fedcba9876543210fedcba9876543210fedcba98";
        let snapshots = [("tools".to_owned(), other.to_owned()), ("lib".to_owned(), SHA.to_owned())];
        let word = run_call(&snapshots);
        assert_eq!(decoded(&word)["SiblingSnapshots"], json!({"lib": SHA, "tools": other}));
        let payload = on_the_box(&word);
        let pair = payload.pair().expect("the box takes the pair").expect("the box finds a pair");
        assert_eq!(payload.sibling_snapshots(&pair), Ok(vec![SHA, other]), "in the pair's order");
    }

    #[test]
    fn a_run_without_sibling_snapshots_sends_none() {
        let word = run_call(&[]);
        assert!(decoded(&word).get("SiblingSnapshots").is_none(), "no SiblingSnapshots key");
        let payload = on_the_box(&word);
        let pair = payload.pair().expect("the box takes the pair").expect("the box finds a pair");
        assert_eq!(
            payload.sibling_snapshots(&pair),
            Err("gate: box payload SiblingSnapshots must name exactly the Pair's siblings: lib, tools".to_owned()),
            "the box refuses a pair run without them"
        );
    }

    #[test]
    fn a_preflight_with_needs_carries_them_in_order_and_one_without_is_the_location_alone() {
        let mut alone = decoded(&location_word(&location()));
        alone["Nonce"] = json!(NONCE);
        assert_eq!(
            decoded(&preflight_word(&location(), &[], NONCE)),
            alone,
            "no needs: the location and the Nonce alone"
        );
        let needs = [
            ("node".to_owned(), "24.0.0".to_owned()),
            ("cargo-nextest".to_owned(), String::new()),
            ("ffmpeg".to_owned(), "7".to_owned()),
        ];
        let with_needs = decoded(&preflight_word(&location(), &needs, NONCE));
        let mut expected = alone;
        expected["Needs"] = json!({"node": "24.0.0", "cargo-nextest": "", "ffmpeg": "7"});
        assert_eq!(with_needs, expected);
        let order: Vec<&String> = with_needs["Needs"].as_object().expect("Needs is an object").keys().collect();
        assert_eq!(order, ["node", "cargo-nextest", "ffmpeg"], "the needs keep their order");
    }

    #[test]
    fn the_payloads_are_base64_json_with_the_box_s_keys() {
        let preflight = decoded(&location_word(&location()));
        let mut keys = json!({
            "Key": "app-1a2b3c4d",
            "Bare": r"D:\gate-remote\app.git",
            "Work": r"D:\gate-remote\work\app-1a2b3c4d",
            "Lock": r"D:\gate-remote\work\app-1a2b3c4d.lock",
            "Index": r"D:\gate-remote\work\app-1a2b3c4d.index",
            "OutputFolder": r"D:\gate-remote\outputs",
            "Job": r"Local\gate-remote-app-1a2b3c4d-0123456789ab",
            "Repo": "app",
            "NoTicket": false,
        });
        if let Some(pc) = pc_name() {
            keys["Pc"] = json!(pc);
        }
        assert_eq!(preflight, keys);
        let words = ["./tool.exe".to_owned(), "a b".to_owned()];
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let run = RunCall {
            words: &words,
            subfolder: "sub",
            nonce: "n1",
            snapshot: sha,
            outputs: &[],
            full: false,
            timeout: 300,
            host: "builder",
            sibling_snapshots: &[],
            inputs_digest: None,
            lfs_digest: None,
            sent_digest: None,
            env: &[],
            godot_version: None,
            unreal_version: None,
            engine_env: &[],
            desktop: false,
        };
        let mut expected = preflight;
        let extra = json!({
            "Command": ["./tool.exe", "a b"],
            "Subfolder": "sub",
            "Identity": {
                "GIT_AUTHOR_NAME": "gate-remote",
                "GIT_AUTHOR_EMAIL": "gate-remote@localhost",
                "GIT_COMMITTER_NAME": "gate-remote",
                "GIT_COMMITTER_EMAIL": "gate-remote@localhost",
            },
            "Outputs": [],
            "Nonce": "n1",
            "OutputsFull": false,
            "Snapshot": sha,
            "Timeout": 300,
            "Host": "builder",
        });
        for (name, value) in extra.as_object().expect("an object") {
            expected[name] = value.clone();
        }
        assert_eq!(decoded(&run_word(&location(), &run)), expected);
    }

    #[test]
    fn a_run_with_output_globs_sends_them_and_outputs_full() {
        let words = ["./tool.exe".to_owned()];
        let globs = ["**/bin/**".to_owned(), "out/*.dll".to_owned()];
        let run = RunCall {
            words: &words,
            subfolder: "",
            nonce: "n2",
            snapshot: "0123456789abcdef0123456789abcdef01234567",
            outputs: &globs,
            full: true,
            timeout: 1,
            host: "builder",
            sibling_snapshots: &[],
            inputs_digest: Some("ab"),
            lfs_digest: None,
            sent_digest: None,
            env: &[],
            godot_version: None,
            unreal_version: None,
            engine_env: &[],
            desktop: false,
        };

        let sent = decoded(&run_word(&location(), &run));

        assert_eq!(sent["InputsDigest"], json!("ab"), "a run with inputs names their digest");
        assert!(sent.get("LfsDigest").is_none(), "a run with no LFS pointers sends no LfsDigest: {sent}");
        let lfs_digest = "cd".repeat(32);
        let with_lfs = RunCall {
            lfs_digest: Some(&lfs_digest),
            ..run
        };
        let sent_lfs = decoded(&run_word(&location(), &with_lfs));
        assert_eq!(sent_lfs["LfsDigest"], json!(lfs_digest), "a run with LFS pointers names their digest");
        assert_eq!(
            on_the_box(&run_word(&location(), &with_lfs)).lfs_digest(),
            Ok(Some(lfs_digest.as_str())),
            "the box reads it back"
        );
        let diff = decoded(&inputs_word(&location(), "n2", InputsStep::Diff, None));
        assert_eq!(
            (&diff["Nonce"], &diff["Step"], &diff["Work"]),
            (&json!("n2"), &json!("diff"), &sent["Work"])
        );
        assert_eq!(decoded(&inputs_word(&location(), "n2", InputsStep::Put, None))["Step"], json!("put"));
        assert_eq!(sent["Outputs"], json!(["**/bin/**", "out/*.dll"]));
        assert_eq!(sent["OutputsFull"], json!(true));
        assert_eq!(sent["Nonce"], json!("n2"));
    }

    #[test]
    fn a_run_with_env_pairs_sends_them_in_order_and_the_box_reads_them_back() {
        let words = ["./tool.exe".to_owned()];
        let env = [("NuGetAudit".to_owned(), "false".to_owned()), ("EMPTY".to_owned(), String::new())];
        let run = RunCall {
            words: &words,
            subfolder: "",
            nonce: "n3",
            snapshot: SHA,
            outputs: &[],
            full: false,
            timeout: 1,
            host: "builder",
            sibling_snapshots: &[],
            inputs_digest: None,
            lfs_digest: None,
            sent_digest: None,
            env: &env,
            godot_version: None,
            unreal_version: None,
            engine_env: &[],
            desktop: false,
        };

        let word = run_word(&location(), &run);

        assert_eq!(decoded(&word)["Env"], json!({"NuGetAudit": "false", "EMPTY": ""}));
        assert_eq!(on_the_box(&word).pairs("Env"), Ok(env.to_vec()), "in the order given");
        assert!(decoded(&run_call(&[])).get("Env").is_none(), "a run with no pairs sends no Env key");
        let commands = warm_commands();
        let warm = WarmCall {
            commands: &commands,
            hash: WARM_HASH,
        };
        assert!(
            decoded(&warm_word(&location(), &run, &warm)).get("Env").is_none(),
            "a warm-up never sends Env"
        );
    }

    #[test]
    fn a_run_and_a_warm_up_send_the_chosen_godot_version_and_none_without_one() {
        let words = ["Godot_console.exe".to_owned()];
        let mut run = RunCall {
            words: &words,
            subfolder: "",
            nonce: "0123456789abcdef0123456789abcdef",
            snapshot: SHA,
            outputs: &[],
            full: false,
            timeout: 1,
            host: "builder",
            sibling_snapshots: &[],
            inputs_digest: None,
            lfs_digest: None,
            sent_digest: None,
            env: &[],
            godot_version: Some("4.7.2"),
            unreal_version: None,
            engine_env: &[],
            desktop: false,
        };
        let commands = warm_commands();
        let warm = WarmCall {
            commands: &commands,
            hash: WARM_HASH,
        };

        let word = run_word(&location(), &run);
        assert_eq!(decoded(&word)["GodotVersion"], json!("4.7.2"));
        assert_eq!(on_the_box(&word).godot_version(), Ok(Some("4.7.2")), "the box reads it back");
        let warm_word_sent = warm_word(&location(), &run, &warm);
        assert_eq!(decoded(&warm_word_sent)["GodotVersion"], json!("4.7.2"), "a warm-up sends it too");
        assert!(on_the_box(&warm_word_sent).commands().is_ok(), "the box takes a warm-up with it");

        run.godot_version = None;
        let word = run_word(&location(), &run);
        assert!(
            decoded(&word).get("GodotVersion").is_none(),
            "no version chosen sends no GodotVersion key"
        );
        assert_eq!(on_the_box(&word).godot_version(), Ok(None));
        assert!(decoded(&warm_word(&location(), &run, &warm)).get("GodotVersion").is_none());
    }

    #[test]
    fn a_run_and_a_warm_up_send_the_chosen_unreal_version_and_none_without_one() {
        let words = ["build.cmd".to_owned()];
        let mut run = RunCall {
            words: &words,
            subfolder: "",
            nonce: "0123456789abcdef0123456789abcdef",
            snapshot: SHA,
            outputs: &[],
            full: false,
            timeout: 1,
            host: "builder",
            sibling_snapshots: &[],
            inputs_digest: None,
            lfs_digest: None,
            sent_digest: None,
            env: &[],
            godot_version: None,
            unreal_version: Some("5.8.0"),
            engine_env: &[],
            desktop: false,
        };
        let commands = warm_commands();
        let warm = WarmCall {
            commands: &commands,
            hash: WARM_HASH,
        };

        let word = run_word(&location(), &run);
        assert_eq!(decoded(&word)["UnrealVersion"], json!("5.8.0"));
        assert_eq!(on_the_box(&word).unreal_version(), Ok(Some("5.8.0")), "the box reads it back");
        let warm_word_sent = warm_word(&location(), &run, &warm);
        assert_eq!(decoded(&warm_word_sent)["UnrealVersion"], json!("5.8.0"), "a warm-up sends it too");
        assert!(on_the_box(&warm_word_sent).commands().is_ok(), "the box takes a warm-up with it");

        run.unreal_version = None;
        let word = run_word(&location(), &run);
        assert!(
            decoded(&word).get("UnrealVersion").is_none(),
            "no version chosen sends no UnrealVersion key"
        );
        assert_eq!(on_the_box(&word).unreal_version(), Ok(None));
        assert!(decoded(&warm_word(&location(), &run, &warm)).get("UnrealVersion").is_none());
    }

    #[test]
    fn a_run_and_a_warm_up_send_the_engine_env_names_and_none_without_any() {
        let words = ["build.cmd".to_owned()];
        let names = ["UE_MCP_TEST_ENGINE_ROOT".to_owned()];
        let mut run = RunCall {
            words: &words,
            subfolder: "",
            nonce: "0123456789abcdef0123456789abcdef",
            snapshot: SHA,
            outputs: &[],
            full: false,
            timeout: 1,
            host: "builder",
            sibling_snapshots: &[],
            inputs_digest: None,
            lfs_digest: None,
            sent_digest: None,
            env: &[],
            godot_version: None,
            unreal_version: None,
            engine_env: &names,
            desktop: false,
        };
        let commands = warm_commands();
        let warm = WarmCall {
            commands: &commands,
            hash: WARM_HASH,
        };

        let word = run_word(&location(), &run);
        assert_eq!(
            decoded(&word)["EngineEnv"],
            json!({ "UE_MCP_TEST_ENGINE_ROOT": "unreal" }),
            "sent with no version chosen too"
        );
        assert_eq!(on_the_box(&word).engine_env(), Ok(names.to_vec()), "the box reads it back");
        let warm_word_sent = warm_word(&location(), &run, &warm);
        assert_eq!(
            decoded(&warm_word_sent)["EngineEnv"],
            json!({ "UE_MCP_TEST_ENGINE_ROOT": "unreal" }),
            "a warm-up sends it too"
        );
        assert!(on_the_box(&warm_word_sent).commands().is_ok(), "the box takes a warm-up with it");

        run.engine_env = &[];
        let word = run_word(&location(), &run);
        assert!(decoded(&word).get("EngineEnv").is_none(), "no names send no EngineEnv key");
        assert_eq!(on_the_box(&word).engine_env(), Ok(Vec::new()));
        assert!(decoded(&warm_word(&location(), &run, &warm)).get("EngineEnv").is_none());
    }

    #[test]
    fn a_desktop_run_asks_for_the_desktop_with_the_usual_remote_wait_and_no_other_call_does() {
        let words = ["./tool.exe".to_owned()];
        let mut run = RunCall {
            words: &words,
            subfolder: "",
            nonce: "0123456789abcdef0123456789abcdef",
            snapshot: SHA,
            outputs: &[],
            full: false,
            timeout: 300,
            host: "builder",
            sibling_snapshots: &[],
            inputs_digest: None,
            lfs_digest: None,
            sent_digest: None,
            env: &[],
            godot_version: None,
            unreal_version: None,
            engine_env: &[],
            desktop: true,
        };
        let commands = warm_commands();
        let warm = WarmCall {
            commands: &commands,
            hash: WARM_HASH,
        };

        let word = run_word(&location(), &run);
        let sent = decoded(&word);
        assert_eq!((&sent["Desktop"], &sent["DesktopWait"]), (&json!(true), &json!(60)));
        assert_eq!(on_the_box(&word).desktop(), Ok(true), "the box reads Desktop back");
        assert_eq!(on_the_box(&word).desktop_wait(), Ok(Some(60)), "the box reads DesktopWait back");
        let warm_sent = decoded(&warm_word(&location(), &run, &warm));
        assert!(
            warm_sent.get("Desktop").is_none() && warm_sent.get("DesktopWait").is_none(),
            "a warm-up never asks for the desktop: {warm_sent}"
        );

        run.desktop = false;
        let sent = decoded(&run_word(&location(), &run));
        assert!(
            sent.get("Desktop").is_none() && sent.get("DesktopWait").is_none(),
            "a run without the desktop sends neither key: {sent}"
        );
    }

    const WARM_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const WARM_NONCE: &str = "fedcba9876543210fedcba9876543210";

    /// The warm-up's commands the warm cases send.
    fn warm_commands() -> Vec<Vec<String>> {
        vec![
            vec!["dotnet".to_owned(), "build".to_owned(), "a b".to_owned()],
            vec!["./tool.exe".to_owned()],
        ]
    }

    /// The run call the warm cases build their word from, with `sibling_snapshots` and `inputs_digest`.
    fn warm_run<'a>(sibling_snapshots: &'a [(String, String)], inputs_digest: Option<&'a str>) -> RunCall<'a> {
        RunCall {
            words: &[],
            subfolder: "sub",
            nonce: WARM_NONCE,
            snapshot: SHA,
            outputs: &[],
            full: false,
            timeout: 1800,
            host: "builder",
            sibling_snapshots,
            inputs_digest,
            lfs_digest: None,
            sent_digest: None,
            env: &[],
            godot_version: None,
            unreal_version: None,
            engine_env: &[],
            desktop: false,
        }
    }

    #[test]
    fn a_warm_word_sets_the_warm_keys_and_no_command_or_outputs() {
        let commands = warm_commands();
        let warm = WarmCall {
            commands: &commands,
            hash: WARM_HASH,
        };
        let sent = decoded(&warm_word(&location(), &warm_run(&[], None), &warm));

        let mut expected = decoded(&location_word(&location()));
        let extra = json!({
            "WarmCommands": [["dotnet", "build", "a b"], ["./tool.exe"]],
            "WarmHash": WARM_HASH,
            "Subfolder": "sub",
            "Identity": {
                "GIT_AUTHOR_NAME": "gate-remote",
                "GIT_AUTHOR_EMAIL": "gate-remote@localhost",
                "GIT_COMMITTER_NAME": "gate-remote",
                "GIT_COMMITTER_EMAIL": "gate-remote@localhost",
            },
            "Nonce": WARM_NONCE,
            "Snapshot": SHA,
            "Timeout": 1800,
            "Host": "builder",
        });
        for (name, value) in extra.as_object().expect("an object") {
            expected[name] = value.clone();
        }
        assert_eq!(sent, expected);
        for key in ["Command", "Outputs", "OutputsFull"] {
            assert!(sent.get(key).is_none(), "a warm word sends no {key}: {sent}");
        }
    }

    #[test]
    fn a_warm_word_is_read_back_by_the_box_s_payload() {
        let commands = warm_commands();
        let warm = WarmCall {
            commands: &commands,
            hash: WARM_HASH,
        };
        let other = "fedcba9876543210fedcba9876543210fedcba98";
        let snapshots = [("tools".to_owned(), other.to_owned()), ("lib".to_owned(), SHA.to_owned())];
        let digest = "ab".repeat(32);
        let paired = || location().with_pair("app", &["lib".to_owned(), "tools".to_owned()]);
        let run = warm_run(&snapshots, Some(&digest));
        let word = warm_word(&paired(), &run, &warm);

        let payload = on_the_box(&word);
        let read = payload.commands().expect("the box takes the warm-up");
        assert_eq!(read.words, commands, "the commands, in order");
        assert_eq!(read.warm_hash, Some(WARM_HASH));
        assert_eq!(payload.timeout(), Ok(Some(1800)));
        assert_eq!(payload.nonce(), Ok(WARM_NONCE));
        assert_eq!(payload.inputs_digest(), Ok(Some(digest.as_str())));
        let pair = payload.pair().expect("the box takes the pair").expect("the box finds a pair");
        assert_eq!(payload.sibling_snapshots(&pair), Ok(vec![SHA, other]), "in the pair's order");

        let as_a_run = decoded(&run_word(&paired(), &run));
        let sent = decoded(&word);
        for key in ["Pair", "SiblingSnapshots", "InputsDigest"] {
            assert_eq!(sent[key], as_a_run[key], "{key} is sent as a run sends it");
        }
    }

    #[test]
    fn the_line_is_the_fixed_box_line_for_the_verb() {
        assert_eq!(host().line(BoxVerb::Run), r#""C:\Program Files\gate\bin\gate.exe" box run --stdin"#);
        assert_eq!(
            host().line(BoxVerb::Preflight),
            r#""C:\Program Files\gate\bin\gate.exe" box preflight --stdin"#
        );
        assert_eq!(
            host().line(BoxVerb::RegisterDesktop),
            r#""C:\Program Files\gate\bin\gate.exe" box register-desktop"#,
            "a verb that reads no request has no --stdin"
        );
    }

    #[test]
    fn the_input_is_the_request_line_then_the_manifest() {
        assert_eq!(&*input("e30=", None), "e30=\n", "no manifest: the request line alone");
        assert_eq!(
            &*input("e30=", Some("{\"files\":[]}\n")),
            "e30=\n{\"files\":[]}\n",
            "the manifest line follows the request line, and the input ends with its newline"
        );
    }

    #[test]
    fn the_nonces_are_hex_of_their_length_and_new_each_time() {
        let job = job_nonce();
        assert!(job.len() == 12 && job.bytes().all(|byte| byte.is_ascii_hexdigit()), "{job}");
        let run = run_nonce();
        assert!(run.len() == 32 && run.bytes().all(|byte| byte.is_ascii_hexdigit()), "{run}");
        assert_ne!(job_nonce(), job_nonce());
    }
}
