//! The PC's end of a data channel for an outputs pull: whether a builder's pull asks for one, the PC's own
//! address toward the builder, the two proofs of one attempt, and the pull itself. The PC dials the builder's `data`
//! address only, at the port the box's own line names, from its own address; it checks the box's proof before it sends
//! anything, then sends its own, reads exactly the archive's size, the box's line over ssh having said it, and sends the
//! SHA-256 of what it read as its ack. It then shuts the connection down and closes it first. No length or digest is
//! taken from the channel: the call's own ssh stderr says what the box sent, which the outputs step compares.
//!
//! A put goes the other way ([`pushed`]): the PC checks the box's proof, sends its own, sends the archive whose length
//! and SHA-256 its request declared, and reads the box's ack, that SHA-256; the box's verdict over ssh, not the ack,
//! says whether the put installed.
//!
//! `gate check --builder` probes the channel ([`probe_dialled`]): the PC checks the box's proof, sends its own, and
//! closes; nothing else crosses it.

use std::io::{self, Read};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

use gate_router::{Builder, BuilderOs};
use gate_win32::socket::{self, Connect, Stream};
use gate_wire::{CHANNEL_FIRST_PORT, CHANNEL_LAST_PORT, PROOF_BYTES, Proof, lower_hex};
use sha2::{Digest, Sha256};

/// How long the PC waits for the box's whole proof once it connected: 10 s, as the box waits for the PC's.
const PROOF_WAIT: Duration = Duration::from_secs(10);
/// How long the PC waits for its connection to the box to be made: the box's accept window, 10 s.
const CONNECT_WAIT: Duration = Duration::from_secs(10);
/// The bytes of the archive read at a time: 1 MiB, as the box sends them.
const CHUNK: usize = 1024 * 1024;
/// How long one wait on the socket lasts before it looks again at whether the call has ended; not an idle limit, which
/// the call's own stall watch is.
const SLICE: Duration = Duration::from_millis(200);

/// The line the log gets when an outputs pull or a put that could go over the data channel goes over ssh instead, for
/// `why`, which never holds a proof.
pub(super) fn not_used_line(why: &str) -> String {
    format!("gate: data channel not used: {why}; sent over ssh")
}

/// The line the log gets once an outputs pull moved `bytes` over the data channel in `took`, in seconds to one decimal.
pub(super) fn pull_line(bytes: u64, took: Duration) -> String {
    format!("gate: data channel: pull {bytes} in {:.1} s", took.as_secs_f64())
}

/// The line the log gets once a put moved `bytes` over the data channel in `took`, in seconds to one decimal, the box
/// having acked them.
pub(super) fn put_line(bytes: u64, took: Duration) -> String {
    format!("gate: data channel: put {bytes} in {:.1} s", took.as_secs_f64())
}

/// The send buffer the PC's connection of a put gets: 16 MiB.
pub(super) const SEND_BUFFER: i32 = 16 * 1024 * 1024;

/// The `data` address an outputs pull from `builder`, or a put to it, dials: `None` for a builder with no `data` key,
/// and for one that is not a Windows builder, which stays on ssh; else why the channel is not used: a loopback
/// address, which only a build with the test seams dials.
pub(crate) fn planned(builder: &Builder) -> Result<Option<Ipv4Addr>, String> {
    let Some(data) = builder.data else {
        return Ok(None);
    };
    if builder.os != BuilderOs::Windows {
        return Ok(None);
    }
    if data.is_loopback() && !cfg!(feature = "test-seams") {
        return Err(format!("data address {data} is loopback"));
    }
    Ok(Some(data))
}

/// This PC's own address toward `data`: the local address of a UDP socket connected to it, a connect that sends no
/// packet; else why it cannot be read.
pub(crate) fn local_toward(data: Ipv4Addr) -> Result<Ipv4Addr, String> {
    let cannot = |error: io::Error| format!("cannot read this PC's address toward {data}: {error}");
    let probe = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(cannot)?;
    probe.connect((data, CHANNEL_FIRST_PORT)).map_err(cannot)?;
    match probe.local_addr().map_err(cannot)? {
        SocketAddr::V4(local) => Ok(*local.ip()),
        SocketAddr::V6(local) => Err(format!("this PC's address toward {data} is no IPv4 address: {local}")),
    }
}

/// The data channel one attempt of an outputs call asks for: the builder's address, the PC's own, and the attempt's
/// two proofs, drawn fresh for it. Its `Debug` shows no proof, and it has no `Display`.
#[derive(Debug)]
pub(crate) struct Asked {
    pub(crate) data: Ipv4Addr,
    pub(crate) local: Ipv4Addr,
    pub(crate) box_proof: Proof,
    pub(crate) pc_proof: Proof,
}

impl Asked {
    /// The channel to `data` from `local`, with two proofs new to it; else why they could not be drawn.
    pub(crate) fn drawn(data: Ipv4Addr, local: Ipv4Addr) -> Result<Asked, String> {
        let draw = || {
            socket::random_proof()
                .map(Proof::new)
                .map_err(|error| format!("cannot draw a proof: {error}"))
        };
        Ok(Asked {
            data,
            local,
            box_proof: draw()?,
            pc_proof: draw()?,
        })
    }
}

/// What an outputs call's channel dials: the channel asked for, the archive's size the run call's box said, `None`
/// when it said none, which the PC does not dial without, and the fault the PC's side takes, which only
/// `--test-remote-channel-fault` names.
#[derive(Debug)]
pub(super) struct Dial {
    pub(super) asked: Asked,
    pub(super) size: Option<u64>,
    pub(super) fault: Option<Fault>,
}

/// What a put's channel dials: the channel asked for, the archive's `length` and `sha256` the request declares, the
/// send buffer the PC's connection gets, and the fault the PC's side takes, which only `--test-remote-channel-fault`
/// names.
#[derive(Debug)]
pub(super) struct Put {
    pub(super) asked: Asked,
    pub(super) length: u64,
    pub(super) sha256: [u8; 32],
    pub(super) send_buffer: i32,
    pub(super) fault: Option<Fault>,
}

/// A fault `--test-remote-channel-fault` has the PC's side of a channel take once it dialled the box: it takes the
/// box's proof for a mismatch (`box-proof`); stops after half the archive's size and drops without an ack
/// (`truncate`); flips the first byte it reads before it hashes and acks, or, on a put, the first byte it sends
/// (`alter`); or drops once it read or sent everything, before the ack (`no-ack`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Fault {
    BoxProof,
    Truncate,
    Alter,
    NoAck,
}

impl Fault {
    /// The fault `name` spells, as the seam takes it; `None` for any other name, which the seam's own parse refuses.
    pub(super) fn named(name: &str) -> Option<Fault> {
        match name {
            "box-proof" => Some(Fault::BoxProof),
            "truncate" => Some(Fault::Truncate),
            "alter" => Some(Fault::Alter),
            "no-ack" => Some(Fault::NoAck),
            _ => None,
        }
    }
}

/// How an outputs call's channel ended on the PC's side: no port line came; the port line came but no size was known,
/// so the PC did not dial; it failed for why, having sent nothing more after the failure; the call ended first, which
/// the PC gave up for, a failure with no reason of its own; its thread did not end within the call's drain, which the
/// PC's caller says; or it read `bytes` whose SHA-256 in lower-case hex is `sha256` and acked them, `took` from its
/// connect to its close; or, for a put, it sent `bytes` and the box acked them with the SHA-256 the request declares,
/// `took` from its connect to its close.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Dialled {
    NoPort,
    NoSize,
    Failed(String),
    CallEnded,
    NotEnded,
    Pulled { bytes: u64, sha256: String, took: Duration },
    Pushed { bytes: u64, took: Duration },
}

/// What a put's channel tells its call: each step of the exchange before the archive, the archive's bytes as they are
/// written to the connection, and the archive's end once its last byte is written.
pub(super) trait Feed {
    /// Marks the call's progress for a step of the exchange that moved no byte of the archive.
    fn mark(&mut self);
    /// Counts `bytes` more of the archive written to the connection.
    fn sent(&mut self, bytes: u64);
    /// Says the archive's last byte is written; what is left is the box's ack and its verdict.
    fn finished(&mut self);
}

/// Where a pull's archive goes: each piece read, the call's progress marked at each step of the exchange before the
/// archive, and the end, once the pull ended, before the connection is shut down.
pub(super) trait Fill {
    /// Takes the piece `bytes` read, marking the call's progress; else why not.
    fn chunk(&mut self, bytes: &[u8]) -> Result<(), String>;
    /// Marks the call's progress for a step of the exchange that moved no byte of the archive.
    fn mark(&mut self);
    /// Closes what the pieces went into; nothing more is taken.
    fn close(&mut self);
}

/// Why a pull stopped short: the call ended, which is no failure of the PC's own, or why it failed.
enum Stop {
    CallEnded,
    Failed(String),
}

impl From<String> for Stop {
    fn from(why: String) -> Stop {
        Stop::Failed(why)
    }
}

/// Whether the PC dials `port`, the box's port line's, on `data`: a port of the channel's range, or, in a build with
/// the test seams and to a loopback address, any port, which the box's `--test-box-channel-ports` seam picks.
fn port_taken(data: Ipv4Addr, port: u16) -> bool {
    (CHANNEL_FIRST_PORT..=CHANNEL_LAST_PORT).contains(&port) || (cfg!(feature = "test-seams") && data.is_loopback() && port != 0)
}

/// Pulls the archive of `size` bytes over the channel `asked` from the box's `port` into `fill`, each piece read handed
/// to it before the next is read, taking `fault` when one is named; every wait, the connect's included, lasts at most
/// [`SLICE`] before it looks again at whether `ended` says the call has ended, and gives up once it has. The call's
/// progress is marked once the connection is made and once the proofs are exchanged. Whatever the end, `fill` is
/// closed first, then the connection is shut down and closed, the PC closing first after its ack.
pub(super) fn pulled(asked: &Asked, (port, size, fault): (u16, u64, Option<Fault>), fill: &mut dyn Fill, ended: &dyn Fn() -> bool) -> Dialled {
    if !port_taken(asked.data, port) {
        return Dialled::Failed(format!("the box's port {port} is outside {CHANNEL_FIRST_PORT}-{CHANNEL_LAST_PORT}"));
    }
    let began = Instant::now();
    let stream = match connected(asked, SocketAddrV4::new(asked.data, port), ended) {
        Ok(stream) => stream,
        Err(stop) => {
            fill.close();
            return stopped(stop);
        }
    };
    fill.mark();
    let end = match served(&stream, asked, (size, fault), fill, ended) {
        Ok((bytes, sha256)) => Dialled::Pulled {
            bytes,
            sha256,
            took: began.elapsed(),
        },
        Err(stop) => stopped(stop),
    };
    fill.close();
    // A shutdown that fails leaves the close alone, which the drop makes either way; how the pull went stands.
    stream.shut_down().ok();
    drop(stream);
    end
}

/// How a probe of a builder's data channel ended on the PC's side ([`probe_dialled`]): the box's proof matched and the
/// PC's was sent; it failed for why, which never holds a proof; or the call ended first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Probed {
    Proved,
    Failed(String),
    CallEnded,
}

/// Probes the data channel `asked` at the box's `port`: connects from the PC's own address, checks the box's proof
/// before it sends anything, sends its own, then shuts the connection down and closes it. Every wait, the connect's
/// included, lasts at most [`SLICE`] before it looks again at whether `ended` says the call has ended, and gives up
/// once it has.
pub(crate) fn probe_dialled(asked: &Asked, port: u16, ended: &dyn Fn() -> bool) -> Probed {
    if !port_taken(asked.data, port) {
        return Probed::Failed(format!("the box's port {port} is outside {CHANNEL_FIRST_PORT}-{CHANNEL_LAST_PORT}"));
    }
    let stream = match connected(asked, SocketAddrV4::new(asked.data, port), ended) {
        Ok(stream) => stream,
        Err(stop) => return probe_stopped(stop),
    };
    let end = match proofs_traded(&stream, asked, ended) {
        Ok(()) => Probed::Proved,
        Err(stop) => probe_stopped(stop),
    };
    // A shutdown that fails leaves the close alone, which the drop makes either way; how the probe went stands.
    stream.shut_down().ok();
    drop(stream);
    end
}

/// A probe's exchange over the connected `stream`: the box's proof checked before anything is sent, then the PC's sent.
fn proofs_traded(stream: &Stream, asked: &Asked, ended: &dyn Fn() -> bool) -> Result<(), Stop> {
    let mut proof = [0_u8; PROOF_BYTES];
    proof_read(stream, &mut proof, ended)?;
    if !asked.box_proof.matches(&proof) {
        return Err(Stop::Failed("the box's proof does not match".to_owned()));
    }
    written(stream, asked.pc_proof.bytes(), ended, &mut |_| {})
}

/// How a probe that stopped short for `stop` ended.
fn probe_stopped(stop: Stop) -> Probed {
    match stop {
        Stop::CallEnded => Probed::CallEnded,
        Stop::Failed(why) => Probed::Failed(why),
    }
}

/// How a pull that stopped short for `stop` ended.
fn stopped(stop: Stop) -> Dialled {
    match stop {
        Stop::CallEnded => Dialled::CallEnded,
        Stop::Failed(why) => Dialled::Failed(why),
    }
}

/// The connection from the PC's own address to `remote`, made within [`CONNECT_WAIT`], waited on in [`SLICE`]s and
/// given up at once when `ended` says the call has ended; else why not.
fn connected(asked: &Asked, remote: SocketAddrV4, ended: &dyn Fn() -> bool) -> Result<Stream, Stop> {
    let end = Instant::now() + CONNECT_WAIT;
    let cannot = |error: io::Error| Stop::Failed(format!("cannot connect to {remote}: {error}"));
    let mut connect = Stream::connecting(asked.local, remote).map_err(cannot)?;
    loop {
        let connecting = match connect {
            Connect::Made(stream) => return Ok(stream),
            Connect::Waiting(connecting) => connecting,
        };
        if ended() {
            return Err(Stop::CallEnded);
        }
        let now = Instant::now();
        if now >= end {
            return Err(Stop::Failed(format!("no connection to {remote} within {} s", CONNECT_WAIT.as_secs())));
        }
        connect = connecting.made(end.min(now + SLICE)).map_err(cannot)?;
    }
}

/// The exchange over the connected `stream`: the box's proof checked before anything is sent, the PC's sent, the
/// call's progress marked, `size` bytes read and handed to `fill`, and their SHA-256 sent back, `fault` taken on the way
/// when one is named; the count and the SHA-256 in lower-case hex, else why not.
fn served(
    stream: &Stream,
    asked: &Asked,
    (size, fault): (u64, Option<Fault>),
    fill: &mut dyn Fill,
    ended: &dyn Fn() -> bool,
) -> Result<(u64, String), Stop> {
    let mut proof = [0_u8; PROOF_BYTES];
    proof_read(stream, &mut proof, ended)?;
    if fault == Some(Fault::BoxProof) || !asked.box_proof.matches(&proof) {
        return Err(Stop::Failed("the box's proof does not match".to_owned()));
    }
    written(stream, asked.pc_proof.bytes(), ended, &mut |_| {})?;
    fill.mark();
    let (got, digest) = archive_read(stream, (size, fault), fill, ended)?;
    if fault == Some(Fault::NoAck) {
        return Err(Stop::Failed(
            "the test seam dropped the channel before the PC's acknowledgement".to_owned(),
        ));
    }
    written(stream, &digest[..], ended, &mut |_| {})?;
    Ok((got, lower_hex(&digest)))
}

/// Reads the archive's `size` bytes from `stream`, each piece handed to `fill`; the count and their SHA-256, else why
/// not. With `fault` a truncation, it stops after half the size and says so; with it an alteration, the first byte
/// read is flipped before it is hashed and handed on.
fn archive_read(
    stream: &Stream,
    (size, fault): (u64, Option<Fault>),
    fill: &mut dyn Fill,
    ended: &dyn Fn() -> bool,
) -> Result<(u64, [u8; 32]), Stop> {
    let taken = if fault == Some(Fault::Truncate) { size / 2 } else { size };
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; CHUNK];
    let mut got = 0_u64;
    while got < taken {
        let want = usize::try_from(taken - got).unwrap_or(usize::MAX).min(CHUNK);
        let read = read_some(stream, &mut buffer[..want], ended)?;
        if read == 0 {
            return Err(Stop::Failed(format!("the box closed the channel after {got} of {size} bytes")));
        }
        if got == 0 && fault == Some(Fault::Alter) {
            buffer[0] ^= 0xff;
        }
        hasher.update(&buffer[..read]);
        fill.chunk(&buffer[..read])?;
        got += read as u64;
    }
    if taken < size {
        return Err(Stop::Failed(format!("the test seam stopped the pull after {got} of {size} bytes")));
    }
    Ok((got, hasher.finalize().into()))
}

/// Sends a put's archive, `put.length` bytes read from `source`, over the channel `put` asks for to the box's `port`,
/// taking `put.fault` when one is named; every wait, the connect's included, lasts at most [`SLICE`] before it looks
/// again at whether `ended` says the call has ended, and gives up once it has. The box's proof is checked before
/// anything is sent; `feed` is marked once the connection is made and once the proofs are exchanged, told each piece
/// written, and told the archive finished once its last byte is written; the ack is then read and compared with the
/// declared SHA-256 to say whether the put is `Pushed`, but a mismatched ack is no verdict: the box's own over ssh is.
/// Whatever the end, the connection is shut down and closed, the PC closing first after the ack.
pub(super) fn pushed(put: &Put, port: u16, source: &mut dyn Read, feed: &mut dyn Feed, ended: &dyn Fn() -> bool) -> Dialled {
    if !port_taken(put.asked.data, port) {
        return Dialled::Failed(format!("the box's port {port} is outside {CHANNEL_FIRST_PORT}-{CHANNEL_LAST_PORT}"));
    }
    let began = Instant::now();
    let stream = match connected(&put.asked, SocketAddrV4::new(put.asked.data, port), ended) {
        Ok(stream) => stream,
        Err(stop) => return stopped(stop),
    };
    feed.mark();
    let end = match delivered(&stream, put, source, feed, ended) {
        Ok(bytes) => Dialled::Pushed {
            bytes,
            took: began.elapsed(),
        },
        Err(stop) => stopped(stop),
    };
    // A shutdown that fails leaves the close alone, which the drop makes either way; how the put went stands.
    stream.shut_down().ok();
    drop(stream);
    end
}

/// A put's exchange over the connected `stream`: the box's proof checked before anything is sent, the PC's sent,
/// `feed` marked, the send buffer set, the archive sent from `source`, and the box's ack read and compared; the bytes
/// sent, else why not.
fn delivered(stream: &Stream, put: &Put, source: &mut dyn Read, feed: &mut dyn Feed, ended: &dyn Fn() -> bool) -> Result<u64, Stop> {
    let mut proof = [0_u8; PROOF_BYTES];
    proof_read(stream, &mut proof, ended)?;
    if put.fault == Some(Fault::BoxProof) || !put.asked.box_proof.matches(&proof) {
        return Err(Stop::Failed("the box's proof does not match".to_owned()));
    }
    written(stream, put.asked.pc_proof.bytes(), ended, &mut |_| {})?;
    feed.mark();
    stream
        .set_send_buffer(put.send_buffer)
        .map_err(|error| Stop::Failed(format!("cannot set the send buffer: {error}")))?;
    let sent = archive_sent(stream, put, source, feed, ended)?;
    feed.finished();
    if put.fault == Some(Fault::NoAck) {
        return Err(Stop::Failed(
            "the test seam dropped the channel before the box's acknowledgement".to_owned(),
        ));
    }
    let mut ack = [0_u8; 32];
    ack_read(stream, &mut ack, ended)?;
    if ack != put.sha256 {
        return Err(Stop::Failed("the box's acknowledgement does not match what was sent".to_owned()));
    }
    Ok(sent)
}

/// Writes the archive's `put.length` bytes from `source` on `stream`, each piece told to `feed` as it is written; the
/// count, else why not. With `put.fault` a truncation, it stops after half the length and says so; with it an
/// alteration, the first byte sent is flipped.
fn archive_sent(stream: &Stream, put: &Put, source: &mut dyn Read, feed: &mut dyn Feed, ended: &dyn Fn() -> bool) -> Result<u64, Stop> {
    let length = put.length;
    let taken = if put.fault == Some(Fault::Truncate) { length / 2 } else { length };
    let mut buffer = vec![0_u8; CHUNK];
    let mut sent = 0_u64;
    while sent < taken {
        let want = usize::try_from(taken - sent).unwrap_or(usize::MAX).min(CHUNK);
        let read = match source.read(&mut buffer[..want]) {
            Ok(0) => return Err(Stop::Failed(format!("the archive ended after {sent} of its {length} bytes"))),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(Stop::Failed(format!("cannot read the archive: {error}"))),
        };
        if sent == 0 && put.fault == Some(Fault::Alter) {
            buffer[0] ^= 0xff;
        }
        written(stream, &buffer[..read], ended, &mut |wrote| feed.sent(wrote))?;
        sent += read as u64;
    }
    if taken < length {
        return Err(Stop::Failed(format!("the test seam stopped the put after {sent} of {length} bytes")));
    }
    Ok(sent)
}

/// Fills `ack` from `stream`, waiting as long as the call goes on, whose stall watch bounds the wait; else why not,
/// the call's end among the reasons.
fn ack_read(stream: &Stream, ack: &mut [u8], ended: &dyn Fn() -> bool) -> Result<(), Stop> {
    let mut filled = 0;
    while filled < ack.len() {
        match read_some(stream, &mut ack[filled..], ended)? {
            0 => return Err(Stop::Failed("the box closed the channel before its acknowledgement".to_owned())),
            read => filled += read,
        }
    }
    Ok(())
}

/// Fills `proof` from `stream` within [`PROOF_WAIT`]; else why not, the call's end among the reasons.
fn proof_read(stream: &Stream, proof: &mut [u8], ended: &dyn Fn() -> bool) -> Result<(), Stop> {
    let end = Instant::now() + PROOF_WAIT;
    let mut filled = 0;
    while filled < proof.len() {
        if ended() {
            return Err(Stop::CallEnded);
        }
        let now = Instant::now();
        if now >= end {
            return Err(Stop::Failed(format!(
                "the box sent too little within {} s before its proof",
                PROOF_WAIT.as_secs()
            )));
        }
        match stream.read(&mut proof[filled..], end.min(now + SLICE)) {
            Ok(Some(0)) => return Err(Stop::Failed("the box closed the channel before its proof".to_owned())),
            Ok(Some(read)) => filled += read,
            Ok(None) => {}
            Err(error) => return Err(Stop::Failed(format!("the channel failed before the box's proof: {error}"))),
        }
    }
    Ok(())
}

/// What `stream` reads into `buffer` once a byte or its end comes: the count, 0 at its end; else why not, the call's end
/// among the reasons.
fn read_some(stream: &Stream, buffer: &mut [u8], ended: &dyn Fn() -> bool) -> Result<usize, Stop> {
    loop {
        if ended() {
            return Err(Stop::CallEnded);
        }
        match stream.read(buffer, Instant::now() + SLICE) {
            Ok(Some(read)) => return Ok(read),
            Ok(None) => {}
            Err(error) => return Err(Stop::Failed(format!("the channel failed: {error}"))),
        }
    }
}

/// Writes all of `bytes` on `stream`, telling `wrote` the count of each write that took some; else why not, the call's
/// end among the reasons.
fn written(stream: &Stream, mut bytes: &[u8], ended: &dyn Fn() -> bool, wrote: &mut dyn FnMut(u64)) -> Result<(), Stop> {
    while !bytes.is_empty() {
        if ended() {
            return Err(Stop::CallEnded);
        }
        match stream.write(bytes, Instant::now() + SLICE) {
            Ok(Some(taken)) => {
                bytes = &bytes[taken..];
                wrote(taken as u64);
            }
            Ok(None) => {}
            Err(error) => return Err(Stop::Failed(format!("the channel failed: {error}"))),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use gate_router::{Builder, BuilderOs};

    use super::planned;

    /// A builder entry running `os` whose `data` key is `data`.
    fn builder(os: BuilderOs, data: Option<Ipv4Addr>) -> Builder {
        Builder {
            alias: "win-2".to_owned(),
            os,
            root: None,
            slots: 1,
            id: Some("084c286b".to_owned()),
            data,
        }
    }

    const TAILNET: Ipv4Addr = Ipv4Addr::new(100, 64, 0, 7);

    #[test]
    fn a_windows_builder_with_data_asks_for_a_channel_to_it() {
        assert_eq!(planned(&builder(BuilderOs::Windows, Some(TAILNET))), Ok(Some(TAILNET)));
    }

    #[test]
    fn a_macos_builder_with_data_asks_for_no_channel() {
        assert_eq!(planned(&builder(BuilderOs::Macos, Some(TAILNET))), Ok(None));
    }

    #[test]
    fn a_builder_without_data_asks_for_no_channel() {
        assert_eq!(planned(&builder(BuilderOs::Windows, None)), Ok(None));
    }

    /// Outside the test seams a loopback `data` is never dialled: the pull goes over ssh, saying why.
    #[cfg(not(feature = "test-seams"))]
    #[test]
    #[expect(clippy::expect_used, reason = "a loopback address taken must fail the case loudly")]
    fn a_loopback_data_address_is_refused_outside_the_test_seams_with_its_not_used_line() {
        let why = planned(&builder(BuilderOs::Windows, Some(Ipv4Addr::LOCALHOST))).expect_err("loopback is refused");

        assert_eq!(
            super::not_used_line(&why),
            "gate: data channel not used: data address 127.0.0.1 is loopback; sent over ssh"
        );
    }

    /// What a pull's fill was handed, in order.
    #[cfg(feature = "test-seams")]
    #[derive(Debug, PartialEq, Eq)]
    enum Seen {
        Mark,
        Chunk(usize),
        Close,
    }

    /// A fill that records what it is handed in `seen`, which the box's side of a case reads too.
    #[cfg(feature = "test-seams")]
    struct Recorded {
        seen: std::sync::Arc<std::sync::Mutex<Vec<Seen>>>,
    }

    #[cfg(feature = "test-seams")]
    impl Recorded {
        #[expect(clippy::expect_used, reason = "a poisoned record must fail the case loudly")]
        fn push(&self, seen: Seen) {
            self.seen.lock().expect("the record is not poisoned").push(seen);
        }
    }

    #[cfg(feature = "test-seams")]
    impl super::Fill for Recorded {
        fn chunk(&mut self, bytes: &[u8]) -> Result<(), String> {
            self.push(Seen::Chunk(bytes.len()));
            Ok(())
        }

        fn mark(&mut self) {
            self.push(Seen::Mark);
        }

        fn close(&mut self) {
            self.push(Seen::Close);
        }
    }

    /// A pull marks the call's progress once its connection is made and once the proofs are exchanged, before the
    /// archive's first byte, so no silence after the box's port line spans the connect and the proof exchange; and it
    /// closes its fill before it shuts the connection down, so the box reads the PC's end only once the archive's file
    /// is closed.
    #[cfg(feature = "test-seams")]
    #[test]
    #[expect(clippy::expect_used, reason = "a case that cannot make its sockets must fail loudly")]
    fn a_pull_marks_progress_at_its_connect_and_its_proof_exchange_and_closes_its_fill_before_its_socket() {
        use std::io::{Read, Write};
        use std::sync::{Arc, Mutex};

        const GUARD: std::time::Duration = std::time::Duration::from_secs(30);
        const ARCHIVE: &[u8] = b"the archive";
        let (box_proof, pc_proof) = ([7_u8; 16], [9_u8; 16]);
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a loopback listener binds");
        let port = listener.local_addr().expect("the listener's address is read").port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_by_box = Arc::clone(&seen);
        let served = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("the PC connects");
            stream.set_read_timeout(Some(GUARD)).expect("the read guard is set");
            stream.write_all(&box_proof).expect("the box sends its proof");
            let mut proof = [0_u8; 16];
            stream.read_exact(&mut proof).expect("the PC sends its proof");
            stream.write_all(ARCHIVE).expect("the box sends the archive");
            let mut ack = [0_u8; 32];
            stream.read_exact(&mut ack).expect("the PC acks");
            let mut rest = Vec::new();
            stream.read_to_end(&mut rest).expect("the PC closes");
            let closed = seen_by_box.lock().expect("the record is not poisoned").contains(&Seen::Close);
            (proof, closed)
        });
        let asked = super::Asked {
            data: Ipv4Addr::LOCALHOST,
            local: Ipv4Addr::LOCALHOST,
            box_proof: gate_wire::Proof::new(box_proof),
            pc_proof: gate_wire::Proof::new(pc_proof),
        };
        let mut fill = Recorded { seen: Arc::clone(&seen) };

        let end = super::pulled(&asked, (port, ARCHIVE.len() as u64, None), &mut fill, &|| false);

        assert!(
            matches!(end, super::Dialled::Pulled { bytes, .. } if bytes == ARCHIVE.len() as u64),
            "{end:?}"
        );
        let (proof, closed_before_the_end) = served.join().expect("the box's side ends");
        assert_eq!(proof, pc_proof, "the PC sent its proof");
        assert!(closed_before_the_end, "the fill was closed before the box read the PC's end");
        let seen = seen.lock().expect("the record is not poisoned");
        assert_eq!(
            seen[..2],
            [Seen::Mark, Seen::Mark],
            "marked at the connect and the proof exchange: {seen:?}"
        );
        assert_eq!(seen.last(), Some(&Seen::Close), "{seen:?}");
        let chunked: usize = seen[2..seen.len() - 1]
            .iter()
            .map(|seen| match seen {
                Seen::Chunk(bytes) => *bytes,
                Seen::Mark | Seen::Close => 0,
            })
            .sum();
        assert_eq!(chunked, ARCHIVE.len(), "{seen:?}");
        assert!(seen[2..seen.len() - 1].iter().all(|seen| matches!(seen, Seen::Chunk(_))), "{seen:?}");
    }

    /// A probe whose box sends a proof that does not match sends nothing back and closes: the PC's proof never reaches a
    /// listener that is not the box; one whose box's proof matches sends the PC's and closes.
    #[cfg(feature = "test-seams")]
    #[test]
    #[expect(clippy::expect_used, reason = "a case that cannot make its sockets must fail loudly")]
    fn a_probe_sends_its_proof_only_to_a_box_whose_proof_matches() {
        use std::io::{Read, Write};

        const GUARD: std::time::Duration = std::time::Duration::from_secs(30);
        let (box_proof, pc_proof) = ([7_u8; 16], [9_u8; 16]);
        for (sent_proof, wanted, pc_sent) in [
            ([1_u8; 16], super::Probed::Failed("the box's proof does not match".to_owned()), Vec::new()),
            (box_proof, super::Probed::Proved, pc_proof.to_vec()),
        ] {
            let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a loopback listener binds");
            let port = listener.local_addr().expect("the listener's address is read").port();
            listener.set_nonblocking(true).expect("the listener does not block");
            // The accept is polled to the guard, so a dial that never connects fails the case as hung, never hangs it.
            let served = std::thread::spawn(move || {
                let hung = std::time::Instant::now() + GUARD;
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if std::time::Instant::now() >= hung {
                                return Err("hung: no connection from the PC".to_owned());
                            }
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(error) => return Err(format!("the accept failed: {error}")),
                    }
                };
                stream.set_nonblocking(false).expect("the connection blocks");
                stream.set_read_timeout(Some(GUARD)).expect("the read guard is set");
                stream.write_all(&sent_proof).expect("the box sends its proof");
                let mut rest = Vec::new();
                stream.read_to_end(&mut rest).expect("the PC closes");
                Ok(rest)
            });
            let asked = super::Asked {
                data: Ipv4Addr::LOCALHOST,
                local: Ipv4Addr::LOCALHOST,
                box_proof: gate_wire::Proof::new(box_proof),
                pc_proof: gate_wire::Proof::new(pc_proof),
            };

            let end = super::probe_dialled(&asked, port, &|| false);

            assert_eq!(end, wanted);
            assert_eq!(served.join().expect("the box's side ends"), Ok(pc_sent), "what the PC sent: {wanted:?}");
        }
    }

    /// What a put's feed was told, in order.
    #[cfg(feature = "test-seams")]
    #[derive(Debug, PartialEq, Eq)]
    enum Told {
        Mark,
        Sent(u64),
        Finished,
    }

    /// A feed that records what it is told.
    #[cfg(feature = "test-seams")]
    #[derive(Default)]
    struct Feeds {
        told: Vec<Told>,
    }

    #[cfg(feature = "test-seams")]
    impl super::Feed for Feeds {
        fn mark(&mut self) {
            self.told.push(Told::Mark);
        }

        fn sent(&mut self, bytes: u64) {
            self.told.push(Told::Sent(bytes));
        }

        fn finished(&mut self) {
            self.told.push(Told::Finished);
        }
    }

    /// A put marks the call's progress once its connection is made and once the proofs are exchanged, counts every
    /// byte of the archive as it is written, and says the archive finished only after its last byte; the box reads
    /// exactly the archive, and then the PC's close after the ack.
    #[cfg(feature = "test-seams")]
    #[test]
    #[expect(clippy::expect_used, reason = "a case that cannot make its sockets must fail loudly")]
    fn a_put_marks_its_connect_and_proofs_counts_each_byte_sent_and_finishes_only_after_the_last() {
        use std::io::{Read, Write};

        use sha2::{Digest, Sha256};

        const GUARD: std::time::Duration = std::time::Duration::from_secs(30);
        const ARCHIVE: &[u8] = b"the archive put over the channel";
        let sha256: [u8; 32] = Sha256::digest(ARCHIVE).into();
        let (box_proof, pc_proof) = ([7_u8; 16], [9_u8; 16]);
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a loopback listener binds");
        let port = listener.local_addr().expect("the listener's address is read").port();
        let served = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("the PC connects");
            stream.set_read_timeout(Some(GUARD)).expect("the read guard is set");
            stream.write_all(&box_proof).expect("the box sends its proof");
            let mut proof = [0_u8; 16];
            stream.read_exact(&mut proof).expect("the PC sends its proof");
            let mut archive = vec![0_u8; ARCHIVE.len()];
            stream.read_exact(&mut archive).expect("the PC sends the archive");
            stream.write_all(&Sha256::digest(&archive)).expect("the box acks");
            let mut rest = Vec::new();
            stream.read_to_end(&mut rest).expect("the PC closes");
            (proof, archive, rest)
        });
        let put = super::Put {
            asked: super::Asked {
                data: Ipv4Addr::LOCALHOST,
                local: Ipv4Addr::LOCALHOST,
                box_proof: gate_wire::Proof::new(box_proof),
                pc_proof: gate_wire::Proof::new(pc_proof),
            },
            length: ARCHIVE.len() as u64,
            sha256,
            send_buffer: super::SEND_BUFFER,
            fault: None,
        };
        let mut feed = Feeds::default();

        let end = super::pushed(&put, port, &mut &ARCHIVE[..], &mut feed, &|| false);

        assert!(
            matches!(end, super::Dialled::Pushed { bytes, .. } if bytes == ARCHIVE.len() as u64),
            "{end:?}"
        );
        let (proof, archive, rest) = served.join().expect("the box's side ends");
        assert_eq!(proof, pc_proof, "the PC sent its proof");
        assert_eq!(archive, ARCHIVE, "the box read the archive whole");
        assert!(rest.is_empty(), "nothing follows the archive: {rest:?}");
        let told = &feed.told;
        assert_eq!(
            told[..2],
            [Told::Mark, Told::Mark],
            "marked at the connect and the proof exchange: {told:?}"
        );
        assert_eq!(told.last(), Some(&Told::Finished), "{told:?}");
        let counted: u64 = told[2..told.len() - 1]
            .iter()
            .map(|told| match told {
                Told::Sent(bytes) => *bytes,
                Told::Mark | Told::Finished => 0,
            })
            .sum();
        assert_eq!(counted, ARCHIVE.len() as u64, "{told:?}");
        assert!(told[2..told.len() - 1].iter().all(|told| matches!(told, Told::Sent(_))), "{told:?}");
    }
}
