//! A folder's security set so only SYSTEM, Administrators and this process's user may use it, its owner one of them,
//! and no process below this one's integrity level may write in it ([`protect_folder`]), an open file's DACL and label set back to what its
//! folder hands down ([`inherit_security`]), and an open file's DACL and label in their string form
//! ([`security_text`]).

use std::fs::File;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_FILE_OBJECT, SetSecurityInfo,
};
use windows_sys::Win32::Security::{
    ACL, ACL_REVISION, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, GetSecurityDescriptorSacl, InitializeAcl,
    LABEL_SECURITY_INFORMATION, OBJECT_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    PSID, UNPROTECTED_DACL_SECURITY_INFORMATION,
};
use windows_sys::core::{BOOL, PWSTR};

use crate::token::token_facts;

/// The security descriptor of the folder [`protect_folder`] protects, for this process's `user` SID, integrity
/// relative ID `integrity` and elevation `elevated`: owned by Administrators when elevated and by `user` when not, a
/// protected DACL granting full control to SYSTEM, Administrators and `user`, handed down to files and folders, and a
/// label of the integrity level that no lower process may write up past, handed down too.
fn protected_sddl(user: &str, integrity: u32, elevated: bool) -> String {
    let owner = if elevated { "BA" } else { user };
    format!("O:{owner}D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{user})S:(ML;OICI;NW;;;S-1-16-{integrity})")
}

/// Sets the open folder `folder`'s owner to Administrators when this process is elevated and to this process's user
/// when it is not, so no other user owns it and may rewrite its DACL; its DACL to a protected one granting full control
/// to SYSTEM, Administrators and this process's user alone; and its mandatory label to this process's integrity level with no write up, both handed down
/// to whatever is made in it. A process may always label an object at or below its own level, so no privilege is
/// needed; an elevated process thus keeps the same user's medium processes from writing in the folder.
///
/// # Errors
///
/// The system's error when this process's token cannot be read or the handle, which must hold `WRITE_DAC` and
/// `WRITE_OWNER`, cannot be given the security, its owner included.
pub fn protect_folder(folder: &File) -> io::Result<()> {
    let facts = token_facts()?;
    let wide: Vec<u16> = protected_sddl(&facts.user, facts.integrity, facts.elevated)
        .encode_utf16()
        .chain([0])
        .collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `wide` is NUL-terminated and outlives the call, and `descriptor` is writable; the size is not asked for.
    let made = unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(wide.as_ptr(), SDDL_REVISION_1, &raw mut descriptor, ptr::null_mut()) };
    if made == 0 {
        return Err(io::Error::last_os_error());
    }
    let set = set_lists_of(folder, descriptor);
    // SAFETY: the descriptor was allocated by the conversion with `LocalAlloc`, and nothing uses it after this.
    unsafe { LocalFree(descriptor) };
    set
}

/// Sets the open `file`'s owner, its DACL, protected, and its label from the security descriptor `descriptor`.
fn set_lists_of(file: &File, descriptor: PSECURITY_DESCRIPTOR) -> io::Result<()> {
    let (mut present, mut defaulted): (BOOL, BOOL) = (0, 0);
    let mut owner: PSID = ptr::null_mut();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut sacl: *mut ACL = ptr::null_mut();
    // SAFETY: `descriptor` is a valid descriptor that outlives the calls, and every out pointer is writable; the owner
    // and the lists returned point into the descriptor.
    let read = unsafe {
        GetSecurityDescriptorOwner(descriptor, &raw mut owner, &raw mut defaulted) != 0
            && GetSecurityDescriptorDacl(descriptor, &raw mut present, &raw mut dacl, &raw mut defaulted) != 0
            && GetSecurityDescriptorSacl(descriptor, &raw mut present, &raw mut sacl, &raw mut defaulted) != 0
    };
    if !read {
        return Err(io::Error::last_os_error());
    }
    let what = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION;
    set_security(file, what, owner, dacl, sacl)
}

/// Sets the open `file`'s DACL and label back to what its folder hands down, dropping every entry of its own: both set
/// unprotected and empty, so the system fills them from the folder's inheritable entries, as for a file made there.
///
/// # Errors
///
/// The system's error when the handle, which must hold `WRITE_DAC` and `WRITE_OWNER`, cannot be given the security.
pub fn inherit_security(file: &File) -> io::Result<()> {
    let mut empty = ACL::default();
    let size = u32::try_from(size_of::<ACL>()).map_err(io::Error::other)?;
    // SAFETY: `empty` is a writable `ACL` of `size` bytes, room for a list of no entries.
    if unsafe { InitializeAcl(&raw mut empty, size, ACL_REVISION) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let what = DACL_SECURITY_INFORMATION | UNPROTECTED_DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION;
    set_security(file, what, ptr::null_mut(), &raw const empty, &raw const empty)
}

/// Sets what `what` names of the open `file`'s security from the owner `owner` and the lists `dacl` and `sacl`.
fn set_security(file: &File, what: OBJECT_SECURITY_INFORMATION, owner: PSID, dacl: *const ACL, sacl: *const ACL) -> io::Result<()> {
    // SAFETY: the handle is open for the whole call, and the owner and each list are valid for it, or null where `what`
    // does not name them; no group is set.
    let error = unsafe { SetSecurityInfo(file.as_raw_handle(), SE_FILE_OBJECT, what, owner, ptr::null_mut(), dacl, sacl) };
    if error != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(error.cast_signed()));
    }
    Ok(())
}

/// The open `file`'s DACL and mandatory label in their string form (SDDL), such as
/// `D:P(A;OICI;FA;;;SY)S:(ML;OICI;NW;;;ME)`; the label part is missing when the file has no label entry.
///
/// # Errors
///
/// The system's error when the handle, which must hold `READ_CONTROL`, cannot be read.
pub fn security_text(file: &File) -> io::Result<String> {
    text_of(file, DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION)
}

/// What `what` names of the open `file`'s security, in its string form.
fn text_of(file: &File, what: OBJECT_SECURITY_INFORMATION) -> io::Result<String> {
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: the handle is open for the whole call, `descriptor` is writable, and no single list is asked for.
    let error = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            what,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    if error != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(error.cast_signed()));
    }
    let text = descriptor_text(descriptor, what);
    // SAFETY: the descriptor was allocated by `GetSecurityInfo` with `LocalAlloc`, and nothing uses it after this.
    unsafe { LocalFree(descriptor) };
    text
}

/// What `what` names of `descriptor`, in its string form.
fn descriptor_text(descriptor: PSECURITY_DESCRIPTOR, what: OBJECT_SECURITY_INFORMATION) -> io::Result<String> {
    let mut wide: PWSTR = ptr::null_mut();
    let mut length = 0_u32;
    // SAFETY: `descriptor` is valid for the call, and `wide` and `length` are writable.
    let made = unsafe { ConvertSecurityDescriptorToStringSecurityDescriptorW(descriptor, SDDL_REVISION_1, what, &raw mut wide, &raw mut length) };
    if made == 0 {
        return Err(io::Error::last_os_error());
    }
    let units = usize::try_from(length).map_err(io::Error::other)?;
    // SAFETY: a success wrote `length` UTF-16 units at `wide`, its NUL included, allocated with `LocalAlloc`.
    let text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(wide, units) });
    // SAFETY: as above; nothing uses the string after this.
    unsafe { LocalFree(wide.cast()) };
    let text = text.trim_end_matches('\0');
    // A label part of no entry, such as `S:AINO_ACCESS_CONTROL` once a label is reset, is no label.
    match text.split_once("S:") {
        Some((dacl, label)) if !label.contains('(') => Ok(dacl.to_owned()),
        _ => Ok(text.to_owned()),
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose files cannot be made must fail loudly")]
mod tests {
    use std::fs::{File, OpenOptions};
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ, READ_CONTROL, WRITE_DAC, WRITE_OWNER};

    use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;

    use super::{inherit_security, protect_folder, security_text, text_of};
    use crate::token::token_facts;

    /// A new, empty folder of the test `name`'s own.
    fn folder(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("gate-win32-security-{name}-{}", std::process::id()));
        if folder.exists() {
            std::fs::remove_dir_all(&folder).expect("an earlier run's folder is removed");
        }
        std::fs::create_dir_all(&folder).expect("the folder can be made");
        folder
    }

    /// `path`, a file or a folder, opened to read and set its security.
    fn securable(path: &Path) -> File {
        OpenOptions::new()
            .access_mode(FILE_GENERIC_READ | READ_CONTROL | WRITE_DAC | WRITE_OWNER | DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .expect("the entry can be opened")
    }

    /// The SDDL alias of the integrity level whose relative ID is `integrity`.
    fn level_alias(integrity: u32) -> String {
        match integrity {
            0x1000 => "LW".to_owned(),
            0x2000 => "ME".to_owned(),
            0x2100 => "MP".to_owned(),
            0x3000 => "HI".to_owned(),
            0x4000 => "SI".to_owned(),
            other => format!("S-1-16-{other}"),
        }
    }

    #[test]
    fn a_protected_folder_grants_system_administrators_and_this_user_and_is_labelled_at_this_level() {
        let folder = folder("protect");
        let facts = token_facts().expect("this process's token can be read");

        protect_folder(&securable(&folder)).expect("the folder can be protected");
        let text = security_text(&securable(&folder)).expect("the folder's security can be read");

        let (dacl, label) = text.split_once("S:").expect("the folder has a label");
        assert!(dacl.starts_with("D:P"), "the DACL is protected: {text}");
        assert_eq!(dacl.matches("(A;").count(), 3, "three grants and no more: {text}");
        for grant in ["(A;OICI;FA;;;SY)", "(A;OICI;FA;;;BA)", &format!("(A;OICI;FA;;;{})", facts.user)] {
            assert!(dacl.contains(grant), "{grant} in {text}");
        }
        // `AI` marks a list set with automatic inheritance, as `SetSecurityInfo` sets it.
        assert_eq!(
            label.trim_start_matches("AI"),
            format!("(ML;OICI;NW;;;{})", level_alias(facts.integrity)),
            "{text}"
        );
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    /// Unelevated, a folder this process makes is its user's already, and elevated, Administrators' by default, so this
    /// case pins the owner after the call but cannot tell a set owner from one kept.
    #[test]
    fn a_protected_folder_is_owned_by_administrators_when_elevated_and_by_this_user_when_not() {
        let folder = folder("owner");
        let facts = token_facts().expect("this process's token can be read");

        protect_folder(&securable(&folder)).expect("the folder can be protected");
        let owner = text_of(&securable(&folder), OWNER_SECURITY_INFORMATION).expect("the folder's owner can be read");

        let expected = if facts.elevated {
            "O:BA".to_owned()
        } else {
            format!("O:{}", facts.user)
        };
        assert_eq!(owner, expected, "{facts:?}");
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_file_moved_out_of_a_protected_folder_takes_its_new_folder_s_security_once_reset() {
        let folder = folder("inherit");
        let (protected, plain) = (folder.join("protected"), folder.join("plain"));
        std::fs::create_dir(&protected).expect("the folder can be made");
        std::fs::create_dir(&plain).expect("the folder can be made");
        protect_folder(&securable(&protected)).expect("the folder can be protected");
        std::fs::write(protected.join("a.txt"), "a").expect("the file can be written");
        std::fs::write(plain.join("fresh.txt"), "fresh").expect("the file can be written");
        std::fs::rename(protected.join("a.txt"), plain.join("a.txt")).expect("the file can be moved");
        let fresh = security_text(&securable(&plain.join("fresh.txt"))).expect("the fresh file's security can be read");
        let moved = securable(&plain.join("a.txt"));
        assert_ne!(
            security_text(&moved).expect("the moved file's security can be read"),
            fresh,
            "a move keeps the old security"
        );

        inherit_security(&moved).expect("the security can be reset");

        assert_eq!(security_text(&moved).expect("the moved file's security can be read"), fresh);
        drop(moved);
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }
}
