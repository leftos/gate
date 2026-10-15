use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use gate_win32::{PipeRead, PipeWrite};

use super::{DecodeError, FRAME_LIMIT, FramePipe, Message, ReadError, Spawn, WriteError, decode, encode, read_message, write_message};

/// A deadline no test reaches.
fn far() -> Instant {
    Instant::now() + Duration::from_secs(3600)
}

/// An in-memory pipe: reads take the queued chunks in order, as much of each as the buffer holds, and a read past the
/// last is the other end closed; writes collect into one buffer. Every deadline a read was given is kept.
#[derive(Default)]
struct Memory {
    incoming: RefCell<VecDeque<Vec<u8>>>,
    deadlines: RefCell<Vec<Instant>>,
    written: RefCell<Vec<u8>>,
}

impl Memory {
    /// A pipe whose reads give `chunks`, in order.
    fn of(chunks: impl IntoIterator<Item = Vec<u8>>) -> Memory {
        Memory {
            incoming: RefCell::new(chunks.into_iter().collect()),
            ..Memory::default()
        }
    }

    /// The count of reads made.
    fn reads(&self) -> usize {
        self.deadlines.borrow().len()
    }
}

impl FramePipe for Memory {
    fn read(&self, buf: &mut [u8], deadline: Instant) -> io::Result<PipeRead> {
        self.deadlines.borrow_mut().push(deadline);
        let Some(mut chunk) = self.incoming.borrow_mut().pop_front() else {
            return Ok(PipeRead::Closed);
        };
        let count = chunk.len().min(buf.len());
        buf[..count].copy_from_slice(&chunk[..count]);
        let rest = chunk.split_off(count);
        if !rest.is_empty() {
            self.incoming.borrow_mut().push_front(rest);
        }
        Ok(PipeRead::Read(count))
    }

    fn write_all(&self, bytes: &[u8], _deadline: Instant) -> io::Result<PipeWrite> {
        self.written.borrow_mut().extend_from_slice(bytes);
        Ok(PipeWrite::Written)
    }
}

/// `body` as one frame: its length, then it.
fn frame(body: &[u8]) -> Vec<u8> {
    let length = u32::try_from(body.len()).expect("a test body fits a frame");
    let mut frame = length.to_le_bytes().to_vec();
    frame.extend_from_slice(body);
    frame
}

/// The message the JSON `json` decodes to, or why not.
fn decoded(json: &str) -> Result<Message, DecodeError> {
    decode(json.as_bytes())
}

/// `text` as UTF-16.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// A `Spawn` with every field set.
fn spawn() -> Spawn {
    Spawn {
        application: wide(r"C:\Windows\System32\cmd.exe"),
        command_line: wide("cmd /d /c echo é"),
        // An unpaired surrogate travels as it is.
        directory: vec![u16::from(b'C'), u16::from(b':'), 0xD800],
        environment: wide("A=1\0B=2\0\0"),
        stdout: 0x1F4,
        stderr: 0x7FFF_FFFC,
    }
}

/// `units` as the wire carries them.
fn b64(units: &[u16]) -> String {
    STANDARD.encode(units.iter().flat_map(|unit| unit.to_le_bytes()).collect::<Vec<u8>>())
}

/// A spawn's JSON with `field` replaced by the raw JSON `value`, or removed when `value` is `None`.
fn spawn_json_with(field: &str, value: Option<&str>) -> String {
    let good = spawn();
    let fields = [
        ("application", format!("\"{}\"", b64(&good.application))),
        ("command_line", format!("\"{}\"", b64(&good.command_line))),
        ("directory", format!("\"{}\"", b64(&wide(r"C:\")))),
        ("environment", format!("\"{}\"", b64(&good.environment))),
        ("stdout", "4".to_owned()),
        ("stderr", "8".to_owned()),
    ];
    let mut parts = vec!["\"type\":\"spawn\"".to_owned()];
    for (name, json) in fields {
        match (name == field, value) {
            (false, _) => parts.push(format!("\"{name}\":{json}")),
            (true, Some(value)) => parts.push(format!("\"{name}\":{value}")),
            (true, None) => {}
        }
    }
    format!("{{{}}}", parts.join(","))
}

#[test]
fn every_message_round_trips_through_a_frame() {
    let messages = [
        Message::Hello {
            version: "0.1.0+dev".to_owned(),
            desktop: "Default".to_owned(),
            ok: true,
        },
        Message::Hello {
            version: "0.1.0+abc".to_owned(),
            desktop: String::new(),
            ok: false,
        },
        Message::Spawn(spawn()),
        Message::Child {
            pid: u32::MAX,
            process: u64::MAX,
            thread: 4,
        },
        Message::Failed { code: 2 },
        Message::Done,
    ];

    for message in messages {
        let pipe = Memory::default();
        write_message(&pipe, &message, far()).expect("the message is written");
        let reader = Memory::of([pipe.written.take()]);

        let read = read_message(&reader, far()).expect("the message is read back");

        assert_eq!(read, message);
    }
}

#[test]
fn a_spawn_s_strings_travel_as_base64_of_utf16le() {
    let body = encode(&Message::Spawn(spawn()));
    let json: serde_json::Value = serde_json::from_slice(&body).expect("the body is JSON");

    assert_eq!(json["type"], "spawn");
    assert_eq!(json["application"], b64(&spawn().application));
    assert_eq!(json["directory"], "QwA6AADY", "C, :, then the unpaired surrogate 0xD800, little-endian");
}

#[test]
fn a_body_that_is_not_one_json_object_is_refused() {
    for body in [&b"[]"[..], b"\"done\"", b"{", b"\xff\xfe", b""] {
        assert_eq!(decode(body), Err(DecodeError::NotAnObject), "{body:?}");
    }
}

#[test]
fn a_missing_or_unknown_type_is_refused() {
    assert_eq!(decoded("{}"), Err(DecodeError::MissingType));
    assert_eq!(decoded(r#"{"code":2}"#), Err(DecodeError::MissingType));
    assert_eq!(decoded(r#"{"type":"Done"}"#), Err(DecodeError::UnknownType));
    assert_eq!(decoded(r#"{"type":"exit"}"#), Err(DecodeError::UnknownType));
    assert_eq!(decoded(r#"{"type":1}"#), Err(DecodeError::WrongType("type")));
    assert_eq!(decoded(r#"{"type":null}"#), Err(DecodeError::WrongType("type")));
}

#[test]
fn a_missing_field_is_refused_by_name() {
    assert_eq!(
        decoded(r#"{"type":"hello","version":"v","ok":true}"#),
        Err(DecodeError::Missing("desktop"))
    );
    assert_eq!(decoded(r#"{"type":"child","pid":1,"process":4}"#), Err(DecodeError::Missing("thread")));
    assert_eq!(decoded(r#"{"type":"failed"}"#), Err(DecodeError::Missing("code")));
    for field in ["application", "command_line", "directory", "environment", "stdout", "stderr"] {
        assert_eq!(decoded(&spawn_json_with(field, None)), Err(DecodeError::Missing(field)), "{field}");
    }
}

#[test]
fn an_extra_field_is_refused() {
    assert_eq!(decoded(r#"{"type":"done","code":0}"#), Err(DecodeError::UnexpectedField));
    assert_eq!(decoded(r#"{"type":"failed","code":2,"why":"x"}"#), Err(DecodeError::UnexpectedField));
    let spawn = spawn_json_with("stderr", Some(r#"8,"token":4"#));
    assert_eq!(decoded(&spawn), Err(DecodeError::UnexpectedField));
}

#[test]
fn a_field_of_the_wrong_json_type_is_refused_by_name() {
    assert_eq!(
        decoded(r#"{"type":"hello","version":1,"desktop":"D","ok":true}"#),
        Err(DecodeError::WrongType("version"))
    );
    assert_eq!(
        decoded(r#"{"type":"hello","version":"v","desktop":"D","ok":"true"}"#),
        Err(DecodeError::WrongType("ok"))
    );
    assert_eq!(
        decoded(r#"{"type":"child","pid":"1","process":4,"thread":8}"#),
        Err(DecodeError::WrongType("pid"))
    );
    assert_eq!(decoded(&spawn_json_with("stdout", Some("[4]"))), Err(DecodeError::WrongType("stdout")));
    assert_eq!(
        decoded(&spawn_json_with("environment", Some("null"))),
        Err(DecodeError::WrongType("environment"))
    );
}

#[test]
fn a_number_out_of_range_is_refused_by_name() {
    assert_eq!(decoded(r#"{"type":"failed","code":4294967296}"#), Err(DecodeError::OutOfRange("code")));
    assert_eq!(decoded(r#"{"type":"failed","code":-1}"#), Err(DecodeError::OutOfRange("code")));
    assert_eq!(decoded(r#"{"type":"failed","code":2.5}"#), Err(DecodeError::OutOfRange("code")));
    assert_eq!(
        decoded(r#"{"type":"child","pid":1,"process":4,"thread":18446744073709551616}"#),
        Err(DecodeError::OutOfRange("thread"))
    );
    assert_eq!(decoded(&spawn_json_with("stderr", Some("-8"))), Err(DecodeError::OutOfRange("stderr")));
}

#[test]
fn a_spawn_string_that_is_not_padded_standard_base64_is_refused_by_name() {
    for bad in ["\"!!!!\"", "\"QwA\"", "\"Qw-_\"", "\"QwA6AA==x\""] {
        assert_eq!(
            decoded(&spawn_json_with("command_line", Some(bad))),
            Err(DecodeError::BadBase64("command_line")),
            "{bad}"
        );
    }
}

#[test]
fn a_spawn_string_of_an_odd_byte_count_is_refused_by_name() {
    let odd = format!("\"{}\"", STANDARD.encode([0x41_u8, 0x00, 0x42]));

    assert_eq!(
        decoded(&spawn_json_with("environment", Some(&odd))),
        Err(DecodeError::OddLength("environment"))
    );
}

#[test]
fn a_nul_in_a_spawn_path_or_command_line_is_refused_by_name() {
    for field in ["application", "command_line", "directory"] {
        let with_nul = format!("\"{}\"", b64(&wide("C:\\a\0b")));
        assert_eq!(
            decoded(&spawn_json_with(field, Some(&with_nul))),
            Err(DecodeError::HoldsNul(field)),
            "{field}"
        );
    }
}

#[test]
fn an_empty_application_or_directory_is_refused_by_name() {
    for field in ["application", "directory"] {
        assert_eq!(decoded(&spawn_json_with(field, Some("\"\""))), Err(DecodeError::Empty(field)), "{field}");
    }
    let empty_command_line = decoded(&spawn_json_with("command_line", Some("\"\""))).expect("an empty command line is allowed");
    assert!(matches!(empty_command_line, Message::Spawn(spawn) if spawn.command_line.is_empty()));
}

#[test]
fn a_refusal_never_shows_what_the_field_held() {
    let secret = "c2VjcmV0LXZhbHVl";
    let refused = decoded(&spawn_json_with("environment", Some(&format!("\"{secret}!\"")))).expect_err("bad base64 is refused");

    assert!(!refused.to_string().contains(secret), "{refused}");
    assert!(!format!("{refused:?}").contains(secret), "{refused:?}");
    assert!(!format!("{:?}", spawn()).contains("cmd"), "a spawn's Debug shows no string");
}

#[test]
fn a_frame_longer_than_the_limit_is_refused_before_its_body_is_read() {
    let too_long = u32::try_from(FRAME_LIMIT + 1).expect("the limit fits a u32");
    let pipe = Memory::of([too_long.to_le_bytes().to_vec(), vec![b'{'; 64]]);

    let refused = read_message(&pipe, far()).expect_err("the frame is refused");

    assert!(matches!(refused, ReadError::TooLong), "{refused}");
    assert_eq!(pipe.reads(), 1, "only the length was read");
    let largest = Memory::of([u32::MAX.to_le_bytes().to_vec()]);
    assert!(matches!(read_message(&largest, far()), Err(ReadError::TooLong)));
}

#[test]
fn a_frame_of_length_zero_is_refused() {
    let pipe = Memory::of([0_u32.to_le_bytes().to_vec()]);

    assert!(matches!(read_message(&pipe, far()), Err(ReadError::Empty)));
}

#[test]
fn a_frame_split_across_many_reads_is_read_under_one_deadline() {
    let bytes = frame(&encode(&Message::Spawn(spawn())));
    let pipe = Memory::of(bytes.iter().map(|&byte| vec![byte]));
    let deadline = far();

    let read = read_message(&pipe, deadline).expect("the frame is read");

    assert_eq!(read, Message::Spawn(spawn()));
    assert_eq!(pipe.reads(), bytes.len(), "one read per byte");
    assert!(
        pipe.deadlines.borrow().iter().all(|&given| given == deadline),
        "every read had the frame's one deadline"
    );
}

#[test]
fn a_frame_cut_short_is_closed() {
    let bytes = frame(&encode(&Message::Done));
    let pipe = Memory::of([bytes[..bytes.len() - 1].to_vec()]);

    assert!(matches!(read_message(&pipe, far()), Err(ReadError::Closed)));
}

#[test]
fn a_deadline_already_passed_reads_nothing() {
    let pipe = Memory::of([frame(&encode(&Message::Done))]);

    let refused = read_message(&pipe, Instant::now()).expect_err("the deadline has passed");

    assert!(matches!(refused, ReadError::TimedOut), "{refused}");
    assert_eq!(pipe.reads(), 0);
}

#[test]
fn a_message_longer_than_the_limit_is_not_written() {
    let mut huge = spawn();
    huge.environment = vec![u16::from(b'A'); FRAME_LIMIT];
    let pipe = Memory::default();

    let refused = write_message(&pipe, &Message::Spawn(huge), far()).expect_err("the message is too long");

    assert!(matches!(refused, WriteError::TooLong), "{refused}");
    assert!(pipe.written.borrow().is_empty(), "nothing was written");
}
