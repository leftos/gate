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
    let name = crate::wide(&format!("{letter}:"));
    let mut buffer = vec![0_u16; 1024];
    loop {
        let size = u32::try_from(buffer.len()).map_err(io::Error::other)?;
        // SAFETY: `name` is NUL-terminated, and `buffer` is writable for `size` units, which the call writes at most.
        let length = unsafe { QueryDosDeviceW(name.as_ptr(), buffer.as_mut_ptr(), size) };
        if length != 0 {
            let written = &buffer[..usize::try_from(length).map_err(io::Error::other)?];
            // The targets are a list of NUL-terminated strings; the first is the one in use.
            let first = written.split(|unit| *unit == 0).next().unwrap_or_default();
            return Ok(Some(String::from_utf16_lossy(first)));
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error().and_then(|code| u32::try_from(code).ok()) {
            Some(ERROR_FILE_NOT_FOUND) => return Ok(None),
            Some(ERROR_INSUFFICIENT_BUFFER) if buffer.len() < MAX_TARGET => buffer.resize(buffer.len() * 2, 0),
            _ => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;

    use super::device_target;

    #[test]
    #[expect(
        clippy::print_stderr,
        reason = "a machine whose every letter names a device is reported, not failed, and this one assertion is skipped"
    )]
    fn a_letter_with_no_drive_has_no_device_and_c_has_one() {
        // SAFETY: the call takes no argument; a zero mask only means no drive is known.
        let drives = unsafe { GetLogicalDrives() };
        let free: Vec<char> = (b'A'..=b'Z')
            .filter(|letter| drives & (1 << (letter - b'A')) == 0)
            .map(char::from)
            .collect();

        if free.is_empty() {
            eprintln!("every drive letter names a device; no letter with no drive is left to assert");
        } else {
            assert!(
                free.iter().copied().any(|letter| matches!(device_target(letter), Ok(None))),
                "none of the letters {free:?} with no drive names no device"
            );
        }
        let system = device_target('C').ok().flatten();
        assert!(system.is_some_and(|target| target.starts_with('\\')), "C: names a device");
    }
}
