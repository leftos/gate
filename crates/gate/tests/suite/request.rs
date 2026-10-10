//! The request the gate resolves, seen through `--test-print-request`: the absolute log, the defaults, and the state
//! root chosen from `--state-dir`, then `GATE_STATE_DIR`.

use serde_json::{Value, json};

use crate::support::{Case, finish, print_run, shown};

fn assert_root_hash(request: &Value) {
    let hash = request["rootHash"].as_str().expect("a non-default root has a hash");
    assert_eq!(hash.len(), 8, "{hash}");
    assert!(
        hash.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')),
        "{hash} is lower-case hex"
    );
}

#[test]
fn print_request_shows_the_new_cli_request() {
    let case = Case::new("print_request_shows_the_new_cli_request");
    let outcome = case.run(&print_run(&["cmd", "/c", "echo hi"]));
    let request = outcome.request();

    assert_root_hash(&request);
    let expected = json!({
        "mode": "run",
        "slot": "heavy",
        "timeout": 5,
        "log": shown(&case.folder().join("out.log")),
        "stall": 120,
        "tail": 20,
        "noMarkers": false,
        "local": false,
        "stateDir": shown(&case.root()),
        "defaultRoot": false,
        "rootHash": request["rootHash"],
        "command": ["cmd", "/c", "echo hi"],
        "testSeams": {
            "sampleMs": null,
            "heartbeatMs": null,
            "pollMs": null,
            "stallMs": null,
            "interruptAfterSamples": null,
            "interruptQueued": false,
            "samplerFail": false,
            "diagnosticsFail": false,
            "remoteClient": null,
            "remoteWaitMs": null,
            "gitLimitMs": null,
            "serversUnder": null,
            "remoteSnapshot": false,
            "remoteTransport": null,
            "remoteSsh": null,
            "remoteRetryPauseMs": null,
        },
    });
    assert_eq!(outcome.stdout, format!("{expected}\n"), "the keys come in the published order");

    let mut seams = print_run(&["cmd"]);
    seams.splice(
        7..7,
        [
            "--test-remote-client",
            r"steps\client.txt",
            "--test-remote-wait-ms",
            "5",
            "--test-git-limit-ms",
            "7",
            "--test-servers-under",
            r"target\case",
            "--test-remote-snapshot",
        ],
    );
    let request = case.run(&seams).request();
    assert_eq!(
        (
            &request["testSeams"]["remoteClient"],
            &request["testSeams"]["remoteWaitMs"],
            &request["testSeams"]["gitLimitMs"],
            &request["testSeams"]["serversUnder"],
            &request["testSeams"]["remoteSnapshot"],
        ),
        (&json!(r"steps\client.txt"), &json!(5), &json!(7), &json!(r"target\case"), &json!(true))
    );
    // The snapshot-only client and the Rust client's transport are refused together, so the transport's seams are
    // printed by a run of their own.
    let mut seams = print_run(&["cmd"]);
    seams.splice(
        7..7,
        [
            "--test-remote-transport",
            r"local:target\builder",
            "--test-remote-ssh",
            "fake-ssh.exe",
            "--test-remote-retry-pause-ms",
            "9",
        ],
    );
    let request = case.run(&seams).request();
    assert_eq!(
        (
            &request["testSeams"]["remoteTransport"],
            &request["testSeams"]["remoteSsh"],
            &request["testSeams"]["remoteRetryPauseMs"],
        ),
        (&json!(r"local:target\builder"), &json!("fake-ssh.exe"), &json!(9))
    );

    let stop = case.run(&["stop-tree", "--test-print-request", "123"]).request();
    assert_eq!(
        stop,
        json!({
            "mode": "stop-tree",
            "pid": 123,
            "stateDir": shown(&case.root()),
            "defaultRoot": false,
            "rootHash": request["rootHash"],
        })
    );
}

#[test]
fn default_root_is_recognised_through_dots_and_doubled_separators() {
    let case = Case::new("default_root_through_dots");
    let mut command = case.gate_without_state_dir(&["stop-tree", "--test-print-request", "1"]);
    let local_app_data = format!(r"{}\appdata\..\appdata\\local", case.root().display());
    command.env("LOCALAPPDATA", &local_app_data);

    let request = finish(command).request();

    assert_eq!(
        request["stateDir"],
        shown(&case.root().join("appdata").join("local").join("gate").join("v1"))
    );
    assert_eq!(request["defaultRoot"], true, "LOCALAPPDATA {local_app_data}");
    assert_eq!(request["rootHash"], Value::Null);
}

#[test]
fn state_dir_flag_beats_env() {
    let case = Case::new("state_dir_flag_beats_env");
    let other = Case::new("state_dir_flag_beats_env_other");
    let mut command = case.gate(&print_run(&["cmd"]));
    command.env("GATE_STATE_DIR", other.root());

    let request = finish(command).request();

    assert_eq!(request["stateDir"], shown(&case.root()));
}

#[test]
fn state_dir_env_used_without_flag() {
    let case = Case::new("state_dir_env_used_without_flag");
    let mut command = case.gate_without_state_dir(&print_run(&["cmd"]));
    command.env("GATE_STATE_DIR", case.root());

    let request = finish(command).request();

    assert_eq!(request["stateDir"], shown(&case.root()));
    assert_eq!(request["defaultRoot"], false);
    assert_root_hash(&request);
}
