//! Named mutexes: one owner at a time across handles, an abandoned mutex taken as owned, a probe that sees whether
//! any handle to a name is open, and a mutex inside a private namespace that only processes within its boundary reach.

use std::io;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, HANDLE, LocalFree, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows_sys::Win32::Security::{PSID, SECURITY_ATTRIBUTES};
use windows_sys::Win32::System::Threading::{
    AddSIDToBoundaryDescriptor, ClosePrivateNamespace, CreateBoundaryDescriptorW, CreateMutexW, CreatePrivateNamespaceW, DeleteBoundaryDescriptor,
    OpenMutexW, OpenPrivateNamespaceW, ReleaseMutex, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject,
};

use crate::security::Descriptor;
use crate::{own_handle, wait_millis, wide};

/// A handle to a named mutex, created unowned when no process has it open. A Windows mutex is owned by a thread, so
/// neither the handle nor its guard leaves the thread that opened it.
pub struct NamedMutex {
    handle: OwnedHandle,
    name: String,
}

/// Why a mutex was not taken.
#[derive(Debug, thiserror::Error)]
pub enum AcquireError {
    /// Another thread held it for the whole wait.
    #[error("not free in time")]
    Timeout,
    /// The wait itself failed.
    #[error("the wait failed: {0}")]
    Failed(io::Error),
}

/// An owned mutex, released when dropped. It borrows the handle it was taken through, which the caller keeps open.
pub struct MutexGuard<'a> {
    mutex: &'a NamedMutex,
    abandoned: bool,
}

/// A mutex owned by the guard itself, released when dropped. Its handle stays open for as long as the guard lives,
/// which is what a caller that holds a slot for a whole run needs.
pub struct OwnedMutexGuard {
    mutex: NamedMutex,
    abandoned: bool,
}

impl NamedMutex {
    /// Opens the mutex `name` (such as `Local\gate-journal`), creating it unowned when it does not exist.
    ///
    /// # Errors
    ///
    /// The system's error when the mutex cannot be created or opened.
    pub fn open(name: &str) -> io::Result<NamedMutex> {
        let wide_name = wide(name);
        // SAFETY: `wide_name` is NUL-terminated and outlives the call; null security attributes are allowed. A success
        // is a fresh handle nothing else owns.
        let handle = unsafe { own_handle(CreateMutexW(std::ptr::null(), 0, wide_name.as_ptr())) }?;
        Ok(NamedMutex {
            handle,
            name: name.to_owned(),
        })
    }

    /// The name it was opened by.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Takes the mutex, waiting at most `timeout`. A mutex whose owner thread ended without releasing it comes back
    /// owned, marked [`MutexGuard::abandoned`].
    ///
    /// # Errors
    ///
    /// [`AcquireError::Timeout`] when it was not free within `timeout`, [`AcquireError::Failed`] when the wait failed.
    pub fn acquire(&self, timeout: Duration) -> Result<MutexGuard<'_>, AcquireError> {
        let abandoned = wait(self.handle.raw(), timeout)?;
        Ok(MutexGuard { mutex: self, abandoned })
    }

    /// Takes the mutex and its handle together, waiting at most `timeout`, so the guard owns what it holds. A mutex
    /// whose owner thread ended without releasing it comes back owned, marked [`OwnedMutexGuard::abandoned`]. A try
    /// that fails drops the handle at once: the create, try, drop of a gate that is only trying a slot.
    ///
    /// # Errors
    ///
    /// [`AcquireError::Timeout`] when it was not free within `timeout`, [`AcquireError::Failed`] when the wait failed.
    pub fn acquire_owned(self, timeout: Duration) -> Result<OwnedMutexGuard, AcquireError> {
        let abandoned = wait(self.handle.raw(), timeout)?;
        Ok(OwnedMutexGuard { mutex: self, abandoned })
    }
}

/// Waits on an open handle for at most `timeout`, mapping the result to whether the object came back abandoned.
fn wait(handle: HANDLE, timeout: Duration) -> Result<bool, AcquireError> {
    // SAFETY: the handle is open for the whole call.
    let result = unsafe { WaitForSingleObject(handle, wait_millis(timeout)) };
    match result {
        WAIT_OBJECT_0 => Ok(false),
        WAIT_ABANDONED => Ok(true),
        WAIT_TIMEOUT => Err(AcquireError::Timeout),
        _ => Err(AcquireError::Failed(io::Error::last_os_error())),
    }
}

impl MutexGuard<'_> {
    /// True when the mutex was taken abandoned: its last owner's thread ended while it held it.
    #[must_use]
    pub fn abandoned(&self) -> bool {
        self.abandoned
    }
}

impl Drop for MutexGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: this thread owns the mutex through the open handle. A release that fails leaves it to be abandoned
        // when the thread ends, which the next taker handles.
        unsafe { ReleaseMutex(self.mutex.handle.raw()) };
    }
}

impl OwnedMutexGuard {
    /// True when the mutex was taken abandoned: its last owner's thread ended while it held it.
    #[must_use]
    pub fn abandoned(&self) -> bool {
        self.abandoned
    }

    /// The name the mutex was opened by.
    #[must_use]
    pub fn name(&self) -> &str {
        self.mutex.name()
    }
}

impl Drop for OwnedMutexGuard {
    fn drop(&mut self) {
        // SAFETY: this thread owns the mutex through the open handle. A release that fails leaves it to be abandoned
        // when the thread ends, which the next taker handles. The fields drop after this body in declaration order,
        // `mutex` first, so the release always runs while its `OwnedHandle` still has the handle open.
        unsafe { ReleaseMutex(self.mutex.handle.raw()) };
    }
}

/// True while a mutex of this name exists. A Windows mutex lives as long as any process holds a handle to it, and a
/// gate holds one only while it tries or holds the slot, so a `true` may be a gate's own instant of trying. A name
/// this process may not open counts as existing too.
///
/// # Errors
///
/// The system's error when the open fails for any reason other than the mutex's absence or an access it is refused.
pub fn mutex_exists(name: &str) -> io::Result<bool> {
    let wide_name = wide(name);
    // SAFETY: `wide_name` is NUL-terminated and outlives the call. Only `SYNCHRONIZE` is asked for and the handle is
    // never waited on, so the mutex is not taken. A success is a fresh handle nothing else owns, which the guard
    // closes at once.
    match unsafe { own_handle(OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, wide_name.as_ptr())) } {
        Ok(_open) => Ok(true),
        // An access this process is refused proves the name exists: an elevated gate made it with a DACL this process
        // cannot open. Only elevation produces it, so it has no test.
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Ok(true),
        // `OpenMutexW` reports a name no process holds as `ERROR_FILE_NOT_FOUND`.
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// The well-known SID of the Administrators group, the one the production boundary holds.
const ADMINISTRATORS_SID: &str = "S-1-5-32-544";

/// The boundary of the production desktop namespace.
const DESKTOP_BOUNDARY: &str = "gate-desktop";

/// The alias prefix the desktop namespace is opened under, so its mutex is `gate\desktop`.
const DESKTOP_PREFIX: &str = "gate";

/// The desktop mutex's name inside its namespace.
const DESKTOP_MUTEX: &str = "desktop";

/// The DACL of the production namespace: full control to SYSTEM and to Administrators, nobody else.
const DESKTOP_NAMESPACE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)";

/// How many times a namespace that vanishes between its create and its open is looked for again.
const NAMESPACE_TRIES: u32 = 3;

/// Where a [`PrivateMutex`] lives: the boundary that isolates its private namespace (a name and the SID a process
/// must hold to be within it), the namespace's DACL, the alias prefix this process opens the namespace under, and the
/// mutex's name inside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrivateName {
    boundary: String,
    boundary_sid: String,
    namespace_sddl: String,
    prefix: String,
    mutex: String,
}

impl PrivateName {
    /// The builder's desktop mutex, `gate\desktop`: a boundary `gate-desktop` holding the Administrators SID, so only
    /// an elevated process can create or open the namespace, whose DACL grants SYSTEM and Administrators alone. No
    /// other local account can squat its name.
    #[must_use]
    pub fn desktop() -> PrivateName {
        PrivateName {
            boundary: DESKTOP_BOUNDARY.to_owned(),
            boundary_sid: ADMINISTRATORS_SID.to_owned(),
            namespace_sddl: DESKTOP_NAMESPACE_SDDL.to_owned(),
            prefix: DESKTOP_PREFIX.to_owned(),
            mutex: DESKTOP_MUTEX.to_owned(),
        }
    }

    /// The desktop mutex as an unelevated test run reaches it: the boundary `gate-desktop-<unique>` holds the user SID
    /// `user_sid` in place of Administrators, the namespace's DACL grants that user too, and the alias prefix is
    /// `gate-<unique>`, so each `unique` names a namespace of its own. Test builds only.
    ///
    /// # Errors
    ///
    /// An error of kind [`io::ErrorKind::InvalidInput`] when `unique` is empty or holds anything but ASCII letters,
    /// digits and `-`.
    #[cfg(any(test, feature = "test-seams"))]
    pub fn for_user(user_sid: &str, unique: &str) -> io::Result<PrivateName> {
        if unique.is_empty() || !unique.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{unique:?} is not a private namespace's unique part: only ASCII letters, digits and - are"),
            ));
        }
        Ok(PrivateName {
            boundary: format!("{DESKTOP_BOUNDARY}-{unique}"),
            boundary_sid: user_sid.to_owned(),
            namespace_sddl: format!("{DESKTOP_NAMESPACE_SDDL}(A;;GA;;;{user_sid})"),
            prefix: format!("{DESKTOP_PREFIX}-{unique}"),
            mutex: DESKTOP_MUTEX.to_owned(),
        })
    }

    /// The boundary descriptor's name.
    #[must_use]
    pub fn boundary(&self) -> &str {
        &self.boundary
    }

    /// The SID the boundary holds, in its string form.
    #[must_use]
    pub fn boundary_sid(&self) -> &str {
        &self.boundary_sid
    }

    /// The namespace's security descriptor, in SDDL.
    #[must_use]
    pub fn namespace_sddl(&self) -> &str {
        &self.namespace_sddl
    }

    /// The mutex's full name in this process once its namespace is open: `<prefix>\<mutex>`.
    #[must_use]
    pub fn mutex_name(&self) -> String {
        format!("{}\\{}", self.prefix, self.mutex)
    }
}

/// A named mutex inside a private namespace, which stays open for the rest of this process's life once any
/// [`PrivateMutex`] has opened it. Taking it is [`NamedMutex`]'s: one owner thread at a time, an abandoned mutex taken
/// as owned.
pub struct PrivateMutex {
    mutex: NamedMutex,
    created: bool,
}

/// The namespaces this process has open, each with the name it was opened by. A process can open a namespace under a
/// prefix only once (a second open under it fails with `ERROR_DUP_NAME`), so every [`PrivateMutex`] of one name shares
/// one handle. A namespace is never closed: a static is never dropped, so no open can race a close.
static OPEN_NAMESPACES: Mutex<Vec<(PrivateName, PrivateNamespace)>> = Mutex::new(Vec::new());

impl PrivateMutex {
    /// Creates the namespace `name` describes, or opens it when it exists already, then creates or opens the mutex
    /// inside it, unowned. A namespace this process opened before under the same name is shared, not opened again.
    ///
    /// # Errors
    ///
    /// An error of kind [`io::ErrorKind::InvalidInput`] when this process has another namespace open under the same
    /// prefix (another boundary or DACL); the system's error when the boundary, the namespace's descriptor, the
    /// namespace or the mutex cannot be made or opened: a process outside the boundary (one whose token does not hold
    /// its SID) is refused here.
    pub fn open(name: &PrivateName) -> io::Result<PrivateMutex> {
        let created = open_namespace(name)?;
        let mutex = NamedMutex::open(&name.mutex_name())?;
        Ok(PrivateMutex { mutex, created })
    }

    /// Whether this open created the namespace; false when it opened one that existed or shared one this process
    /// held open.
    #[must_use]
    pub fn created_namespace(&self) -> bool {
        self.created
    }

    /// The mutex's full name in this process, `<prefix>\<mutex>`.
    #[must_use]
    pub fn name(&self) -> &str {
        self.mutex.name()
    }

    /// Takes the mutex, waiting at most `timeout`, as [`NamedMutex::acquire`] does.
    ///
    /// # Errors
    ///
    /// [`AcquireError::Timeout`] when it was not free within `timeout`, [`AcquireError::Failed`] when the wait failed.
    pub fn acquire(&self, timeout: Duration) -> Result<MutexGuard<'_>, AcquireError> {
        self.mutex.acquire(timeout)
    }
}

/// Opens the namespace `name` describes for the rest of this process's life, unless this process opened it before;
/// true when this call created it.
fn open_namespace(name: &PrivateName) -> io::Result<bool> {
    let mut open = OPEN_NAMESPACES.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((opened, _)) = open.iter().find(|(opened, _)| opened.prefix == name.prefix) {
        if opened == name {
            return Ok(false);
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "the private namespace prefix {} is open in this process for another namespace ({opened:?}), not {name:?}",
                name.prefix
            ),
        ));
    }
    let namespace = PrivateNamespace::create_or_open(name)?;
    let created = namespace.created;
    open.push((name.clone(), namespace));
    Ok(created)
}

/// An open private namespace, closed without being destroyed when dropped, so another process that holds it open
/// keeps using it.
struct PrivateNamespace {
    handle: HANDLE,
    created: bool,
}

// SAFETY: the handle is a kernel handle, valid from any thread; it is only closed, once, by the drop.
unsafe impl Send for PrivateNamespace {}

impl PrivateNamespace {
    /// The namespace `name` describes: created, or opened when it exists. A namespace that exists at the create and is
    /// gone at the open is looked for again, a few times.
    fn create_or_open(name: &PrivateName) -> io::Result<PrivateNamespace> {
        let boundary = Boundary::new(name)?;
        let descriptor = Descriptor::from_sddl(&name.namespace_sddl)?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).map_err(io::Error::other)?,
            lpSecurityDescriptor: descriptor.as_ptr(),
            bInheritHandle: 0,
        };
        let prefix = wide(&name.prefix);
        let mut last = io::Error::other("the private namespace was never looked for");
        for _ in 0..NAMESPACE_TRIES {
            // SAFETY: the attributes point to a descriptor alive for the call, the boundary is open, and `prefix` is
            // NUL-terminated and outlives the call. A success is a fresh namespace handle nothing else owns.
            let created = unsafe { CreatePrivateNamespaceW(&raw const attributes, boundary.handle, prefix.as_ptr()) };
            if !created.is_null() {
                return Ok(PrivateNamespace {
                    handle: created,
                    created: true,
                });
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_ALREADY_EXISTS.cast_signed()) {
                return Err(error);
            }
            // SAFETY: as above.
            let opened = unsafe { OpenPrivateNamespaceW(boundary.handle, prefix.as_ptr()) };
            if !opened.is_null() {
                return Ok(PrivateNamespace {
                    handle: opened,
                    created: false,
                });
            }
            last = io::Error::last_os_error();
            if last.kind() != io::ErrorKind::NotFound {
                return Err(last);
            }
        }
        Err(last)
    }
}

impl Drop for PrivateNamespace {
    fn drop(&mut self) {
        // SAFETY: the handle is a namespace handle this guard alone owns, closed once; flag 0 leaves the namespace to
        // live on while any other handle holds it.
        unsafe { ClosePrivateNamespace(self.handle, 0) };
    }
}

/// A boundary descriptor, deleted when dropped.
struct Boundary {
    handle: HANDLE,
}

impl Boundary {
    /// The boundary `name` describes: its name and the one SID it holds.
    fn new(name: &PrivateName) -> io::Result<Boundary> {
        let boundary_name = wide(&name.boundary);
        // SAFETY: `boundary_name` is NUL-terminated and outlives the call; no flag is passed.
        let handle = unsafe { CreateBoundaryDescriptorW(boundary_name.as_ptr(), 0) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut boundary = Boundary { handle };
        let sid = Sid::from_string(&name.boundary_sid)?;
        // SAFETY: the boundary is open and the call may replace it through the pointer, which the guard then owns;
        // the SID is valid for the call, which copies it.
        if unsafe { AddSIDToBoundaryDescriptor(&raw mut boundary.handle, sid.0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(boundary)
    }
}

impl Drop for Boundary {
    fn drop(&mut self) {
        // SAFETY: the descriptor is this guard's alone and deleted once.
        unsafe { DeleteBoundaryDescriptor(self.handle) };
    }
}

/// A SID made from its string form, freed when dropped.
struct Sid(PSID);

impl Sid {
    /// The SID `text` names, such as `S-1-5-32-544`.
    fn from_string(text: &str) -> io::Result<Sid> {
        let wide_text = wide(text);
        let mut sid: PSID = std::ptr::null_mut();
        // SAFETY: `wide_text` is NUL-terminated and outlives the call, and `sid` is writable.
        if unsafe { ConvertStringSidToSidW(wide_text.as_ptr(), &raw mut sid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Sid(sid))
    }
}

impl Drop for Sid {
    fn drop(&mut self) {
        // SAFETY: the conversion allocated the SID with `LocalAlloc`; nothing uses it after this.
        unsafe { LocalFree(self.0) };
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose mutex cannot be made must fail loudly")]
mod tests {
    use std::mem::ManuallyDrop;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use super::{AcquireError, NamedMutex, PrivateMutex, PrivateName, PrivateNamespace, mutex_exists};

    /// A guard against a hang, never a timing bound.
    const HANG_GUARD: Duration = Duration::from_secs(300);

    /// A namespace of this test's own: the current user's boundary, named after this process and a counter.
    fn unique_name(case: &str) -> PrivateName {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let user = crate::current_user_sid().expect("the user is read");
        let unique = format!("{case}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        PrivateName::for_user(&user, &unique).expect("the unique part is letters, digits and -")
    }

    #[test]
    fn a_namespace_unique_part_holds_only_ascii_letters_digits_and_hyphens() {
        let user = crate::current_user_sid().expect("the user is read");

        assert!(PrivateName::for_user(&user, "case-1-AB").is_ok());
        for bad in ["", "a b", "a\\b", "a/b", "a_b", "a.b", "é"] {
            let refused = PrivateName::for_user(&user, bad).expect_err("the unique part is refused");
            assert_eq!(refused.kind(), std::io::ErrorKind::InvalidInput, "{bad:?}: {refused}");
        }
    }

    #[test]
    fn a_prefix_open_for_another_namespace_is_refused() {
        let name = unique_name("prefix");
        let _open = PrivateMutex::open(&name).expect("the mutex is made");
        let mut other = name.clone();
        other.boundary = format!("{}-other", name.boundary);

        let refused = PrivateMutex::open(&other).err().expect("the same prefix for another boundary is refused");

        assert_eq!(refused.kind(), std::io::ErrorKind::InvalidInput, "{refused}");
        let mut other_dacl = name.clone();
        other_dacl.namespace_sddl = super::DESKTOP_NAMESPACE_SDDL.to_owned();
        let refused = PrivateMutex::open(&other_dacl)
            .err()
            .expect("the same prefix with another DACL is refused");
        assert_eq!(refused.kind(), std::io::ErrorKind::InvalidInput, "{refused}");
    }

    #[test]
    fn the_production_namespace_is_refused_to_an_unelevated_process() {
        if crate::token_facts().expect("this process's token is read").elevated {
            // An elevated process is within the Administrators boundary, so there is nothing to refuse.
            return;
        }

        let refused = PrivateMutex::open(&PrivateName::desktop())
            .err()
            .expect("an unelevated process is outside the boundary");

        assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied, "{refused}");
    }

    #[test]
    fn a_private_namespace_mutex_excludes_a_second_handle() {
        let name = unique_name("excludes");
        let first = PrivateMutex::open(&name).expect("the mutex is made");
        let held = first.acquire(HANG_GUARD).expect("the mutex is taken");

        let other = name.clone();
        let refused = std::thread::spawn(move || {
            let second = PrivateMutex::open(&other).expect("the mutex opens again");
            matches!(second.acquire(Duration::ZERO), Err(AcquireError::Timeout))
        })
        .join()
        .expect("the other thread ends");

        assert!(refused, "a second handle cannot take a held mutex");
        drop(held);
        let other = name.clone();
        let taken = std::thread::spawn(move || {
            let second = PrivateMutex::open(&other).expect("the mutex opens again");
            second.acquire(HANG_GUARD).map(|guard| guard.abandoned()).ok()
        })
        .join()
        .expect("the other thread ends");
        assert_eq!(taken, Some(false), "a released mutex is taken, not abandoned");
    }

    #[test]
    fn a_private_namespace_mutex_is_taken_abandoned_after_its_owner_thread_ends() {
        let name = unique_name("abandoned");
        let mine = PrivateMutex::open(&name).expect("the mutex is made");
        let other = name.clone();
        std::thread::spawn(move || {
            // The owner thread ends holding it: neither the guard nor the handle is ever dropped to release it.
            let theirs = ManuallyDrop::new(PrivateMutex::open(&other).expect("the mutex opens again"));
            let _held = ManuallyDrop::new(theirs.acquire(HANG_GUARD).expect("the other thread takes it"));
        })
        .join()
        .expect("the owner thread ends");

        let guard = mine.acquire(HANG_GUARD).expect("the abandoned mutex is taken");

        assert!(guard.abandoned(), "the mutex comes back abandoned");
    }

    #[test]
    fn a_private_namespace_is_created_once_and_opened_after() {
        let fresh = unique_name("once");
        let made = PrivateMutex::open(&fresh).expect("the mutex is made");
        let shared = PrivateMutex::open(&fresh).expect("the mutex opens again in this process");
        assert!(made.created_namespace(), "the first open creates the namespace");
        assert!(!shared.created_namespace(), "a second open in this process shares it");

        // Another process's namespace, as one made under another prefix stands for: this process opens it.
        let name = unique_name("opened");
        let mut elsewhere = name.clone();
        elsewhere.prefix = format!("{}-elsewhere", name.prefix);
        let creator = PrivateNamespace::create_or_open(&elsewhere).expect("the other namespace is made");
        assert!(creator.created, "the other prefix's open creates it");

        let opened = PrivateMutex::open(&name).expect("the existing namespace opens");

        assert!(!opened.created_namespace(), "an existing namespace is opened, not created");
        let held = opened.acquire(HANG_GUARD).expect("the opened mutex is taken");
        let other_name = elsewhere.mutex_name();
        let refused = std::thread::spawn(move || {
            let theirs = NamedMutex::open(&other_name).expect("the mutex opens under the other prefix");
            matches!(theirs.acquire(Duration::ZERO), Err(AcquireError::Timeout))
        })
        .join()
        .expect("the other thread ends");
        assert!(refused, "both prefixes name one namespace and one mutex");
        drop(held);
        drop(creator);
    }

    #[test]
    fn a_private_namespace_mutex_cannot_be_opened_by_its_name_in_the_plain_namespace() {
        let name = unique_name("plain");
        let mutex = PrivateMutex::open(&name).expect("the mutex is made");
        let bare = mutex.name().rsplit('\\').next().expect("the name has a last part").to_owned();

        for plain in [format!("Local\\{bare}"), format!("Global\\{bare}")] {
            assert!(!mutex_exists(&plain).expect("the name is looked up"), "{plain} names no mutex");
        }
        assert!(mutex_exists(mutex.name()).expect("the name is looked up"), "the private name finds it");
    }

    #[test]
    fn the_production_boundary_and_namespace_descriptors_name_administrators_and_system() {
        let name = PrivateName::desktop();

        assert_eq!(name.boundary(), "gate-desktop");
        assert_eq!(name.boundary_sid(), "S-1-5-32-544", "the boundary holds the Administrators SID");
        assert_eq!(name.namespace_sddl(), "D:P(A;;GA;;;SY)(A;;GA;;;BA)", "SYSTEM and Administrators alone");
        assert_eq!(name.mutex_name(), "gate\\desktop");
        crate::Descriptor::from_sddl(name.namespace_sddl()).expect("the namespace's descriptor parses");
    }
}
