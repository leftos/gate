//! What the client sends `gate box`: the work folder's place on the builder, the payload word of each verb (base64,
//! standard alphabet and padded, of compact UTF-8 JSON, as `gate box` reads it), and the line ssh carries, which must
//! fit the most cmd takes.

use std::hash::{BuildHasher, RandomState};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

use crate::remote::snapshot::IDENTITY;

/// The longest command line cmd runs, and the part of it kept for the `cmd.exe /c` sshd puts before the line.
const CMD_LIMIT: usize = 8191;
const CMD_MARGIN: usize = 64;
/// The longest line a call may carry, once cmd has expanded it.
pub(crate) const LINE_LIMIT: usize = CMD_LIMIT - CMD_MARGIN;
/// The gate a builder runs, from its own `%LOCALAPPDATA%\gate\bin`, as cmd expands it.
pub(crate) const BOX_EXE: &str = r#""%LOCALAPPDATA%\gate\bin\gate.exe""#;
/// The variable the line names the builder's gate by, which cmd expands before its limit applies.
const LOCAL_APP_DATA: &str = "%LOCALAPPDATA%";
/// The characters `%LOCALAPPDATA%` is counted as: the builder's profile path is unknown here, so a line within the cap
/// stays within cmd's limit for any profile path up to this long.
const LOCAL_APP_DATA_EXPANDED: usize = 100;
/// The hex digits of a run's job nonce, as the box checks them, and of its outputs nonce.
const JOB_NONCE_DIGITS: usize = 12;
const RUN_NONCE_DIGITS: usize = 32;

/// Where one work folder's state lives on the builder: the bare repository, the work folder, its lock and sync-back
/// index files, the folder outputs wait in, the job of this run, named with a nonce new to it, and the sibling pair the
/// box lays out in the work folder, `None` for a run of one repo.
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
}

/// A sibling pair as the payload's `Pair` carries it: the main checkout's folder name and each sibling's, in order.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Pair {
    pub leaf: String,
    pub siblings: Vec<String>,
}

impl Location {
    /// The work folder `key` of `repo` under the builder's `root`, its job named with `job_nonce`, with no pair.
    pub(crate) fn under(root: &str, repo: &str, key: &str, job_nonce: &str) -> Location {
        let bare = bare(root, repo);
        let root = root.trim_end_matches(['\\', '/']);
        let work = format!(r"{root}\work\{key}");
        Location {
            key: key.to_owned(),
            bare,
            lock: format!("{work}.lock"),
            index: format!("{work}.index"),
            work,
            output_folder: format!(r"{root}\outputs"),
            job: format!(r"Local\gate-remote-{key}-{job_nonce}"),
            pair: None,
        }
    }

    /// This location with the sibling pair of the main checkout's folder `leaf` and the folders `siblings`.
    #[cfg_attr(not(test), expect(dead_code, reason = "the client sends no Pair; the tests build one"))]
    pub(crate) fn with_pair(self, leaf: &str, siblings: &[String]) -> Location {
        Location {
            pair: Some(Pair {
                leaf: leaf.to_owned(),
                siblings: siblings.to_vec(),
            }),
            ..self
        }
    }

    /// The payload keys every verb reads, `Pair` only when there is one.
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
        ] {
            object.insert(name.to_owned(), Value::String(value.clone()));
        }
        if let Some(pair) = &self.pair {
            object.insert("Pair".to_owned(), json!({"Leaf": pair.leaf, "Siblings": pair.siblings}));
        }
        object
    }
}

/// The bare repository of `repo` under the builder's `root`.
pub(crate) fn bare(root: &str, repo: &str) -> String {
    format!(r"{}\{repo}.git", root.trim_end_matches(['\\', '/']))
}

/// What a run call carries beside the location: the words the builder runs, the folder below the work folder they run
/// in, the run's outputs nonce, the snapshot the box checks out, the output globs the run brings back (none asks for no
/// outputs), and whether every file under them counts as changed (`OutputsFull`), as after an outputs step that did not
/// complete; the run's ceiling in whole seconds, which the box counts in load-adjusted time (`Timeout`), and the PC's
/// name for the builder, which the box's kill line names (`Host`); and each sibling's name and the commit its snapshot
/// is (`SiblingSnapshots`, sent only when there is one).
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
}

/// The payload word of the preflight, and of every verb that reads only the location.
pub(crate) fn location_word(location: &Location) -> String {
    word(&location.object())
}

/// The payload word of a preflight that asks the builder to probe tools: the location and `Needs`, an object of each
/// tool's name and the minimum version the run needs of it (empty for any version), in the order of `needs`. With no
/// needs there is no `Needs`, and the word is [`location_word`]'s.
pub(crate) fn preflight_word(location: &Location, needs: &[(String, String)]) -> String {
    let mut object = location.object();
    if !needs.is_empty() {
        let needs: Map<String, Value> = needs
            .iter()
            .map(|(name, minimum)| (name.clone(), Value::String(minimum.clone())))
            .collect();
        object.insert("Needs".to_owned(), Value::Object(needs));
    }
    word(&object)
}

/// The preflight's line, as [`capped`] lets it through, and whether it left the needs out.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PreflightLine {
    pub line: String,
    /// Whether the needs made the line longer than cmd takes, so it carries the location alone.
    pub needs_dropped: bool,
}

/// The preflight's line for `location`, carrying `needs` ([`preflight_word`]) when cmd takes it so, else the location
/// alone ([`location_word`]); `Err` with [`capped`]'s reason when cmd takes neither.
pub(crate) fn preflight_line(location: &Location, needs: &[(String, String)]) -> Result<PreflightLine, String> {
    match capped("preflight", line("preflight", &preflight_word(location, needs))) {
        Ok(line) => Ok(PreflightLine { line, needs_dropped: false }),
        Err(_) if !needs.is_empty() => {
            capped("preflight", line("preflight", &location_word(location))).map(|line| PreflightLine { line, needs_dropped: true })
        }
        Err(why) => Err(why),
    }
}

/// The payload word of the outputs call: the location and the outputs archive's `Name`, `name`.
pub(crate) fn outputs_word(location: &Location, name: &str) -> String {
    let mut object = location.object();
    object.insert("Name".to_owned(), Value::String(name.to_owned()));
    word(&object)
}

/// The payload word of the run call.
pub(crate) fn run_word(location: &Location, run: &RunCall<'_>) -> String {
    let mut object = location.object();
    let identity: Map<String, Value> = IDENTITY
        .iter()
        .map(|(name, value)| ((*name).to_owned(), Value::String((*value).to_owned())))
        .collect();
    for (name, value) in [
        ("Command", json!(run.words)),
        ("Subfolder", json!(run.subfolder)),
        ("Identity", Value::Object(identity)),
        ("Outputs", json!(run.outputs)),
        ("Nonce", json!(run.nonce)),
        ("OutputsFull", json!(run.full)),
        ("Snapshot", json!(run.snapshot)),
        ("Timeout", json!(run.timeout)),
        ("Host", json!(run.host)),
    ] {
        object.insert(name.to_owned(), value);
    }
    if !run.sibling_snapshots.is_empty() {
        let snapshots: Map<String, Value> = run
            .sibling_snapshots
            .iter()
            .map(|(name, commit)| (name.clone(), Value::String(commit.clone())))
            .collect();
        object.insert("SiblingSnapshots".to_owned(), Value::Object(snapshots));
    }
    word(&object)
}

/// `object` as one word: base64 of its compact UTF-8 JSON.
fn word(object: &Map<String, Value>) -> String {
    STANDARD.encode(Value::Object(object.clone()).to_string())
}

/// The line ssh carries for `verb`: the builder's gate, `box`, the verb and the payload word.
pub(crate) fn line(verb: &str, word: &str) -> String {
    format!("{BOX_EXE} box {verb} {word}")
}

/// `line` of the call `verb` when cmd takes it, each `%LOCALAPPDATA%` counted as the path it expands to; else why not,
/// in the PowerShell client's words.
pub(crate) fn capped(verb: &str, line: String) -> Result<String, String> {
    let expansions = line.matches(LOCAL_APP_DATA).count();
    let length = line.chars().count() - expansions * LOCAL_APP_DATA.len() + expansions * LOCAL_APP_DATA_EXPANDED;
    if length > LINE_LIMIT {
        return Err(format!(
            "the {verb} call is {length} characters, longer than the {LINE_LIMIT} cmd takes ({CMD_LIMIT} less {CMD_MARGIN} \
             for sshd's own cmd.exe /c); shorten the command"
        ));
    }
    Ok(line)
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
    use std::ffi::OsStr;

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde_json::{Value, json};

    use super::{
        BOX_EXE, LINE_LIMIT, Location, Pair, PreflightLine, RunCall, capped, job_nonce, line, location_word, preflight_line, preflight_word,
        run_nonce, run_word,
    };
    use crate::boxside::payload::Payload;

    fn decoded(word: &str) -> Value {
        let bytes = STANDARD.decode(word).expect("the word is base64");
        serde_json::from_str(&String::from_utf8(bytes).expect("the JSON is UTF-8")).expect("the word is JSON")
    }

    fn location() -> Location {
        Location::under(r"D:\gate-remote\", "app", "app-1a2b3c4d", "0123456789ab")
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
        };
        run_word(&location().with_pair("app", &["lib".to_owned(), "tools".to_owned()]), &run)
    }

    /// The box's reading of the payload word `word`.
    fn on_the_box(word: &str) -> Payload {
        Payload::decode(OsStr::new(word)).expect("the box decodes the client's word")
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
        assert_eq!(
            preflight_word(&location(), &[]),
            location_word(&location()),
            "no needs: the word is byte for byte the location's"
        );
        let needs = [
            ("node".to_owned(), "24.0.0".to_owned()),
            ("cargo-nextest".to_owned(), String::new()),
            ("ffmpeg".to_owned(), "7".to_owned()),
        ];
        let with_needs = decoded(&preflight_word(&location(), &needs));
        let mut expected = decoded(&location_word(&location()));
        expected["Needs"] = json!({"node": "24.0.0", "cargo-nextest": "", "ffmpeg": "7"});
        assert_eq!(with_needs, expected);
        let order: Vec<&String> = with_needs["Needs"].as_object().expect("Needs is an object").keys().collect();
        assert_eq!(order, ["node", "cargo-nextest", "ffmpeg"], "the needs keep their order");
    }

    fn needs() -> Vec<(String, String)> {
        ["rust", "cargo-nextest", "node", "pnpm", "pwsh", "godot", "ffmpeg"]
            .iter()
            .map(|name| ((*name).to_owned(), "1.2.3".to_owned()))
            .collect()
    }

    /// A location under a root of `length` letters.
    fn location_of(length: usize) -> Location {
        Location::under(&format!("D:\\{}\\", "r".repeat(length)), "app", "app-1a2b3c4d", "0123456789ab")
    }

    #[test]
    fn a_preflight_line_carries_its_needs_when_cmd_takes_it_so() {
        let preflight = preflight_line(&location(), &needs()).expect("a short line is taken");
        assert_eq!(
            preflight,
            PreflightLine {
                line: line("preflight", &preflight_word(&location(), &needs())),
                needs_dropped: false,
            }
        );
    }

    #[test]
    fn a_preflight_line_its_needs_make_too_long_carries_the_location_alone() {
        let length = (1..4000)
            .find(|&length| capped("preflight", line("preflight", &preflight_word(&location_of(length), &needs()))).is_err())
            .expect("a root that long makes the line with needs too long");
        let location = location_of(length);
        assert_eq!(
            preflight_line(&location, &needs()).expect("the location alone is taken"),
            PreflightLine {
                line: line("preflight", &location_word(&location)),
                needs_dropped: true,
            }
        );
        assert!(
            !preflight_line(&location_of(length - 1), &needs())
                .expect("a root one shorter is taken")
                .needs_dropped,
            "the needs are dropped only past the cap"
        );
    }

    #[test]
    fn a_preflight_line_too_long_without_its_needs_is_err() {
        let location = location_of(LINE_LIMIT);
        let why = preflight_line(&location, &needs()).expect_err("the location alone is too long");
        assert!(why.starts_with("the preflight call is "), "{why}");
        assert_eq!(preflight_line(&location, &[]).expect_err("no needs to drop"), why);
    }

    #[test]
    fn the_payloads_are_base64_json_with_the_box_s_keys() {
        let preflight = decoded(&location_word(&location()));
        assert_eq!(
            preflight,
            json!({
                "Key": "app-1a2b3c4d",
                "Bare": r"D:\gate-remote\app.git",
                "Work": r"D:\gate-remote\work\app-1a2b3c4d",
                "Lock": r"D:\gate-remote\work\app-1a2b3c4d.lock",
                "Index": r"D:\gate-remote\work\app-1a2b3c4d.index",
                "OutputFolder": r"D:\gate-remote\outputs",
                "Job": r"Local\gate-remote-app-1a2b3c4d-0123456789ab",
            })
        );
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
        };

        let sent = decoded(&run_word(&location(), &run));

        assert_eq!(sent["Outputs"], json!(["**/bin/**", "out/*.dll"]));
        assert_eq!(sent["OutputsFull"], json!(true));
        assert_eq!(sent["Nonce"], json!("n2"));
    }

    #[test]
    fn the_line_runs_the_builder_s_gate_with_the_verb_and_the_word() {
        assert_eq!(line("run", "e30="), r#""%LOCALAPPDATA%\gate\bin\gate.exe" box run e30="#);
    }

    #[test]
    fn the_cap_counts_localappdata_as_100_characters() {
        // The builder's gate is 34 characters as sent, of them the 14 of `%LOCALAPPDATA%`, which cmd expands to a
        // profile path of up to 100: 120 counted, so a run line's other 8007 characters fit and 8008 do not.
        let sent = |rest: usize| format!("{BOX_EXE}{}", "x".repeat(rest));
        assert_eq!(BOX_EXE.chars().count(), 34);
        assert_eq!(capped("run", sent(8007)), Ok(sent(8007)), "8127 characters once expanded fit");
        assert_eq!(
            capped("run", sent(8008)),
            Err(
                "the run call is 8128 characters, longer than the 8127 cmd takes (8191 less 64 for sshd's own cmd.exe /c); \
                 shorten the command"
                    .to_owned()
            )
        );
    }

    #[test]
    fn a_line_longer_than_8127_characters_is_refused() {
        let at = |length: usize| "x".repeat(length);
        assert_eq!(LINE_LIMIT, 8127);
        assert_eq!(capped("run", at(8127)), Ok(at(8127)), "8127 characters fit");
        assert_eq!(
            capped("run", at(8128)),
            Err(
                "the run call is 8128 characters, longer than the 8127 cmd takes (8191 less 64 for sshd's own cmd.exe /c); \
                 shorten the command"
                    .to_owned()
            )
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
