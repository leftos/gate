//! Pool counts from the processor count and the counts file, writing the file, and refusing request values.

use std::fs;

use gate_state::{ParseError, SlotCount, SlotCounts, SlotKind, SlotState, apply_slot_count, read_slot_count_setting, write_slot_counts};
use serde_json::{Value, json};

use crate::support::{Machine, pool};

fn count(value: u32) -> SlotCount {
    SlotCount::new(value).expect("a test count is 1 to 64")
}

#[test]
fn idle_defaults_with_16_processors() {
    let state = Machine::new("idle_defaults_with_16_processors").state();

    assert_eq!(state.now, "2026-10-01T12:00:00.1234567Z");
    assert_eq!(state.machine, "TESTBOX");
    assert_eq!(state.processors, 16);
    let kinds: Vec<SlotKind> = state.pools.iter().map(|pool| pool.kind).collect();
    assert_eq!(kinds, [SlotKind::Heavy, SlotKind::Light]);
    for (kind, expected) in [(SlotKind::Heavy, 3), (SlotKind::Light, 7)] {
        let pool = pool(&state, kind);
        assert_eq!((pool.count, pool.default_count), (expected, expected), "{kind:?}");
        assert_eq!(pool.set, None, "{kind:?}");
        assert_eq!(pool.problem, None, "{kind:?}");
        assert!(pool.waiting.is_empty(), "{kind:?}");
        let indexes: Vec<u32> = pool.slots.iter().map(|slot| slot.index).collect();
        assert_eq!(indexes, (0..expected).collect::<Vec<_>>(), "{kind:?}");
        for slot in &pool.slots {
            assert_eq!(slot.state, SlotState::Free, "{}", slot.name);
            assert_eq!(slot.name, format!("{}{}", kind.prefix(), slot.index));
            assert_eq!(slot.extra, Some(false), "{}", slot.name);
        }
    }
}

#[test]
fn counts_file_sets_count() {
    let machine = Machine::new("counts_file_sets_count");
    machine.write_counts(r#"{"heavy":4}"#);
    let state = machine.state();

    let heavy = pool(&state, SlotKind::Heavy);
    assert_eq!((heavy.count, heavy.default_count, heavy.set), (4, 3, Some(count(4))));
    assert_eq!(heavy.slots.len(), 4);
    assert!(heavy.slots.iter().all(|slot| slot.extra == Some(false)));
    let light = pool(&state, SlotKind::Light);
    assert_eq!(
        (light.count, light.default_count, light.set, light.problem.as_deref()),
        (7, 7, None, None)
    );
}

#[test]
fn counts_file_not_json_is_ignored_with_problem() {
    let machine = Machine::new("counts_file_not_json_is_ignored_with_problem");
    machine.write_counts("{heavy: 4");
    let state = machine.state();

    let prefix = format!("gate: ignored {}: it is not valid JSON: ", machine.counts_path().display());
    for (kind, suffix) in [(SlotKind::Heavy, "; using 3 heavy slots"), (SlotKind::Light, "; using 7 light slots")] {
        let pool = pool(&state, kind);
        let problem = pool.problem.as_deref().expect("an invalid counts file is a problem");
        assert!(problem.starts_with(&prefix), "{problem}");
        assert!(problem.ends_with(suffix), "{problem}");
        assert!(problem.len() > prefix.len() + suffix.len(), "the problem quotes the parser: {problem}");
        assert_eq!((pool.count, pool.set), (pool.default_count, None));
    }
}

#[test]
fn counts_file_not_object_is_ignored_with_problem() {
    let machine = Machine::new("counts_file_not_object_is_ignored_with_problem");
    machine.write_counts("[4]");
    let state = machine.state();

    let heavy = pool(&state, SlotKind::Heavy);
    let expected = format!(
        "gate: ignored {}: it is not a JSON object; using 3 heavy slots",
        machine.counts_path().display()
    );
    assert_eq!(heavy.problem.as_deref(), Some(expected.as_str()));
    assert_eq!((heavy.count, heavy.set), (3, None));
}

#[test]
fn counts_file_empty_is_not_an_object() {
    let machine = Machine::new("counts_file_empty_is_not_an_object");
    let expected = format!(
        "gate: ignored {}: it is not a JSON object; using 3 heavy slots",
        machine.counts_path().display()
    );
    for text in ["", " \r\n\t"] {
        machine.write_counts(text);
        let state = machine.state();

        let heavy = pool(&state, SlotKind::Heavy);
        assert_eq!(heavy.problem.as_deref(), Some(expected.as_str()), "{text:?}");
        assert_eq!((heavy.count, heavy.set), (3, None), "{text:?}");
    }
}

#[test]
fn counts_file_unreadable_is_ignored_with_problem() {
    let machine = Machine::new("counts_file_unreadable_is_ignored_with_problem");
    fs::create_dir_all(machine.counts_path()).expect("a folder can stand where the counts file goes");
    let state = machine.state();

    let prefix = format!("gate: ignored {}: it could not be read: ", machine.counts_path().display());
    for (kind, suffix) in [(SlotKind::Heavy, "; using 3 heavy slots"), (SlotKind::Light, "; using 7 light slots")] {
        let pool = pool(&state, kind);
        let problem = pool.problem.as_deref().expect("an unreadable counts file is a problem");
        assert!(problem.starts_with(&prefix), "{problem}");
        assert!(problem.ends_with(suffix), "{problem}");
        assert!(problem.len() > prefix.len() + suffix.len(), "the problem quotes the error: {problem}");
        assert_eq!((pool.count, pool.set), (pool.default_count, None));
    }
}

#[test]
fn counts_value_out_of_range_is_ignored_with_problem() {
    let machine = Machine::new("counts_value_out_of_range_is_ignored_with_problem");
    machine.write_counts(r#"{"light":65}"#);
    let state = machine.state();

    let light = pool(&state, SlotKind::Light);
    let expected = format!(
        "gate: ignored {}: light must be a whole number 1 to 64, got '65'; using 7 light slots",
        machine.counts_path().display()
    );
    assert_eq!(light.problem.as_deref(), Some(expected.as_str()));
    assert_eq!((light.count, light.set), (7, None));
    let heavy = pool(&state, SlotKind::Heavy);
    assert_eq!((heavy.count, heavy.problem.as_deref()), (3, None));
}

#[test]
fn write_then_read_round_trips_and_empty_deletes() {
    let machine = Machine::new("write_then_read_round_trips_and_empty_deletes");
    let path = machine.dir.join("fresh").join("slot-counts.json");
    let counts = SlotCounts {
        heavy: Some(count(4)),
        light: Some(count(2)),
    };

    write_slot_counts(&path, &counts).expect("the counts file can be written");

    let bytes = fs::read(&path).expect("the counts file exists");
    assert_eq!(bytes, br#"{"heavy":4,"light":2}"#);
    assert_eq!(read_slot_count_setting(&path), counts);
    let leftovers: Vec<_> = fs::read_dir(path.parent().expect("the path has a folder"))
        .expect("the folder exists")
        .flatten()
        .map(|entry| entry.file_name())
        .collect();
    assert_eq!(leftovers, ["slot-counts.json"], "no temp file is left beside the counts");

    write_slot_counts(&path, &SlotCounts::default()).expect("empty counts delete the file");
    assert!(!path.exists());
    write_slot_counts(&path, &SlotCounts::default()).expect("empty counts with no file are no error");
    assert_eq!(read_slot_count_setting(&path), SlotCounts::default());
}

#[test]
fn apply_slot_count_keeps_other_kind_and_drops_invalid_values() {
    let machine = Machine::new("apply_slot_count_keeps_other_kind_and_drops_invalid_values");
    let path = machine.counts_path();
    let read = || fs::read_to_string(&path).expect("the counts file exists");

    machine.write_counts(r#"{"heavy":4,"light":65,"other":1}"#);
    apply_slot_count(&path, SlotKind::Light, Some(count(2))).expect("a light count can be set");
    assert_eq!(read(), r#"{"heavy":4,"light":2}"#);

    apply_slot_count(&path, SlotKind::Heavy, None).expect("the heavy count can be unset");
    assert_eq!(read(), r#"{"light":2}"#);

    machine.write_counts(r#"{"heavy":"4","light":3}"#);
    apply_slot_count(&path, SlotKind::Light, Some(count(5))).expect("a light count can be set");
    assert_eq!(read(), r#"{"light":5}"#, "the string heavy value is dropped");

    apply_slot_count(&path, SlotKind::Light, None).expect("the last count can be unset");
    assert!(!path.exists(), "a file with no count left is deleted");
}

#[test]
fn slot_count_parse_messages() {
    for (value, shown) in [(json!(0), "'0'"), (json!(65), "'65'"), (json!(2.5), "'2.5'"), (json!("x"), "'x'")] {
        let error = SlotCount::from_request(&value).expect_err("the count is refused");
        assert_eq!(error.to_string(), format!("count must be a whole number 1 to 64 or null, got {shown}"));
    }
    for (value, expected) in [(json!(1), 1), (json!(64), 64), (json!(3.0), 3)] {
        assert_eq!(SlotCount::from_request(&value), Ok(Some(count(expected))), "{value}");
    }
    assert_eq!(SlotCount::from_request(&Value::Null), Ok(None));

    assert_eq!(SlotKind::from_request(&json!("LIGHT")), Ok(SlotKind::Light));
    assert_eq!(SlotKind::from_request(&json!("heavy")), Ok(SlotKind::Heavy));
    for (value, shown) in [(json!("medium"), "'medium'"), (Value::Null, "''")] {
        let error = SlotKind::from_request(&value).expect_err("the kind is refused");
        assert_eq!(error, ParseError::Kind(shown.to_owned()));
        assert_eq!(error.to_string(), format!("kind must be heavy or light, got {shown}"));
    }
}
