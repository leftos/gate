//! `gate-box box boot`: the builder's last boot, `gate: remote boot <time>`, read from `kern.boottime`.

use crate::console::Console;
use crate::lines::{FAILED_STATUS, boot_failure, boot_line};

/// Says the boot line on stdout, or why the boot time cannot be read on stderr; whether it was read. Only the `boot`
/// verb turns the failure into a status ([`status`]); the preflight fails nothing.
pub fn say(console: &mut Console) -> bool {
    match boot_time().and_then(|(seconds, micros)| boot_line(seconds, micros)) {
        Ok(line) => {
            console.out(&line);
            true
        }
        Err(message) => {
            console.err(&boot_failure(&message));
            false
        }
    }
}

/// The status of `boot`: 0 when the boot time was `read`, else [`FAILED_STATUS`], so a boot query after a lost
/// connection carries the failure rather than a status 0 with no time.
#[must_use]
pub fn status(read: bool) -> u8 {
    if read { 0 } else { FAILED_STATUS }
}

/// The boot time as seconds and microseconds after the Unix epoch, from `sysctl kern.boottime`; else why not.
#[cfg(target_os = "macos")]
pub(crate) fn boot_time() -> Result<(i64, i64), String> {
    let mut time = libc::timeval { tv_sec: 0, tv_usec: 0 };
    let mut size = std::mem::size_of::<libc::timeval>();
    // SAFETY: the name is a NUL-terminated C string, and the old value and its size point at a `timeval` and its size,
    // which `sysctlbyname` fills no further than that size; no new value is given.
    let read = unsafe { libc::sysctlbyname(c"kern.boottime".as_ptr(), (&raw mut time).cast(), &raw mut size, std::ptr::null_mut(), 0) };
    if read != 0 {
        return Err(format!("sysctl kern.boottime failed: {}", std::io::Error::last_os_error()));
    }
    if size != std::mem::size_of::<libc::timeval>() {
        return Err(format!("sysctl kern.boottime gave {size} bytes, not a timeval"));
    }
    Ok((time.tv_sec, i64::from(time.tv_usec)))
}

/// The boot time cannot be read on a Unix other than macOS.
#[cfg(not(target_os = "macos"))]
pub(crate) fn boot_time() -> Result<(i64, i64), String> {
    Err(format!("reading the boot time is not built for {}", std::env::consts::OS))
}

#[cfg(test)]
mod tests {
    use super::{say, status};
    use crate::console::Console;
    use crate::lines::FAILED_STATUS;

    #[test]
    fn a_boot_time_that_cannot_be_read_is_status_1() {
        assert_eq!(status(true), 0);
        assert_eq!(status(false), FAILED_STATUS);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_boot_time_is_read_as_the_boot_line() {
        let mut console = Console::captured();
        assert!(say(&mut console), "{:?}", console.err);
        assert_eq!(console.out.len(), 1);
        assert!(console.out[0].starts_with("gate: remote boot "), "{:?}", console.out);
        assert!(console.out[0].ends_with('Z'), "{:?}", console.out);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn the_boot_time_is_not_read_off_macos() {
        let mut console = Console::captured();
        assert!(!say(&mut console));
        assert!(console.out.is_empty());
    }
}
