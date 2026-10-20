//! SHA-256 over a stream, read through a fixed buffer so that a file of any length hashes in the same memory.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::outputs::walk::in_context;

/// The hex digit of each hash byte's high nibble, the low nibble reading from the same table.
const HEX: &[u8; 16] = b"0123456789abcdef";

/// A reader that hashes and counts every byte read through it.
pub(crate) struct Hashing<R> {
    inner: R,
    hasher: Sha256,
    count: u64,
}

impl<R: Read> Hashing<R> {
    pub(crate) fn new(inner: R) -> Hashing<R> {
        Hashing {
            inner,
            hasher: Sha256::new(),
            count: 0,
        }
    }

    /// The SHA-256 of the bytes read so far, as lower-case hex, and their count.
    pub(crate) fn finish(self) -> (String, u64) {
        (hex(&self.hasher.finalize()), self.count)
    }
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.hasher.update(&buffer[..read]);
        self.count += read as u64;
        Ok(read)
    }
}

/// The SHA-256 of everything `reader` holds, read to its end through a fixed buffer, and the count of its bytes.
pub(crate) fn sha256_of(reader: impl Read) -> io::Result<(String, u64)> {
    let mut hashing = Hashing::new(reader);
    io::copy(&mut hashing, &mut io::sink())?;
    Ok(hashing.finish())
}

/// The SHA-256 of the file at `path`; else why it cannot be read, its path named.
pub(crate) fn file_sha256(path: &Path) -> io::Result<String> {
    let file = File::open(path).map_err(|error| in_context(path, &error))?;
    sha256_of(file).map(|(sha256, _)| sha256).map_err(|error| in_context(path, &error))
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(HEX[usize::from(byte >> 4)]));
        text.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    text
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose reader fails must fail loudly")]
mod tests {
    use std::io::{self, Read};

    use sha2::{Digest, Sha256};

    use gate_wire::{SHA256_DIGITS, is_lower_hex};

    use super::{file_sha256, hex, sha256_of};

    fn is_sha256(text: &str) -> bool {
        is_lower_hex(text, SHA256_DIGITS)
    }

    /// A reader of `bytes` that gives at most seven of them per read, so a stream crosses every buffer boundary.
    struct Trickle<'a>(&'a [u8]);

    impl Read for Trickle<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let count = self.0.len().min(buffer.len()).min(7);
            buffer[..count].copy_from_slice(&self.0[..count]);
            self.0 = &self.0[count..];
            Ok(count)
        }
    }

    #[test]
    fn the_known_digests_come_out_as_lower_case_hex() {
        let (empty, _) = sha256_of(io::empty()).expect("nothing hashes");
        assert_eq!(empty, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        let (abc, count) = sha256_of(&b"abc"[..]).expect("abc hashes");
        assert_eq!(abc, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(count, 3);
    }

    #[test]
    fn a_stream_hashes_to_what_one_pass_over_its_bytes_does() {
        let bytes: Vec<u8> = (0..200_003_u32).map(|index| u8::try_from(index % 251).expect("below 251")).collect();
        let (streamed, count) = sha256_of(Trickle(&bytes)).expect("the stream hashes");
        assert_eq!(streamed, hex(&Sha256::digest(&bytes)));
        assert_eq!(count, 200_003);
    }

    #[test]
    fn a_file_that_cannot_be_read_is_named() {
        let missing = crate::test_support::temp_dir().join(format!("gate-hash-{}-absent", std::process::id()));
        let error = file_sha256(&missing).expect_err("a missing file does not hash");
        assert!(error.to_string().starts_with(&missing.display().to_string()), "{error}");
    }

    #[test]
    fn a_sha256_is_64_lower_case_hex_digits() {
        assert!(is_sha256(&"0a".repeat(32)));
        assert!(!is_sha256(&"0A".repeat(32)), "upper case");
        assert!(!is_sha256(&"0a".repeat(31)), "62 digits");
        assert!(!is_sha256(&format!("{}0", "0a".repeat(32))), "65 digits");
        assert!(!is_sha256(&"0g".repeat(32)), "not hex");
    }
}
