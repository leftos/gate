//! Whether an installed file may be trusted to run elevated ([`check_installed_file`]): only SYSTEM, Administrators and
//! `TrustedInstaller` ([`TRUSTED_OWNERS`]) own it, its folder and every folder above it up to the volume root, and no
//! one else may change the file, add to or empty its folder, or remove, rename, re-permission or take any folder above.

use std::ffi::OsString;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, GENERIC_ALL, GENERIC_EXECUTE, GENERIC_READ, GENERIC_WRITE, LocalFree};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACCESS_ALLOWED_OBJECT_ACE, ACE_HEADER, ACE_INHERITED_OBJECT_TYPE_PRESENT, ACE_OBJECT_TYPE_PRESENT, ACL,
    DACL_SECURITY_INFORMATION, GetAce, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, INHERIT_ONLY_ACE, IsValidAcl,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_ALL_ACCESS, FILE_APPEND_DATA, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DELETE_CHILD,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_NAME_NORMALIZED,
    FILE_READ_ATTRIBUTES, FILE_REMOTE_PROTOCOL_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA,
    FILE_WRITE_EA, FileRemoteProtocolInfo, GetFileInformationByHandleEx, GetFinalPathNameByHandleW, READ_CONTROL, VOLUME_NAME_DOS, WRITE_DAC,
    WRITE_OWNER,
};
use windows_sys::Win32::System::SystemServices::{
    ACCESS_ALLOWED_ACE_TYPE, ACCESS_ALLOWED_CALLBACK_ACE_TYPE, ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE, ACCESS_ALLOWED_OBJECT_ACE_TYPE,
    ACCESS_DENIED_ACE_TYPE, ACCESS_DENIED_CALLBACK_ACE_TYPE, ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE, ACCESS_DENIED_OBJECT_ACE_TYPE,
};

/// The owners a trusted file or folder may have, as SIDs' string forms: SYSTEM, Administrators and `TrustedInstaller`.
/// Only an elevated process or the system itself acts as one of them.
pub const TRUSTED_OWNERS: [&str; 3] = [
    "S-1-5-18",
    "S-1-5-32-544",
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464",
];

/// The prefix of a path the system gives whole, past its own parsing (`\\?\`).
const VERBATIM: &str = r"\\?\";
/// The prefix of a network path the system gives whole (`\\?\UNC\`), which stands for `\\`.
const VERBATIM_UNC: &str = r"\\?\UNC\";
/// What an access list the check cannot walk is said to be.
const MALFORMED: &str = "its access list is malformed";

/// Why an installed file is not trusted: the file or folder at fault and what is wrong with it, shown as one line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Untrusted {
    object: PathBuf,
    problem: Problem,
}

/// What is wrong with a file or folder that is not trusted.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Problem {
    /// It is owned by this SID, which is none of [`TRUSTED_OWNERS`].
    Owner(String),
    /// Its DACL lets this SID, none of [`TRUSTED_OWNERS`], use this right.
    Grants { sid: String, right: Right },
    /// It has no DACL, which lets anyone do anything.
    NoAccessList,
    /// Its DACL holds an entry of this type, which the check does not read.
    UnreadableEntry(u8),
    /// It cannot be opened, or its owner or DACL read, for this reason.
    Unreadable(String),
    /// The file is a reparse point, such as a symbolic link.
    Link,
    /// A folder is a reparse point, such as a symbolic link or a junction.
    LinkOrJunction,
    /// It is on a network share, whose owner and DACL another machine decides.
    Network,
    /// Its final path, in its usual form, is this one, not the path given.
    ResolvesTo(PathBuf),
}

impl fmt::Display for Untrusted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let object = self.object.display();
        match &self.problem {
            Problem::Owner(sid) => write!(formatter, "{object} is owned by {sid}, not SYSTEM, Administrators or TrustedInstaller"),
            Problem::Grants { sid, right } => write!(formatter, "{object} lets {sid} {}", right.words()),
            Problem::NoAccessList => write!(formatter, "{object} has no access list"),
            Problem::UnreadableEntry(kind) => {
                write!(
                    formatter,
                    "{object} has an access list entry of type {kind}, which the check does not read"
                )
            }
            Problem::Unreadable(why) => write!(formatter, "{object} cannot be read: {why}"),
            Problem::Link => write!(formatter, "{object} is a link"),
            Problem::LinkOrJunction => write!(formatter, "{object} is a link or junction"),
            Problem::Network => write!(formatter, "{object} is on a network drive"),
            Problem::ResolvesTo(full) => write!(formatter, "{object} resolves to {}", full.display()),
        }
    }
}

impl std::error::Error for Untrusted {}

/// A right over a file or folder that only a trusted SID may hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Right {
    /// Write the file's data, append to it, or write its extended attributes or attributes.
    Write,
    /// Delete or rename the object itself.
    Delete,
    /// Rewrite its DACL.
    ChangePermissions,
    /// Make itself the owner.
    TakeOwnership,
    /// Make a file in the folder.
    AddFiles,
    /// Make a folder in the folder.
    AddFolders,
    /// Delete or rename anything in the folder, whatever that thing's own DACL says.
    DeleteChildren,
}

impl Right {
    /// The access bits that grant the right, once generic rights are mapped.
    fn mask(self) -> u32 {
        match self {
            Right::Write => FILE_WRITE_DATA | FILE_APPEND_DATA | FILE_WRITE_EA | FILE_WRITE_ATTRIBUTES,
            Right::Delete => DELETE,
            Right::ChangePermissions => WRITE_DAC,
            Right::TakeOwnership => WRITE_OWNER,
            Right::AddFiles => FILE_ADD_FILE,
            Right::AddFolders => FILE_ADD_SUBDIRECTORY,
            Right::DeleteChildren => FILE_DELETE_CHILD,
        }
    }

    /// What the right lets its holder do, as the refusal says it.
    fn words(self) -> &'static str {
        match self {
            Right::Write => "write it",
            Right::Delete => "delete it",
            Right::ChangePermissions => "change its permissions",
            Right::TakeOwnership => "take ownership of it",
            Right::AddFiles => "add files to it",
            Right::AddFolders => "add folders to it",
            Right::DeleteChildren => "delete what it holds",
        }
    }
}

/// Where an object stands to the installed file, which decides the rights no one else may hold over it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    /// The installed file.
    File,
    /// The folder holding it.
    Folder,
    /// A folder above that one, the volume root among them.
    Above,
}

impl Role {
    /// The rights only a trusted SID may hold over an object of this role, in the order a refusal names the first.
    fn rights(self) -> &'static [Right] {
        match self {
            Role::File => &[Right::Write, Right::Delete, Right::ChangePermissions, Right::TakeOwnership],
            Role::Folder => &[
                Right::AddFiles,
                Right::AddFolders,
                Right::DeleteChildren,
                Right::Delete,
                Right::ChangePermissions,
                Right::TakeOwnership,
            ],
            Role::Above => &[
                Right::AddFiles,
                Right::Delete,
                Right::DeleteChildren,
                Right::ChangePermissions,
                Right::TakeOwnership,
            ],
        }
    }
}

/// An entry of a DACL, as far as the check needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Ace {
    flags: u8,
    kind: Kind,
}

/// What an entry of a DACL does.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    /// It allows the SID the mask, plain, object or callback alike.
    Allow { mask: u32, sid: String },
    /// It denies something, which never makes an object less trusted.
    Deny,
    /// It is of this type, which the check does not read.
    Unknown(u8),
}

/// Refuses an installed file `path` unless only SYSTEM, Administrators and `TrustedInstaller` can change what runs
/// from it.
///
/// The file is opened for `READ_CONTROL` and its attributes, with every kind of sharing and without following a
/// reparse point at its own name: a file that is one is refused, and so is one on a network share. Its final path
/// must be `path` made absolute (`.` and `..` resolved by their text, no link followed), equal without regard to ASCII
/// case, so that what is judged is what a caller runs. Its folders come from that final path, and each is opened the
/// same way, refused when it is a reparse point or its final path is not the one expected; the volume root is opened
/// too.
///
/// The file, its folder and each folder above it up to and including the volume root, in that order, must be owned by
/// one of [`TRUSTED_OWNERS`] and hold a DACL; in it, no entry that allows anyone else may grant, once generic rights
/// are mapped, a right to write or delete the file, to add files or folders to its folder or delete what that holds,
/// to add files to a folder above, delete it or what it holds, or, on any of them, to change its permissions or take
/// ownership. Inherit-only entries, which do not apply to the object itself, are passed over, and so are deny entries.
///
/// The caller launches exactly `path`, quoted, and the window between this check and that use is the caller's to
/// narrow, such as by holding a handle to the file that denies writes. A holder of `SeRestorePrivilege` or
/// `SeTakeOwnershipPrivilege`, Backup Operators among them, passes any DACL, and is outside the check.
///
/// # Errors
///
/// The first file or folder that fails, with what is wrong with it: the file given as `path` when it cannot be opened,
/// is a link, is on a network drive or resolves elsewhere; else the object at fault named in its final path's usual
/// form, for its owner, the first right an entry grants, no DACL, an entry of a type the check does not read, a folder
/// that is a link or junction or resolves elsewhere, or the system's error when it cannot be opened or read.
pub fn check_installed_file(path: &Path) -> Result<(), Untrusted> {
    let refused = |problem| Untrusted {
        object: path.to_owned(),
        problem,
    };
    let unreadable = |error: io::Error| refused(Problem::Unreadable(error.to_string()));
    let expected = std::path::absolute(path).map_err(unreadable)?;
    let file = open_unfollowed(path, 0).map_err(unreadable)?;
    if is_reparse_point(&file).map_err(unreadable)? {
        return Err(refused(Problem::Link));
    }
    let full = final_path(&file).map_err(unreadable)?;
    if is_remote(&full, &file) {
        return Err(refused(Problem::Network));
    }
    if !same_path(&shown(&full), &shown(&expected)) {
        return Err(refused(Problem::ResolvesTo(shown(&full))));
    }
    let object = shown(&full);
    let (owner, dacl) = security_of(&file).map_err(|error| Untrusted {
        object: object.clone(),
        problem: Problem::Unreadable(error.to_string()),
    })?;
    judge(Role::File, &owner, dacl.as_deref()).map_err(|problem| Untrusted { object, problem })?;
    for (index, folder) in full.ancestors().skip(1).enumerate() {
        check_folder(folder, if index == 0 { Role::Folder } else { Role::Above })?;
    }
    Ok(())
}

/// Refuses the folder `folder`, a final path, as an object of `role`: opened without following a reparse point and
/// read through that handle, it must not be a reparse point, its final path must be `folder`, and [`judge`] must pass
/// its owner and DACL. A refusal names the folder in its usual form.
fn check_folder(folder: &Path, role: Role) -> Result<(), Untrusted> {
    let object = shown(folder);
    let refused = |problem| Untrusted {
        object: object.clone(),
        problem,
    };
    let unreadable = |error: io::Error| refused(Problem::Unreadable(error.to_string()));
    let handle = open_unfollowed(folder, FILE_FLAG_BACKUP_SEMANTICS).map_err(unreadable)?;
    if is_reparse_point(&handle).map_err(unreadable)? {
        return Err(refused(Problem::LinkOrJunction));
    }
    let full = shown(&final_path(&handle).map_err(unreadable)?);
    if !same_path(&full, &object) {
        return Err(refused(Problem::ResolvesTo(full)));
    }
    let (owner, dacl) = security_of(&handle).map_err(unreadable)?;
    judge(role, &owner, dacl.as_deref()).map_err(refused)
}

/// The first thing wrong with an object of `role` owned by `owner` and holding the DACL `dacl` (`None` when it has
/// none): an owner outside [`TRUSTED_OWNERS`], no DACL, then, in the DACL's order and passing over inherit-only and
/// deny entries, an entry of a type the check does not read or one allowing an untrusted SID one of the role's rights.
fn judge(role: Role, owner: &str, dacl: Option<&[Ace]>) -> Result<(), Problem> {
    if !is_trusted(owner) {
        return Err(Problem::Owner(owner.to_owned()));
    }
    let Some(entries) = dacl else {
        return Err(Problem::NoAccessList);
    };
    for ace in entries.iter().filter(|ace| u32::from(ace.flags) & INHERIT_ONLY_ACE == 0) {
        match &ace.kind {
            Kind::Allow { mask, sid } if !is_trusted(sid) => {
                let granted = mapped(*mask);
                if let Some(right) = role.rights().iter().find(|right| granted & right.mask() != 0) {
                    return Err(Problem::Grants {
                        sid: sid.clone(),
                        right: *right,
                    });
                }
            }
            Kind::Unknown(kind) => return Err(Problem::UnreadableEntry(*kind)),
            Kind::Allow { .. } | Kind::Deny => {}
        }
    }
    Ok(())
}

/// Whether the SID `sid`, in its string form, is one of [`TRUSTED_OWNERS`].
fn is_trusted(sid: &str) -> bool {
    TRUSTED_OWNERS.iter().any(|trusted| sid.eq_ignore_ascii_case(trusted))
}

/// The access mask `mask` with its generic rights replaced by the file rights they stand for (the file generic mapping).
fn mapped(mask: u32) -> u32 {
    let generic = [
        (GENERIC_READ, FILE_GENERIC_READ),
        (GENERIC_WRITE, FILE_GENERIC_WRITE),
        (GENERIC_EXECUTE, FILE_GENERIC_EXECUTE),
        (GENERIC_ALL, FILE_ALL_ACCESS),
    ];
    generic.iter().fold(
        mask,
        |mapped, &(right, specific)| if mask & right == 0 { mapped } else { (mapped & !right) | specific },
    )
}

/// `path` opened for `READ_CONTROL` and its attributes, with every kind of sharing, a reparse point at its own name
/// opened itself rather than followed, and the further flags `flags`.
fn open_unfollowed(path: &Path, flags: u32) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | flags)
        .open(path)
}

/// Whether the open `file` is a reparse point: a symbolic link, a junction or another.
fn is_reparse_point(file: &File) -> io::Result<bool> {
    Ok(file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

/// Whether the open `file`, whose final path is `full`, is on a network share: its final path is a `\\?\UNC\` one, or
/// the system has a remote protocol for its handle.
fn is_remote(full: &Path, file: &File) -> bool {
    let unc: Vec<u16> = VERBATIM_UNC.encode_utf16().collect();
    let wide: Vec<u16> = full.as_os_str().encode_wide().collect();
    if wide.len() >= unc.len() && same_units(&wide[..unc.len()], &unc) {
        return true;
    }
    let mut info = FILE_REMOTE_PROTOCOL_INFO::default();
    let Ok(size) = u32::try_from(size_of::<FILE_REMOTE_PROTOCOL_INFO>()) else {
        return true;
    };
    // SAFETY: the handle is open for the whole call, and `info` is writable for `size` bytes, the most it writes.
    unsafe { GetFileInformationByHandleEx(file.as_raw_handle(), FileRemoteProtocolInfo, (&raw mut info).cast(), size) != 0 }
}

/// Whether the paths `left` and `right` are equal without regard to ASCII case.
fn same_path(left: &Path, right: &Path) -> bool {
    let left: Vec<u16> = left.as_os_str().encode_wide().collect();
    let right: Vec<u16> = right.as_os_str().encode_wide().collect();
    same_units(&left, &right)
}

/// Whether the UTF-16 texts `left` and `right` are equal without regard to ASCII case.
fn same_units(left: &[u16], right: &[u16]) -> bool {
    let fold = |unit: &u16| {
        if (u16::from(b'a')..=u16::from(b'z')).contains(unit) {
            unit - 32
        } else {
            *unit
        }
    };
    left.len() == right.len() && left.iter().map(fold).eq(right.iter().map(fold))
}

/// The final path of the open `file`, normalized, as the system gives it (`\\?\C:\…`).
fn final_path(file: &File) -> io::Result<PathBuf> {
    let mut buffer = vec![0_u16; 512];
    loop {
        let size = u32::try_from(buffer.len()).map_err(io::Error::other)?;
        // SAFETY: the handle is open for the whole call, and `buffer` is writable for `size` units, the most it writes.
        let length = unsafe { GetFinalPathNameByHandleW(file.as_raw_handle(), buffer.as_mut_ptr(), size, FILE_NAME_NORMALIZED | VOLUME_NAME_DOS) };
        let length = usize::try_from(length).map_err(io::Error::other)?;
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        if length < buffer.len() {
            buffer.truncate(length);
            return Ok(OsString::from_wide(&buffer).into());
        }
        // Too small: `length` is the size the path needs, its terminating NUL included.
        buffer.resize(length, 0);
    }
}

/// The path `path` in its usual form: `\\?\UNC\` written `\\`, and `\\?\` left out.
fn shown(path: &Path) -> PathBuf {
    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    let unc: Vec<u16> = VERBATIM_UNC.encode_utf16().collect();
    let verbatim: Vec<u16> = VERBATIM.encode_utf16().collect();
    if let Some(rest) = wide.strip_prefix(unc.as_slice()) {
        let mut usual: Vec<u16> = r"\\".encode_utf16().collect();
        usual.extend_from_slice(rest);
        return OsString::from_wide(&usual).into();
    }
    match wide.strip_prefix(verbatim.as_slice()) {
        Some(rest) => OsString::from_wide(rest).into(),
        None => path.to_owned(),
    }
}

/// The owner and the DACL of the open file or folder `file`, which holds `READ_CONTROL`.
fn security_of(file: &File) -> io::Result<(String, Option<Vec<Ace>>)> {
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: the handle is open for the whole call, `descriptor` is writable, and no single part is asked for.
    let error = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
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
    // SAFETY: a success leaves a valid descriptor, which lives until it is freed below.
    let read = unsafe { owner_and_dacl(descriptor) };
    // SAFETY: the system allocated the descriptor with `LocalAlloc`; it is freed once, after its last use.
    unsafe { LocalFree(descriptor) };
    read
}

/// The SID owning `descriptor`, in its string form, and the entries of its DACL, `None` when it has none.
///
/// # Safety
///
/// `descriptor` is a valid security descriptor for the whole call.
unsafe fn owner_and_dacl(descriptor: PSECURITY_DESCRIPTOR) -> io::Result<(String, Option<Vec<Ace>>)> {
    let (mut owner, mut defaulted): (PSID, _) = (ptr::null_mut(), 0);
    // SAFETY: the caller guarantees the descriptor, and every out pointer is to a local.
    if unsafe { GetSecurityDescriptorOwner(descriptor, &raw mut owner, &raw mut defaulted) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if owner.is_null() {
        return Err(io::Error::other("its security descriptor names no owner"));
    }
    // SAFETY: `owner` points into the descriptor, valid for the call.
    let owner = unsafe { crate::token::sid_string(owner) }?;
    let (mut present, mut acl) = (0, ptr::null_mut::<ACL>());
    // SAFETY: as above.
    if unsafe { GetSecurityDescriptorDacl(descriptor, &raw mut present, &raw mut acl, &raw mut defaulted) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if present == 0 || acl.is_null() {
        return Ok((owner, None));
    }
    // SAFETY: `acl` points into the descriptor, valid for the call.
    let entries = unsafe { aces_of(acl) }?;
    Ok((owner, Some(entries)))
}

/// The entries of `acl`, in their order; an error saying it is malformed when the system finds it is not a valid ACL.
///
/// # Safety
///
/// `acl` points to an ACL header, and to as many bytes as its size says, for the whole call.
unsafe fn aces_of(acl: *const ACL) -> io::Result<Vec<Ace>> {
    // SAFETY: the caller guarantees `acl`.
    if unsafe { IsValidAcl(acl) } == 0 {
        return Err(io::Error::other(MALFORMED));
    }
    // SAFETY: as above.
    let (count, size) = unsafe { ((*acl).AceCount, usize::from((*acl).AclSize)) };
    // SAFETY: as above, the ACL valid, and each index is below its count of entries.
    (0..u32::from(count)).map(|index| unsafe { ace_at(acl, size, index) }).collect()
}

/// The entry `index` of `acl`, an ACL of `acl_size` bytes: an allow entry of any of the four allow types read for its
/// mask and SID, a deny entry of any of the four deny types, else its type. An entry that would reach past the ACL, or
/// whose size leaves no room for what its type holds, is malformed.
///
/// # Safety
///
/// `acl` points to a valid ACL of `acl_size` bytes holding more than `index` entries, for the whole call.
unsafe fn ace_at(acl: *const ACL, acl_size: usize, index: u32) -> io::Result<Ace> {
    let mut ace = ptr::null_mut();
    // SAFETY: the caller guarantees `acl` and `index`, and `ace` is writable.
    if unsafe { GetAce(acl, index, &raw mut ace) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let start: *const u8 = ace.cast_const().cast();
    let offset = start.addr().checked_sub(acl.addr()).ok_or_else(|| io::Error::other(MALFORMED))?;
    if offset.checked_add(size_of::<ACE_HEADER>()).is_none_or(|end| end > acl_size) {
        return Err(io::Error::other(MALFORMED));
    }
    // SAFETY: the header is within the ACL, as just checked.
    let header = unsafe { start.cast::<ACE_HEADER>().read_unaligned() };
    let (kind, flags, size) = (header.AceType, header.AceFlags, usize::from(header.AceSize));
    if offset.checked_add(size).is_none_or(|end| end > acl_size) {
        return Err(io::Error::other(MALFORMED));
    }
    let sid_offset = match u32::from(kind) {
        ACCESS_ALLOWED_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_ACE_TYPE => std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart),
        ACCESS_ALLOWED_OBJECT_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE => {
            if size < std::mem::offset_of!(ACCESS_ALLOWED_OBJECT_ACE, ObjectType) {
                return Err(io::Error::other(MALFORMED));
            }
            // SAFETY: the entry, within the ACL, is long enough to hold its object flags, as just checked.
            unsafe { object_sid_offset(start) }
        }
        ACCESS_DENIED_ACE_TYPE | ACCESS_DENIED_CALLBACK_ACE_TYPE | ACCESS_DENIED_OBJECT_ACE_TYPE | ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE => {
            return Ok(Ace { flags, kind: Kind::Deny });
        }
        _ => {
            return Ok(Ace {
                flags,
                kind: Kind::Unknown(kind),
            });
        }
    };
    // A SID is 8 bytes of revision, count and authority, then 4 bytes for each sub-authority, its count the second byte.
    let fits = |length: usize| sid_offset.checked_add(length).is_some_and(|end| end <= size);
    if !fits(8) {
        return Err(io::Error::other(MALFORMED));
    }
    // SAFETY: the SID's first 8 bytes are within the entry, as just checked.
    let sub_authorities = unsafe { start.add(sid_offset + 1).read() };
    if !fits(8 + 4 * usize::from(sub_authorities)) {
        return Err(io::Error::other(MALFORMED));
    }
    // SAFETY: every allow type holds its mask right after its header, within the entry, which holds its SID after it.
    let mask = unsafe { start.add(std::mem::offset_of!(ACCESS_ALLOWED_ACE, Mask)).cast::<u32>().read_unaligned() };
    // SAFETY: the whole SID is within the entry, as just checked.
    let sid = unsafe { crate::token::sid_string(start.add(sid_offset).cast_mut().cast()) }?;
    Ok(Ace {
        flags,
        kind: Kind::Allow { mask, sid },
    })
}

/// Where the SID of the object entry at `start` begins: after its header, mask and object flags, and the object type
/// and inherited object type GUIDs its flags say it holds.
///
/// # Safety
///
/// `start` points to an allow object entry long enough to hold its object flags, valid for the whole call.
unsafe fn object_sid_offset(start: *const u8) -> usize {
    // SAFETY: the caller guarantees an object entry, which holds its object flags at this offset.
    let object_flags = unsafe {
        start
            .add(std::mem::offset_of!(ACCESS_ALLOWED_OBJECT_ACE, Flags))
            .cast::<u32>()
            .read_unaligned()
    };
    let guids = [ACE_OBJECT_TYPE_PRESENT, ACE_INHERITED_OBJECT_TYPE_PRESENT]
        .iter()
        .filter(|&&present| object_flags & present != 0)
        .count();
    std::mem::offset_of!(ACCESS_ALLOWED_OBJECT_ACE, ObjectType) + guids * size_of::<windows_sys::core::GUID>()
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose descriptor or file cannot be made must fail loudly")]
mod tests {
    use std::fs::{File, OpenOptions};
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ};

    use super::{Ace, Kind, Role, Untrusted, check_folder, check_installed_file, judge, owner_and_dacl, shown};
    use crate::file::{Share, make_folder_beneath};
    use crate::security::Descriptor;

    /// Program Files' own descriptor: owned by `TrustedInstaller`, which alone has full control of the folder itself;
    /// SYSTEM and Administrators modify, and Users and the app packages read and run, the rest inherit-only.
    const PROGRAM_FILES: &str = "O:S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464\
        D:PAI(A;;FA;;;S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464)\
        (A;CIIO;GA;;;S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464)\
        (A;;0x1301bf;;;SY)(A;OICIIO;GA;;;SY)(A;;0x1301bf;;;BA)(A;OICIIO;GA;;;BA)\
        (A;;0x1200a9;;;BU)(A;OICIIO;GXGR;;;BU)(A;OICIIO;GA;;;CO)\
        (A;;0x1200a9;;;AC)(A;OICIIO;GXGR;;;AC)(A;;0x1200a9;;;S-1-15-2-2)(A;OICIIO;GXGR;;;S-1-15-2-2)";

    /// A GUID for the object entries, which the check does not look at.
    const GUID: &str = "bf967aba-0de6-11d0-a285-00aa003049e2";

    /// The refusal of an object of `role` at `C:\t\o` whose descriptor the SDDL `sddl` names, read as the real check
    /// reads one, as its line.
    fn judged(role: Role, sddl: &str) -> Result<(), String> {
        let descriptor = Descriptor::from_sddl(sddl)
            .map_err(|error| format!("{sddl}: {error}"))
            .expect("the SDDL names a descriptor");
        // SAFETY: the descriptor lives for the whole call.
        let (owner, dacl) = unsafe { owner_and_dacl(descriptor.as_ptr()) }.expect("the descriptor's owner and DACL can be read");
        judge(role, &owner, dacl.as_deref()).map_err(|problem| {
            Untrusted {
                object: Path::new(r"C:\t\o").to_owned(),
                problem,
            }
            .to_string()
        })
    }

    /// The scratch folder of the unit tests in its final path's usual form, so a path made in it is its own final path.
    fn scratch() -> PathBuf {
        shown(&std::fs::canonicalize(crate::test_support::temp_dir()).expect("the scratch folder's final path can be read"))
    }

    /// Makes `link` a junction to the folder `target`, which needs no privilege; whether it was made.
    fn junction(link: &Path, target: &Path) -> bool {
        let cmd = crate::system_directory().expect("the system folder is found").join("cmd.exe");
        let mut command = Command::new(cmd);
        command
            .args(["/d", "/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        crate::spawn(&mut command)
            .and_then(|mut child| child.wait())
            .is_ok_and(|status| status.success())
    }

    /// The folder `path` opened to make folders inside it.
    fn held(path: &Path) -> File {
        OpenOptions::new()
            .access_mode(FILE_GENERIC_READ)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .expect("the folder can be opened")
    }

    #[test]
    fn a_program_files_descriptor_passes_as_the_file_its_folder_and_a_folder_above() {
        for role in [Role::File, Role::Folder, Role::Above] {
            assert_eq!(judged(role, PROGRAM_FILES), Ok(()), "{role:?}");
        }
    }

    #[test]
    fn users_granted_write_on_the_file_fail() {
        assert_eq!(
            judged(Role::File, "O:BAD:(A;;FA;;;SY)(A;;0x1200a9;;;BU)(A;;FW;;;BU)"),
            Err(r"C:\t\o lets S-1-5-32-545 write it".to_owned())
        );
    }

    #[test]
    fn authenticated_users_granted_add_file_on_the_folder_fail() {
        assert_eq!(
            judged(Role::Folder, "O:BAD:(A;;FA;;;SY)(A;;0x2;;;AU)"),
            Err(r"C:\t\o lets S-1-5-11 add files to it".to_owned())
        );
    }

    /// Each right a role keeps to the trusted, granted alone to Everyone, is refused with its own words, and append,
    /// extended attributes, attributes and `GENERIC_WRITE` each count as writing the file.
    #[test]
    fn each_right_a_role_keeps_to_the_trusted_is_refused_alone() {
        let cases = [
            (Role::File, "0x2", "write it"),
            (Role::File, "0x4", "write it"),
            (Role::File, "0x10", "write it"),
            (Role::File, "0x100", "write it"),
            (Role::File, "GW", "write it"),
            (Role::File, "SD", "delete it"),
            (Role::File, "WD", "change its permissions"),
            (Role::File, "WO", "take ownership of it"),
            (Role::Folder, "0x2", "add files to it"),
            (Role::Folder, "0x4", "add folders to it"),
            (Role::Folder, "0x40", "delete what it holds"),
            (Role::Folder, "SD", "delete it"),
            (Role::Folder, "WD", "change its permissions"),
            (Role::Folder, "WO", "take ownership of it"),
            (Role::Above, "0x2", "add files to it"),
            (Role::Above, "0x40", "delete what it holds"),
            (Role::Above, "SD", "delete it"),
            (Role::Above, "WD", "change its permissions"),
            (Role::Above, "WO", "take ownership of it"),
        ];
        for (role, right, words) in cases {
            assert_eq!(
                judged(role, &format!("O:BAD:(A;;FA;;;SY)(A;;{right};;;WD)")),
                Err(format!(r"C:\t\o lets S-1-1-0 {words}")),
                "{role:?} {right}"
            );
        }
    }

    /// Object and callback entries that allow are read as allowing, their SID found past whichever GUIDs they hold.
    #[test]
    fn object_and_callback_entries_count_as_allowing() {
        let condition = "(Member_of {SID(BA)})";
        let entries = [
            "(OA;;FA;;;WD)".to_owned(),
            format!("(OA;;FA;{GUID};;WD)"),
            format!("(OA;;FA;;{GUID};WD)"),
            format!("(OA;;FA;{GUID};{GUID};WD)"),
            format!("(XA;;FA;;;WD;{condition})"),
            format!("(ZA;;FA;{GUID};;WD;{condition})"),
            format!("(ZA;;FA;{GUID};{GUID};WD;{condition})"),
        ];
        for entry in entries {
            assert_eq!(
                judged(Role::File, &format!("O:BAD:(A;;FA;;;SY){entry}")),
                Err(r"C:\t\o lets S-1-1-0 write it".to_owned()),
                "{entry}"
            );
        }
    }

    #[test]
    fn an_entry_of_a_type_the_check_does_not_read_fails() {
        let audit = Ace {
            flags: 0,
            kind: Kind::Unknown(2),
        };
        let why = judge(Role::File, "S-1-5-18", Some(&[audit])).map_err(|problem| {
            Untrusted {
                object: Path::new(r"C:\t\o").to_owned(),
                problem,
            }
            .to_string()
        });
        assert_eq!(
            why,
            Err(r"C:\t\o has an access list entry of type 2, which the check does not read".to_owned())
        );
    }

    #[test]
    fn an_inherit_only_modify_for_users_on_a_folder_above_is_passed_over() {
        assert_eq!(judged(Role::Above, "O:BAD:(A;;FA;;;SY)(A;OICIIO;0x1301bf;;;BU)"), Ok(()));
    }

    #[test]
    fn generic_all_for_an_untrusted_sid_fails() {
        assert_eq!(judged(Role::File, "O:BAD:(A;;GA;;;WD)"), Err(r"C:\t\o lets S-1-1-0 write it".to_owned()));
        assert_eq!(
            judged(Role::Above, "O:BAD:(A;;GA;;;WD)"),
            Err(r"C:\t\o lets S-1-1-0 add files to it".to_owned())
        );
    }

    #[test]
    fn a_deny_entry_is_ignored() {
        assert_eq!(judged(Role::File, "O:BAD:(D;;FA;;;WD)(A;;FA;;;SY)"), Ok(()));
    }

    #[test]
    fn an_untrusted_owner_fails() {
        assert_eq!(
            judged(Role::Above, "O:BUD:(A;;FA;;;SY)"),
            Err(r"C:\t\o is owned by S-1-5-32-545, not SYSTEM, Administrators or TrustedInstaller".to_owned())
        );
    }

    #[test]
    fn a_null_dacl_fails() {
        assert_eq!(
            judged(Role::File, "O:BAD:NO_ACCESS_CONTROL"),
            Err(r"C:\t\o has no access list".to_owned())
        );
    }

    /// `C:\`'s default: Authenticated Users may make folders in the root itself (append-data) and modify what is made
    /// there (inherit-only), neither a right over the root a folder above the file must keep to the trusted.
    #[test]
    fn the_default_volume_root_passes_as_a_folder_above() {
        let root = "O:SYD:PAI(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)(A;OICI;0x1200a9;;;BU)(A;OICIIO;SDGXGWGR;;;AU)(A;;0x4;;;AU)";
        assert_eq!(judged(Role::Above, root), Ok(()));
    }

    #[test]
    fn cmd_exe_passes() {
        let cmd = crate::system_directory().expect("the system folder is found").join("cmd.exe");
        assert_eq!(check_installed_file(&cmd).map_err(|why| why.to_string()), Ok(()));
    }

    #[test]
    fn a_file_this_test_makes_fails_naming_it() {
        let file = scratch().join(format!("gate-win32-trust-{}.txt", std::process::id()));
        std::fs::write(&file, "made").expect("the file can be written");
        let user = crate::current_user_sid().expect("this process's user can be read");

        let why = check_installed_file(&file).expect_err("a file this user made is not trusted").to_string();

        std::fs::remove_file(&file).expect("the file can be removed");
        let owned = format!("{} is owned by {user}, not SYSTEM, Administrators or TrustedInstaller", file.display());
        let writable = why.starts_with(&format!("{} lets ", file.display())) && why.ends_with(" write it");
        assert!(why == owned || writable, "{why}");
    }

    #[test]
    fn a_path_that_does_not_exist_cannot_be_read() {
        let missing = crate::test_support::temp_dir().join(format!("gate-win32-trust-missing-{}", std::process::id()));

        let why = check_installed_file(&missing).expect_err("a missing file is not trusted").to_string();

        assert!(why.starts_with(&format!("{} cannot be read: ", missing.display())), "{why}");
    }

    /// The walk refuses the file's own folder when it lets Everyone add files, and a folder above when it lets
    /// Everyone delete it, each with a file inside whose own descriptor passes. Only an elevated process can make a
    /// folder owned by Administrators, so the case is passed over unelevated; the pure judge covers each role there.
    #[test]
    #[expect(clippy::print_stderr, reason = "a case passed over says why")]
    fn the_walk_refuses_the_file_s_folder_and_a_folder_above() {
        if !crate::token_facts().expect("this process's token can be read").elevated {
            eprintln!("passed over: an unelevated process cannot make a folder owned by Administrators");
            return;
        }
        let trusted = "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
        let name = format!("gate-win32-trust-walk-{}", std::process::id());
        let base = scratch().join(&name);
        if base.exists() {
            std::fs::remove_dir_all(&base).expect("an earlier run's folder is removed");
        }
        let made = |parent: &Path, name: &str, sddl: &str| {
            let descriptor = Descriptor::from_sddl(sddl).expect("the SDDL names a descriptor");
            drop(make_folder_beneath(&held(parent), name, FILE_GENERIC_READ, Share::All, &descriptor).expect("the folder can be made"));
            parent.join(name)
        };
        made(&scratch(), &name, &format!("{trusted}(A;;SD;;;WD)"));
        let kept = made(&base, "kept", trusted);
        let open = made(&base, "open", &format!("{trusted}(A;;0x2;;;WD)"));
        std::fs::write(kept.join("f.exe"), "f").expect("the file can be written");
        std::fs::write(open.join("f.exe"), "f").expect("the file can be written");

        let above = check_installed_file(&kept.join("f.exe")).map_err(|why| why.to_string());
        let folder = check_installed_file(&open.join("f.exe")).map_err(|why| why.to_string());

        std::fs::remove_dir_all(&base).expect("the folder can be removed");
        assert_eq!(above, Err(format!("{} lets S-1-1-0 delete it", base.display())));
        assert_eq!(folder, Err(format!("{} lets S-1-1-0 add files to it", open.display())));
    }

    #[test]
    fn a_path_through_a_junction_resolves_to_the_file_it_reaches() {
        let system = crate::system_directory().expect("the system folder is found");
        let link = scratch().join(format!("gate-win32-trust-junction-{}", std::process::id()));
        let _ = std::fs::remove_dir(&link);
        assert!(junction(&link, &system), "a junction needs no privilege");
        let full = shown(&std::fs::canonicalize(system.join("cmd.exe")).expect("cmd.exe's final path can be read"));

        let why = check_installed_file(&link.join("cmd.exe")).map_err(|why| why.to_string());
        let folder = check_folder(&link, Role::Above).map_err(|why| why.to_string());

        std::fs::remove_dir(&link).expect("the junction can be removed");
        assert_eq!(why, Err(format!(r"{}\cmd.exe resolves to {}", link.display(), full.display())));
        assert_eq!(folder, Err(format!("{} is a link or junction", link.display())));
    }

    #[test]
    #[expect(clippy::print_stderr, reason = "a case passed over says why")]
    fn a_file_that_is_a_symbolic_link_is_a_link() {
        let cmd = crate::system_directory().expect("the system folder is found").join("cmd.exe");
        let link = scratch().join(format!("gate-win32-trust-link-{}.exe", std::process::id()));
        let _ = std::fs::remove_file(&link);
        if let Err(error) = std::os::windows::fs::symlink_file(&cmd, &link) {
            eprintln!("passed over: this process cannot make a symbolic link: {error}");
            return;
        }

        let why = check_installed_file(&link).map_err(|why| why.to_string());

        std::fs::remove_file(&link).expect("the link can be removed");
        assert_eq!(why, Err(format!("{} is a link", link.display())));
    }

    #[test]
    #[expect(clippy::print_stderr, reason = "a case passed over says why")]
    fn a_file_on_a_share_is_on_a_network_drive() {
        let shared = Path::new(r"\\localhost\C$\Windows\System32\cmd.exe");
        if let Err(error) = std::fs::metadata(shared) {
            eprintln!("passed over: the administrative share cannot be reached: {error}");
            return;
        }

        let why = check_installed_file(shared).map_err(|why| why.to_string());

        assert_eq!(why, Err(format!("{} is on a network drive", shared.display())));
    }
}
