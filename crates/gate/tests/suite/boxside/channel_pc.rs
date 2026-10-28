//! The PC's end of a box's data channel as the box suites play it, with a std `TcpStream` on loopback: the case's
//! proofs, the box's channel and refusal lines, and the PC's connect and proof exchange.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};

use serde_json::Value;

use super::field;
use crate::support::{HANG_GUARD, Outcome, Running};

/// The proof the box sends first over a case's data channel.
pub(super) const BOX_PROOF: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";
/// The proof a case's PC sends back.
pub(super) const PC_PROOF: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90";

/// The seam of how long a data channel waits for the PC's connection.
const ACCEPT_MS: &str = "--test-box-channel-accept-ms";
/// The seam of how long a data channel waits for the PC's whole proof.
const PROOF_MS: &str = "--test-box-channel-proof-ms";
/// The window the suites give the accept and proof waits unless a case gives its own: ten minutes, so no case leans
/// on the box's 10 s defaults on a busy machine.
const WINDOW_MS: &str = "600000";

/// The seams `seams` after the accept and proof windows each case gets, leaving out a window `seams` gives itself,
/// since the box refuses a seam given twice.
pub(super) fn with_windows<'a>(seams: &[&'a str]) -> Vec<&'a str> {
    let mut args = Vec::new();
    for seam in [ACCEPT_MS, PROOF_MS] {
        if !seams.contains(&seam) {
            args.extend([seam, WINDOW_MS]);
        }
    }
    args.extend_from_slice(seams);
    args
}

/// The bytes the hex digits `hex` spell.
pub(super) fn hex_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).expect("the case's hex is hex"))
        .collect()
}

/// A loopback port free a moment ago: the system's pick for a listener closed at once.
pub(super) fn free_port() -> u16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a loopback listener binds");
    listener.local_addr().expect("the listener's address is read").port()
}

/// Whether `text` holds the whole line `line`.
pub(super) fn has_line(text: &str, line: &str) -> bool {
    text.lines().any(|said| said == line)
}

/// The data channel line of the call of `payload` on `port`.
pub(super) fn channel_line(payload: &Value, port: u16) -> String {
    format!("gate-remote-channel: {} {port}", field(payload, "Nonce"))
}

/// What a call of `payload` whose data channel did not complete for `why` says on stderr after anything before it.
pub(super) fn not_completed(payload: &Value, why: &str) -> String {
    format!(
        "gate-remote-refused: {} channel\ngate: data channel not completed: {why}\n",
        field(payload, "Nonce")
    )
}

/// Waits for the box to say its data channel line on `port`.
pub(super) fn listening(running: &Running, payload: &Value, port: u16) {
    let line = channel_line(payload, port);
    running.wait_for_stderr_where("the data channel line", |said| has_line(said, &line));
}

/// The PC's connection to the box's data channel on `port`, its reads ending at the hang guard.
pub(super) fn pc_connected(port: u16) -> TcpStream {
    let stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("the PC connects");
    stream.set_read_timeout(Some(HANG_GUARD)).expect("the read guard is set");
    stream
}

/// Reads the box's proof on `stream`, checks it and sends `proof` back, as the PC does.
pub(super) fn proved(stream: &mut TcpStream, proof: &str) {
    let mut box_proof = [0_u8; 16];
    stream.read_exact(&mut box_proof).expect("the box's proof comes");
    assert_eq!(box_proof.to_vec(), hex_bytes(BOX_PROOF), "the box sends its proof first");
    stream.write_all(&hex_bytes(proof)).expect("the PC's proof is sent");
}

/// Fails when a proof's hex shows in what the box said.
pub(super) fn assert_no_proof(outcome: &Outcome) {
    let stdout = String::from_utf8_lossy(&outcome.raw_stdout);
    for proof in [BOX_PROOF, PC_PROOF] {
        assert!(
            !outcome.stderr.contains(proof) && !stdout.contains(proof),
            "a proof is shown: {outcome:?}"
        );
    }
}
