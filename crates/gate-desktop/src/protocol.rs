//! The messages the box and the desktop helper exchange on the control pipe, and their framing: a frame is a `u32`
//! little-endian length, then that many bytes of UTF-8 JSON, one object whose `"type"` names the message. A length of
//! 0 or above [`FRAME_LIMIT`] is refused before anything is allocated for it. Every field is untrusted: decoding
//! refuses anything but the exact fields of the message, each of its JSON type and range, and an error names the field
//! alone, never what it held.
//!
//! The four strings of [`Spawn`] travel as base64 (standard alphabet, padded) of UTF-16LE, so any UTF-16, unpaired
//! surrogates included, arrives as it was sent.

use std::fmt;
use std::io;
use std::time::Instant;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use gate_win32::{PipeClient, PipeRead, PipeServer, PipeWrite};
use serde_json::{Map, Value};

/// The longest frame body, in bytes.
pub const FRAME_LIMIT: usize = 4 * 1024 * 1024;

/// One message on the control pipe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// Helper to box, first: the helper's build and the desktop that takes input in its session.
    Hello {
        /// The helper's version text, as [`crate::version::build_version`] gives it.
        version: String,
        /// The input desktop's name, or empty when it could not be read.
        desktop: String,
        /// Whether `desktop` is `Default`, the user's own desktop.
        ok: bool,
    },
    /// Box to helper: the command to start.
    Spawn(Spawn),
    /// Helper to box: the command, created suspended, and the values of its handles in the helper's table.
    Child {
        /// The command's process id.
        pid: u32,
        /// The value of the helper's handle to the command's process.
        process: u64,
        /// The value of the helper's handle to the command's first thread.
        thread: u64,
    },
    /// Helper to box: the command could not be created.
    Failed {
        /// The system's error code.
        code: u32,
    },
    /// Box to helper: the box has what it needs; the helper may exit.
    Done,
}

/// The command a [`Message::Spawn`] asks for. Decoded, none of `application`, `command_line` and `directory` holds a
/// NUL, and neither `application` nor `directory` is empty; `environment` is checked only by whoever builds the block.
#[derive(Clone, PartialEq, Eq)]
pub struct Spawn {
    /// The program file, as UTF-16.
    pub application: Vec<u16>,
    /// The command line, as UTF-16.
    pub command_line: Vec<u16>,
    /// The working folder, as UTF-16.
    pub directory: Vec<u16>,
    /// The whole environment block, as UTF-16, its two closing NULs included.
    pub environment: Vec<u16>,
    /// The value of the stdout pipe's write end in the helper's table.
    pub stdout: u64,
    /// The value of the stderr pipe's write end in the helper's table.
    pub stderr: u64,
}

impl fmt::Debug for Spawn {
    /// The handle values and the sizes alone: the command line and the environment may hold secrets.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Spawn")
            .field("application_units", &self.application.len())
            .field("command_line_units", &self.command_line.len())
            .field("directory_units", &self.directory.len())
            .field("environment_units", &self.environment.len())
            .field("stdout", &self.stdout)
            .field("stderr", &self.stderr)
            .finish()
    }
}

/// Why a frame body was refused as a message. A field is named; what it held never is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The body is not UTF-8 JSON holding one object.
    NotAnObject,
    /// The object has no `type`.
    MissingType,
    /// The `type` names no message.
    UnknownType,
    /// The message lacks this field.
    Missing(&'static str),
    /// The object holds a field the message does not have.
    UnexpectedField,
    /// This field is of another JSON type than the message's.
    WrongType(&'static str),
    /// This number field is negative, fractional or too large.
    OutOfRange(&'static str),
    /// This field is not standard padded base64.
    BadBase64(&'static str),
    /// This field's bytes are an odd count, so not UTF-16.
    OddLength(&'static str),
    /// This field holds a NUL.
    HoldsNul(&'static str),
    /// This field is empty.
    Empty(&'static str),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::NotAnObject => write!(formatter, "the message is not a JSON object"),
            DecodeError::MissingType => write!(formatter, "the message has no type"),
            DecodeError::UnknownType => write!(formatter, "the message's type is not one of the protocol's"),
            DecodeError::Missing(field) => write!(formatter, "the message has no {field}"),
            DecodeError::UnexpectedField => write!(formatter, "the message holds a field its type does not have"),
            DecodeError::WrongType(field) => write!(formatter, "the message's {field} is of the wrong JSON type"),
            DecodeError::OutOfRange(field) => write!(formatter, "the message's {field} is out of range"),
            DecodeError::BadBase64(field) => write!(formatter, "the message's {field} is not base64"),
            DecodeError::OddLength(field) => write!(formatter, "the message's {field} is not whole UTF-16 units"),
            DecodeError::HoldsNul(field) => write!(formatter, "the message's {field} holds a NUL"),
            DecodeError::Empty(field) => write!(formatter, "the message's {field} is empty"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// The bytes of `message` as JSON, the body of its frame.
#[must_use]
pub fn encode(message: &Message) -> Vec<u8> {
    let mut fields = Map::new();
    let kind = match message {
        Message::Hello { version, desktop, ok } => {
            fields.insert("version".to_owned(), Value::from(version.as_str()));
            fields.insert("desktop".to_owned(), Value::from(desktop.as_str()));
            fields.insert("ok".to_owned(), Value::from(*ok));
            "hello"
        }
        Message::Spawn(spawn) => {
            fields.insert("application".to_owned(), wide_field(&spawn.application));
            fields.insert("command_line".to_owned(), wide_field(&spawn.command_line));
            fields.insert("directory".to_owned(), wide_field(&spawn.directory));
            fields.insert("environment".to_owned(), wide_field(&spawn.environment));
            fields.insert("stdout".to_owned(), Value::from(spawn.stdout));
            fields.insert("stderr".to_owned(), Value::from(spawn.stderr));
            "spawn"
        }
        Message::Child { pid, process, thread } => {
            fields.insert("pid".to_owned(), Value::from(*pid));
            fields.insert("process".to_owned(), Value::from(*process));
            fields.insert("thread".to_owned(), Value::from(*thread));
            "child"
        }
        Message::Failed { code } => {
            fields.insert("code".to_owned(), Value::from(*code));
            "failed"
        }
        Message::Done => "done",
    };
    let mut object = Map::new();
    object.insert("type".to_owned(), Value::from(kind));
    object.extend(fields);
    Value::Object(object).to_string().into_bytes()
}

/// `units` as the wire carries it: base64 of its UTF-16LE bytes.
fn wide_field(units: &[u16]) -> Value {
    let bytes: Vec<u8> = units.iter().flat_map(|unit| unit.to_le_bytes()).collect();
    Value::from(STANDARD.encode(bytes))
}

/// The message a frame body holds.
///
/// # Errors
///
/// A [`DecodeError`] naming the first thing refused.
pub fn decode(body: &[u8]) -> Result<Message, DecodeError> {
    let Ok(Value::Object(mut fields)) = serde_json::from_slice::<Value>(body) else {
        return Err(DecodeError::NotAnObject);
    };
    let kind = match fields.remove("type") {
        None => return Err(DecodeError::MissingType),
        Some(Value::String(kind)) => kind,
        Some(_) => return Err(DecodeError::WrongType("type")),
    };
    let message = match kind.as_str() {
        "hello" => Message::Hello {
            version: take_string(&mut fields, "version")?,
            desktop: take_string(&mut fields, "desktop")?,
            ok: take_bool(&mut fields, "ok")?,
        },
        "spawn" => Message::Spawn(take_spawn(&mut fields)?),
        "child" => Message::Child {
            pid: take_u32(&mut fields, "pid")?,
            process: take_u64(&mut fields, "process")?,
            thread: take_u64(&mut fields, "thread")?,
        },
        "failed" => Message::Failed {
            code: take_u32(&mut fields, "code")?,
        },
        "done" => Message::Done,
        _ => return Err(DecodeError::UnknownType),
    };
    if !fields.is_empty() {
        return Err(DecodeError::UnexpectedField);
    }
    Ok(message)
}

/// The fields of a `Spawn`, taken out of `fields` and checked.
fn take_spawn(fields: &mut Map<String, Value>) -> Result<Spawn, DecodeError> {
    let spawn = Spawn {
        application: take_wide(fields, "application")?,
        command_line: take_wide(fields, "command_line")?,
        directory: take_wide(fields, "directory")?,
        environment: take_wide(fields, "environment")?,
        stdout: take_u64(fields, "stdout")?,
        stderr: take_u64(fields, "stderr")?,
    };
    for (field, units) in [
        ("application", &spawn.application),
        ("command_line", &spawn.command_line),
        ("directory", &spawn.directory),
    ] {
        if units.contains(&0) {
            return Err(DecodeError::HoldsNul(field));
        }
    }
    for (field, units) in [("application", &spawn.application), ("directory", &spawn.directory)] {
        if units.is_empty() {
            return Err(DecodeError::Empty(field));
        }
    }
    Ok(spawn)
}

/// The field `name`, taken out of `fields`.
fn take(fields: &mut Map<String, Value>, name: &'static str) -> Result<Value, DecodeError> {
    fields.remove(name).ok_or(DecodeError::Missing(name))
}

/// The string field `name`.
fn take_string(fields: &mut Map<String, Value>, name: &'static str) -> Result<String, DecodeError> {
    match take(fields, name)? {
        Value::String(text) => Ok(text),
        _ => Err(DecodeError::WrongType(name)),
    }
}

/// The boolean field `name`.
fn take_bool(fields: &mut Map<String, Value>, name: &'static str) -> Result<bool, DecodeError> {
    match take(fields, name)? {
        Value::Bool(value) => Ok(value),
        _ => Err(DecodeError::WrongType(name)),
    }
}

/// The number field `name`, a whole number from 0 to `u64::MAX`.
fn take_u64(fields: &mut Map<String, Value>, name: &'static str) -> Result<u64, DecodeError> {
    match take(fields, name)? {
        Value::Number(number) => number.as_u64().ok_or(DecodeError::OutOfRange(name)),
        _ => Err(DecodeError::WrongType(name)),
    }
}

/// The number field `name`, a whole number from 0 to `u32::MAX`.
fn take_u32(fields: &mut Map<String, Value>, name: &'static str) -> Result<u32, DecodeError> {
    u32::try_from(take_u64(fields, name)?).map_err(|_| DecodeError::OutOfRange(name))
}

/// The base64 UTF-16LE field `name`, as UTF-16.
fn take_wide(fields: &mut Map<String, Value>, name: &'static str) -> Result<Vec<u16>, DecodeError> {
    let text = take_string(fields, name)?;
    let bytes = STANDARD.decode(text).map_err(|_| DecodeError::BadBase64(name))?;
    if bytes.len() % 2 != 0 {
        return Err(DecodeError::OddLength(name));
    }
    Ok(bytes.as_chunks::<2>().0.iter().map(|&pair| u16::from_le_bytes(pair)).collect())
}

/// A byte stream frames are read from and written to: the control pipe's client or server end, or a test's own.
pub trait FramePipe {
    /// Reads into the start of `buf` what has arrived, at least one byte, as [`PipeClient::read`] does, giving up at
    /// `deadline`.
    ///
    /// # Errors
    ///
    /// The system's error when the read fails.
    fn read(&self, buf: &mut [u8], deadline: Instant) -> io::Result<PipeRead>;

    /// Writes every byte of `bytes`, as [`PipeClient::write_all`] does, giving up at `deadline`.
    ///
    /// # Errors
    ///
    /// The system's error when the write fails.
    fn write_all(&self, bytes: &[u8], deadline: Instant) -> io::Result<PipeWrite>;
}

impl FramePipe for PipeClient {
    fn read(&self, buf: &mut [u8], deadline: Instant) -> io::Result<PipeRead> {
        PipeClient::read(self, buf, deadline, &[])
    }

    fn write_all(&self, bytes: &[u8], deadline: Instant) -> io::Result<PipeWrite> {
        PipeClient::write_all(self, bytes, deadline, &[])
    }
}

impl FramePipe for PipeServer {
    fn read(&self, buf: &mut [u8], deadline: Instant) -> io::Result<PipeRead> {
        PipeServer::read(self, buf, deadline, &[])
    }

    fn write_all(&self, bytes: &[u8], deadline: Instant) -> io::Result<PipeWrite> {
        PipeServer::write_all(self, bytes, deadline, &[])
    }
}

/// Why no message was read.
#[derive(Debug)]
pub enum ReadError {
    /// The other end closed before a whole frame arrived.
    Closed,
    /// The deadline passed before a whole frame arrived.
    TimedOut,
    /// A wake handle at this index was signalled before a whole frame arrived.
    Woken(usize),
    /// The frame's length is 0.
    Empty,
    /// The frame's length is above [`FRAME_LIMIT`].
    TooLong,
    /// The frame's body is no message.
    Decode(DecodeError),
    /// The read failed.
    Io(io::Error),
}

impl fmt::Display for ReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::Closed => write!(formatter, "the pipe closed before a whole message arrived"),
            ReadError::TimedOut => write!(formatter, "no whole message arrived in time"),
            ReadError::Woken(index) => write!(formatter, "the wait was ended by wake handle {index}"),
            ReadError::Empty => write!(formatter, "the message's frame is empty"),
            ReadError::TooLong => write!(formatter, "the message's frame is longer than {FRAME_LIMIT} bytes"),
            ReadError::Decode(error) => error.fmt(formatter),
            ReadError::Io(error) => write!(formatter, "the pipe could not be read: {error}"),
        }
    }
}

impl std::error::Error for ReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ReadError::Decode(error) => Some(error),
            ReadError::Io(error) => Some(error),
            _ => None,
        }
    }
}

/// Why a message was not written whole. Any of these leaves the stream out of step: the pipe is done with.
#[derive(Debug)]
pub enum WriteError {
    /// The other end closed first.
    Closed,
    /// The deadline passed first.
    TimedOut,
    /// A wake handle at this index was signalled first.
    Woken(usize),
    /// The message is longer than [`FRAME_LIMIT`]; nothing was written.
    TooLong,
    /// The write failed.
    Io(io::Error),
}

impl fmt::Display for WriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteError::Closed => write!(formatter, "the pipe closed before the whole message was written"),
            WriteError::TimedOut => write!(formatter, "the message was not written in time"),
            WriteError::Woken(index) => write!(formatter, "the write was ended by wake handle {index}"),
            WriteError::TooLong => write!(formatter, "the message is longer than {FRAME_LIMIT} bytes"),
            WriteError::Io(error) => write!(formatter, "the pipe could not be written: {error}"),
        }
    }
}

impl std::error::Error for WriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WriteError::Io(error) => Some(error),
            _ => None,
        }
    }
}

/// Reads one whole frame from `pipe` and decodes it, the length and the body together under the one `deadline`.
///
/// # Errors
///
/// A [`ReadError`] saying why no message was read.
pub fn read_message(pipe: &impl FramePipe, deadline: Instant) -> Result<Message, ReadError> {
    let mut prefix = [0_u8; 4];
    read_exact(pipe, &mut prefix, deadline)?;
    let length = u32::from_le_bytes(prefix);
    if length == 0 {
        return Err(ReadError::Empty);
    }
    let length = usize::try_from(length)
        .ok()
        .filter(|&length| length <= FRAME_LIMIT)
        .ok_or(ReadError::TooLong)?;
    let mut body = vec![0_u8; length];
    read_exact(pipe, &mut body, deadline)?;
    decode(&body).map_err(ReadError::Decode)
}

/// Fills `buf` from `pipe`, read after read, until `deadline`, which is checked before each read: a peer that keeps
/// the pipe full is not read past it.
fn read_exact(pipe: &impl FramePipe, buf: &mut [u8], deadline: Instant) -> Result<(), ReadError> {
    let mut filled = 0;
    while let Some(rest) = buf.get_mut(filled..).filter(|rest| !rest.is_empty()) {
        if Instant::now() >= deadline {
            return Err(ReadError::TimedOut);
        }
        let room = rest.len();
        match pipe.read(rest, deadline).map_err(ReadError::Io)? {
            PipeRead::Read(count) if (1..=room).contains(&count) => filled += count,
            PipeRead::Read(count) => {
                return Err(ReadError::Io(io::Error::other(format!(
                    "the pipe reported reading {count} bytes into room for {room}"
                ))));
            }
            PipeRead::Closed => return Err(ReadError::Closed),
            PipeRead::TimedOut => return Err(ReadError::TimedOut),
            PipeRead::Woken(index) => return Err(ReadError::Woken(index)),
        }
    }
    Ok(())
}

/// Writes `message` to `pipe` as one frame, in one write under `deadline`.
///
/// # Errors
///
/// A [`WriteError`] saying why the frame was not written whole.
pub fn write_message(pipe: &impl FramePipe, message: &Message, deadline: Instant) -> Result<(), WriteError> {
    let body = encode(message);
    if body.len() > FRAME_LIMIT {
        return Err(WriteError::TooLong);
    }
    let length = u32::try_from(body.len()).map_err(|_| WriteError::TooLong)?;
    let mut frame = Vec::with_capacity(body.len() + 4);
    frame.extend_from_slice(&length.to_le_bytes());
    frame.extend_from_slice(&body);
    match pipe.write_all(&frame, deadline).map_err(WriteError::Io)? {
        PipeWrite::Written => Ok(()),
        PipeWrite::Closed => Err(WriteError::Closed),
        PipeWrite::TimedOut => Err(WriteError::TimedOut),
        PipeWrite::Woken(index) => Err(WriteError::Woken(index)),
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose message cannot be read must fail loudly")]
mod tests;
