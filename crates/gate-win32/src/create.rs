//! The one way [`crate::Launch`] and [`crate::PipedLaunch`] create a command: the checks before the call, the
//! inheritable copies of the handles it inherits, the startup information naming its standard handles, and the
//! `CreateProcessW` call itself, or `CreateProcessAsUserW` for a command given a [`crate::RestrictedToken`], under the
//! spawn lock that [`spawn`] takes too.

use std::ffi::c_void;
use std::io;
use std::marker::PhantomData;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::process::{Child, Command};
use std::sync::{Mutex, MutexGuard, PoisonError};

use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, GetLastError, HANDLE};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetCurrentProcess, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_DESKTOP_APP_POLICY,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
    UpdateProcThreadAttribute,
};

use crate::launch::{end_unstarted, end_unstarted_by_id, is_batch, refused, setup, struct_size, wide_os};
use crate::{EnvironmentBlock, LaunchError, RestrictedToken, Suspended, own_handle, win32_message};

/// Held while this crate starts a process: across making the inheritable copies of a launch's handles, its
/// `CreateProcessW` call and closing the copies, and across each [`spawn`]. No inheritable handle of a launch is open
/// while it is not held.
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// Takes the spawn lock. A thread that panicked while holding it left nothing half done that the lock guards, so a
/// poisoned lock is taken all the same.
fn lock_spawns() -> MutexGuard<'static, ()> {
    SPAWN_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Starts `command` as [`Command::spawn`] does, holding the spawn lock that [`crate::Launch`] and
/// [`crate::PipedLaunch`] hold while their inheritable handle copies are open, so the process cannot inherit those
/// copies. The lock is held for the start alone: the caller waits for the child without it.
///
/// A process started any other way, with std's own spawn or another library's, is not kept out: started while a
/// launch's copies are open, it inherits them too, as std lets every inheritable handle through.
///
/// # Errors
///
/// The error [`Command::spawn`] returns.
#[expect(
    clippy::disallowed_methods,
    reason = "the one place outside tests that starts a std process: under the spawn lock"
)]
pub fn spawn(command: &mut Command) -> io::Result<Child> {
    let _spawning = lock_spawns();
    command.spawn()
}

/// The handles a command inherits: its stdin, stdout and stderr, then `others`. None of them is inheritable; the
/// command inherits copies made for the call.
pub(crate) struct Inherited<'h> {
    /// The command's stdin, stdout and stderr.
    pub(crate) stdio: [&'h OwnedHandle; 3],
    /// Any more handles the command inherits, each with the access its copy is made with: `None` for the original's
    /// own access.
    pub(crate) others: &'h [(&'h OwnedHandle, Option<u32>)],
}

/// The `PROC_THREAD_ATTRIBUTE_DESKTOP_APP_POLICY` a command is created with.
#[derive(Clone, Copy, Debug)]
pub(crate) enum DesktopPolicy {
    /// None.
    Absent,
    /// This policy; a system that cannot set it fails the launch.
    Required(u32),
    /// This policy where the system can set it (Windows 10 1703 and later); elsewhere the command is created without
    /// it, and its [`Suspended::note`] says so.
    Wanted(u32),
}

/// Creates the command of `creation` as [`create_process`] does, inheriting the handles of `inherited` and no other,
/// with `desktop_policy` as its `PROC_THREAD_ATTRIBUTE_DESKTOP_APP_POLICY`. Under the spawn lock it makes an
/// inheritable copy of each handle, creates the command with the copies and closes them, so a copy is open only while
/// the lock is held. The command is created on this process's own window station and desktop.
pub(crate) fn create_inheriting(
    creation: &Creation<'_>,
    flags: PROCESS_CREATION_FLAGS,
    inherited: &Inherited<'_>,
    desktop_policy: DesktopPolicy,
) -> Result<Suspended, LaunchError> {
    create_inheriting_on(creation, flags, inherited, desktop_policy, None)
}

/// Creates the command as [`create_inheriting`] does, on the window station and desktop `desktop`
/// (`STARTUPINFO.lpDesktop`, such as `WinSta0\Default`), or on this process's own for `None`.
pub(crate) fn create_inheriting_on(
    creation: &Creation<'_>,
    flags: PROCESS_CREATION_FLAGS,
    inherited: &Inherited<'_>,
    desktop_policy: DesktopPolicy,
    desktop: Option<&str>,
) -> Result<Suspended, LaunchError> {
    // Declared first, so it is released last, after the copies are closed.
    let _spawning = lock_spawns();
    let [input, output, errors] = inherited.stdio;
    let stdio = [
        inheritable_copy(input, None)?,
        inheritable_copy(output, None)?,
        inheritable_copy(errors, None)?,
    ];
    let others = inherited
        .others
        .iter()
        .map(|&(handle, access)| inheritable_copy(handle, access))
        .collect::<Result<Vec<_>, _>>()?;
    let other_refs: Vec<&OwnedHandle> = others.iter().collect();
    let (policy, required) = match desktop_policy {
        DesktopPolicy::Absent => (None, false),
        DesktopPolicy::Required(policy) => (Some(policy), true),
        DesktopPolicy::Wanted(policy) => (Some(policy), false),
    };
    let mut attributes = AttributeList::new(u32::from(policy.is_some()), stdio.each_ref(), &other_refs)?;
    let mut note = None;
    if let Some(policy) = policy.as_ref() {
        // SAFETY: `policy` points into the local `policy`, which outlives the list and is not changed while the list
        // lives; it is one DWORD, as the attribute wants.
        let set = unsafe {
            attributes.update(
                PROC_THREAD_ATTRIBUTE_DESKTOP_APP_POLICY,
                std::ptr::from_ref(policy).cast(),
                size_of::<u32>(),
            )
        };
        // A list whose update failed is unchanged, so the command is created as though the policy was never asked for.
        match set {
            Err(error) if !required => note = Some(format!("created without the desktop-app policy: {error}")),
            other => other?,
        }
    }
    let mut desktop = desktop.map(crate::wide);
    let startup = startup_info(&mut attributes, desktop.as_deref_mut());
    #[cfg(test)]
    {
        let named = inherited.others.iter().map(|&(handle, _)| handle);
        let originals: Vec<HANDLE> = inherited.stdio.iter().copied().chain(named).map(OwnedHandle::raw).collect();
        let copies: Vec<HANDLE> = stdio.iter().chain(&others).map(OwnedHandle::raw).collect();
        tests::in_window(&originals, &copies);
    }
    let mut suspended = create_process(creation, flags, &startup)?;
    suspended.note = note;
    Ok(suspended)
}

/// An inheritable copy of `handle` for one `CreateProcessW` call, with `access`, or the same access as `handle` for
/// `None`.
fn inheritable_copy(handle: &OwnedHandle, access: Option<u32>) -> Result<OwnedHandle, LaunchError> {
    let operation = "copy a handle for the command to inherit";
    let (desired, options) = access.map_or((0, DUPLICATE_SAME_ACCESS), |access| (access, 0));
    let mut copy: HANDLE = std::ptr::null_mut();
    // SAFETY: both process handles are this process's pseudo-handle, which is always open; `handle` is open for the
    // whole call and `copy` is writable.
    let copied = unsafe { DuplicateHandle(GetCurrentProcess(), handle.raw(), GetCurrentProcess(), &raw mut copy, desired, 1, options) };
    if copied == 0 {
        return Err(setup(io::Error::last_os_error(), operation));
    }
    // SAFETY: a success is a fresh handle nothing else owns.
    unsafe { own_handle(copy) }.map_err(|error| setup(error, operation))
}

/// The command to create: the parts every launch decides alike.
pub(crate) struct Creation<'a> {
    /// The program file, as an absolute path.
    pub(crate) application: &'a Path,
    /// The command line, as the program reads it, without a terminating NUL.
    pub(crate) command_line: &'a [u16],
    /// The working folder.
    pub(crate) directory: &'a Path,
    /// The command's whole environment.
    pub(crate) environment: &'a EnvironmentBlock,
    /// The token the command runs with, or `None` for this process's own.
    pub(crate) token: Option<&'a RestrictedToken>,
}

/// Refuses a launch `CreateProcessW` would read otherwise than as written: a NUL cuts a string short, a relative
/// application is searched for, and a batch file runs through cmd.exe without the caller's quoting. The command line
/// is checked for a NUL first, then `application`, then each of `paths`, named by its field.
pub(crate) fn check_launch(command_line: &[u16], application: &Path, paths: &[(&str, &Path)]) -> Result<(), LaunchError> {
    if command_line.contains(&0) {
        return Err(refused("the command line holds a NUL".to_owned()));
    }
    for (field, path) in std::iter::once(("application", application)).chain(paths.iter().copied()) {
        if path.as_os_str().encode_wide().any(|unit| unit == 0) {
            return Err(refused(format!("the {field} {} holds a NUL", path.display())));
        }
    }
    if !application.is_absolute() {
        return Err(refused(format!("the application {} is not an absolute path", application.display())));
    }
    if is_batch(application) {
        return Err(refused(format!(
            "the application {} is a batch file, which runs only through cmd.exe",
            application.display()
        )));
    }
    Ok(())
}

/// A process-and-thread attribute list whose handle list names the handles a command inherits, deleted when dropped.
/// It borrows those handles, so none of them can be closed while it lives.
struct AttributeList<'h> {
    buffer: Vec<usize>,
    /// The command's stdin, stdout and stderr, the first three of `handles`.
    stdio: [HANDLE; 3],
    /// The handle list's handles, which the list points to: never changed after [`AttributeList::new`], and a `Vec`'s
    /// heap block stays where it is when the list moves.
    handles: Vec<HANDLE>,
    /// The handles named, open for as long as the list lives.
    borrowed: PhantomData<&'h OwnedHandle>,
}

impl<'h> AttributeList<'h> {
    /// A list with room for the handle list and `extra` more attributes, whose handle list names `stdio` (the
    /// command's stdin, stdout and stderr) and `others`: a command created with the list inherits those handles and no
    /// other.
    fn new(extra: u32, stdio: [&'h OwnedHandle; 3], others: &[&'h OwnedHandle]) -> Result<AttributeList<'h>, LaunchError> {
        let count = extra + 1;
        let mut size = 0;
        // SAFETY: a null list asks only for the size a list of `count` attributes needs, written to `size`.
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &raw mut size) };
        if size == 0 {
            let error = io::Error::last_os_error();
            return Err(setup(error, "size the attribute list"));
        }
        let mut buffer = vec![0_usize; size.div_ceil(size_of::<usize>())];
        // SAFETY: `buffer` holds at least `size` bytes, aligned for the pointers the list holds.
        if unsafe { InitializeProcThreadAttributeList(buffer.as_mut_ptr().cast(), count, 0, &raw mut size) } == 0 {
            let error = io::Error::last_os_error();
            return Err(setup(error, "initialize the attribute list"));
        }
        let stdio = stdio.map(OwnedHandle::raw);
        let handles = stdio.iter().copied().chain(others.iter().map(|handle| handle.raw())).collect();
        let mut list = AttributeList {
            buffer,
            stdio,
            handles,
            borrowed: PhantomData,
        };
        let value = list.handles.as_ptr().cast();
        let bytes = size_of_val(list.handles.as_slice());
        // SAFETY: the list owns `handles`, never changes it after this and keeps it as long as the list lives, and its
        // heap block does not move with the list, so the values stay where the list points; they are an array of
        // handles, as the attribute wants, each open for as long as the list borrows it.
        unsafe { list.update(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, value, bytes) }?;
        Ok(list)
    }

    /// The list, as `STARTUPINFOEXW` points to it.
    fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buffer.as_mut_ptr().cast()
    }

    /// Sets `attribute` to the `size` bytes at `value`.
    ///
    /// # Safety
    ///
    /// `value` points to `size` readable bytes, as `attribute` wants them, that stay valid and unchanged for as long
    /// as the list lives: the list keeps the pointer, not a copy.
    unsafe fn update(&mut self, attribute: u32, value: *const c_void, size: usize) -> Result<(), LaunchError> {
        // SAFETY: the list was initialized; the caller guarantees `value` and `size`.
        let updated = unsafe { UpdateProcThreadAttribute(self.as_ptr(), 0, attribute as usize, value, size, std::ptr::null_mut(), std::ptr::null()) };
        if updated == 0 {
            let error = io::Error::last_os_error();
            return Err(setup(error, &format!("set the process attribute 0x{attribute:x}")));
        }
        Ok(())
    }
}

impl Drop for AttributeList<'_> {
    fn drop(&mut self) {
        // SAFETY: the list was initialized, and is deleted only here, once.
        unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
    }
}

/// The startup information of a command, which borrows the attribute list it points to, and through it the handles
/// the list names, and the desktop name it points to, so none can be dropped while [`create_process`] may read it.
struct Startup<'a> {
    info: STARTUPINFOEXW,
    borrowed: PhantomData<&'a ()>,
}

/// The startup information of a command created with `attributes`, its stdin, stdout and stderr the list's `stdio`,
/// on `desktop` (a NUL-terminated `station\desktop` name) or, for `None`, on this process's own.
fn startup_info<'a>(attributes: &'a mut AttributeList<'_>, desktop: Option<&'a mut [u16]>) -> Startup<'a> {
    let [input, output, errors] = attributes.stdio;
    let mut info = STARTUPINFOEXW::default();
    info.StartupInfo.cb = struct_size::<STARTUPINFOEXW>();
    if let Some(desktop) = desktop {
        info.StartupInfo.lpDesktop = desktop.as_mut_ptr();
    }
    info.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    info.StartupInfo.hStdInput = input;
    info.StartupInfo.hStdOutput = output;
    info.StartupInfo.hStdError = errors;
    info.lpAttributeList = attributes.as_ptr();
    Startup { info, borrowed: PhantomData }
}

/// Calls `CreateProcessW` for `creation` with `startup`, whose list and handles stay open for the whole call, or
/// `CreateProcessAsUserW` with its token when it has one. The command is created suspended, reading its environment as
/// UTF-16 and `startup` as a `STARTUPINFOEXW`; `flags` are the caller's own creation flags on top of those.
fn create_process(creation: &Creation<'_>, flags: PROCESS_CREATION_FLAGS, startup: &Startup<'_>) -> Result<Suspended, LaunchError> {
    let application = wide_os(creation.application.as_os_str());
    let mut line: Vec<u16> = creation.command_line.iter().copied().chain(Some(0)).collect();
    let directory = wide_os(creation.directory.as_os_str());
    let flags = CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT | flags;
    let environment = creation.environment.0.as_ptr().cast();
    let info = std::ptr::from_ref(&startup.info).cast::<STARTUPINFOW>();
    let mut created = PROCESS_INFORMATION::default();
    // SAFETY: `application` and `directory` are NUL-terminated, `line` is a writable NUL-terminated buffer the call
    // may change, the environment is a block `EnvironmentBlock` built (ended by two NULs) and
    // `CREATE_UNICODE_ENVIRONMENT` says it is UTF-16; `startup` is a whole `STARTUPINFOEXW` whose `cb` says so and
    // `EXTENDED_STARTUPINFO_PRESENT` says so too, and it borrows its attribute list, which borrows the handles it
    // names, and its desktop name, NUL-terminated, when it has one; every buffer outlives the call and `created` is writable. A token is borrowed by `creation` for its whole
    // life, so it stays open across the call, with the access `RestrictedToken` opened it with: primary assignment,
    // duplication, query and default adjustment.
    let ok = unsafe {
        match creation.token {
            None => CreateProcessW(
                application.as_ptr(),
                line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                flags,
                environment,
                directory.as_ptr(),
                info,
                &raw mut created,
            ),
            Some(token) => CreateProcessAsUserW(
                token.handle().raw(),
                application.as_ptr(),
                line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                flags,
                environment,
                directory.as_ptr(),
                info,
                &raw mut created,
            ),
        }
    };
    if ok == 0 {
        // SAFETY: `GetLastError` takes no pointers; it is read before anything else can set it.
        let code = unsafe { GetLastError() };
        return Err(LaunchError::CreateProcess {
            code,
            message: win32_message(code),
        });
    }
    // The suspended command is in no job yet: a failure below ends it, so it cannot hold its log or pipes open for good.
    // SAFETY: a success returns a fresh thread handle nothing else owns.
    let thread = unsafe { own_handle(created.hThread) };
    // SAFETY: a success returns a fresh process handle nothing else owns.
    let process = match unsafe { own_handle(created.hProcess) } {
        Ok(process) => process,
        Err(error) => {
            end_unstarted_by_id(created.dwProcessId);
            return Err(setup(error, "own the command's process"));
        }
    };
    let thread = match thread {
        Ok(thread) => thread,
        Err(error) => {
            end_unstarted(&process);
            return Err(setup(error, "own the command's thread"));
        }
    };
    Ok(Suspended {
        process: Some(process),
        thread,
        pid: created.dwProcessId,
        pipes: None,
        note: None,
    })
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot start its command must fail loudly")]
mod tests {
    use std::cell::Cell;
    use std::path::PathBuf;
    use std::sync::{Mutex, PoisonError, TryLockError};

    use std::os::windows::io::{AsRawHandle, BorrowedHandle};

    use proc_table::win32::OwnedHandle;
    use windows_sys::Win32::Foundation::{CompareObjectHandles, GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::System::Threading::{
        BELOW_NORMAL_PRIORITY_CLASS, CreateEventW, GetCurrentProcess, GetPriorityClass, NORMAL_PRIORITY_CLASS, SetPriorityClass,
    };

    use super::SPAWN_LOCK;
    use crate::piped::pipe_from_child;
    use crate::{DesktopLaunch, EnvironmentBlock, Job, Launch, LaunchOutput, PipedConsole, PipedLaunch, RemoteProcess, thread_is_suspended};

    /// Keeps these tests from running at once, so that one's launch never holds the spawn lock while the other checks
    /// that it is free. Every unit test of this crate that starts a process takes it for its whole run, or that check
    /// races.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    thread_local! {
        /// How many handles the last spawn window of this thread checked, until a test takes it.
        static CHECKED: Cell<Option<usize>> = const { Cell::new(None) };
        /// The values of the inheritable copies the last spawn window of this thread checked, stdin, stdout and stderr
        /// first, until a test takes them: the command inherits each under the same value.
        static COPIES: Cell<Vec<usize>> = const { Cell::new(Vec::new()) };
    }

    /// Whether the system says `handle` is inheritable.
    fn inheritable(handle: HANDLE) -> bool {
        let mut flags = 0;
        // SAFETY: the caller keeps the handle open for the whole call, and `flags` is writable.
        let read = unsafe { GetHandleInformation(handle, &raw mut flags) };
        assert_ne!(read, 0, "the handle's flags are read: {}", std::io::Error::last_os_error());
        flags & HANDLE_FLAG_INHERIT != 0
    }

    /// Whether the spawn lock is held, by this thread or another.
    fn spawn_lock_held() -> bool {
        matches!(SPAWN_LOCK.try_lock(), Err(TryLockError::WouldBlock))
    }

    /// Runs inside the spawn window, after the inheritable copies are made and before `CreateProcessW`: asserts that
    /// no original is inheritable, that every copy is and that the spawn lock is held, and records how many handles
    /// it checked.
    pub(super) fn in_window(originals: &[HANDLE], copies: &[HANDLE]) {
        assert_eq!(originals.len(), copies.len(), "each original has one copy");
        for &original in originals {
            assert!(!inheritable(original), "an original handle is not inheritable");
        }
        for &copy in copies {
            assert!(inheritable(copy), "a copy is inheritable");
        }
        assert!(spawn_lock_held(), "the spawn lock is held in the window");
        CHECKED.set(Some(originals.len()));
        COPIES.set(copies.iter().map(|copy| copy.addr()).collect());
    }

    /// The system folder, where cmd.exe is.
    fn system_folder() -> PathBuf {
        PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot is set")).join("System32")
    }

    /// This process's environment, as a block.
    fn environment() -> EnvironmentBlock {
        EnvironmentBlock::new(std::env::vars_os()).expect("the block is built")
    }

    #[test]
    fn a_launch_hands_over_inheritable_copies_under_the_spawn_lock() {
        let _alone = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!("gate-win32-create-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("the case folder is created");
        let system = system_folder();
        let launch = Launch {
            application: system.join("cmd.exe"),
            command_line: "cmd /d /c exit 0".encode_utf16().collect(),
            directory: system,
            environment: environment(),
            output: LaunchOutput::Files {
                log: dir.join("out.log"),
                err_log: dir.join("out.log.err"),
            },
            priority: None,
        };

        let suspended = launch.start_suspended().expect("the command is created");

        assert_eq!(CHECKED.take(), Some(3), "the window checked NUL, the log and the .err file");
        assert!(!spawn_lock_held(), "the spawn lock is free once the launch returns");
        drop(suspended);
        std::fs::remove_dir_all(&dir).expect("the case folder is removed");
    }

    #[test]
    fn a_piped_launch_hands_over_inheritable_copies_under_the_spawn_lock() {
        let _alone = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        let job = Job::create(&format!(r"Local\gate-win32-create-{}", std::process::id())).expect("the job is created");
        let system = system_folder();
        let launch = PipedLaunch {
            application: system.join("cmd.exe"),
            command_line: "cmd /d /c exit 0".encode_utf16().collect(),
            directory: system,
            environment: environment(),
            console: PipedConsole::Hidden,
            token: None,
        };

        // `CreateProcessW` refuses the command when this process's job refuses breakaway; the window comes before the
        // call either way, and a command that was created is ended when dropped.
        drop(launch.start_suspended(&job));

        assert_eq!(CHECKED.take(), Some(4), "the window checked NUL, the two pipe write ends and the job");
        assert!(!spawn_lock_held(), "the spawn lock is free once the launch returns");
    }

    /// A desktop launch of cmd that would exit at once, were it ever resumed.
    fn desktop_launch() -> DesktopLaunch {
        let system = system_folder();
        DesktopLaunch {
            application: system.join("cmd.exe"),
            command_line: "cmd /d /c exit 0".encode_utf16().collect(),
            directory: system,
            environment: environment(),
        }
    }

    /// `handle`, borrowed.
    fn borrow(handle: &OwnedHandle) -> BorrowedHandle<'_> {
        // SAFETY: the guard keeps the handle open for as long as the borrow lives.
        unsafe { BorrowedHandle::borrow_raw(handle.raw()) }
    }

    /// The priority class of the process behind `process`.
    fn class_of(process: HANDLE) -> u32 {
        // SAFETY: the caller keeps the handle open, with query access, for the whole call.
        let class = unsafe { GetPriorityClass(process) };
        assert_ne!(class, 0, "the priority class is read: {}", std::io::Error::last_os_error());
        class
    }

    /// This process lowered to the below-normal class, put back to the class it had when dropped.
    struct Lowered(u32);

    impl Lowered {
        fn set() -> Lowered {
            // SAFETY: this process's pseudo-handle is always open, with every access.
            let before = class_of(unsafe { GetCurrentProcess() });
            // SAFETY: as above.
            assert_ne!(
                unsafe { SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS) },
                0,
                "the class is lowered"
            );
            Lowered(before)
        }
    }

    impl Drop for Lowered {
        fn drop(&mut self) {
            // SAFETY: as in `Lowered::set`. A restore that fails leaves the class lowered for the rest of this test's
            // own process alone.
            unsafe { SetPriorityClass(GetCurrentProcess(), self.0) };
        }
    }

    #[test]
    fn a_helper_created_child_is_suspended_at_normal_priority_under_a_below_normal_parent() {
        let _alone = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        let (_stdout, output) = pipe_from_child("stdout").expect("the stdout pipe is made");
        let (_stderr, errors) = pipe_from_child("stderr").expect("the stderr pipe is made");

        let suspended = {
            let _lowered = Lowered::set();
            assert!(
                crate::runs_below_normal().expect("this process's class is read"),
                "the parent runs below normal"
            );
            desktop_launch()
                .start_suspended(borrow(&output), borrow(&errors))
                .expect("the command is created")
        };

        let process = suspended.process.as_ref().expect("the process is held");
        assert_eq!(class_of(process.raw()), NORMAL_PRIORITY_CLASS);
        assert!(thread_is_suspended(borrow(&suspended.thread)).expect("the thread is read"));
    }

    #[test]
    fn a_helper_created_child_inherits_only_its_listed_handles() {
        let _alone = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        let (_stdout, output) = pipe_from_child("stdout").expect("the stdout pipe is made");
        let (_stderr, errors) = pipe_from_child("stderr").expect("the stderr pipe is made");
        let inheritable_attributes = SECURITY_ATTRIBUTES {
            nLength: crate::launch::struct_size::<SECURITY_ATTRIBUTES>(),
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 1,
        };
        // SAFETY: the attributes are whole and outlive the call, and a null name is allowed. A success is a fresh
        // handle nothing else owns.
        let stray = unsafe { crate::own_handle(CreateEventW(&raw const inheritable_attributes, 1, 0, std::ptr::null())) }.expect("the event is made");
        assert!(inheritable(stray.raw()), "a handle any child that inherits every handle would get");

        let suspended = desktop_launch()
            .start_suspended(borrow(&output), borrow(&errors))
            .expect("the command is created");

        let copies = COPIES.take();
        assert_eq!(CHECKED.take(), Some(3), "the window checked NUL and the two write ends");
        let child = RemoteProcess::open(suspended.pid()).expect("the child opens");
        let held: Vec<std::os::windows::io::OwnedHandle> = copies
            .iter()
            .map(|&value| child.pull(value).expect("the child holds each listed handle"))
            .collect();
        for (pulled, original) in held[1..].iter().zip([&output, &errors]) {
            // SAFETY: both handles are open for the whole call.
            let same = unsafe { CompareObjectHandles(pulled.as_raw_handle(), original.raw()) } != 0;
            assert!(same, "the child's stdout and stderr are the given write ends");
        }
        let not_inherited = child.pull(stray.raw().addr()).map_or(true, |pulled| {
            // SAFETY: both handles are open for the whole call.
            unsafe { CompareObjectHandles(pulled.as_raw_handle(), stray.raw()) == 0 }
        });
        assert!(not_inherited, "the child does not hold the inheritable handle it was not given");
    }
}
