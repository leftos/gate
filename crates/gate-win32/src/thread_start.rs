//! Where a thread began and how long it has waited, read without stopping it: its Win32 start address
//! (`NtQueryInformationThread(ThreadQuerySetWin32StartAddress)`), the modules a process has loaded, as name, base and
//! size, so an address reads as `name+0xoffset`, and the milliseconds since a tick count the process list gives.

use std::io;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::ptr;

use proc_table::win32::open_process;
use windows_sys::Wdk::System::Threading::{NtQueryInformationThread, ThreadQuerySetWin32StartAddress};
use windows_sys::Win32::Foundation::{HANDLE, HMODULE, NTSTATUS, STATUS_ACCESS_DENIED};
use windows_sys::Win32::System::ProcessStatus::{EnumProcessModulesEx, GetModuleBaseNameW, GetModuleInformation, LIST_MODULES_ALL, MODULEINFO};
use windows_sys::Win32::System::Threading::{
    GetProcessIdOfThread, OpenThread, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ, THREAD_ACCESS_RIGHTS, THREAD_QUERY_INFORMATION,
    THREAD_QUERY_LIMITED_INFORMATION,
};

/// How many module handles the first read of a process's module list makes room for, and how many times the list is
/// read before one that keeps outgrowing its buffer is an error.
const MODULES_FIRST_READ: usize = 256;
const MODULE_LIST_ATTEMPTS: usize = 4;
/// The most characters a module name read holds (`MAX_PATH`).
const MODULE_NAME_CHARACTERS: usize = 260;

/// One module a process has loaded: its file name, its base address and its size in bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Module {
    /// The module's file name, as `GetModuleBaseNameW` gives it.
    pub name: String,
    /// The address it is loaded at.
    pub base: usize,
    /// Its size in memory, in bytes.
    pub size: usize,
}

/// The Win32 start address of thread `tid` of process `pid`: the function `CreateThread` was given. A thread id that
/// now names a thread of another process, its own having ended and the id been reused, is refused.
///
/// # Errors
///
/// The system's error when the thread cannot be opened, an error naming both processes when it belongs to another,
/// or the call's status when the address cannot be read.
pub fn thread_start_address(pid: u32, tid: u32) -> io::Result<usize> {
    let limited = open_thread(pid, tid, THREAD_QUERY_LIMITED_INFORMATION)?;
    match start_address_of(&limited) {
        Err(STATUS_ACCESS_DENIED) => start_address_of(&open_thread(pid, tid, THREAD_QUERY_INFORMATION)?).map_err(|status| query_failed(tid, status)),
        read => read.map_err(|status| query_failed(tid, status)),
    }
}

/// Thread `tid` of process `pid`, opened with `access`.
fn open_thread(pid: u32, tid: u32, access: THREAD_ACCESS_RIGHTS) -> io::Result<OwnedHandle> {
    // SAFETY: OpenThread takes no pointers; it returns null on failure.
    let handle = unsafe { OpenThread(access, 0, tid) };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        return Err(io::Error::new(error.kind(), format!("OpenThread({tid}) failed: {error}")));
    }
    // SAFETY: the handle is a fresh one nothing else owns, so the guard alone closes it.
    let thread = unsafe { OwnedHandle::from_raw_handle(handle) };
    // SAFETY: the handle is open with query access, which the call needs; it returns 0 on failure.
    let owner = unsafe { GetProcessIdOfThread(thread.as_raw_handle()) };
    if owner != pid {
        return Err(io::Error::other(format!(
            "thread {tid} belongs to process {owner}, not {pid}: its id was reused"
        )));
    }
    Ok(thread)
}

/// The Win32 start address of the open thread `thread`, or the status of the query that failed.
fn start_address_of(thread: &OwnedHandle) -> Result<usize, NTSTATUS> {
    let mut address = 0_usize;
    let size = u32::try_from(size_of::<usize>()).unwrap_or(u32::MAX);
    // SAFETY: the handle is open; the buffer is a writable usize of `size` bytes, the size the class writes; the
    // returned length is optional.
    let status = unsafe {
        NtQueryInformationThread(
            thread.as_raw_handle(),
            ThreadQuerySetWin32StartAddress,
            (&raw mut address).cast(),
            size,
            ptr::null_mut(),
        )
    };
    if status < 0 { Err(status) } else { Ok(address) }
}

/// The error of a start address query of thread `tid` that failed with `status`.
fn query_failed(tid: u32, status: NTSTATUS) -> io::Error {
    io::Error::other(format!(
        "NtQueryInformationThread({tid}, ThreadQuerySetWin32StartAddress) failed with status 0x{status:08X}"
    ))
}

/// The modules process `pid` has loaded, 32-bit and 64-bit alike.
///
/// # Errors
///
/// The system's error when the process cannot be opened or its modules listed.
pub fn process_modules(pid: u32) -> io::Result<Vec<Module>> {
    let process = open_process(pid, PROCESS_QUERY_INFORMATION | PROCESS_VM_READ)
        .ok_or_else(|| io::Error::other(format!("OpenProcess({pid}) failed: {}", io::Error::last_os_error())))?;
    let handles = module_handles(pid, process.raw())?;
    // A module that unloads between the list and its read is left out.
    Ok(handles.into_iter().filter_map(|handle| module_of(process.raw(), handle)).collect())
}

/// The handles of the modules the open process `process` (`pid`) has loaded, the list grown until it fits.
fn module_handles(pid: u32, process: HANDLE) -> io::Result<Vec<HMODULE>> {
    let mut handles: Vec<HMODULE> = vec![ptr::null_mut(); MODULES_FIRST_READ];
    for _ in 0..MODULE_LIST_ATTEMPTS {
        let bytes = u32::try_from(size_of_val(handles.as_slice())).map_err(io::Error::other)?;
        let mut needed = 0_u32;
        // SAFETY: the process handle is open; the buffer is writable for `bytes` bytes and `needed` is writable.
        let listed = unsafe { EnumProcessModulesEx(process, handles.as_mut_ptr(), bytes, &raw mut needed, LIST_MODULES_ALL) };
        if listed == 0 {
            let error = io::Error::last_os_error();
            return Err(io::Error::new(error.kind(), format!("EnumProcessModulesEx({pid}) failed: {error}")));
        }
        let count = usize::try_from(needed).map_err(io::Error::other)? / size_of::<HMODULE>();
        if needed <= bytes {
            handles.truncate(count);
            return Ok(handles);
        }
        handles.resize(count + MODULES_FIRST_READ, ptr::null_mut());
    }
    Err(io::Error::other(format!(
        "EnumProcessModulesEx({pid}): the module list kept outgrowing its buffer after {MODULE_LIST_ATTEMPTS} attempts"
    )))
}

/// The module `module` of the open process `process`, or `None` when its name or extent cannot be read.
fn module_of(process: HANDLE, module: HMODULE) -> Option<Module> {
    let mut name = [0_u16; MODULE_NAME_CHARACTERS];
    let capacity = u32::try_from(name.len()).ok()?;
    // SAFETY: the process handle is open; the buffer is writable for `capacity` characters.
    let length = unsafe { GetModuleBaseNameW(process, module, name.as_mut_ptr(), capacity) };
    let mut info = MODULEINFO::default();
    let size = u32::try_from(size_of::<MODULEINFO>()).ok()?;
    // SAFETY: the process handle is open; `info` is a writable MODULEINFO of `size` bytes.
    let read = unsafe { GetModuleInformation(process, module, &raw mut info, size) };
    if length == 0 || read == 0 {
        return None;
    }
    Some(Module {
        name: String::from_utf16_lossy(name.get(..usize::try_from(length).ok()?)?),
        base: info.lpBaseOfDll.addr(),
        size: usize::try_from(info.SizeOfImage).ok()?,
    })
}

/// The module of `modules` that holds `address`, and the address's offset into it; `None` when no module holds it.
#[must_use]
pub fn resolve_address(modules: &[Module], address: usize) -> Option<(&Module, usize)> {
    modules.iter().find_map(|module| {
        let offset = address.checked_sub(module.base)?;
        (offset < module.size).then_some((module, offset))
    })
}

/// Where `KUSER_SHARED_DATA`, the page the kernel maps read-only into every process, keeps the system's tick count in
/// clock ticks (`TickCount.LowPart`) and the multiplier that turns ticks into milliseconds (`TickCountMultiplier`, a
/// 24-bit fixed-point factor), as `ntddk.h` fixes them; `GetTickCount` reads the same two fields.
const TICK_COUNT_ADDRESS: usize = 0x7FFE_0320;
const TICK_COUNT_MULTIPLIER_ADDRESS: usize = 0x7FFE_0004;

/// The milliseconds since the system's tick count was `tick`, a tick count in clock ticks as the process list gives a
/// thread's wait start; a tick count past now wraps, as the 32-bit count does.
#[must_use]
pub fn waited_ms(tick: u32) -> u64 {
    let ticks = std::ptr::with_exposed_provenance::<u32>(TICK_COUNT_ADDRESS);
    let multiplier = std::ptr::with_exposed_provenance::<u32>(TICK_COUNT_MULTIPLIER_ADDRESS);
    // SAFETY: the shared user data page is mapped readable at a fixed address in every Windows process for its whole
    // life, and both fields are aligned `u32`s the kernel updates atomically.
    let (now, multiplier) = unsafe { (ticks.read_volatile(), multiplier.read_volatile()) };
    (u64::from(now.wrapping_sub(tick)) * u64::from(multiplier)) >> 24
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a read that fails must fail the test loudly")]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use windows_sys::Win32::System::Threading::GetCurrentThreadId;

    use super::{process_modules, resolve_address, thread_start_address};

    #[test]
    fn a_spawned_thread_starts_in_this_test_binary() {
        let (said, tid) = mpsc::channel();
        let (release, held) = mpsc::channel::<()>();
        let spawned = thread::spawn(move || {
            // SAFETY: the call takes no arguments and cannot fail.
            said.send(unsafe { GetCurrentThreadId() }).expect("the test reads the id");
            // Held until the test has read where the thread began.
            let _ = held.recv();
        });
        let tid = tid.recv().expect("the thread says its id");

        let start = thread_start_address(std::process::id(), tid);
        let modules = process_modules(std::process::id());
        release.send(()).expect("the thread waits for its release");
        spawned.join().expect("the thread ends");

        let start = start.expect("the start address of a thread of this process reads");
        let modules = modules.expect("this process's modules list");
        let exe = std::env::current_exe().expect("the test executable has a path");
        let file = exe.file_name().expect("the path names a file").to_string_lossy().into_owned();
        let (module, offset) = resolve_address(&modules, start).expect("the start address is inside a module of this process");
        assert!(
            module.name.eq_ignore_ascii_case(&file),
            "the thread began at {}+0x{offset:x}, not in the test binary {file:?}",
            module.name
        );
        assert!(
            offset < module.size,
            "the offset 0x{offset:x} is inside the module's 0x{:x} bytes",
            module.size
        );
    }

    #[test]
    fn a_thread_asked_for_under_another_process_gives_no_start() {
        // SAFETY: the call takes no arguments and cannot fail.
        let tid = unsafe { GetCurrentThreadId() };
        // Process ids are multiples of 4, so this one is never this process's.
        let other = std::process::id() + 4;

        let start = thread_start_address(other, tid);

        assert!(
            start.is_err(),
            "thread {tid} of this process is not taken for one of process {other}: {start:?}"
        );
    }

    #[test]
    fn an_address_outside_every_module_does_not_resolve() {
        let modules = process_modules(std::process::id()).expect("this process's modules list");
        assert!(!modules.is_empty(), "this process has modules");
        // The first 64 KiB of the address space are never mapped, and no module ends at the top of it.
        for address in [0x10, usize::MAX] {
            assert_eq!(resolve_address(&modules, address), None, "0x{address:x} is in no module");
        }
    }
}
