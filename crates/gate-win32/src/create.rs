//! The one way [`crate::Launch`] and [`crate::PipedLaunch`] create a command: the checks before the call, the
//! startup information naming its standard handles, and the `CreateProcessW` call itself.

use std::ffi::c_void;
use std::io;
use std::marker::PhantomData;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::{GetLastError, HANDLE};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW, UpdateProcThreadAttribute,
};

use crate::launch::{is_batch, refused, setup, struct_size, wide_os};
use crate::{EnvironmentBlock, LaunchError, Suspended, own_handle, win32_message};

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
pub(crate) struct AttributeList<'h> {
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
    pub(crate) fn new(extra: u32, stdio: [&'h OwnedHandle; 3], others: &[&'h OwnedHandle]) -> Result<AttributeList<'h>, LaunchError> {
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
    pub(crate) unsafe fn update(&mut self, attribute: u32, value: *const c_void, size: usize) -> Result<(), LaunchError> {
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
/// the list names, so neither can be dropped while [`create_process`] may read it.
pub(crate) struct Startup<'a> {
    info: STARTUPINFOEXW,
    borrowed: PhantomData<&'a ()>,
}

/// The startup information of a command created with `attributes`, its stdin, stdout and stderr the list's `stdio`.
pub(crate) fn startup_info<'a>(attributes: &'a mut AttributeList<'_>) -> Startup<'a> {
    let [input, output, errors] = attributes.stdio;
    let mut info = STARTUPINFOEXW::default();
    info.StartupInfo.cb = struct_size::<STARTUPINFOEXW>();
    info.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    info.StartupInfo.hStdInput = input;
    info.StartupInfo.hStdOutput = output;
    info.StartupInfo.hStdError = errors;
    info.lpAttributeList = attributes.as_ptr();
    Startup { info, borrowed: PhantomData }
}

/// Calls `CreateProcessW` for `creation` with `startup`, whose list and handles stay open for the whole call. The
/// command is created suspended, reading its environment as UTF-16 and `startup` as a `STARTUPINFOEXW`; `flags` are
/// the caller's own creation flags on top of those.
pub(crate) fn create_process(creation: &Creation<'_>, flags: PROCESS_CREATION_FLAGS, startup: &Startup<'_>) -> Result<Suspended, LaunchError> {
    let application = wide_os(creation.application.as_os_str());
    let mut line: Vec<u16> = creation.command_line.iter().copied().chain(Some(0)).collect();
    let directory = wide_os(creation.directory.as_os_str());
    let flags = CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT | flags;
    let mut created = PROCESS_INFORMATION::default();
    // SAFETY: `application` and `directory` are NUL-terminated, `line` is a writable NUL-terminated buffer the call
    // may change, the environment is a block `EnvironmentBlock` built (ended by two NULs) and
    // `CREATE_UNICODE_ENVIRONMENT` says it is UTF-16; `startup` is a whole `STARTUPINFOEXW` whose `cb` says so and
    // `EXTENDED_STARTUPINFO_PRESENT` says so too, and it borrows its attribute list, which borrows the handles it
    // names; every buffer outlives the call and `created` is writable.
    let ok = unsafe {
        CreateProcessW(
            application.as_ptr(),
            line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            flags,
            creation.environment.0.as_ptr().cast(),
            directory.as_ptr(),
            std::ptr::from_ref(&startup.info).cast::<STARTUPINFOW>(),
            &raw mut created,
        )
    };
    if ok == 0 {
        // SAFETY: `GetLastError` takes no pointers; it is read before anything else can set it.
        let code = unsafe { GetLastError() };
        return Err(LaunchError::CreateProcess {
            code,
            message: win32_message(code),
        });
    }
    // SAFETY: a success returns a fresh process handle nothing else owns.
    let process = unsafe { own_handle(created.hProcess) }.map_err(|error| setup(error, "own the command's process"))?;
    // SAFETY: a success returns a fresh thread handle nothing else owns.
    let thread = unsafe { own_handle(created.hThread) }.map_err(|error| setup(error, "own the command's thread"))?;
    Ok(Suspended {
        process: Some(process),
        thread,
        pid: created.dwProcessId,
    })
}
