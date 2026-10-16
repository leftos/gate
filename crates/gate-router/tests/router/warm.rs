//! The builder warm-up: `.gate/warmup.toml` read, its hash, the judge's whole truth table and the lines.

use std::time::{Duration, SystemTime};

use gate_router::warm::{
    FailureClass, FileRead, Judgement, Record, RecordRead, RecordState, Situation, WarmupFile, failed_line, file_problem_line, judge, read_warmup,
    record_problem_line, starting_line, warming_line, warmup_hash,
};

fn problems(text: &str) -> Vec<String> {
    read_warmup(text.as_bytes()).expect_err("the file has a problem")
}

fn words(texts: &[&str]) -> Vec<String> {
    texts.iter().map(|text| (*text).to_owned()).collect()
}

#[test]
fn a_file_of_commands_reads_with_the_default_timeout() {
    let file = read_warmup(b"commands = [['dotnet', 'build', 'yaat.slnx'], ['pwsh', '-File', 'tools/restore.ps1']]\n").expect("reads");

    assert_eq!(
        file,
        WarmupFile {
            commands: vec![words(&["dotnet", "build", "yaat.slnx"]), words(&["pwsh", "-File", "tools/restore.ps1"])],
            timeout_seconds: 1800,
        }
    );
}

#[test]
fn a_timeout_from_one_to_86400_seconds_is_kept() {
    for seconds in [1, 600, 86_400] {
        let file = read_warmup(format!("commands = [['a']]\ntimeout = {seconds}\n").as_bytes()).expect("reads");
        assert_eq!(file.timeout_seconds, seconds);
    }
}

#[test]
fn a_timeout_out_of_range_or_not_whole_seconds_is_a_problem() {
    for value in ["0", "-5", "86401", "1.5", "'600'", "true", "[600]"] {
        assert_eq!(
            problems(&format!("commands = [['a']]\ntimeout = {value}\n")),
            ["timeout is not a whole number of seconds from 1 to 86400"],
            "{value}"
        );
    }
}

#[test]
fn a_file_without_commands_declares_none() {
    for text in ["", "# nothing\n", "timeout = 60\n", "commands = []\n"] {
        assert_eq!(problems(text), ["declares no commands"], "{text:?}");
    }
}

#[test]
fn commands_that_is_not_an_array_is_a_problem() {
    assert_eq!(problems("commands = 'dotnet build'\n"), ["commands is not an array (TOML string)"]);
    assert_eq!(problems("[commands]\na = 1\n"), ["commands is not an array (TOML table)"]);
}

#[test]
fn an_empty_command_or_a_bad_word_is_a_problem_naming_it() {
    assert_eq!(
        problems("commands = [['a'], [], ['b', 3, ''], 'c']\n"),
        [
            "commands[1] is empty",
            "commands[2][1] is not a string (TOML integer)",
            "commands[2][2] is empty",
            "commands[3] is not an array (TOML string)",
        ]
    );
}

#[test]
fn a_ninth_command_is_a_problem() {
    let commands = |count: usize| (0..count).map(|index| format!("['c{index}']")).collect::<Vec<_>>().join(", ");
    let eight = read_warmup(format!("commands = [{}]\n", commands(8)).as_bytes()).expect("eight commands read");
    assert_eq!(eight.commands.len(), 8);

    assert_eq!(
        problems(&format!("commands = [{}, [3]]\n", commands(8))),
        ["has more than 8 commands"],
        "nothing past the ninth is read"
    );
}

#[test]
fn any_other_key_is_a_problem() {
    assert_eq!(
        problems("commands = [['a']]\nshell = 'pwsh'\n\"a b\" = 1\n"),
        ["shell is not a key (commands, timeout)", "\"a b\" is not a key (commands, timeout)"]
    );
}

#[test]
fn a_byte_order_mark_is_skipped() {
    let file = read_warmup(b"\xef\xbb\xbfcommands = [['a']]\n").expect("reads");

    assert_eq!(file.commands, [words(&["a"])]);
}

#[test]
fn bytes_that_are_not_utf8_or_not_toml_are_one_problem() {
    let why = read_warmup(b"commands = [['\xff']]\n").expect_err("no UTF-8");
    assert_eq!(why.len(), 1);
    assert!(why[0].starts_with("not valid UTF-8 ("), "{why:?}");

    let why = read_warmup(b"commands = [['a']\n").expect_err("no TOML");
    assert_eq!(why.len(), 1);
    assert!(why[0].starts_with("line "), "{why:?}");
}

#[test]
fn the_hash_is_the_lower_case_hex_sha256_of_the_raw_bytes() {
    assert_eq!(warmup_hash(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    assert_ne!(warmup_hash(b"\xef\xbb\xbfabc"), warmup_hash(b"abc"), "the byte order mark is hashed");
}

const HASH: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
const HOST: &str = "builder";
const GATE: &str = "0.1.0+abc1234";

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_hours(500_000)
}

fn parsed_file() -> FileRead {
    FileRead::Parsed {
        file: WarmupFile {
            commands: vec![words(&["dotnet", "build"])],
            timeout_seconds: 1800,
        },
        hash: HASH.to_owned(),
    }
}

/// A record matching [`HOST`], [`HASH`] and [`GATE`] in `state`, written `ago` before [`now`].
fn record(state: RecordState, ago: Duration) -> RecordRead {
    RecordRead::Parsed(Record {
        host: HOST.to_owned(),
        state,
        at: now() - ago,
        hash: HASH.to_owned(),
        gate: GATE.to_owned(),
    })
}

fn failed(class: FailureClass) -> RecordState {
    RecordState::Failed {
        class,
        reason: "command 1 exited with status 1".to_owned(),
    }
}

/// The judgement for a consented repo with the parsed file, `record` and the lock `lock_held`.
fn judged(record: &RecordRead, lock_held: bool) -> Judgement {
    judge(&Situation {
        consented: true,
        file: &parsed_file(),
        record,
        lock_held,
        host: HOST,
        gate: GATE,
        now: now(),
    })
}

#[test]
fn no_consent_routes_whatever_the_file_and_record_say() {
    let unreadable = RecordRead::Unreadable {
        path: "r.json".to_owned(),
        error: "bad".to_owned(),
    };
    for file in [FileRead::Absent, FileRead::Problem("x".to_owned()), parsed_file()] {
        for record in [RecordRead::Absent, unreadable.clone(), record(RecordState::Warming, Duration::ZERO)] {
            let judgement = judge(&Situation {
                consented: false,
                file: &file,
                record: &record,
                lock_held: true,
                host: HOST,
                gate: GATE,
                now: now(),
            });
            assert_eq!(judgement, Judgement::Route, "{file:?} {record:?}");
        }
    }
}

#[test]
fn no_file_routes_whatever_the_record_says() {
    let judgement = judge(&Situation {
        consented: true,
        file: &FileRead::Absent,
        record: &record(failed(FailureClass::Commands), Duration::ZERO),
        lock_held: false,
        host: HOST,
        gate: GATE,
        now: now(),
    });

    assert_eq!(judgement, Judgement::Route);
}

#[test]
fn a_file_problem_runs_here_saying_so() {
    let judgement = judge(&Situation {
        consented: true,
        file: &FileRead::Problem("commands[1] is empty".to_owned()),
        record: &record(RecordState::Warm, Duration::ZERO),
        lock_held: false,
        host: HOST,
        gate: GATE,
        now: now(),
    });

    assert_eq!(
        judgement,
        Judgement::Problem("gate: not routed: .gate/warmup.toml: commands[1] is empty".to_owned())
    );
}

#[test]
fn an_unreadable_record_runs_here_saying_so() {
    let unreadable = RecordRead::Unreadable {
        path: r"C:\state\remote\warm\app-1a2b3c4d.json".to_owned(),
        error: "missing key \"v\"".to_owned(),
    };

    assert_eq!(
        judged(&unreadable, false),
        Judgement::Problem(
            r#"gate: not routed: the warm-up record C:\state\remote\warm\app-1a2b3c4d.json cannot be read: missing key "v""#.to_owned()
        )
    );
}

#[test]
fn no_record_starts_a_warm_up() {
    assert_eq!(judged(&RecordRead::Absent, false), Judgement::Start);
}

#[test]
fn a_matching_warm_record_routes() {
    assert_eq!(judged(&record(RecordState::Warm, Duration::from_hours(24)), false), Judgement::Ready);
}

#[test]
fn a_warm_record_of_another_host_or_hash_starts_a_warm_up_and_one_of_another_build_routes() {
    let [other_host, other_hash, other_build] = voided(RecordState::Warm);
    assert_eq!(judged(&other_host, false), Judgement::Start, "another host");
    assert_eq!(judged(&other_hash, false), Judgement::Start, "another warm-up file hash");
    assert_eq!(
        judged(&other_build, false),
        Judgement::Ready,
        "the gate build is compared only for a failed record"
    );

    for state in [failed(FailureClass::Commands), failed(FailureClass::Refused)] {
        let [_, _, another_build] = voided(state);
        assert_eq!(judged(&another_build, false), Judgement::Start, "a failed record of another build");
    }
}

#[test]
fn a_warming_record_with_the_lock_held_runs_here_and_starts_nothing() {
    assert_eq!(
        judged(&record(RecordState::Warming, Duration::ZERO), true),
        Judgement::Warming { host: HOST.to_owned() }
    );
}

#[test]
fn a_warming_record_with_the_lock_free_starts_a_warm_up() {
    assert_eq!(judged(&record(RecordState::Warming, Duration::ZERO), false), Judgement::Start);
}

#[test]
fn a_held_lock_with_no_record_is_warming() {
    assert_eq!(judged(&RecordRead::Absent, true), Judgement::Warming { host: HOST.to_owned() });
}

#[test]
fn a_held_lock_with_an_unreadable_record_is_warming() {
    let unreadable = RecordRead::Unreadable {
        path: r"C:\state\remote\warm\app-1a2b3c4d.json".to_owned(),
        error: "missing key \"v\"".to_owned(),
    };

    assert_eq!(
        judged(&unreadable, true),
        Judgement::Warming { host: HOST.to_owned() },
        "there is no record to name a host with, so the current one"
    );
}

#[test]
fn a_held_lock_with_a_record_of_another_host_names_that_host() {
    let [other_host, _, _] = voided(RecordState::Warm);

    assert_eq!(
        judged(&other_host, true),
        Judgement::Warming {
            host: "other-builder".to_owned()
        }
    );
}

#[test]
fn a_held_lock_with_a_record_of_another_hash_is_warming() {
    let [_, other_hash, _] = voided(RecordState::Warm);
    assert_eq!(judged(&other_hash, true), Judgement::Warming { host: HOST.to_owned() });
}

#[test]
fn a_held_lock_with_a_warm_record_is_warming() {
    assert_eq!(
        judged(&record(RecordState::Warm, Duration::from_hours(24)), true),
        Judgement::Warming { host: HOST.to_owned() }
    );
}

#[test]
fn a_held_lock_with_a_failed_record_is_warming() {
    assert_eq!(
        judged(&record(failed(FailureClass::Commands), Duration::from_mins(1)), true),
        Judgement::Warming { host: HOST.to_owned() }
    );
}

#[test]
fn a_warming_record_of_another_host_hash_or_build_is_warming_while_the_lock_is_held() {
    let records = voided(RecordState::Warming);
    let hosts = ["other-builder", HOST, HOST];

    for (record, host) in records.iter().zip(hosts) {
        assert_eq!(judged(record, true), Judgement::Warming { host: host.to_owned() }, "{record:?}");
        assert_eq!(judged(record, false), Judgement::Start, "the lock is free: {record:?}");
    }
}

#[test]
fn a_commands_failure_runs_here_for_thirty_minutes() {
    let reason = "command 1 exited with status 1".to_owned();
    for (ago, minutes_ago) in [(0, 0), (59, 0), (60, 1), (29 * 60 + 59, 29)] {
        assert_eq!(
            judged(&record(failed(FailureClass::Commands), Duration::from_secs(ago)), false),
            Judgement::Failed {
                minutes_ago,
                reason: reason.clone()
            },
            "{ago} s ago"
        );
    }
    assert_eq!(
        judged(&record(failed(FailureClass::Commands), Duration::from_mins(30)), false),
        Judgement::Start,
        "the backoff has passed"
    );
}

#[test]
fn a_refused_failure_runs_here_for_sixty_seconds() {
    assert_eq!(
        judged(&record(failed(FailureClass::Refused), Duration::from_secs(59)), false),
        Judgement::Failed {
            minutes_ago: 0,
            reason: "command 1 exited with status 1".to_owned()
        }
    );
    assert_eq!(
        judged(&record(failed(FailureClass::Refused), Duration::from_secs(60)), false),
        Judgement::Start,
        "the backoff has passed"
    );
}

#[test]
fn a_failure_written_in_the_future_starts_a_warm_up() {
    let mut future = record(failed(FailureClass::Refused), Duration::ZERO);
    if let RecordRead::Parsed(record) = &mut future {
        record.at = now() + Duration::from_secs(3600);
    }

    assert_eq!(judged(&future, false), Judgement::Start, "a backoff never outlives its cause");
}

#[test]
fn another_host_hash_or_build_voids_a_commands_backoff() {
    for record in voided(failed(FailureClass::Commands)) {
        assert_eq!(judged(&record, false), Judgement::Start, "{record:?}");
    }
}

#[test]
fn another_host_hash_or_build_voids_a_refused_backoff() {
    for record in voided(failed(FailureClass::Refused)) {
        assert_eq!(judged(&record, false), Judgement::Start, "{record:?}");
    }
}

/// Three records in `state` written just now, each differing from the situation in one of host, hash and build.
fn voided(state: RecordState) -> [RecordRead; 3] {
    let base = Record {
        host: HOST.to_owned(),
        state,
        at: now(),
        hash: HASH.to_owned(),
        gate: GATE.to_owned(),
    };
    [
        RecordRead::Parsed(Record {
            host: "other-builder".to_owned(),
            ..base.clone()
        }),
        RecordRead::Parsed(Record {
            hash: "0".repeat(64),
            ..base.clone()
        }),
        RecordRead::Parsed(Record {
            gate: "0.1.0+def5678".to_owned(),
            ..base
        }),
    ]
}

#[test]
fn the_lines_name_the_host_the_age_and_the_reason_escaped() {
    assert_eq!(
        warming_line("builder"),
        "gate: not routed: builder is warming the work folder for this worktree"
    );
    assert_eq!(
        starting_line("builder"),
        "gate: not routed: starting a warm-up of the work folder for this worktree on builder"
    );
    assert_eq!(
        failed_line(0, "the builder is busy"),
        "gate: not routed: the warm-up failed less than a minute ago: the builder is busy"
    );
    assert_eq!(
        failed_line(12, "command 2 timed out"),
        "gate: not routed: the warm-up failed 12 min ago: command 2 timed out"
    );
    assert_eq!(file_problem_line("a\nb"), r"gate: not routed: .gate/warmup.toml: a\nb");
    assert_eq!(
        record_problem_line("r\u{202e}.json", "bad\u{1b}"),
        r"gate: not routed: the warm-up record r\u{202e}.json cannot be read: bad\u{1b}"
    );
    assert_eq!(
        failed_line(1, "x\r\ngate: forged"),
        r"gate: not routed: the warm-up failed 1 min ago: x\r\ngate: forged"
    );
    assert_eq!(
        warming_line("b\u{1b}"),
        r"gate: not routed: b\u{1b} is warming the work folder for this worktree"
    );
}
