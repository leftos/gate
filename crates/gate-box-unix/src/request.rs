//! The box request: stdin's first line, one base64 word of the JSON payload, read as the Windows box reads it and
//! parsed under [`PathForm::Unix`].

use std::io::{self, BufRead, Read};

use gate_wire::{PathForm, Payload};

/// The most bytes the request line on stdin may take, its newline included.
pub const REQUEST_LIMIT: u64 = 16 * 1024 * 1024;

/// The request on `input`'s first line, decoded under [`PathForm::Unix`]. The line ends at its newline or at the end
/// of the input, and one newline (and a carriage return before it) is not part of it. The read has no deadline: a
/// client that stops sending is ended by its own watchdog, which closes the connection.
///
/// # Errors
///
/// The usage line saying why the request cannot be read: missing, over [`REQUEST_LIMIT`] without a newline, a failed
/// read, not UTF-8, or not base64 JSON.
pub fn read_request(input: impl BufRead) -> Result<Payload, String> {
    let mut line = match bounded_line(input, REQUEST_LIMIT) {
        Ok(line) if line.is_empty() => return Err("gate: the box request is missing on standard input".to_owned()),
        Ok(line) => line,
        Err(Unread::OverLimit) => return Err("gate: the box request on standard input reaches 16 MiB without a newline".to_owned()),
        Err(Unread::Failed(error)) => return Err(format!("gate: the box request on standard input could not be read: {error}")),
    };
    if line.ends_with(b"\n") {
        line.pop();
        if line.ends_with(b"\r") {
            line.pop();
        }
    }
    let word = std::str::from_utf8(&line).map_err(|error| format!("gate: box payload is not base64 JSON: the line is not UTF-8: {error}"))?;
    Payload::decode(word, PathForm::Unix)
}

/// Why a line could not be read.
#[derive(Debug)]
enum Unread {
    /// The line reached its limit without a newline.
    OverLimit,
    /// The input failed.
    Failed(io::Error),
}

/// The next line of `input`, up to and with its newline, or to the end of the input; empty at the end of the input. It
/// never waits past the newline and never reads more than `limit` bytes: a line that reaches `limit` without its
/// newline cannot be read.
fn bounded_line(mut input: impl BufRead, limit: u64) -> Result<Vec<u8>, Unread> {
    let mut line = Vec::new();
    input.by_ref().take(limit).read_until(b'\n', &mut line).map_err(Unread::Failed)?;
    if !line.ends_with(b"\n") && line.len() as u64 >= limit {
        return Err(Unread::OverLimit);
    }
    Ok(line)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a request read the wrong way must fail the test loudly")]
mod tests {
    use std::io::{Cursor, Read};

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use gate_wire::Payload;

    use super::{REQUEST_LIMIT, Unread, bounded_line, read_request};

    /// The request `{"Name":"x","Work":"/g/work/k"}` as its base64 word.
    fn word() -> String {
        STANDARD.encode(r#"{"Name":"x","Work":"/g/work/k"}"#)
    }

    fn read(input: &mut Cursor<Vec<u8>>) -> Payload {
        read_request(input).expect("the request is read")
    }

    fn refused(input: Vec<u8>) -> String {
        read_request(Cursor::new(input)).err().expect("the request is refused")
    }

    #[test]
    fn a_carriage_return_before_the_newline_is_stripped_once() {
        let payload = read(&mut Cursor::new(format!("{}\r\n", word()).into_bytes()));
        assert_eq!(payload.text("Name"), Ok("x"));

        let line = refused(format!("{}\r\r\n", word()).into_bytes());
        assert!(
            line.starts_with("gate: box payload is not base64 JSON"),
            "a second carriage return stays in the word: {line}"
        );
    }

    #[test]
    fn the_payload_is_read_under_the_unix_path_form() {
        let payload = read(&mut Cursor::new(format!("{}\n", word()).into_bytes()));
        assert_eq!(payload.path("Work"), Ok("/g/work/k"), "a slash-rooted path is absolute here");
    }

    #[test]
    fn a_request_with_no_newline_at_the_end_of_the_input_is_read_whole() {
        let payload = read(&mut Cursor::new(word().into_bytes()));
        assert_eq!(payload.text("Name"), Ok("x"));
    }

    #[test]
    fn an_empty_input_is_the_missing_request() {
        assert_eq!(refused(Vec::new()), "gate: the box request is missing on standard input");
    }

    #[test]
    fn a_line_one_byte_under_the_limit_with_its_newline_is_read_and_one_more_is_over() {
        let mut fits = vec![b'a'; 7];
        fits.push(b'\n');
        assert_eq!(
            bounded_line(Cursor::new(fits.clone()), 8).ok(),
            Some(fits),
            "limit - 1 bytes and the newline"
        );

        let mut over = vec![b'a'; 8];
        over.push(b'\n');
        assert!(
            matches!(bounded_line(Cursor::new(over), 8), Err(Unread::OverLimit)),
            "limit bytes before the newline"
        );
    }

    #[test]
    fn a_request_line_past_the_request_limit_is_the_over_cap_line() {
        let mut over = vec![b'a'; usize::try_from(REQUEST_LIMIT).expect("the limit fits a usize")];
        over.push(b'\n');
        assert_eq!(refused(over), "gate: the box request on standard input reaches 16 MiB without a newline");
    }

    #[test]
    fn the_bytes_after_the_request_s_newline_are_left_for_the_next_read() {
        let mut input = Cursor::new(format!("{}\r\n{{\"files\":[]}}\n", word()).into_bytes());

        let payload = read(&mut input);
        let mut rest = String::new();
        input.read_to_string(&mut rest).expect("the rest reads");

        assert_eq!(payload.text("Name"), Ok("x"));
        assert_eq!(rest, "{\"files\":[]}\n", "the manifest in the same chunk is the next read's");
    }

    #[test]
    fn a_request_line_that_is_not_utf_8_is_the_not_base64_line() {
        let line = refused(vec![0xff, 0xfe, b'\n']);
        assert!(line.starts_with("gate: box payload is not base64 JSON: the line is not UTF-8"), "{line}");
    }

    #[test]
    fn a_word_that_is_not_base64_is_the_not_base64_line() {
        let line = refused(b"not base64!\n".to_vec());
        assert!(line.starts_with("gate: box payload is not base64 JSON: "), "{line}");
    }
}
