//! The registry read as a probe of what a machine has installed reads it: the subkeys of one key, each with one string
//! value of its own. Nothing here writes to the registry.

use std::io;

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS, WIN32_ERROR};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_ENUMERATE_SUB_KEYS, KEY_QUERY_VALUE, KEY_WOW64_32KEY, REG_SAM_FLAGS, RRF_RT_REG_SZ, RegCloseKey,
    RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
};

use crate::wide;

/// The root a key is read under, and the view of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hive {
    /// `HKEY_LOCAL_MACHINE` in its 32-bit view, where a 64-bit process finds what lies under `WOW6432Node`.
    LocalMachine32,
    /// `HKEY_CURRENT_USER`.
    CurrentUser,
}

/// One subkey of a key: its name, and the string value asked of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subkey {
    /// The subkey's name.
    pub name: String,
    /// The value asked of it, when it holds a string value of that name that can be read; else `None`.
    pub value: Option<String>,
}

/// The most UTF-16 units a key's name has, its terminating NUL besides.
const KEY_NAME_MAX: usize = 255;

/// The most UTF-16 units a string value read here has, its terminating NUL among them: a longer one reads as none.
const VALUE_MAX: usize = 32 * 1024;

/// An open key, closed when dropped.
struct OpenKey(HKEY);

impl OpenKey {
    /// The key `path` under `parent`, opened for `access`; `None` when there is no such key.
    fn open(parent: HKEY, path: &str, access: REG_SAM_FLAGS) -> io::Result<Option<OpenKey>> {
        let path = wide(path);
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: `path` is NUL-terminated and outlives the call, and `key` is a place for the handle the call opens.
        let code = unsafe { RegOpenKeyExW(parent, path.as_ptr(), 0, access, &raw mut key) };
        match code {
            ERROR_SUCCESS => Ok(Some(OpenKey(key))),
            ERROR_FILE_NOT_FOUND => Ok(None),
            code => Err(os_error(code)),
        }
    }

    /// The name of the subkey at `index`; `None` past the last one.
    fn subkey_name(&self, index: u32) -> io::Result<Option<String>> {
        let mut name = [0u16; KEY_NAME_MAX + 1];
        let mut length = u32::try_from(name.len()).unwrap_or(u32::MAX);
        let (reserved, class, class_length, written) = (std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
        // SAFETY: the key is open, `name` holds `length` units, and the class and time it may give are not asked for.
        let code = unsafe { RegEnumKeyExW(self.0, index, name.as_mut_ptr(), &raw mut length, reserved, class, class_length, written) };
        match code {
            ERROR_SUCCESS => Ok(Some(String::from_utf16_lossy(&name[..(length as usize).min(KEY_NAME_MAX)]))),
            ERROR_NO_MORE_ITEMS => Ok(None),
            code => Err(os_error(code)),
        }
    }

    /// The string value `value` of the subkey `subkey`; `None` when it has none of that name or type, it is longer
    /// than [`VALUE_MAX`], or it cannot be read.
    fn string(&self, subkey: &str, value: &str) -> Option<String> {
        let subkey = wide(subkey);
        let value = wide(value);
        let mut text = vec![0u16; VALUE_MAX];
        let mut bytes = u32::try_from(text.len() * 2).unwrap_or(u32::MAX);
        // SAFETY: the key is open, both names are NUL-terminated and outlive the call, and `text` holds `bytes` bytes.
        let code = unsafe {
            RegGetValueW(
                self.0,
                subkey.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                text.as_mut_ptr().cast(),
                &raw mut bytes,
            )
        };
        if code != ERROR_SUCCESS {
            return None;
        }
        let units = (bytes as usize / 2).min(text.len());
        Some(String::from_utf16_lossy(&text[..units]).trim_end_matches('\0').to_owned())
    }
}

impl Drop for OpenKey {
    fn drop(&mut self) {
        // SAFETY: the key was opened by `RegOpenKeyExW`, this guard alone owns it, and it is closed once.
        unsafe { RegCloseKey(self.0) };
    }
}

/// The error of the Win32 code `code`, its code kept.
fn os_error(code: WIN32_ERROR) -> io::Error {
    io::Error::from_raw_os_error(code.cast_signed())
}

/// The subkeys of the key `path` under `hive`, at most `most` of them in the registry's order, each with its string
/// value `value`; `None` when there is no such key.
///
/// # Errors
///
/// The system's error, its code kept, when the key cannot be opened or its subkeys cannot be listed.
pub fn registry_subkeys(hive: Hive, path: &str, value: &str, most: usize) -> io::Result<Option<Vec<Subkey>>> {
    let access = KEY_ENUMERATE_SUB_KEYS | KEY_QUERY_VALUE;
    let (root, access) = match hive {
        Hive::LocalMachine32 => (HKEY_LOCAL_MACHINE, access | KEY_WOW64_32KEY),
        Hive::CurrentUser => (HKEY_CURRENT_USER, access),
    };
    let Some(key) = OpenKey::open(root, path, access)? else {
        return Ok(None);
    };
    let mut subkeys = Vec::new();
    for index in 0..u32::try_from(most).unwrap_or(u32::MAX) {
        let Some(name) = key.subkey_name(index)? else {
            break;
        };
        let value = key.string(&name, value);
        subkeys.push(Subkey { name, value });
    }
    Ok(Some(subkeys))
}
