//! Whether an installed file may be trusted to run elevated ([`check_installed_file`]): only SYSTEM, Administrators and
//! `TrustedInstaller` ([`TRUSTED_OWNERS`]) own it, its folder and every folder above it up to the volume root, and no
//! one else may change the file, add to or empty its folder, or remove, rename, re-permission or take any folder above.
//! Whether the box's work root is locked down ([`check_work_root`]): only those owners and the box's own user may write
//! in it, and no one else may empty, remove, re-permission or take a folder above it.

use std::ffi::OsString;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_SHARING_VIOLATION, ERROR_SUCCESS, GENERIC_ALL, GENERIC_EXECUTE, GENERIC_READ, GENERIC_WRITE, LocalFree};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACCESS_ALLOWED_OBJECT_ACE, ACE_HEADER, ACE_INHERITED_OBJECT_TYPE_PRESENT, ACE_OBJECT_TYPE_PRESENT, ACL,
    DACL_SECURITY_INFORMATION, GetAce, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, INHERIT_ONLY_ACE, IsValidAcl,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_ALL_ACCESS, FILE_APPEND_DATA, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_DELETE_CHILD, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_REMOTE_PROTOCOL_INFO, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA,
    FileRemoteProtocolInfo, GetFileInformationByHandleEx, GetFinalPathNameByHandleW, READ_CONTROL, VOLUME_NAME_DOS, WRITE_DAC, WRITE_OWNER,
};
use windows_sys::Win32::System::SystemServices::{
    ACCESS_ALLOWED_ACE_TYPE, ACCESS_ALLOWED_CALLBACK_ACE_TYPE, ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE, ACCESS_ALLOWED_OBJECT_ACE_TYPE,
    ACCESS_DENIED_ACE_TYPE, ACCESS_DENIED_CALLBACK_ACE_TYPE, ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE, ACCESS_DENIED_OBJECT_ACE_TYPE,
};

use crate::file::{Share, delete_opened, make_folder_beneath};
use crate::security::{Descriptor, protected_sddl};

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
    /// A work root or a folder above it is a reparse point, such as a symbolic link or a junction.
    ReparsePoint,
    /// The work root is on a network volume, whose owner and DACL another machine decides.
    NetworkVolume,
    /// The work root's parent folder does not exist.
    NoParent,
    /// The work root is a file.
    NotAFolder,
    /// The work root was missing and cannot be made, for this reason.
    Unmade(String),
    /// The work root is open in another process with delete access, or without sharing reading, so it cannot be held
    /// without delete sharing, though it and the folders above pass.
    Busy,
    /// The work root this check made was refused for this problem and cannot be removed, for this reason.
    Kept(Box<Problem>, String),
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
            Problem::ReparsePoint => write!(formatter, "{object} is a reparse point"),
            Problem::NetworkVolume => write!(formatter, "{object} is on a network volume"),
            Problem::NoParent => write!(formatter, "{object} has no parent folder"),
            Problem::NotAFolder => write!(formatter, "{object} is not a folder"),
            Problem::Unmade(why) => write!(formatter, "{object} cannot be made: {why}"),
            Problem::Busy => write!(formatter, "{object} is open in another process; retry when it is closed"),
            Problem::Kept(problem, why) => {
                let refused = Untrusted {
                    object: self.object.clone(),
                    problem: (**problem).clone(),
                };
                write!(formatter, "{refused}; it was made by this check and cannot be removed: {why}")
            }
        }
    }
}

impl Untrusted {
    /// Whether the work root was refused only because another process has it open with delete access, the root and the
    /// folders above passing all the same: retrying once that process closes it may pass.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.problem == Problem::Busy
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
    /// Write the folder's extended attributes.
    WriteExtendedAttributes,
    /// Write the folder's attributes.
    WriteAttributes,
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
            Right::WriteExtendedAttributes => FILE_WRITE_EA,
            Right::WriteAttributes => FILE_WRITE_ATTRIBUTES,
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
            Right::WriteExtendedAttributes => "write its extended attributes",
            Right::WriteAttributes => "write its attributes",
        }
    }
}

/// Where an object stands to the installed file or to the box's work root, which decides the rights no one else may
/// hold over it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    /// The installed file.
    File,
    /// The folder holding it.
    Folder,
    /// A folder above that one, the volume root among them.
    Above,
    /// The box's work root, whose every allow entry counts, inherit-only ones included.
    WorkRoot,
    /// A folder above the work root, below its volume root.
    AboveWorkRoot,
    /// The volume root above the work root, which may let anyone delete it, as a drive root's default does.
    WorkVolume,
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
            Role::WorkRoot => &[
                Right::AddFiles,
                Right::AddFolders,
                Right::DeleteChildren,
                Right::WriteExtendedAttributes,
                Right::WriteAttributes,
                Right::Delete,
                Right::ChangePermissions,
                Right::TakeOwnership,
            ],
            Role::AboveWorkRoot => &[Right::DeleteChildren, Right::Delete, Right::ChangePermissions, Right::TakeOwnership],
            Role::WorkVolume => &[Right::DeleteChildren, Right::ChangePermissions, Right::TakeOwnership],
        }
    }

    /// Whether an inherit-only entry, which does not apply to the object itself, counts: only on the work root, whose
    /// entries are what every folder a run makes in it is given.
    fn judges_inherit_only(self) -> bool {
        self == Role::WorkRoot
    }

    /// What a folder of this role that is a reparse point is said to be.
    fn reparse_problem(self) -> Problem {
        match self {
            Role::File => Problem::Link,
            Role::Folder | Role::Above => Problem::LinkOrJunction,
            Role::WorkRoot | Role::AboveWorkRoot | Role::WorkVolume => Problem::ReparsePoint,
        }
    }
}

/// Whom a judgement trusts besides [`TRUSTED_OWNERS`]: the box's user, read from the box's own token, as a SID an
/// entry may grant anything, and as an owner when the box is not elevated.
#[derive(Clone, Copy, Debug)]
struct Trust<'a> {
    user: Option<&'a str>,
    user_owns: bool,
}

impl<'a> Trust<'a> {
    /// [`TRUSTED_OWNERS`] alone, as an installed file is judged.
    const OWNERS: Trust<'static> = Trust {
        user: None,
        user_owns: false,
    };

    /// [`TRUSTED_OWNERS`] and the box's user `user`, an owner too when the box is not `elevated`, as the box makes its
    /// own folders ([`crate::protected_descriptor`]).
    fn of_box(user: &'a str, elevated: bool) -> Trust<'a> {
        Trust {
            user: Some(user),
            user_owns: !elevated,
        }
    }

    /// Whether `sid` is the box's user this trusts.
    fn is_user(self, sid: &str) -> bool {
        self.user.is_some_and(|user| sid.eq_ignore_ascii_case(user))
    }

    /// Whether an entry allowing `sid` may grant it anything.
    fn grantee(self, sid: &str) -> bool {
        is_trusted(sid) || self.is_user(sid)
    }

    /// Whether `sid` may own the object.
    fn owner(self, sid: &str) -> bool {
        is_trusted(sid) || (self.user_owns && self.is_user(sid))
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
    let file = open_unfollowed(path, 0, Share::All).map_err(unreadable)?;
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
    judge(Role::File, Trust::OWNERS, &owner, dacl.as_deref()).map_err(|problem| Untrusted { object, problem })?;
    for (index, folder) in full.ancestors().skip(1).enumerate() {
        check_folder(folder, if index == 0 { Role::Folder } else { Role::Above }, Trust::OWNERS)?;
    }
    Ok(())
}

/// Opens the box's work root `root`, making it first when it is missing, and returns it held: opened as itself, never
/// followed, without delete sharing, so no one can rename or remove it while the handle lives. Refuses it unless only
/// SYSTEM, Administrators, `TrustedInstaller` and the box's user, read from this process's token, can write in it.
///
/// Its parent is opened as itself first, for its security and attributes alone so no process holding it open keeps
/// the check out, and kept open until the check ends: it must exist, must not be a reparse point, its final path must
/// be the root's parent, and it must not be on a network volume. An existing root is judged, then the folders above
/// it; for a missing one the folders above are judged first, and only then is it made, in one call inside that parent
/// with [`protected_sddl`]'s security less the label, so it is never seen with any other and nothing is made beneath a
/// folder that fails. One made by someone else meanwhile is judged as any
/// other. A root this call made and a later step refuses is deleted through its own handle. Last, the held root's
/// final path is read again and must still be the root's.
///
/// The root must be a folder, not a reparse point, not on a network volume, and its final path must be `root` made
/// absolute (equal without regard to ASCII case). Its owner must be SYSTEM, Administrators or `TrustedInstaller`, or
/// the box's user when the box is not elevated; no allow entry of its DACL, whatever its inheritance flags, may grant
/// anyone else, once generic rights are mapped, a right to add files or folders to it, delete what it holds, write its
/// extended attributes or attributes, delete it, change its permissions or take ownership. Each folder above it, up to
/// and including the volume root, must not be a reparse point, must have the owner the root may have, and its entries
/// that apply to itself may grant no one else a right to delete what it holds, delete it (except on the volume root),
/// change its permissions or take ownership. Deny entries are passed over, and labels are not read.
///
/// `walk_stop`, the suite's seam, ends the walk over the folders above at that folder: it and the folders above it are
/// not judged, though the parent is still opened, held and its final path compared. A stop the walk never reaches
/// leaves the walk whole.
///
/// # Errors
///
/// The first object that fails, with what is wrong with it: the root named as `root` is given, for a missing parent,
/// a root that cannot be made, opened or read, is open in another process with delete access, or without sharing
/// reading, while it and the folders above pass ([`Untrusted::is_busy`]; one that fails is refused for that,
/// [`unheld`]), is not a folder, is a reparse point, is on a network volume, resolves elsewhere, or whose owner or DACL
/// fails; else the folder above at fault, named in its final path's usual form.
pub fn check_work_root(root: &Path, walk_stop: Option<&Path>) -> Result<File, Untrusted> {
    let refused = |problem| Untrusted {
        object: root.to_owned(),
        problem,
    };
    let unreadable = |error: io::Error| refused(Problem::Unreadable(error.to_string()));
    let facts = crate::token_facts().map_err(unreadable)?;
    let expected = std::path::absolute(root).map_err(unreadable)?;
    let stop = walk_stop
        .map(|stop| std::path::absolute(stop).map(|full| full.components().collect::<PathBuf>()))
        .transpose()
        .map_err(unreadable)?;
    let walk = Walk {
        trust: Trust::of_box(&facts.user, facts.elevated),
        stop: stop.as_deref(),
    };
    let Some(parent) = expected.parent() else {
        // A volume root has no folder above it to walk, nor one to be made in.
        let held = open_held(&expected).map_err(|error| unheld(&expected, &error, walk.trust, || Ok(())).naming(root))?;
        judge_held_root(&held, &expected, walk.trust).map_err(refused)?;
        return Ok(held);
    };
    let (held_parent, parent_full) = held_parent(parent, root)?;
    let opened = open_or_make(&held_parent, &parent_full, &expected, &facts, walk).map_err(|why| why.naming(root))?;
    let checked = judge_held_root(&opened.held, &expected, walk.trust)
        .map_err(refused)
        .and_then(|()| if opened.walked { Ok(()) } else { walk.above(&parent_full) })
        .and_then(|()| still_at(&opened.held, &expected).map_err(refused));
    match checked {
        Ok(()) => Ok(opened.held),
        Err(why) if opened.made => Err(removed(&opened.held, why)),
        Err(why) => Err(why),
    }
}

/// The walk [`check_work_root`] makes over the folders above the work root: whom it trusts, and the folder, made
/// absolute and without a trailing separator, at which it ends unjudged.
#[derive(Clone, Copy)]
struct Walk<'a> {
    trust: Trust<'a>,
    stop: Option<&'a Path>,
}

impl Walk<'_> {
    /// Refuses the folder `parent`, a final path in its usual form, or any folder above it up to and including the
    /// volume root, as [`check_folder`] judges a folder above the work root, the volume root as itself; the walk ends,
    /// unjudged, at its stop.
    fn above(self, parent: &Path) -> Result<(), Untrusted> {
        for folder in parent.ancestors() {
            if self.stop.is_some_and(|stop| same_path(stop, folder)) {
                break;
            }
            let role = if folder.parent().is_none() {
                Role::WorkVolume
            } else {
                Role::AboveWorkRoot
            };
            check_folder(folder, role, self.trust)?;
        }
        Ok(())
    }
}

/// The work root as [`open_or_make`] opened it: the handle, whether this call made it, and whether the folders above
/// were judged already, before it was made.
struct Opened {
    held: File,
    made: bool,
    walked: bool,
}

/// The folder `path` opened as itself, never followed, to read, without delete sharing. Reading, not only its
/// attributes and security, is asked for because the system checks sharing only for an open that asks to read, write
/// or delete: an open asking for less neither keeps another from deleting or renaming the folder nor is kept out by a
/// handle that may.
fn open_held(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(FILE_GENERIC_READ)
        .share_mode(Share::NoDelete.mode())
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

/// Why the work root `root`, an absolute path, could not be held for `error`, trusting `trust`. A sharing violation,
/// which another process's handle with delete access causes, is judged first: the root is opened again as itself for
/// its security and attributes alone, which the system checks no sharing for, and judged as a held root is
/// ([`judge_held_root`]), then the folders above by `above`. What they find is the refusal, so a handle another process
/// holds cannot hide an unsafe root, and [`Problem::Busy`] only when both pass. Any other error is
/// [`Problem::Unreadable`].
fn unheld(root: &Path, error: &io::Error, trust: Trust<'_>, above: impl FnOnce() -> Result<(), Untrusted>) -> Refusal {
    if !matches!(error.raw_os_error().map(u32::try_from), Some(Ok(ERROR_SHARING_VIOLATION))) {
        return Refusal::Root(Problem::Unreadable(error.to_string()));
    }
    let judged = open_unfollowed(root, FILE_FLAG_BACKUP_SEMANTICS, Share::All)
        .map_err(|error| Problem::Unreadable(error.to_string()))
        .and_then(|seen| judge_held_root(&seen, root, trust));
    if let Err(problem) = judged {
        return Refusal::Root(problem);
    }
    match above() {
        Ok(()) => Refusal::Root(Problem::Busy),
        Err(why) => Refusal::Above(why),
    }
}

/// The parent `parent` of the work root `root`, opened as itself for its security and attributes alone, which the
/// system checks no sharing for, so no process holding it open keeps the check out, and its final path in its usual
/// form. The root is made beneath this handle; a folder above renamed meanwhile is caught as the held root's final
/// path is read again ([`still_at`]). Refuses it, naming the root, when it is missing or on a network volume, and, naming the parent, when it
/// cannot be opened or read, is a reparse point or resolves elsewhere.
fn held_parent(parent: &Path, root: &Path) -> Result<(File, PathBuf), Untrusted> {
    let refused = |object: &Path, problem| Untrusted {
        object: object.to_owned(),
        problem,
    };
    let unreadable = |error: io::Error| refused(parent, Problem::Unreadable(error.to_string()));
    let held = match open_unfollowed(parent, FILE_FLAG_BACKUP_SEMANTICS, Share::All) {
        Ok(held) => held,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(refused(root, Problem::NoParent)),
        Err(error) => return Err(unreadable(error)),
    };
    if is_reparse_point(&held).map_err(unreadable)? {
        return Err(refused(parent, Problem::ReparsePoint));
    }
    let full = final_path(&held).map_err(unreadable)?;
    if is_remote(&full, &held) {
        return Err(refused(root, Problem::NetworkVolume));
    }
    let full = shown(&full);
    if !same_path(&full, &shown(parent)) {
        return Err(refused(parent, Problem::ResolvesTo(full)));
    }
    Ok((held, full))
}

/// The work root `root`, an absolute path below a folder, opened as [`check_work_root`] holds it. When nothing has its
/// name, the folders above its parent `parent`, opened by [`held_parent`], whose final path is `parent_full`, are
/// judged by `walk`, and only then is it made inside `parent` with the security of the box whose token says `facts`,
/// opened with delete access too so a refusal can remove it. Else what stops it, a refusal of the root naming no object yet.
fn open_or_make(parent: &File, parent_full: &Path, root: &Path, facts: &crate::TokenFacts, walk: Walk<'_>) -> Result<Opened, Refusal> {
    let open_root = || open_held(root);
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Refusal::Root(Problem::Unreadable("its name is not Unicode".to_owned())))?;
    match open_root() {
        Ok(held) => {
            return Ok(Opened {
                held,
                made: false,
                walked: false,
            });
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(unheld(root, &error, walk.trust, || walk.above(parent_full))),
    }
    walk.above(parent_full).map_err(Refusal::Above)?;
    let sddl = protected_sddl(&facts.user, facts.elevated, None);
    let security = Descriptor::from_sddl(&sddl).map_err(|error| Refusal::Root(Problem::Unmade(error.to_string())))?;
    match make_folder_beneath(parent, name, FILE_GENERIC_READ | DELETE, Share::NoDelete, &security) {
        Ok(held) => Ok(Opened {
            held,
            made: true,
            walked: true,
        }),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => match open_root() {
            Ok(held) => Ok(Opened {
                held,
                made: false,
                walked: true,
            }),
            Err(error) => Err(unheld(root, &error, walk.trust, || Ok(()))),
        },
        Err(error) => Err(Refusal::Root(Problem::Unmade(error.to_string()))),
    }
}

/// Why [`open_or_make`] stopped: a problem of the work root, or the refusal of a folder above it.
enum Refusal {
    Root(Problem),
    Above(Untrusted),
}

impl Refusal {
    /// The refusal, a problem of the root naming it `root`.
    fn naming(self, root: &Path) -> Untrusted {
        match self {
            Refusal::Root(problem) => Untrusted {
                object: root.to_owned(),
                problem,
            },
            Refusal::Above(untrusted) => untrusted,
        }
    }
}

/// The refusal `why` of the work root `held`, which this check made: the root deleted through its handle, opened with
/// delete access, so a refusal leaves nothing made; when it cannot be, the refusal says so too.
fn removed(held: &File, why: Untrusted) -> Untrusted {
    match delete_opened(held) {
        Ok(()) => why,
        Err(error) => Untrusted {
            object: why.object.clone(),
            problem: Problem::Kept(Box::new(why.problem), error.to_string()),
        },
    }
}

/// Refuses the held work root `held` unless its final path, read now, is still `expected`, so a folder above renamed
/// or replaced while the walk read it is not passed.
fn still_at(held: &File, expected: &Path) -> Result<(), Problem> {
    let full = shown(&final_path(held).map_err(|error| Problem::Unreadable(error.to_string()))?);
    if same_path(&full, &shown(expected)) {
        Ok(())
    } else {
        Err(Problem::ResolvesTo(full))
    }
}

/// Refuses the held work root `held` unless it is a folder, not a reparse point and not on a network volume, its final
/// path is `expected`, and [`judge`] passes its owner and DACL as the work root's.
fn judge_held_root(held: &File, expected: &Path, trust: Trust<'_>) -> Result<(), Problem> {
    let unreadable = |error: io::Error| Problem::Unreadable(error.to_string());
    let attributes = held.metadata().map_err(unreadable)?.file_attributes();
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(Problem::ReparsePoint);
    }
    if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(Problem::NotAFolder);
    }
    let full = final_path(held).map_err(unreadable)?;
    if is_remote(&full, held) {
        return Err(Problem::NetworkVolume);
    }
    still_at(held, expected)?;
    let (owner, dacl) = security_of(held).map_err(unreadable)?;
    judge(Role::WorkRoot, trust, &owner, dacl.as_deref())
}

/// Refuses the folder `folder`, a final path, as an object of `role` judged trusting `trust`: opened without following
/// a reparse point and read through that handle, it must not be a reparse point, its final path must be `folder`, and
/// [`judge`] must pass its owner and DACL. A refusal names the folder in its usual form.
fn check_folder(folder: &Path, role: Role, trust: Trust<'_>) -> Result<(), Untrusted> {
    let object = shown(folder);
    let refused = |problem| Untrusted {
        object: object.clone(),
        problem,
    };
    let unreadable = |error: io::Error| refused(Problem::Unreadable(error.to_string()));
    let handle = open_unfollowed(folder, FILE_FLAG_BACKUP_SEMANTICS, Share::All).map_err(unreadable)?;
    if is_reparse_point(&handle).map_err(unreadable)? {
        return Err(refused(role.reparse_problem()));
    }
    let full = shown(&final_path(&handle).map_err(unreadable)?);
    if !same_path(&full, &object) {
        return Err(refused(Problem::ResolvesTo(full)));
    }
    let (owner, dacl) = security_of(&handle).map_err(unreadable)?;
    judge(role, trust, &owner, dacl.as_deref()).map_err(refused)
}

/// The first thing wrong with an object of `role` owned by `owner` and holding the DACL `dacl` (`None` when it has
/// none), trusting [`TRUSTED_OWNERS`] and what `trust` adds: an owner it does not trust, no DACL, then, in the DACL's
/// order and passing over deny entries, and inherit-only ones unless the role judges them, an entry of a type the check
/// does not read or one allowing a SID it does not trust one of the role's rights.
fn judge(role: Role, trust: Trust<'_>, owner: &str, dacl: Option<&[Ace]>) -> Result<(), Problem> {
    if !trust.owner(owner) {
        return Err(Problem::Owner(owner.to_owned()));
    }
    let Some(entries) = dacl else {
        return Err(Problem::NoAccessList);
    };
    let applies = |ace: &&Ace| role.judges_inherit_only() || u32::from(ace.flags) & INHERIT_ONLY_ACE == 0;
    for ace in entries.iter().filter(applies) {
        match &ace.kind {
            Kind::Allow { mask, sid } if !trust.grantee(sid) => {
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

/// `path` opened for `READ_CONTROL` and its attributes, sharing what `share` says, a reparse point at its own name
/// opened itself rather than followed, and the further flags `flags`.
fn open_unfollowed(path: &Path, flags: u32, share: Share) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .share_mode(share.mode())
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

    use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ};

    use super::{
        Ace, Kind, Problem, Role, Trust, Untrusted, check_folder, check_installed_file, check_work_root, judge, open_unfollowed, owner_and_dacl,
        removed, shown, still_at,
    };
    use crate::file::{Share, make_folder_beneath};
    use crate::security::{Descriptor, protected_sddl};

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
        judged_by(role, Trust::OWNERS, sddl)
    }

    /// [`judged`], trusting what `trust` names.
    fn judged_by(role: Role, trust: Trust<'_>, sddl: &str) -> Result<(), String> {
        let descriptor = Descriptor::from_sddl(sddl)
            .map_err(|error| format!("{sddl}: {error}"))
            .expect("the SDDL names a descriptor");
        // SAFETY: the descriptor lives for the whole call.
        let (owner, dacl) = unsafe { owner_and_dacl(descriptor.as_ptr()) }.expect("the descriptor's owner and DACL can be read");
        judge(role, trust, &owner, dacl.as_deref()).map_err(|problem| {
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
        let why = judge(Role::File, Trust::OWNERS, "S-1-5-18", Some(&[audit])).map_err(|problem| {
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

    /// The box user the work-root cases trust, as a box's token would name it.
    const BOX_USER: &str = "S-1-5-21-1-2-3-1001";

    /// The descriptor the installer's `Gate work root` step gives the root, owned by Administrators.
    const INSTALLED_ROOT: &str = "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;S-1-5-21-1-2-3-1001)";

    /// What an elevated box makes of a work root whose descriptor the SDDL `sddl` names.
    fn root_judged(sddl: &str) -> Result<(), String> {
        judged_by(Role::WorkRoot, Trust::of_box(BOX_USER, true), sddl)
    }

    #[test]
    fn the_installer_s_work_root_passes_elevated_and_owned_by_the_user_unelevated() {
        assert_eq!(root_judged(INSTALLED_ROOT), Ok(()));
        let owned_by_user = INSTALLED_ROOT.replacen("O:BA", &format!("O:{BOX_USER}"), 1);
        assert_eq!(judged_by(Role::WorkRoot, Trust::of_box(BOX_USER, false), &owned_by_user), Ok(()));
    }

    /// GATE-576's open root: Authenticated Users modify, inherit-only, handed down from the drive.
    #[test]
    fn an_inherit_only_modify_for_authenticated_users_on_the_work_root_fails() {
        assert_eq!(
            root_judged(&format!("{INSTALLED_ROOT}(A;OICIIO;SDGXGWGR;;;AU)")),
            Err(r"C:\t\o lets S-1-5-11 add files to it".to_owned())
        );
    }

    #[test]
    fn generic_all_or_write_for_an_untrusted_sid_on_the_work_root_fails() {
        for right in ["GA", "GW"] {
            assert_eq!(
                root_judged(&format!("{INSTALLED_ROOT}(A;;{right};;;WD)")),
                Err(r"C:\t\o lets S-1-1-0 add files to it".to_owned()),
                "{right}"
            );
        }
    }

    #[test]
    fn each_write_right_on_the_work_root_is_refused_alone() {
        let cases = [
            ("0x2", "add files to it"),
            ("0x4", "add folders to it"),
            ("0x10", "write its extended attributes"),
            ("0x40", "delete what it holds"),
            ("0x100", "write its attributes"),
            ("SD", "delete it"),
            ("WD", "change its permissions"),
            ("WO", "take ownership of it"),
        ];
        for (right, words) in cases {
            assert_eq!(
                root_judged(&format!("{INSTALLED_ROOT}(A;;{right};;;WD)")),
                Err(format!(r"C:\t\o lets S-1-1-0 {words}")),
                "{right}"
            );
        }
    }

    /// An entry that hands a right down, to folders, files or the next level only, counts on the root whether or not it
    /// also applies to the root itself.
    #[test]
    fn inheritable_entries_on_the_work_root_count_whatever_their_flags() {
        for flags in ["CI", "OI", "CIIO", "OIIO", "OICINP", "OICINPIO", "CINPIO"] {
            assert_eq!(
                root_judged(&format!("{INSTALLED_ROOT}(A;{flags};0x2;;;WD)")),
                Err(r"C:\t\o lets S-1-1-0 add files to it".to_owned()),
                "{flags}"
            );
        }
    }

    #[test]
    fn read_and_run_for_users_on_the_work_root_passes() {
        assert_eq!(root_judged(&format!("{INSTALLED_ROOT}(A;OICI;0x1200a9;;;BU)")), Ok(()));
    }

    #[test]
    fn a_deny_entry_on_the_work_root_is_ignored() {
        assert_eq!(
            root_judged(&format!("O:BAD:P(D;OICI;FA;;;WD){}", &INSTALLED_ROOT["O:BAD:P".len()..])),
            Ok(())
        );
    }

    #[test]
    fn object_and_callback_entries_on_the_work_root_count_as_allowing() {
        let condition = "(Member_of {SID(BA)})";
        for entry in [
            format!("(OA;;0x2;{GUID};;WD)"),
            format!("(XA;;0x2;;;WD;{condition})"),
            format!("(ZA;;0x2;{GUID};;WD;{condition})"),
        ] {
            assert_eq!(
                root_judged(&format!("{INSTALLED_ROOT}{entry}")),
                Err(r"C:\t\o lets S-1-1-0 add files to it".to_owned()),
                "{entry}"
            );
        }
    }

    #[test]
    fn an_unknown_entry_or_no_access_list_on_the_work_root_fails() {
        let audit = Ace {
            flags: 0,
            kind: Kind::Unknown(2),
        };
        let unknown = judge(Role::WorkRoot, Trust::of_box(BOX_USER, true), "S-1-5-18", Some(&[audit]));
        assert_eq!(unknown, Err(Problem::UnreadableEntry(2)));
        assert_eq!(root_judged("O:BAD:NO_ACCESS_CONTROL"), Err(r"C:\t\o has no access list".to_owned()));
    }

    /// The box's user may own the root only when the box is not elevated; no other user ever may.
    #[test]
    fn the_work_root_s_owner_is_a_trusted_owner_or_the_unelevated_box_s_user() {
        let by_user = format!("O:{BOX_USER}D:P(A;OICI;FA;;;SY)");
        assert_eq!(
            judged_by(Role::WorkRoot, Trust::of_box(BOX_USER, true), &by_user),
            Err(format!(r"C:\t\o is owned by {BOX_USER}, not SYSTEM, Administrators or TrustedInstaller"))
        );
        assert_eq!(judged_by(Role::WorkRoot, Trust::of_box(BOX_USER, false), &by_user), Ok(()));
        for elevated in [true, false] {
            assert_eq!(
                judged_by(Role::WorkRoot, Trust::of_box(BOX_USER, elevated), "O:BUD:P(A;OICI;FA;;;SY)"),
                Err(r"C:\t\o is owned by S-1-5-32-545, not SYSTEM, Administrators or TrustedInstaller".to_owned()),
                "elevated {elevated}"
            );
        }
        assert_eq!(
            judged_by(Role::WorkRoot, Trust::of_box(BOX_USER, true), "O:SYD:P(A;OICI;FA;;;SY)"),
            Ok(())
        );
    }

    #[test]
    fn creator_and_owner_rights_entries_with_a_write_right_fail_and_owner_rights_to_read_passes() {
        for (alias, sid) in [("CO", "S-1-3-0"), ("CG", "S-1-3-1"), ("OW", "S-1-3-4")] {
            assert_eq!(
                root_judged(&format!("{INSTALLED_ROOT}(A;OICIIO;0x2;;;{alias})")),
                Err(format!(r"C:\t\o lets {sid} add files to it")),
                "{alias}"
            );
        }
        assert_eq!(root_judged(&format!("{INSTALLED_ROOT}(A;;0x1200a9;;;OW)")), Ok(()));
    }

    /// The builder's drive root lets Authenticated Users delete the root itself and add to it, which a volume root
    /// above the work root may; deleting what it holds it may not, and a folder between may not even delete itself.
    /// Delete-child is written `0x40`: SDDL's `DC` names the directory service's right, `0x2`.
    #[test]
    fn the_measured_drive_root_passes_as_the_volume_above_and_a_folder_above_keeps_its_deletes() {
        let trust = Trust::of_box(BOX_USER, true);
        let drive = "O:SYD:PAI(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)(A;OICIIO;SDGXGWGR;;;AU)(A;;0x1301bf;;;AU)";
        assert_eq!(judged_by(Role::WorkVolume, trust, drive), Ok(()));
        assert_eq!(
            judged_by(Role::WorkVolume, trust, "O:SYD:(A;;0x40;;;AU)"),
            Err(r"C:\t\o lets S-1-5-11 delete what it holds".to_owned())
        );
        assert_eq!(
            judged_by(Role::AboveWorkRoot, trust, "O:SYD:(A;;0x40;;;AU)"),
            Err(r"C:\t\o lets S-1-5-11 delete what it holds".to_owned())
        );
        assert_eq!(
            judged_by(Role::AboveWorkRoot, trust, "O:SYD:(A;;0x1301bf;;;AU)"),
            Err(r"C:\t\o lets S-1-5-11 delete it".to_owned())
        );
        assert_eq!(
            judged_by(Role::AboveWorkRoot, trust, "O:SYD:(A;OICIIO;GA;;;AU)(A;;FA;;;S-1-5-21-1-2-3-1001)"),
            Ok(())
        );
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
        let folder = check_folder(&link, Role::Above, Trust::OWNERS).map_err(|why| why.to_string());

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

    /// A fresh folder in the scratch folder, named for `name` and this process, and empty.
    fn fresh(name: &str) -> PathBuf {
        let folder = scratch().join(format!("gate-win32-trust-{name}-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&folder));
        std::fs::create_dir_all(&folder).expect("the folder can be made");
        folder
    }

    /// The work root held as `x` whose final path is no longer the root's, as when a folder above was renamed while
    /// the walk read it, is refused naming where it now is.
    #[test]
    fn a_held_work_root_no_longer_at_its_path_is_refused() {
        let folder = fresh("moved");
        let (moved, root) = (folder.join("x"), folder.join("root"));
        std::fs::create_dir(&moved).expect("the folder can be made");
        let handle = held(&moved);

        let why = still_at(&handle, &root);

        assert_eq!(why, Err(Problem::ResolvesTo(moved.clone())));
        assert_eq!(still_at(&handle, &moved), Ok(()), "a root still at its path passes");
        drop(handle);
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    /// Another handle with delete access keeps the check from holding the root without delete sharing; a root that
    /// passes all the same is said as busy, its own problem.
    #[test]
    fn a_work_root_open_elsewhere_with_delete_access_is_busy() {
        let folder = fresh("busy");
        let root = folder.join("remote");
        // Made locked by the check, so the root passes once it is judged.
        drop(check_work_root(&root, Some(&folder)).expect("a missing root is made locked"));
        let other = OpenOptions::new()
            .access_mode(DELETE)
            .share_mode(Share::All.mode())
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(&root)
            .expect("the root can be opened with delete access");

        let why = check_work_root(&root, Some(&folder)).expect_err("a root open elsewhere with delete access is refused");

        drop(other);
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
        assert!(why.is_busy(), "{why}");
        assert_eq!(
            why.to_string(),
            format!("{} is open in another process; retry when it is closed", root.display())
        );
    }

    /// A root another process holds open is judged all the same, so its handle cannot hide that the root is unsafe.
    #[test]
    fn an_unsafe_work_root_open_elsewhere_is_unsafe_not_busy() {
        let folder = fresh("busy-unsafe");
        let root = folder.join("remote");
        drop(check_work_root(&root, Some(&folder)).expect("a missing root is made locked"));
        grant(&root, "*S-1-0-0:(DE)");
        let other = OpenOptions::new()
            .access_mode(DELETE)
            .share_mode(Share::All.mode())
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(&root)
            .expect("the root can be opened with delete access");

        let why = check_work_root(&root, Some(&folder)).expect_err("an unsafe root is refused");

        drop(other);
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
        assert!(!why.is_busy(), "{why}");
        assert_eq!(why.to_string(), format!("{} lets S-1-0-0 delete it", root.display()));
    }

    /// Runs icacls on `path` granting `grant`, which must succeed.
    fn grant(path: &Path, grant: &str) {
        let mut command = Command::new("icacls");
        command.arg(path).args(["/grant", grant, "/q"]).stdin(Stdio::null()).stdout(Stdio::null());
        let status = crate::spawn(&mut command).and_then(|mut child| child.wait()).expect("icacls runs");
        assert!(status.success(), "icacls {} /grant {grant}: {status:?}", path.display());
    }

    /// The parent is read for its security and attributes alone, which the system checks no sharing for, so another
    /// process holding it open, sharing nothing, does not keep the root from being made and held.
    #[test]
    fn a_parent_open_elsewhere_sharing_nothing_does_not_stop_the_check() {
        let folder = fresh("parent-open");
        let root = folder.join("remote");
        let other = OpenOptions::new()
            .access_mode(FILE_GENERIC_READ)
            .share_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(&folder)
            .expect("the parent can be opened sharing nothing");

        let checked = check_work_root(&root, Some(&folder)).map(drop).map_err(|why| why.to_string());

        drop(other);
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
        assert_eq!(checked, Ok(()));
    }

    /// A folder is made beneath a parent opened for its security and attributes alone, as the check opens the parent.
    #[test]
    fn a_folder_is_made_beneath_a_parent_opened_for_its_security_alone() {
        let folder = fresh("beneath");
        let parent = open_unfollowed(&folder, FILE_FLAG_BACKUP_SEMANTICS, Share::All).expect("the parent can be opened");

        let made = make_folder_beneath(&parent, "made", FILE_GENERIC_READ, Share::All, &locked()).map(drop);

        let there = folder.join("made").is_dir();
        drop(parent);
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
        assert_eq!(made.map_err(|error| error.to_string()), Ok(()));
        assert!(there, "the folder is there");
    }

    /// The security the check makes a missing work root with, for this process's token.
    fn locked() -> Descriptor {
        let facts = crate::token_facts().expect("this process's token can be read");
        Descriptor::from_sddl(&protected_sddl(&facts.user, facts.elevated, None)).expect("the SDDL names a descriptor")
    }

    /// The root the check returns is held, whether it made the root or found it: no one can rename or remove it while
    /// the handle is open.
    #[test]
    fn a_held_work_root_cannot_be_renamed_or_removed() {
        let folder = fresh("held");
        let root = folder.join("remote");
        let tried = || {
            let renamed = std::fs::rename(&root, folder.join("elsewhere")).map_err(|error| error.raw_os_error());
            let removed = std::fs::remove_dir(&root).map_err(|error| error.raw_os_error());
            (renamed, removed)
        };
        let made = check_work_root(&root, Some(&folder)).expect("a missing root is made and held");
        let while_made = tried();
        drop(made);
        let found = check_work_root(&root, Some(&folder)).expect("the root made is found and held");
        let while_found = tried();
        drop(found);

        std::fs::remove_dir_all(&folder).expect("the folder can be removed");
        let refused = (Err(Some(32)), Err(Some(32)));
        assert_eq!(
            while_made, refused,
            "a rename and a removal of a root the check made are sharing violations"
        );
        assert_eq!(
            while_found, refused,
            "a rename and a removal of a root the check found are sharing violations"
        );
    }

    /// A refused root this check made is removed through its handle; one holding a file cannot be, and says so.
    #[test]
    fn a_made_root_is_removed_unless_it_holds_a_file() {
        let folder = fresh("removed");
        let parent = held(&folder);
        let why = |object: &Path| Untrusted {
            object: object.to_owned(),
            problem: Problem::NoAccessList,
        };
        let made = make_folder_beneath(&parent, "empty", FILE_GENERIC_READ | DELETE, Share::NoDelete, &locked()).expect("the folder is made");
        let emptied = removed(&made, why(&folder.join("empty")));
        drop(made);
        let gone = !folder.join("empty").exists();
        let full = make_folder_beneath(&parent, "full", FILE_GENERIC_READ | DELETE, Share::NoDelete, &locked()).expect("the folder is made");
        std::fs::write(folder.join("full").join("f.txt"), "f").expect("the file can be written");
        let kept = removed(&full, why(&folder.join("full")));
        drop(full);
        drop(parent);
        std::fs::remove_dir_all(&folder).expect("the folder can be removed");

        assert_eq!(emptied, why(&folder.join("empty")), "an empty root's refusal is said as it is");
        assert!(gone, "the empty root is removed");
        assert!(
            matches!(&kept.problem, Problem::Kept(problem, _) if **problem == Problem::NoAccessList),
            "{kept:?}"
        );
    }
}
