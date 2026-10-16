//! What a drive letter stands for, as `QueryDosDevice` gives it.

use std::io;

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER};
use windows_sys::Win32::Storage::FileSystem::QueryDosDeviceW;

/// The most UTF-16 units a device target is read into before the call is given up as failed.
const MAX_TARGET: usize = 32_768;

/// The device the drive `letter` stands for, as `QueryDosDevice` gives its first target: `\??\X:\dev` for a subst
/// drive, `\Device\HarddiskVolume3` for a volume. `None` when the letter names no device.
///
/// # Errors
///
/// The system's error when the device cannot be read.
pub fn device_target(letter: char) -> io::Result<Option<String>> {
    target_with(letter, |name, buffer| {
        let size = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        // SAFETY: `name` is NUL-terminated, and `buffer` is writable for `size` units, which the call writes at most.
        let length = unsafe { QueryDosDeviceW(name.as_ptr(), buffer.as_mut_ptr(), size) };
        if length != 0 {
            return (length, None);
        }
        (0, io::Error::last_os_error().raw_os_error().and_then(|code| u32::try_from(code).ok()))
    })
}

/// The device the drive `letter` stands for, as `query` answers the system's lookup of `letter:`: how many UTF-16 units
/// it wrote into the buffer and no error, or nothing and the code of the error that refused it. `None` when the letter
/// names no device, else the first of the targets written, which is the one in use.
///
/// # Errors
///
/// The error `query` answered with, when it is neither "no such device" nor "the buffer is too small"; the buffer is
/// grown until it is at [`MAX_TARGET`] units long before that answer is given up on.
fn target_with(letter: char, mut query: impl FnMut(&[u16], &mut [u16]) -> (u32, Option<u32>)) -> io::Result<Option<String>> {
    let name = crate::wide(&format!("{letter}:"));
    let mut buffer = vec![0_u16; 1024];
    loop {
        let (length, error) = query(&name, &mut buffer);
        if length != 0 {
            let written = &buffer[..usize::try_from(length).map_err(io::Error::other)?];
            // The targets are a list of NUL-terminated strings; the first is the one in use.
            let first = written.split(|unit| *unit == 0).next().unwrap_or_default();
            return Ok(Some(String::from_utf16_lossy(first)));
        }
        match error {
            Some(ERROR_FILE_NOT_FOUND) => return Ok(None),
            Some(ERROR_INSUFFICIENT_BUFFER) if buffer.len() < MAX_TARGET => buffer.resize(buffer.len() * 2, 0),
            Some(code) => return Err(io::Error::from_raw_os_error(code.cast_signed())),
            None => return Err(io::Error::other("the device could not be read and the system named no error")),
        }
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose fake query cannot be answered must fail loudly")]
mod tests {
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER};

    use super::{device_target, target_with};

    #[test]
    fn a_query_that_finds_no_device_gives_none() {
        let target = target_with('Q', |_name, _buffer| (0, Some(ERROR_FILE_NOT_FOUND))).expect("a query answers");
        assert_eq!(target, None, "a letter whose device the system does not know names none");
    }

    #[test]
    fn a_query_that_needs_a_bigger_buffer_gives_the_first_target() {
        let mut asked = 0;
        let target = target_with('Q', |_name, buffer| {
            asked += 1;
            if asked == 1 {
                return (0, Some(ERROR_INSUFFICIENT_BUFFER));
            }
            let listed: Vec<u16> = "a\0b\0\0".encode_utf16().collect();
            buffer[..listed.len()].copy_from_slice(&listed);
            (u32::try_from(listed.len()).expect("the list fits the buffer"), None)
        })
        .expect("a query answers");
        assert_eq!(target.as_deref(), Some("a"), "the first target of the list is the one in use");
        assert_eq!(asked, 2, "the answer that needed a bigger buffer made the query ask again");
    }

    #[test]
    fn a_query_refused_for_another_reason_is_an_error() {
        let error = target_with('Q', |_name, _buffer| (0, Some(ERROR_ACCESS_DENIED))).expect_err("a refused query is an error");
        assert_eq!(error.raw_os_error(), Some(ERROR_ACCESS_DENIED.cast_signed()));
    }

    /// A builder's routed command runs under a token the system refuses the device namespace (access denied), which
    /// the route reads as "not a subst drive"; anywhere else C: names its device.
    #[test]
    fn c_names_a_device_or_is_refused() {
        let target = device_target('C');
        let named = target
            .as_ref()
            .is_ok_and(|target| target.as_deref().is_some_and(|target| target.starts_with('\\')));
        let refused = target.as_ref().is_err_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied);
        assert!(named || refused, "C: names a device or its lookup is refused, got {target:?}");
    }
}
