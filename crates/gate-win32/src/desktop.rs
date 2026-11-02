//! A desktop this process creates for a command no one is to see.

use std::io;
use std::mem::ManuallyDrop;

use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::GENERIC_ALL;
use windows_sys::Win32::System::StationsAndDesktops::{CloseDesktop, CreateDesktopW, GetProcessWindowStation};

/// A desktop this process created in its own window station, closed (`CloseDesktop`) when dropped. Its handle is not
/// inheritable: a [`crate::DesktopLaunch`] on it hands its command a copy made for the call, so the command holds the
/// desktop from its creation on, and the desktop lives while any handle to it is open, this one or a command's.
pub struct HiddenDesktop {
    /// The desktop's handle, closed by `CloseDesktop` in [`Drop`], never by the guard's own `CloseHandle`.
    handle: ManuallyDrop<OwnedHandle>,
    /// `<station>\<name>`, as `STARTUPINFO.lpDesktop` names it.
    full_name: String,
}

impl HiddenDesktop {
    /// Creates the desktop `name` in this process's window station (`CreateDesktopW` with `GENERIC_ALL`, the default
    /// security of this process's token and a handle that is not inheritable), the station's name read first
    /// (`GetProcessWindowStation`, then `GetUserObjectInformationW` with `UOI_NAME`).
    ///
    /// # Errors
    ///
    /// An error of kind [`io::ErrorKind::InvalidInput`] for a name that is empty or holds a NUL or a backslash; else the
    /// system's error when the station cannot be read or the desktop cannot be created.
    pub fn create(name: &str) -> io::Result<HiddenDesktop> {
        check_name(name)?;
        // SAFETY: the call takes no argument; its handle is this process's own and is never closed.
        let station = unsafe { GetProcessWindowStation() };
        if station.is_null() {
            return Err(io::Error::last_os_error());
        }
        let station_name = crate::session::object_name(station)?;
        let wide = crate::wide(name);
        // SAFETY: `wide` is NUL-terminated and outlives the call; no device, mode or security attributes are passed: a
        // desktop of the default display, with the default security and a handle that is not inheritable.
        let desktop = unsafe { CreateDesktopW(wide.as_ptr(), std::ptr::null(), std::ptr::null(), 0, GENERIC_ALL, std::ptr::null()) };
        if desktop.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a success is a fresh handle nothing else owns; the guard is never dropped, only closed in `Drop`.
        let Some(handle) = (unsafe { OwnedHandle::new(desktop) }) else {
            // SAFETY: the handle was created above and is closed once, here.
            unsafe { CloseDesktop(desktop) };
            return Err(io::Error::other("the desktop handle CreateDesktopW returned was refused"));
        };
        Ok(HiddenDesktop {
            handle: ManuallyDrop::new(handle),
            full_name: format!(r"{station_name}\{name}"),
        })
    }

    /// `<station>\<name>`: the window station it was created in and its own name.
    #[must_use]
    pub fn full_name(&self) -> &str {
        &self.full_name
    }

    /// The desktop's handle as the launch's handle list takes it.
    pub(crate) fn owned(&self) -> &OwnedHandle {
        &self.handle
    }
}

impl Drop for HiddenDesktop {
    fn drop(&mut self) {
        // SAFETY: the handle is open and closed once, here; the guard is never dropped. `CloseDesktop` fails only for a
        // desktop a thread of this process is on, which no thread joins: a failure leaves a handle the process's end
        // closes, and nothing to report from a drop.
        unsafe { CloseDesktop(self.handle.raw()) };
    }
}

/// Whether a desktop named `name` exists in this process's window station (`OpenDesktopW` with `DESKTOP_READOBJECTS`,
/// the handle closed at once): the tests' look at whether a hidden desktop outlived its run, the black-box suite's
/// among them. Test seams only.
///
/// # Errors
///
/// An error of kind [`io::ErrorKind::InvalidInput`] for a name [`HiddenDesktop::create`] refuses; else the system's
/// error when the desktop cannot be opened for any reason but that none of the name exists.
#[cfg(any(test, feature = "test-seams"))]
pub fn desktop_exists(name: &str) -> io::Result<bool> {
    use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows_sys::Win32::System::StationsAndDesktops::{DESKTOP_READOBJECTS, OpenDesktopW};

    check_name(name)?;
    let wide = crate::wide(name);
    // SAFETY: `wide` is NUL-terminated and outlives the call; a null result is a failure, read at once.
    let desktop = unsafe { OpenDesktopW(wide.as_ptr(), 0, 0, DESKTOP_READOBJECTS) };
    if desktop.is_null() {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(code) if code == ERROR_FILE_NOT_FOUND.cast_signed() => Ok(false),
            _ => Err(error),
        };
    }
    // SAFETY: the handle was opened above, is used by nothing else and is closed once.
    unsafe { CloseDesktop(desktop) };
    Ok(true)
}

/// Refuses a desktop name that is empty or holds a NUL, which would cut it short, or a backslash, which names a station.
fn check_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name.contains(['\0', '\\']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a desktop name is not empty and holds no NUL and no backslash",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose desktop cannot be made or looked for must fail loudly")]
mod tests {
    use super::{HiddenDesktop, desktop_exists};

    /// A desktop name no other test process uses: nextest runs each test in a process of its own.
    fn unique(tag: &str) -> String {
        format!("gate-test-{}-{tag}", std::process::id())
    }

    /// Whether the desktop `name` exists in this process's window station.
    fn exists(name: &str) -> bool {
        desktop_exists(name).expect("the desktop is looked for")
    }

    #[test]
    fn a_hidden_desktop_is_named_in_its_station_and_opens_while_it_lives() {
        let name = unique("lives");

        let desktop = HiddenDesktop::create(&name).expect("the desktop is created");

        let full = desktop.full_name();
        assert!(full.ends_with(&format!(r"\{name}")), "{full}");
        let station = full.strip_suffix(&format!(r"\{name}")).expect("the name ends the full name");
        assert!(!station.is_empty() && !station.contains('\\'), "one station name before it: {full}");
        assert!(exists(&name), "the desktop opens while it lives");
    }

    #[test]
    fn a_hidden_desktop_nothing_else_holds_is_gone_once_dropped() {
        let name = unique("gone");
        let desktop = HiddenDesktop::create(&name).expect("the desktop is created");

        drop(desktop);

        assert!(!exists(&name), "no desktop of the name is left");
    }

    #[test]
    fn two_hidden_desktop_names_give_two_desktops() {
        let (first, second) = (unique("first"), unique("second"));

        let one = HiddenDesktop::create(&first).expect("the first desktop is created");
        let other = HiddenDesktop::create(&second).expect("the second desktop is created");

        assert_ne!(one.full_name(), other.full_name());
        drop(one);
        assert!(!exists(&first), "the first is gone once dropped");
        assert!(exists(&second), "the second lives on beside it");
    }
}
