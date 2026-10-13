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
/// index files, the folder outputs wait in, and the job of this run, named with a nonce new to it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Location {
    pub key: String,
    pub bare: String,
    pub work: String,
    pub lock: String,
    pub index: String,
    pub output_folder: String,
    pub job: String,
}

impl Location {
    /// The work folder `key` of `repo` under the builder's `root`, its job named with `job_nonce`.
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
        }
    }

    /// The payload keys every verb reads.
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
/// complete.
pub(crate) struct RunCall<'a> {
    pub words: &'a [String],
    pub subfolder: &'a str,
    pub nonce: &'a str,
    pub snapshot: &'a str,
    pub outputs: &'a [String],
    pub full: bool,
}

/// The payload word of the preflight, and of every verb that reads only the location.
pub(crate) fn location_word(location: &Location) -> String {
    word(&location.object())
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
    ] {
        object.insert(name.to_owned(), value);
    }
    word(&object)
}

/// The payload word of the inflight probe, which asks a builder how many of its work folders a run holds: the `Root`
/// its work folders live under, which is all the probe reads.
#[cfg_attr(not(test), expect(dead_code, reason = "only its unit test calls it"))]
pub(crate) fn inflight_word(root: &str) -> String {
    let mut object = Map::new();
    object.insert("Root".to_owned(), Value::String(root.to_owned()));
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
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde_json::{Value, json};

    use super::{BOX_EXE, LINE_LIMIT, Location, RunCall, capped, inflight_word, job_nonce, line, location_word, run_nonce, run_word};

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
            }
        );
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
    fn the_inflight_word_carries_the_builders_root_and_nothing_else() {
        assert_eq!(decoded(&inflight_word(r"D:\gate-remote")), json!({ "Root": r"D:\gate-remote" }));
        assert_eq!(
            decoded(&inflight_word(r"D:\gate-remote\")),
            json!({ "Root": r"D:\gate-remote\" }),
            "the builder sends the root as it is given, and the probe folds a trailing separator off it"
        );
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
