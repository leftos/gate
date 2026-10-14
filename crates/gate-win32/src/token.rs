//! A restricted copy of this process's token, for a command that must not run with the elevation this process may
//! have: what `CreateRestrictedToken` with `LUA_TOKEN` makes of it, lowered to medium integrity when it is above that,
//! owning the objects it creates as its user and granting them by default to that user and to SYSTEM alone. And what a
//! process's own token says of its elevation, its integrity, the owner of the objects it creates, its user and its
//! default DACL.

use std::ffi::c_void;
use std::io;
use std::marker::PhantomData;

use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::{GENERIC_ALL, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAce, CreateRestrictedToken, CreateWellKnownSid, EqualSid, GetAce, GetLengthSid,
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, InitializeAcl, LUA_TOKEN, PSID, SECURITY_MAX_SID_SIZE, SID_AND_ATTRIBUTES,
    SetTokenInformation, TOKEN_ACCESS_MASK, TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY, TOKEN_DEFAULT_DACL, TOKEN_DUPLICATE, TOKEN_ELEVATION,
    TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER, TokenDefaultDacl, TokenElevation, TokenIntegrityLevel,
    TokenOwner, TokenUser, WELL_KNOWN_SID_TYPE, WinLocalSystemSid, WinMediumLabelSid,
};
use windows_sys::Win32::System::SystemServices::{ACCESS_ALLOWED_ACE_TYPE, SE_GROUP_INTEGRITY, SECURITY_MANDATORY_MEDIUM_RID};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::core::PWSTR;

use crate::own_handle;

/// A primary token restricted from this process's own: `CreateRestrictedToken` with `LUA_TOKEN`, which leaves it
/// unelevated, its Administrators group deny-only and its privileges cut to a standard user's; at medium integrity, or
/// lower when this process's token already was; owning the objects it creates as its user; and with a default DACL
/// granting `GENERIC_ALL` to that user and to SYSTEM, so a process it starts can open its own process, threads and
/// console. Being a restricted copy of this process's token, it starts a process without `SeAssignPrimaryTokenPrivilege`.
pub struct RestrictedToken(OwnedHandle);

impl RestrictedToken {
    /// This process's token, restricted as [`RestrictedToken`] says.
    ///
    /// # Errors
    ///
    /// The system's error when this process's token cannot be opened, restricted, read or changed.
    pub fn of_this_process() -> io::Result<RestrictedToken> {
        let own = this_process_token(TOKEN_ASSIGN_PRIMARY | TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ADJUST_DEFAULT)?;
        let mut restricted: HANDLE = std::ptr::null_mut();
        // SAFETY: `own` is open with `TOKEN_DUPLICATE`; no SID to disable, privilege to delete or restricting SID is
        // passed, each a count of 0 with a null array, and `restricted` is writable.
        let made = unsafe {
            CreateRestrictedToken(
                own.raw(),
                LUA_TOKEN,
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                &raw mut restricted,
            )
        };
        if made == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a success returns a fresh token handle nothing else owns, with the access of `own`.
        let token = unsafe { own_handle(restricted) }?;
        lower_to_medium(&token)?;
        let user = Information::<TOKEN_USER>::read(&token, TokenUser)?;
        let user_sid = user.get().User.Sid;
        set_owner(&token, user_sid)?;
        set_default_dacl(&token, user_sid)?;
        Ok(RestrictedToken(token))
    }

    /// The token's handle, for the call that starts a process with it.
    pub(crate) fn handle(&self) -> &OwnedHandle {
        &self.0
    }
}

/// What a token says of itself: whether it is elevated, its integrity level, whether the objects it creates are owned
/// by its user, that user and its default DACL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenFacts {
    /// Whether the token is elevated (`TokenElevation`).
    pub elevated: bool,
    /// The relative ID of its integrity level: 0x2000 for medium, 0x3000 for high.
    pub integrity: u32,
    /// Whether its default owner, the owner of the objects it creates, is its user.
    pub owner_is_user: bool,
    /// Its user's SID, in its string form (`S-1-5-21-…`).
    pub user: String,
    /// The entries of its default DACL, the one the objects it creates without a security descriptor get, in their
    /// order; `None` for a null default DACL, which grants everyone everything.
    pub default_dacl: Option<Vec<Grant>>,
}

/// An access-allowed entry of a DACL: the SID it names, in its string form, and the access mask it allows, as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    /// The SID, in its string form (`S-1-5-18`).
    pub sid: String,
    /// The access mask the entry allows.
    pub mask: u32,
}

/// What this process's own token says of itself.
///
/// # Errors
///
/// The system's error when the token cannot be opened or read.
pub fn token_facts() -> io::Result<TokenFacts> {
    facts_of(&this_process_token(TOKEN_QUERY)?)
}

/// What `token`, open with `TOKEN_QUERY`, says of itself.
fn facts_of(token: &OwnedHandle) -> io::Result<TokenFacts> {
    let elevation = Information::<TOKEN_ELEVATION>::read(token, TokenElevation)?;
    let user = Information::<TOKEN_USER>::read(token, TokenUser)?;
    let owner = Information::<TOKEN_OWNER>::read(token, TokenOwner)?;
    // SAFETY: both SIDs point into the buffers the system wrote them in, which live across the call.
    let owner_is_user = unsafe { EqualSid(user.get().User.Sid, owner.get().Owner) } != 0;
    Ok(TokenFacts {
        elevated: elevation.get().TokenIsElevated != 0,
        integrity: integrity_of(token)?,
        owner_is_user,
        // SAFETY: the SID points into the buffer the system wrote it in, which lives across the call.
        user: unsafe { sid_string(user.get().User.Sid) }?,
        default_dacl: default_dacl_of(token)?,
    })
}

/// The entries of `token`'s default DACL, in their order; `None` for a null default DACL.
///
/// # Errors
///
/// The system's error when the DACL cannot be read, or an error naming an entry that does not allow access.
fn default_dacl_of(token: &OwnedHandle) -> io::Result<Option<Vec<Grant>>> {
    let dacl = Information::<TOKEN_DEFAULT_DACL>::read(token, TokenDefaultDacl)?;
    let acl = dacl.get().DefaultDacl;
    if acl.is_null() {
        return Ok(None);
    }
    // SAFETY: `acl` points to the ACL the system wrote into `dacl`'s buffer, which outlives these reads.
    let count = unsafe { (*acl).AceCount };
    // SAFETY: as above, and each index is below the ACL's count of entries.
    let grants = (0..u32::from(count)).map(|index| unsafe { grant_at(acl, index) });
    grants.collect::<io::Result<Vec<Grant>>>().map(Some)
}

/// The entry `index` of `acl`, which must allow access.
///
/// # Safety
///
/// `acl` points to a valid ACL holding more than `index` entries, for the whole call.
unsafe fn grant_at(acl: *const ACL, index: u32) -> io::Result<Grant> {
    let mut ace = std::ptr::null_mut();
    // SAFETY: the caller guarantees `acl` and `index`, and `ace` is writable.
    if unsafe { GetAce(acl, index, &raw mut ace) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let ace: *const ACCESS_ALLOWED_ACE = ace.cast_const().cast();
    // SAFETY: every entry starts with its header, within the ACL.
    let kind = unsafe { (*ace).Header.AceType };
    if u32::from(kind) != ACCESS_ALLOWED_ACE_TYPE {
        return Err(io::Error::other(format!(
            "entry {index} of the default DACL is of type {kind}, not one that allows access"
        )));
    }
    // SAFETY: an access-allowed entry holds its mask and, from `SidStart`, its SID, both within the entry.
    let (mask, sid) = unsafe { ((*ace).Mask, (&raw const (*ace).SidStart).cast_mut().cast()) };
    // SAFETY: `sid` is the entry's SID, valid as the ACL is.
    Ok(Grant {
        sid: unsafe { sid_string(sid) }?,
        mask,
    })
}

/// The string form of `sid` (`S-1-5-18`).
///
/// # Safety
///
/// `sid` points to a valid SID for the whole call.
unsafe fn sid_string(sid: PSID) -> io::Result<String> {
    let mut text: PWSTR = std::ptr::null_mut();
    // SAFETY: the caller guarantees `sid`, and `text` is writable.
    if unsafe { ConvertSidToStringSidW(sid, &raw mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut length = 0;
    // SAFETY: a success leaves `text` a NUL-terminated string the system allocated, so every read up to its NUL is
    // within it.
    while unsafe { *text.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: as above: `length` characters precede the NUL.
    let decoded = String::from_utf16(unsafe { std::slice::from_raw_parts(text, length) });
    // SAFETY: the system allocated `text` with `LocalAlloc`; it is freed once, after its last read.
    unsafe { LocalFree(text.cast()) };
    decoded.map_err(|error| io::Error::other(format!("a SID's string form is not UTF-16: {error}")))
}

/// This process's token, opened with `access`.
fn this_process_token(access: TOKEN_ACCESS_MASK) -> io::Result<OwnedHandle> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: the pseudo-handle of this process is always open, and `token` is writable.
    if unsafe { OpenProcessToken(GetCurrentProcess(), access, &raw mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a success returns a fresh token handle nothing else owns.
    unsafe { own_handle(token) }
}

/// A token's information of one class, in a buffer aligned for the structure `T` it starts with, which may point
/// further into the buffer.
struct Information<T> {
    buffer: Vec<usize>,
    kind: PhantomData<T>,
}

impl<T> Information<T> {
    /// The information of `class`, whose structure is `T`, that `token`, open with `TOKEN_QUERY`, holds.
    fn read(token: &OwnedHandle, class: TOKEN_INFORMATION_CLASS) -> io::Result<Information<T>> {
        let mut size = 0;
        // SAFETY: a null buffer of no size asks only for the size the information needs, written to `size`.
        unsafe { GetTokenInformation(token.raw(), class, std::ptr::null_mut(), 0, &raw mut size) };
        if (size as usize) < size_of::<T>() {
            return Err(io::Error::other(format!(
                "the token's information of class {class} takes {size} bytes, fewer than the {} its structure needs",
                size_of::<T>()
            )));
        }
        let mut buffer = vec![0_usize; (size as usize).div_ceil(size_of::<usize>())];
        // SAFETY: `buffer` is writable for at least `size` bytes, and `size` is the size the call asked for.
        if unsafe { GetTokenInformation(token.raw(), class, buffer.as_mut_ptr().cast(), size, &raw mut size) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Information { buffer, kind: PhantomData })
    }

    /// The structure the information starts with.
    fn get(&self) -> &T {
        // SAFETY: the system wrote a whole `T` at the buffer's start, which is aligned for a pointer, as each of the
        // structures read here is; the buffer is never changed after the read and lives as long as the borrow.
        unsafe { &*self.buffer.as_ptr().cast::<T>() }
    }
}

/// The relative ID of `token`'s integrity level: the last sub-authority of its label's SID.
fn integrity_of(token: &OwnedHandle) -> io::Result<u32> {
    let label = Information::<TOKEN_MANDATORY_LABEL>::read(token, TokenIntegrityLevel)?;
    let sid = label.get().Label.Sid;
    // SAFETY: `sid` is a valid SID the system wrote into `label`'s buffer, which outlives the call.
    let count = unsafe { *GetSidSubAuthorityCount(sid) };
    let Some(last) = count.checked_sub(1) else {
        return Err(io::Error::other("the token's integrity label has no sub-authority"));
    };
    // SAFETY: as above, and `last` is below the SID's count of sub-authorities.
    Ok(unsafe { *GetSidSubAuthority(sid, u32::from(last)) })
}

/// A well-known SID, in a buffer of the largest size a SID takes, aligned for its fields.
#[repr(C)]
struct WellKnownSid([u32; (SECURITY_MAX_SID_SIZE as usize).div_ceil(size_of::<u32>())]);

impl WellKnownSid {
    /// The well-known SID `kind`, one that needs no domain.
    fn of(kind: WELL_KNOWN_SID_TYPE) -> io::Result<WellKnownSid> {
        let mut sid = WellKnownSid([0; (SECURITY_MAX_SID_SIZE as usize).div_ceil(size_of::<u32>())]);
        let mut size = SECURITY_MAX_SID_SIZE;
        // SAFETY: the buffer is writable for `size` bytes, the most a SID takes, and no domain SID is passed.
        if unsafe { CreateWellKnownSid(kind, std::ptr::null_mut(), sid.0.as_mut_ptr().cast(), &raw mut size) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(sid)
    }

    /// The SID, as a call that reads one takes it: none of the calls given it here writes to it.
    fn as_psid(&self) -> PSID {
        self.0.as_ptr().cast_mut().cast()
    }
}

/// Sets `token`'s information of `class` to the `size` bytes at `value`.
///
/// # Safety
///
/// `value` points to `size` readable bytes holding the structure of `class`, and every pointer in it points to memory
/// valid for the whole call.
unsafe fn set_information(token: &OwnedHandle, class: TOKEN_INFORMATION_CLASS, value: *const c_void, size: usize) -> io::Result<()> {
    let size = u32::try_from(size).map_err(io::Error::other)?;
    // SAFETY: `token` is open with `TOKEN_ADJUST_DEFAULT`; the caller guarantees `value` and `size`.
    if unsafe { SetTokenInformation(token.raw(), class, value, size) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A medium integrity label and the SID it points to, in one block, so the size the label is set with covers both.
#[repr(C)]
struct MediumLabel {
    label: TOKEN_MANDATORY_LABEL,
    sid: WellKnownSid,
}

/// Lowers `token` to medium integrity when it is above that; a token at medium or lower is left as it is, as a token's
/// integrity can be lowered but never raised.
fn lower_to_medium(token: &OwnedHandle) -> io::Result<()> {
    if integrity_of(token)? <= SECURITY_MANDATORY_MEDIUM_RID.cast_unsigned() {
        return Ok(());
    }
    let mut medium = MediumLabel {
        label: TOKEN_MANDATORY_LABEL::default(),
        sid: WellKnownSid::of(WinMediumLabelSid)?,
    };
    medium.label.Label = SID_AND_ATTRIBUTES {
        Sid: medium.sid.as_psid(),
        Attributes: SE_GROUP_INTEGRITY.cast_unsigned(),
    };
    // SAFETY: `medium` is a whole label followed by the SID it points to, both in the local, which is not moved
    // while the call reads it.
    unsafe { set_information(token, TokenIntegrityLevel, (&raw const medium).cast(), size_of::<MediumLabel>()) }
}

/// Makes `user` the owner of the objects `token` creates.
fn set_owner(token: &OwnedHandle, user: PSID) -> io::Result<()> {
    let owner = TOKEN_OWNER { Owner: user };
    // SAFETY: `owner` is a whole `TOKEN_OWNER` whose SID is the token's user's, valid for the whole call.
    unsafe { set_information(token, TokenOwner, (&raw const owner).cast(), size_of::<TOKEN_OWNER>()) }
}

/// Sets `token`'s default DACL, the one the objects it creates without a security descriptor get, to one granting
/// `GENERIC_ALL` to `user` and to SYSTEM.
fn set_default_dacl(token: &OwnedHandle, user: PSID) -> io::Result<()> {
    let system = WellKnownSid::of(WinLocalSystemSid)?;
    let grantees = [user, system.as_psid()];
    // SAFETY: each SID is valid: the token's user's, read from its information, and SYSTEM's, made above.
    let sids: usize = grantees.iter().map(|&sid| unsafe { GetLengthSid(sid) } as usize).sum();
    let ace_without_sid = size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>();
    let size = size_of::<ACL>() + grantees.len() * ace_without_sid + sids;
    let mut buffer = vec![0_u32; size.div_ceil(size_of::<u32>())];
    let length = u32::try_from(buffer.len() * size_of::<u32>()).map_err(io::Error::other)?;
    let acl: *mut ACL = buffer.as_mut_ptr().cast();
    // SAFETY: `buffer` is writable for `length` bytes and aligned for the DWORDs an ACL holds.
    if unsafe { InitializeAcl(acl, length, ACL_REVISION) } == 0 {
        return Err(io::Error::last_os_error());
    }
    for sid in grantees {
        // SAFETY: the ACL was initialized with room for both entries, and `sid` is valid as above.
        if unsafe { AddAccessAllowedAce(acl, ACL_REVISION, GENERIC_ALL, sid) } == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let dacl = TOKEN_DEFAULT_DACL { DefaultDacl: acl };
    // SAFETY: `dacl` is a whole `TOKEN_DEFAULT_DACL` pointing to the ACL in `buffer`, which outlives the call.
    unsafe { set_information(token, TokenDefaultDacl, (&raw const dacl).cast(), size_of::<TOKEN_DEFAULT_DACL>()) }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose token cannot be made or read must fail loudly")]
mod tests {
    use windows_sys::Win32::Foundation::{GENERIC_ALL, LUID};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, EqualSid, GetAce, LUID_AND_ATTRIBUTES, LookupPrivilegeNameW, SID_AND_ATTRIBUTES, TOKEN_DEFAULT_DACL, TOKEN_GROUPS,
        TOKEN_PRIVILEGES, TOKEN_USER, TokenDefaultDacl, TokenGroups, TokenPrivileges, TokenUser, WinBuiltinAdministratorsSid, WinLocalSystemSid,
    };
    use windows_sys::Win32::System::SystemServices::{
        ACCESS_ALLOWED_ACE_TYPE, SE_GROUP_ENABLED, SE_GROUP_USE_FOR_DENY_ONLY, SECURITY_MANDATORY_MEDIUM_RID,
    };

    use super::{Information, RestrictedToken, WellKnownSid, facts_of};

    /// This process's token, restricted.
    fn restricted() -> RestrictedToken {
        RestrictedToken::of_this_process().expect("the restricted token is made")
    }

    #[test]
    fn a_restricted_token_is_not_elevated() {
        let facts = facts_of(restricted().handle()).expect("the token is read");

        assert!(!facts.elevated, "{facts:?}");
    }

    #[test]
    fn a_restricted_token_owns_new_objects_as_the_user() {
        let facts = facts_of(restricted().handle()).expect("the token is read");

        assert!(facts.owner_is_user, "{facts:?}");
    }

    #[test]
    fn a_restricted_token_is_at_most_medium_integrity() {
        let facts = facts_of(restricted().handle()).expect("the token is read");

        assert!(facts.integrity <= SECURITY_MANDATORY_MEDIUM_RID.cast_unsigned(), "{facts:?}");
    }

    #[test]
    fn a_restricted_token_s_default_dacl_grants_the_user_and_system_full_control() {
        let token = restricted();
        let user = Information::<TOKEN_USER>::read(token.handle(), TokenUser).expect("the user is read");
        let system = WellKnownSid::of(WinLocalSystemSid).expect("SYSTEM's SID is made");
        let dacl = Information::<TOKEN_DEFAULT_DACL>::read(token.handle(), TokenDefaultDacl).expect("the default DACL is read");
        let acl = dacl.get().DefaultDacl;
        assert!(!acl.is_null(), "the token has a default DACL");

        // SAFETY: `acl` points to the ACL the system wrote into `dacl`'s buffer, which outlives these reads.
        let count = unsafe { (*acl).AceCount };
        let granted: Vec<(bool, bool, u32)> = (0..u32::from(count))
            .map(|index| {
                let mut ace = std::ptr::null_mut();
                // SAFETY: `acl` is valid as above, `index` is below its entry count and `ace` is writable.
                assert_ne!(unsafe { GetAce(acl, index, &raw mut ace) }, 0, "entry {index} is read");
                let ace: *const ACCESS_ALLOWED_ACE = ace.cast_const().cast();
                // SAFETY: the entry lies within the ACL; its type is checked before its SID is read.
                let (kind, mask) = unsafe { (u32::from((*ace).Header.AceType), (*ace).Mask) };
                assert_eq!(kind, ACCESS_ALLOWED_ACE_TYPE, "entry {index} allows");
                // SAFETY: an access-allowed entry's SID starts at `SidStart`, within the entry; both SIDs are valid.
                let sid = unsafe { (&raw const (*ace).SidStart).cast_mut().cast() };
                // SAFETY: as above.
                let (is_user, is_system) = unsafe { (EqualSid(sid, user.get().User.Sid) != 0, EqualSid(sid, system.as_psid()) != 0) };
                (is_user, is_system, mask)
            })
            .collect();

        assert_eq!(granted, [(true, false, GENERIC_ALL), (false, true, GENERIC_ALL)]);
    }

    /// The name of the privilege `luid`, such as `SeShutdownPrivilege`.
    fn privilege_name(luid: &LUID) -> String {
        let mut name = [0_u16; 128];
        let mut length = u32::try_from(name.len()).expect("the buffer's length fits");
        // SAFETY: `luid` is a valid LUID, `name` is writable for `length` characters and `length` is writable.
        let found = unsafe { LookupPrivilegeNameW(std::ptr::null(), luid, name.as_mut_ptr(), &raw mut length) };
        assert_ne!(found, 0, "the privilege's name is found");
        String::from_utf16(&name[..length as usize]).expect("the name is UTF-16")
    }

    #[test]
    fn a_restricted_token_has_administrators_deny_only_and_no_admin_privileges() {
        let token = restricted();
        let administrators = WellKnownSid::of(WinBuiltinAdministratorsSid).expect("the Administrators SID is made");

        let groups = Information::<TOKEN_GROUPS>::read(token.handle(), TokenGroups).expect("the groups are read");
        let base = groups.buffer.as_ptr().cast::<TOKEN_GROUPS>();
        // SAFETY: the system wrote a whole `TOKEN_GROUPS` at the buffer's start, followed by its `GroupCount` entries
        // from `Groups` on, all within the buffer, which outlives the slice.
        let groups = unsafe { std::slice::from_raw_parts((&raw const (*base).Groups).cast::<SID_AND_ATTRIBUTES>(), (*base).GroupCount as usize) };
        for group in groups {
            // SAFETY: both SIDs are valid: the group's points into the buffer, and the Administrators SID was made above.
            if unsafe { EqualSid(group.Sid, administrators.as_psid()) } != 0 {
                assert_ne!(
                    group.Attributes & SE_GROUP_USE_FOR_DENY_ONLY.cast_unsigned(),
                    0,
                    "Administrators is deny-only"
                );
                assert_eq!(group.Attributes & SE_GROUP_ENABLED.cast_unsigned(), 0, "Administrators is not enabled");
            }
        }

        let privileges = Information::<TOKEN_PRIVILEGES>::read(token.handle(), TokenPrivileges).expect("the privileges are read");
        let base = privileges.buffer.as_ptr().cast::<TOKEN_PRIVILEGES>();
        // SAFETY: as for the groups: `PrivilegeCount` entries from `Privileges` on, within the buffer.
        let privileges = unsafe {
            std::slice::from_raw_parts(
                (&raw const (*base).Privileges).cast::<LUID_AND_ATTRIBUTES>(),
                (*base).PrivilegeCount as usize,
            )
        };
        let names: Vec<String> = privileges.iter().map(|privilege| privilege_name(&privilege.Luid)).collect();
        let standard = [
            "SeChangeNotifyPrivilege",
            "SeShutdownPrivilege",
            "SeUndockPrivilege",
            "SeIncreaseWorkingSetPrivilege",
            "SeTimeZonePrivilege",
        ];
        let others: Vec<&String> = names.iter().filter(|name| !standard.contains(&name.as_str())).collect();
        assert!(others.is_empty(), "privileges beyond a standard user's: {others:?} of {names:?}");
    }
}
