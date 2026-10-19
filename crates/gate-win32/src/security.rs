//! The security a folder of this process's is made with, so that from the instant it exists only SYSTEM, Administrators
//! and this process's user may use it, its owner one of them, and no process below this one's integrity level may
//! write in it ([`protected_descriptor`], [`Descriptor`]); whether a folder's owner, DACL and label are those
//! ([`is_protected_folder`]); an open file's DACL and label set back to what its folder hands down
//! ([`inherit_security`]), and an open file's DACL and label in their string form ([`security_text`]).

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
    ACL, ACL_REVISION, DACL_SECURITY_INFORMATION, InitializeAcl, LABEL_SECURITY_INFORMATION, OBJECT_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, UNPROTECTED_DACL_SECURITY_INFORMATION,
};
use windows_sys::core::PWSTR;

use crate::token::{TokenFacts, token_facts};

/// A security descriptor, as the system holds it, for a file or folder to be made with ([`crate::make_folder_beneath`]):
/// allocated by the system when made from its string form, and freed when dropped.
pub struct Descriptor(PSECURITY_DESCRIPTOR);

impl Descriptor {
    /// The security descriptor the SDDL `sddl` names, such as `O:BAD:P(A;OICI;FA;;;SY)`.
    ///
    /// # Errors
    ///
    /// The system's error when `sddl` is not a security descriptor's string form.
    pub fn from_sddl(sddl: &str) -> io::Result<Descriptor> {
        let wide: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `wide` is NUL-terminated and outlives the call, and `descriptor` is writable; the size is not asked for.
        let made =
            unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(wide.as_ptr(), SDDL_REVISION_1, &raw mut descriptor, ptr::null_mut()) };
        if made == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Descriptor(descriptor))
    }

    /// The descriptor as the system takes it, valid while `self` lives.
    pub(crate) fn as_ptr(&self) -> PSECURITY_DESCRIPTOR {
        self.0
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: the descriptor was allocated by the conversion with `LocalAlloc`, and nothing uses it after this.
        unsafe { LocalFree(self.0) };
    }
}

/// The SDDL of the security a folder of this process's is made with, for this process's `user` SID, integrity relative
/// ID `integrity` and elevation `elevated`: owned by Administrators when elevated and by `user` when not, a protected
/// DACL granting full control to SYSTEM, Administrators and `user`, handed down to files and folders, and a label of
/// the integrity level that no lower process may write up past, handed down too. The label is not marked protected:
/// a creation descriptor with a protected SACL needs `SeSecurityPrivilege`, which an unelevated box lacks, and a label
/// a folder above hands down does not join a label given at creation.
fn protected_sddl(user: &str, integrity: u32, elevated: bool) -> String {
    let owner = if elevated { "BA" } else { user };
    format!("O:{owner}D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{user})S:(ML;OICI;NW;;;S-1-16-{integrity})")
}

/// The security a folder of the process whose token says `facts` is made with, so that no other account and no lower
/// process may use it from the instant it exists: its owner Administrators when the process is elevated and the
/// process's user when it is not, so no other user owns it and may rewrite its DACL; a protected DACL granting full
/// control to SYSTEM, Administrators and the process's user alone; and a mandatory label of the process's integrity
/// level with no write up, all handed down to whatever is made in it. A process may always label an object at or
/// below its own level, so no privilege is needed; an elevated process thus keeps the same user's medium processes
/// from writing in the folder.
///
/// # Errors
///
/// The system's error when the descriptor cannot be made.
pub fn protected_descriptor(facts: &TokenFacts) -> io::Result<Descriptor> {
    Descriptor::from_sddl(&protected_sddl(&facts.user, facts.integrity, facts.elevated))
}

/// The SDDL of the security a box run's job is made with, for the box's `user` SID and elevation `elevated`: a
/// protected DACL granting full access to SYSTEM and Administrators, and to `user` only `JOB_OBJECT_QUERY` (0x4) when
/// the box is elevated, so the command's unelevated copy of its token may list the job and nothing more, or full
/// access when it is not, as the user's own default DACL grants it, which the command shares anyway.
fn box_job_sddl(user: &str, elevated: bool) -> String {
    let access = if elevated { "0x4" } else { "GA" };
    format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;{access};;;{user})")
}

/// The security a box run's job is made with, for the box whose token says `facts` ([`box_job_sddl`]): a command run
/// with the unelevated copy of an elevated box's token may open the job to list it, as a gate inside the run asks
/// whether the job holds it, where the elevated token's default DACL grants only SYSTEM and Administrators.
///
/// # Errors
///
/// The system's error when the descriptor cannot be made.
pub fn box_job_descriptor(facts: &TokenFacts) -> io::Result<Descriptor> {
    Descriptor::from_sddl(&box_job_sddl(&facts.user, facts.elevated))
}

/// Whether the open folder `folder`, opened with `READ_CONTROL`, has the owner, the DACL and the label
/// [`protected_descriptor`] gives a folder of this process's: owned by Administrators when this process is elevated and
/// by this process's user when it is not, with the protected DACL granting full control to SYSTEM, Administrators and
/// this process's user alone, and the label of this process's integrity level with no write up. The three are compared
/// in their string form, each written by the system, the automatic-inheritance flags left out.
///
/// # Errors
///
/// The system's error when this process's token or the folder's security cannot be read.
pub fn is_protected_folder(folder: &File) -> io::Result<bool> {
    let facts = token_facts()?;
    let what = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION;
    let expected = descriptor_text(protected_descriptor(&facts)?.as_ptr(), what)?;
    Ok(without_auto_inherit(&text_of(folder, what)?) == without_auto_inherit(&expected))
}

/// Whether the open file `file`, opened with `READ_CONTROL`, was made with [`protected_descriptor`]'s security, as
/// [`is_protected_folder`] reads a folder's: the same owner, DACL and label, but for the flags that hand an entry down,
/// which the system keeps on a file's DACL entries and drops from its label's.
///
/// # Errors
///
/// The system's error when this process's token or the file's security cannot be read.
pub fn is_protected_file(file: &File) -> io::Result<bool> {
    let facts = token_facts()?;
    let what = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION;
    let expected = descriptor_text(protected_descriptor(&facts)?.as_ptr(), what)?;
    let handed_down = |text: &str| without_auto_inherit(text).replace(";OICI;", ";;");
    Ok(handed_down(&text_of(file, what)?) == handed_down(&expected))
}

/// The SDDL `text` with the automatic-inheritance flags (`AI`, `AR`) left out of its DACL and its label, which the
/// system sets on a list it was given and the descriptor it was given lacks, its ACE list kept whole; public so a
/// caller may compare two texts whose only difference is those flags.
#[must_use]
pub fn without_auto_inherit(text: &str) -> String {
    let (before, label) = text.split_once("S:").map_or((text, None), |(before, label)| (before, Some(label)));
    let (owner, dacl) = before.split_once("D:").map_or((before, None), |(owner, dacl)| (owner, Some(dacl)));
    let mut stripped = owner.to_owned();
    for (prefix, list) in [("D:", dacl), ("S:", label)] {
        if let Some(list) = list {
            let (flags, entries) = list.split_at(list.find('(').unwrap_or(list.len()));
            stripped.push_str(prefix);
            stripped.push_str(&flags.replace("AI", "").replace("AR", ""));
            stripped.push_str(entries);
        }
    }
    stripped
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
    use std::ptr;

    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetSecurityDescriptorSacl, LABEL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
    };
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ, READ_CONTROL, WRITE_DAC, WRITE_OWNER};
    use windows_sys::core::BOOL;

    use super::{
        Descriptor, box_job_descriptor, box_job_sddl, descriptor_text, inherit_security, is_protected_folder, protected_descriptor, security_text,
        set_security, text_of, without_auto_inherit,
    };
    use crate::file::{Make, Share, make_folder_beneath, open_beneath_as};
    use crate::token::token_facts;

    #[test]
    fn a_box_job_lets_the_elevated_box_s_user_only_list_it_and_the_unelevated_one_do_anything() {
        let user = "S-1-5-21-1-2-3-1001";
        assert_eq!(box_job_sddl(user, true), format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x4;;;{user})"));
        assert_eq!(box_job_sddl(user, false), format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{user})"));
        for elevated in [true, false] {
            let facts = crate::token::TokenFacts {
                elevated,
                ..token_facts().expect("this process's token can be read")
            };
            let descriptor = box_job_descriptor(&facts).expect("the descriptor is made");
            let text = descriptor_text(descriptor.as_ptr(), DACL_SECURITY_INFORMATION).expect("the descriptor reads back");
            // The system writes 0x4 back by the SDDL name it shares with a folder's list-children right.
            let user_entry = if elevated { "LC" } else { "GA" };
            assert_eq!(
                text,
                format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;{user_entry};;;{})", facts.user),
                "elevated {elevated}"
            );
        }
    }

    /// A new, empty folder of the test `name`'s own, granted to this user outright: its DACL is what a case's
    /// assertions are made of, so it must not depend on the ACL the work folder above it hands down. An owner is
    /// granted only `READ_CONTROL` and `WRITE_DAC` by owning, while a case that sets a label or an owner needs
    /// `WRITE_OWNER`, and a builder runs the suite as a user whose work folder may grant it no more than modify.
    fn folder(name: &str) -> PathBuf {
        let folder = crate::test_support::temp_dir().join(format!("gate-win32-security-{name}-{}", std::process::id()));
        if folder.exists() {
            std::fs::remove_dir_all(&folder).expect("an earlier run's folder is removed");
        }
        std::fs::create_dir_all(&folder).expect("the folder can be made");
        let facts = token_facts().expect("this process's token can be read");
        let owned = OpenOptions::new()
            .access_mode(READ_CONTROL | WRITE_DAC)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(&folder)
            .expect("the folder this process just made can be opened to grant its user control");
        set_lists(&owned, &format!("D:(A;OICI;FA;;;{})", facts.user));
        folder
    }

    /// `path`, a file or a folder, opened to read and set its security.
    fn securable(path: &Path) -> File {
        OpenOptions::new()
            .access_mode(FILE_GENERIC_READ | READ_CONTROL | WRITE_DAC | WRITE_OWNER)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .expect("the entry can be opened")
    }

    /// The folder `name` made inside the held folder `parent` with this process's protected security, held for reading.
    fn protected_beneath(parent: &File, name: &str) -> File {
        let facts = token_facts().expect("this process's token can be read");
        let descriptor = protected_descriptor(&facts).expect("the descriptor can be made");
        make_folder_beneath(parent, name, FILE_GENERIC_READ, Share::All, &descriptor).expect("the folder can be made protected")
    }

    /// Gives the open folder `folder` the DACL and the label the SDDL `sddl` names, whichever it holds, as a planter
    /// with the right to would.
    fn set_lists(folder: &File, sddl: &str) {
        let descriptor = Descriptor::from_sddl(sddl).expect("the SDDL names a descriptor");
        let (mut dacl_present, mut sacl_present, mut defaulted): (BOOL, BOOL, BOOL) = (0, 0, 0);
        let (mut dacl, mut sacl) = (ptr::null_mut(), ptr::null_mut());
        // SAFETY: the descriptor outlives the calls, every out pointer is writable, and the lists returned point into it.
        let read = unsafe {
            GetSecurityDescriptorDacl(descriptor.as_ptr(), &raw mut dacl_present, &raw mut dacl, &raw mut defaulted) != 0
                && GetSecurityDescriptorSacl(descriptor.as_ptr(), &raw mut sacl_present, &raw mut sacl, &raw mut defaulted) != 0
        };
        assert!(read, "the descriptor's lists can be read");
        let mut what = 0;
        if dacl_present != 0 {
            what |= DACL_SECURITY_INFORMATION;
        }
        if sacl_present != 0 {
            what |= LABEL_SECURITY_INFORMATION;
        }
        set_security(folder, what, ptr::null_mut(), dacl, sacl).expect("the lists can be set");
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

        let made = protected_beneath(&securable(&folder), "made");
        let text = security_text(&made).expect("the folder's security can be read");

        let (dacl, label) = text.split_once("S:").expect("the folder has a label");
        assert!(dacl.starts_with("D:P"), "the DACL is protected: {text}");
        assert_eq!(dacl.matches("(A;").count(), 3, "three grants and no more: {text}");
        for grant in ["(A;OICI;FA;;;SY)", "(A;OICI;FA;;;BA)", &format!("(A;OICI;FA;;;{})", facts.user)] {
            assert!(dacl.contains(grant), "{grant} in {text}");
        }
        // `AI` marks a list set with automatic inheritance, as the system may mark one it was given at creation.
        assert_eq!(
            label.trim_start_matches("AI"),
            format!("(ML;OICI;NW;;;{})", level_alias(facts.integrity)),
            "{text}"
        );
        drop(made);
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    /// Unelevated, a folder this process makes is its user's already, and elevated, Administrators' by default, so this
    /// case pins the owner of a folder made protected but cannot tell a set owner from one kept.
    #[test]
    fn a_protected_folder_is_owned_by_administrators_when_elevated_and_by_this_user_when_not() {
        let folder = folder("owner");
        let facts = token_facts().expect("this process's token can be read");

        let made = protected_beneath(&securable(&folder), "made");
        let owner = text_of(&made, OWNER_SECURITY_INFORMATION).expect("the folder's owner can be read");

        let expected = if facts.elevated {
            "O:BA".to_owned()
        } else {
            format!("O:{}", facts.user)
        };
        assert_eq!(owner, expected, "{facts:?}");
        drop(made);
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_folder_made_protected_is_protected_on_its_first_handle() {
        let folder = folder("first-handle");
        let parent = securable(&folder);
        set_lists(&parent, "D:(A;OICI;FA;;;WD)");
        let plain = open_beneath_as(&parent, "plain", FILE_GENERIC_READ, Share::All, Make::NewFolder).expect("a plain folder can be made");
        assert!(
            !is_protected_folder(&plain).expect("its security can be read"),
            "the parent hands Everyone's grant down to a folder made without a descriptor: {}",
            security_text(&plain).expect("its security can be read")
        );

        let made = protected_beneath(&parent, "made");

        let text = security_text(&made).expect("the folder's security can be read through its first handle");
        assert!(is_protected_folder(&made).expect("its security can be read"), "{text}");
        assert!(!text.contains(";WD)"), "no grant to Everyone is inherited: {text}");
        drop((made, plain, parent));
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_folder_made_protected_beneath_a_parent_handing_a_label_down_is_still_protected() {
        let folder = folder("inherited-label");
        let parent = securable(&folder);
        set_lists(&parent, "S:(ML;OICI;NW;;;LW)");
        let plain = open_beneath_as(&parent, "plain", FILE_GENERIC_READ, Share::All, Make::NewFolder).expect("a plain folder can be made");
        assert!(
            security_text(&plain).expect("its security can be read").contains("(ML;OICIID;NW;;;LW)"),
            "the parent hands its label down to a folder made without a descriptor: {}",
            security_text(&plain).expect("its security can be read")
        );

        let made = protected_beneath(&parent, "made");

        let text = security_text(&made).expect("the folder's security can be read");
        assert!(is_protected_folder(&made).expect("its security can be read"), "{text}");
        assert!(!text.contains(";LW)"), "the parent's label is not inherited: {text}");
        drop((made, plain, parent));
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_folder_with_the_box_dacl_but_no_label_or_another_dacl_is_not_protected() {
        let folder = folder("not-protected");
        let parent = securable(&folder);
        let facts = token_facts().expect("this process's token can be read");
        let owner = if facts.elevated { "BA".to_owned() } else { facts.user.clone() };
        let unlabelled = Descriptor::from_sddl(&format!("O:{owner}D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{})", facts.user))
            .expect("the SDDL names a descriptor");
        let everyone = Descriptor::from_sddl(&format!("O:{owner}D:P(A;OICI;FA;;;WD)S:(ML;OICI;NW;;;S-1-16-{})", facts.integrity))
            .expect("the SDDL names a descriptor");

        let no_label = make_folder_beneath(&parent, "no-label", FILE_GENERIC_READ, Share::All, &unlabelled).expect("the folder can be made");
        let other_dacl = make_folder_beneath(&parent, "other-dacl", FILE_GENERIC_READ, Share::All, &everyone).expect("the folder can be made");
        let protected = protected_beneath(&parent, "protected");

        assert!(
            !is_protected_folder(&no_label).expect("its security can be read"),
            "the box's DACL without its label"
        );
        assert!(
            !is_protected_folder(&other_dacl).expect("its security can be read"),
            "the box's label without its DACL"
        );
        assert!(is_protected_folder(&protected).expect("its security can be read"), "both");
        drop((no_label, other_dacl, protected, parent));
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn the_automatic_inheritance_flags_are_left_out_of_the_dacl_and_the_label() {
        assert_eq!(
            without_auto_inherit("O:BAD:PAI(A;OICI;FA;;;SY)S:AI(ML;OICI;NW;;;HI)"),
            "O:BAD:P(A;OICI;FA;;;SY)S:(ML;OICI;NW;;;HI)"
        );
        assert_eq!(
            without_auto_inherit("O:BAD:P(A;OICI;FA;;;SY)S:(ML;OICI;NW;;;HI)"),
            "O:BAD:P(A;OICI;FA;;;SY)S:(ML;OICI;NW;;;HI)"
        );
        assert_eq!(without_auto_inherit("O:BAD:AI(A;OICI;FA;;;SY)"), "O:BAD:(A;OICI;FA;;;SY)");
        assert_eq!(without_auto_inherit("O:BA"), "O:BA");
    }

    #[test]
    fn a_file_moved_out_of_a_protected_folder_takes_its_new_folder_s_security_once_reset() {
        let folder = folder("inherit");
        let (protected, plain) = (folder.join("protected"), folder.join("plain"));
        drop(protected_beneath(&securable(&folder), "protected"));
        std::fs::create_dir(&plain).expect("the folder can be made");
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

        assert_eq!(
            without_auto_inherit(&security_text(&moved).expect("the moved file's security can be read")),
            without_auto_inherit(&fresh)
        );
        drop(moved);
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }
}
