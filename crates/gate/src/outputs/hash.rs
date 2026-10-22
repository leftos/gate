//! SHA-256 of a file on this PC; the stream hash itself is [`gate_inputs::hash`]'s, re-exported here.

use std::fs::File;
use std::io;
use std::path::Path;

pub(crate) use gate_inputs::hash::{Hashing, sha256_of};

use crate::outputs::walk::in_context;

/// The SHA-256 of the file at `path`; else why it cannot be read, its path named.
pub(crate) fn file_sha256(path: &Path) -> io::Result<String> {
    let file = File::open(path).map_err(|error| in_context(path, &error))?;
    sha256_of(file).map(|(sha256, _)| sha256).map_err(|error| in_context(path, &error))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose reader fails must fail loudly")]
mod tests {
    use super::file_sha256;

    #[test]
    fn a_file_that_cannot_be_read_is_named() {
        let missing = crate::test_support::temp_dir().join(format!("gate-hash-{}-absent", std::process::id()));
        let error = file_sha256(&missing).expect_err("a missing file does not hash");
        assert!(error.to_string().starts_with(&missing.display().to_string()), "{error}");
    }
}
