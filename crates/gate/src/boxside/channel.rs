//! The box's end of a data channel: the waits, reads and writes the `outputs` verb's pull and the `inputs` verb's put
//! share. Each wait is bounded by its own limit and probes stderr for a gone client ([`Watch`]), so a box whose PC or
//! control call went never waits past its limits.
//!
//! The box opens the channel ([`proved`]): it binds a port of the range, says the channel line, takes the first
//! connection from the PC's address, sends its proof and checks the PC's. A pull then sends; a put receives
//! ([`received`]): exactly the length its request declares, never a byte more, checked against the request's SHA-256
//! before the box acks it. The `channel-check` verb opens one and drops it at once ([`channel_check`]).

use std::io::{self, Read};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use gate_args::BoxSeams;
use gate_inputs::hash::Hashing;
use gate_router::escaped;
use gate_win32::socket::{self, Listener, Stream};
use gate_wire::{CHANNEL_FIRST_PORT, CHANNEL_LAST_PORT, Channel, PROOF_BYTES, Payload, PutChannel, channel_line, lower_hex};

use crate::answer::{FAILED_STATUS, USAGE_STATUS};
use crate::boxside::forward::{ClientGone, OutputLock};
use crate::boxside::heartbeat;
use crate::boxside::stamping;
use crate::console::Console;

/// The ports a data channel may listen on: 47100 to 47131.
const CHANNEL_PORTS: (u16, u16) = (CHANNEL_FIRST_PORT, CHANNEL_LAST_PORT);
/// How long a data channel waits for the PC's connection.
const ACCEPT_WINDOW: Duration = Duration::from_secs(10);
/// How long a data channel waits for the PC's whole proof once it connected; no byte moves its end.
const PROOF_WAIT: Duration = Duration::from_secs(10);
/// How long each read and write of a data channel may wait: 10 s, shorter than the PC's 15 s stall floor, because the
/// idle limit must be shorter than the PC's stall window so the PC hears the box's `channel` verdict before its stall
/// watch ends the call.
const IDLE: Duration = Duration::from_secs(10);
/// How often a data channel's waits probe stderr for a gone client.
const PROBE_EVERY: Duration = Duration::from_secs(1);
/// The send buffer a data channel's connection gets: 16 MiB.
pub(super) const SEND_BUFFER: i32 = 16 * 1024 * 1024;
/// The bytes of the archive read and written at a time over a data channel.
pub(super) const CHANNEL_CHUNK: usize = 1024 * 1024;

/// What a data channel's waits watch besides their own deadline: whether the client of the call is gone, by a zero-byte
/// probe of stderr at most every [`PROBE_EVERY`].
pub(super) struct Watch {
    lock: OutputLock,
    gone: ClientGone,
    probed: Instant,
}

impl Watch {
    pub(super) fn new() -> Watch {
        Watch {
            lock: OutputLock::default(),
            gone: ClientGone::default(),
            probed: Instant::now(),
        }
    }

    /// Probes stderr when the last probe is [`PROBE_EVERY`] old; why the channel ends when the client is gone.
    pub(super) fn looked(&mut self) -> Result<(), String> {
        if self.probed.elapsed() >= PROBE_EVERY {
            self.probed = Instant::now();
            heartbeat::probe_stderr(&self.lock, &self.gone);
        }
        if self.gone.load(Ordering::SeqCst) {
            Err("the client has gone".to_owned())
        } else {
            Ok(())
        }
    }
}

/// The deadline of one wait that must end by `end`: `end`, or sooner so the next probe of a [`Watch`] is on time.
fn probe_slice(end: Instant) -> Instant {
    end.min(Instant::now() + PROBE_EVERY)
}

/// How long a read of a data channel may wait for its bytes.
#[derive(Clone, Copy)]
pub(super) enum Limit {
    /// Each read may wait this long for a byte: every byte read moves the end.
    Idle(Duration),
    /// The whole read must end by then: no byte moves it.
    Until(Instant, Duration),
}

/// The PC's connection once its ack matched, which the box keeps open until the PC closes it.
pub(super) struct Acked {
    pub(super) stream: Stream,
    pub(super) idle: Duration,
    pub(super) watch: Watch,
}

impl Acked {
    /// Waits for the PC to close the connection, up to the idle limit from the ack and never longer, stopping at the
    /// first byte the PC sends, which it has no reason to; then closes it. The archive is delivered, so how the wait ends
    /// changes nothing.
    pub(super) fn closed(mut self) {
        let mut buffer = [0_u8; 1];
        let end = Instant::now() + self.idle;
        while Instant::now() < end && self.watch.looked().is_ok() {
            if !matches!(self.stream.read(&mut buffer, probe_slice(end)), Ok(None)) {
                return;
            }
        }
    }
}

/// Opens the data channel `channel` of the call carrying `nonce`: binds a port of the range, says the channel line,
/// takes the first connection from the PC's address within the accept window (closing any other unread), sends the
/// box's proof and checks the PC's, reading nothing more before it passed. The connection, its idle limit and its
/// watch; else why the channel did not open, which never holds a proof.
pub(super) fn proved(channel: &Channel, nonce: &str, seams: &BoxSeams, console: &mut Console) -> Result<(Stream, Duration, Watch), String> {
    let (first, last) = seams.channel_ports.unwrap_or(CHANNEL_PORTS);
    let start = socket::random_port(first, last).map_err(|error| format!("cannot draw a port: {}", shown_error(&error)))?;
    let listener = Listener::bind_in_range(channel.address, first, last, start)
        .map_err(|error| format!("cannot listen: {}", shown_error(&error)))?
        .ok_or_else(|| "no data channel port is free".to_owned())?;
    stamping::protocol(console, &channel_line(nonce, listener.port()));
    let mut watch = Watch::new();
    let accept = seams.channel_accept_ms.map_or(ACCEPT_WINDOW, Duration::from_millis);
    let stream = accepted(&listener, channel, accept, &mut watch)?;
    drop(listener);
    let idle = seams.channel_idle_ms.map_or(IDLE, Duration::from_millis);
    written(&stream, channel.box_proof.bytes(), idle, &mut watch)?;
    let mut proof = [0_u8; PROOF_BYTES];
    let proof_wait = seams.channel_proof_ms.map_or(PROOF_WAIT, Duration::from_millis);
    read_exactly(&stream, &mut proof, Limit::Until(Instant::now() + proof_wait, proof_wait), &mut watch)
        .map_err(|why| format!("{why} before its proof"))?;
    if !channel.pc_proof.matches(&proof) {
        return Err("the PC's proof does not match".to_owned());
    }
    Ok((stream, idle, watch))
}

/// Answers `gate box channel-check`: opens the data channel the request's `Channel` asks for ([`proved`]) and drops the
/// connection as soon as the PC's proof matched, saying nothing more. The request carries its `Nonce` and `Channel`
/// alone; the drain and the work folders are not read. The verb's status: 0 once the PC's proof matched; the usage
/// status, its line said, for a request without a channel or a nonce; else the failure status, once `gate: {why}` is
/// said, which never holds a proof.
pub(super) fn channel_check(payload: &Payload, seams: &BoxSeams, console: &mut Console) -> u32 {
    let asked = payload.channel().and_then(|channel| {
        let channel = channel.ok_or_else(|| "gate: box payload lacks Channel".to_owned())?;
        Ok((channel, payload.nonce()?))
    });
    let (channel, nonce) = match asked {
        Ok(asked) => asked,
        Err(line) => {
            stamping::err(console, &line);
            return u32::from(USAGE_STATUS);
        }
    };
    match proved(&channel, nonce, seams, console) {
        Ok((stream, _, _)) => {
            drop(stream);
            0
        }
        Err(why) => {
            stamping::err(console, &format!("gate: {why}"));
            u32::from(FAILED_STATUS)
        }
    }
}

/// Receives a put's archive over its data channel `put` of the call carrying `nonce` ([`proved`]): `take` reads it
/// from a source that ends at the request's `Length` and reads no byte past it; then the rest up to that length is
/// read, and the count and SHA-256 of all of it are checked against the request's. When they match and `take` took
/// it, the box acks with that SHA-256 and waits for the PC to close the connection, which is closed before this
/// returns. What `take` made of it once it matched, a refusal of `take`'s included; else why the channel did not
/// complete: a failed open, a read that failed, waited out the idle limit or found the PC gone or closed short, or
/// bytes that are not the ones declared.
pub(super) fn received<T, E>(
    put: &PutChannel,
    nonce: &str,
    seams: &BoxSeams,
    console: &mut Console,
    take: impl FnOnce(&mut dyn Read) -> Result<T, E>,
) -> Result<Result<T, E>, String> {
    let (stream, idle, mut watch) = proved(&put.channel, nonce, seams, console)?;
    let mut source = Received {
        stream: &stream,
        idle,
        watch: &mut watch,
        length: put.length,
        left: put.length,
        failed: None,
    };
    let mut hashing = Hashing::new(&mut source);
    let taken = take(&mut hashing);
    let drained = io::copy(&mut hashing, &mut io::sink());
    // A source whose reads did not fail ends only at the declared length, so only the digest is left to compare.
    let (sha256, _) = hashing.finish();
    if let Some(why) = source.failed {
        return Err(why);
    }
    drained.map_err(|error| format!("the channel failed: {}", shown_error(&error)))?;
    if sha256 != lower_hex(&put.sha256) {
        return Err("the bytes received do not have the SHA-256 declared".to_owned());
    }
    if taken.is_ok() {
        written(&stream, &put.sha256, idle, &mut watch).map_err(|why| format!("{why}, the acknowledgement unsent"))?;
        Acked { stream, idle, watch }.closed();
    }
    Ok(taken)
}

/// A put's archive as its data channel brings it: the bytes the PC sends, each read waiting at most the idle limit,
/// ending at the declared length with nothing past it read. A read that fails keeps why, and every read after it fails.
struct Received<'a> {
    stream: &'a Stream,
    idle: Duration,
    watch: &'a mut Watch,
    /// The bytes the request declares.
    length: u64,
    /// The bytes of it not read yet.
    left: u64,
    /// Why a read failed, once one did.
    failed: Option<String>,
}

impl Read for Received<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if let Some(why) = &self.failed {
            return Err(io::Error::other(why.clone()));
        }
        if self.left == 0 || buffer.is_empty() {
            return Ok(0);
        }
        let want = usize::try_from(self.left).map_or(buffer.len(), |left| left.min(buffer.len()));
        let read = match read_some(self.stream, &mut buffer[..want], self.idle, self.watch) {
            Ok(0) => Err(format!(
                "the PC closed the channel after {} of the {} bytes declared",
                self.length - self.left,
                self.length
            )),
            other => other,
        };
        match read {
            Ok(read) => {
                self.left -= read as u64;
                Ok(read)
            }
            Err(why) => {
                self.failed = Some(why.clone());
                Err(io::Error::other(why))
            }
        }
    }
}

/// Reads what `stream` has into `buffer`, waiting at most `idle` for a byte: the count read, 0 once the PC closed the
/// connection; else why not, said of the PC.
fn read_some(stream: &Stream, buffer: &mut [u8], idle: Duration, watch: &mut Watch) -> Result<usize, String> {
    let end = Instant::now() + idle;
    loop {
        watch.looked()?;
        if Instant::now() >= end {
            return Err(format!("the PC sent nothing for {} ms", idle.as_millis()));
        }
        match stream.read(buffer, probe_slice(end)) {
            Ok(Some(read)) => return Ok(read),
            Ok(None) => {}
            Err(error) => return Err(format!("the channel failed: {}", shown_error(&error))),
        }
    }
}

/// The system's `error` as a channel's `why` shows it, its control characters escaped.
pub(super) fn shown_error(error: &io::Error) -> String {
    escaped(&error.to_string())
}

/// The first connection from the PC's address within `window`. A connection from any other address is closed with
/// nothing read or written, one whose setup failed included, and the wait goes on; else why none came.
fn accepted(listener: &Listener, channel: &Channel, window: Duration, watch: &mut Watch) -> Result<Stream, String> {
    let end = Instant::now() + window;
    loop {
        watch.looked()?;
        if Instant::now() >= end {
            return Err(format!("no connection from the PC within {} ms", window.as_millis()));
        }
        match listener.accept(probe_slice(end)) {
            Ok(Some(taken)) if *taken.peer.ip() == channel.peer => {
                return taken
                    .stream
                    .map_err(|error| format!("cannot set up the PC's connection: {}", shown_error(&error)));
            }
            Ok(Some(other)) => drop(other),
            Ok(None) => {}
            Err(error) => return Err(format!("cannot accept: {}", shown_error(&error))),
        }
    }
}

/// Writes all of `bytes` on `stream`, each write waiting at most `idle` for room; else why not.
pub(super) fn written(stream: &Stream, mut bytes: &[u8], idle: Duration, watch: &mut Watch) -> Result<(), String> {
    let mut end = Instant::now() + idle;
    while !bytes.is_empty() {
        watch.looked()?;
        if Instant::now() >= end {
            return Err(format!("the PC took nothing for {} ms", idle.as_millis()));
        }
        match stream.write(bytes, probe_slice(end)) {
            Ok(Some(wrote)) => {
                bytes = &bytes[wrote..];
                end = Instant::now() + idle;
            }
            Ok(None) => {}
            Err(error) => return Err(format!("the channel failed: {}", shown_error(&error))),
        }
    }
    Ok(())
}

/// Fills `buffer` from `stream` within `limit`; else why not, said of the PC.
pub(super) fn read_exactly(stream: &Stream, buffer: &mut [u8], limit: Limit, watch: &mut Watch) -> Result<(), String> {
    let mut filled = 0;
    let mut end = match limit {
        Limit::Idle(idle) => Instant::now() + idle,
        Limit::Until(end, _) => end,
    };
    while filled < buffer.len() {
        watch.looked()?;
        if Instant::now() >= end {
            return Err(match limit {
                Limit::Idle(idle) => format!("the PC sent nothing for {} ms", idle.as_millis()),
                Limit::Until(_, within) => format!("the PC sent too little within {} ms", within.as_millis()),
            });
        }
        match stream.read(&mut buffer[filled..], probe_slice(end)) {
            Ok(Some(0)) => return Err("the PC closed the channel".to_owned()),
            Ok(Some(read)) => {
                filled += read;
                if let Limit::Idle(idle) = limit {
                    end = Instant::now() + idle;
                }
            }
            Ok(None) => {}
            Err(error) => return Err(format!("the channel failed: {}", shown_error(&error))),
        }
    }
    Ok(())
}
