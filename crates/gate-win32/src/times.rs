//! The CPU times a watch samples: the system's, those of one process with the time it was created, and the WSL VM's,
//! all in the 100 ns ticks of a `FILETIME`.

use std::io;
use std::mem::offset_of;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows_sys::Wdk::System::SystemInformation::{NtQuerySystemInformation, SystemProcessInformation};
use windows_sys::Win32::Foundation::{FILETIME, STATUS_INFO_LENGTH_MISMATCH, UNICODE_STRING};
use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows_sys::Win32::System::Threading::{GetProcessTimes, GetSystemTimes};
use windows_sys::Win32::System::WindowsProgramming::SYSTEM_PROCESS_INFORMATION;

use crate::open_running;

/// The image name of the process whose CPU is the WSL VM's: Ubuntu and docker-desktop share it.
const VM_IMAGE: &str = "vmmemWSL";
/// What the process list is called in an error.
const PROCESS_LIST: &str = "NtQuerySystemInformation(SystemProcessInformation)";
/// Where an entry of the process list keeps its creation, user and kernel times (each an `i64` of 100 ns ticks), inside
/// the `Reserved1` bytes of `SYSTEM_PROCESS_INFORMATION`.
const CREATE_TIME_AT: usize = 32;
const USER_TIME_AT: usize = 40;
const KERNEL_TIME_AT: usize = 48;
/// Where an entry keeps the link to the next one, and its image name.
const NEXT_AT: usize = offset_of!(SYSTEM_PROCESS_INFORMATION, NextEntryOffset);
const NAME_AT: usize = offset_of!(SYSTEM_PROCESS_INFORMATION, ImageName);
const NAME_BUFFER_AT: usize = NAME_AT + offset_of!(UNICODE_STRING, Buffer);
/// Where an entry keeps its process id.
const PID_AT: usize = offset_of!(SYSTEM_PROCESS_INFORMATION, UniqueProcessId);
/// The bytes of an entry this reader reads: everything up to the end of the process id.
const ENTRY_READ: usize = PID_AT + size_of::<usize>();
/// What the buffer grows by over the size the system last asked for, as the list can grow between two calls.
const LIST_SLACK: usize = 64 * 1024;
/// How many times the list is asked for before a list that keeps outgrowing its buffer is an error.
const LIST_ATTEMPTS: usize = 8;
/// How far past now a VM entry's creation time may lie, in 100 ns ticks, before it reads as a changed layout: a day,
/// so a clock stepped back since the VM started is no error.
const CLOCK_STEP_MARGIN: u64 = 24 * 60 * 60 * 10_000_000;

/// The size in bytes of the last buffer the process list fit in, which the next read starts at, so a steady machine
/// asks for the list once a read.
static LIST_SIZE: AtomicUsize = AtomicUsize::new(LIST_SLACK);

/// One entry of the process list, as the walk decodes it; the times are `i64`s of 100 ns ticks, as the list holds them.
struct Entry<'a> {
    #[cfg_attr(not(test), expect(dead_code, reason = "only the test of the walk finds an entry by its pid"))]
    pid: usize,
    /// The image name, UTF-16 in the list's byte order; empty for the idle process.
    name: &'a [u8],
    create: i64,
    kernel: i64,
    user: i64,
}

/// The system's CPU times since it started, summed over every processor, in 100 ns ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SystemTimes {
    /// The ticks the processors spent idle.
    pub idle: u64,
    /// The ticks they spent in kernel mode, idle time included.
    pub kernel: u64,
    /// The ticks they spent in user mode.
    pub user: u64,
}

/// One process's creation time and the CPU time it has used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessTimes {
    /// When it was created, in 100 ns ticks since 1601 as a `FILETIME` counts them.
    pub created: u64,
    /// Its user and kernel time, in 100 ns ticks.
    pub cpu: u64,
}

/// The system's idle, kernel and user times, as `GetSystemTimes` gives them.
///
/// # Errors
///
/// The system's error when the times cannot be read.
pub fn system_times() -> io::Result<SystemTimes> {
    let mut idle = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: every pointer is to a writable FILETIME that outlives the call.
    if unsafe { GetSystemTimes(&raw mut idle, &raw mut kernel, &raw mut user) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(SystemTimes {
        idle: ticks(idle),
        kernel: ticks(kernel),
        user: ticks(user),
    })
}

/// The time now as the system clock that stamps a process's creation reads it, in 100 ns ticks since 1601, so a
/// process created after this call has a creation time at or after it.
#[must_use]
pub fn file_time_now() -> u64 {
    let mut now = FILETIME::default();
    // SAFETY: the pointer is to a writable FILETIME that outlives the call, which cannot fail.
    unsafe { GetSystemTimeAsFileTime(&raw mut now) };
    ticks(now)
}

/// The creation time and the CPU time of the running process `pid`; `None` when no process has that pid or the process
/// has exited, which a process that left a job between its listing and this call has.
///
/// # Errors
///
/// The system's error when the process exists but cannot be opened or its times read.
pub fn process_times(pid: u32) -> io::Result<Option<ProcessTimes>> {
    let Some(process) = open_running(pid)? else {
        return Ok(None);
    };
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: the handle is open for the whole call with query access, and every pointer is to a writable FILETIME.
    let read = unsafe { GetProcessTimes(process.raw(), &raw mut created, &raw mut exited, &raw mut kernel, &raw mut user) };
    if read == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(ProcessTimes {
        created: ticks(created),
        cpu: ticks(kernel) + ticks(user),
    }))
}

/// A `FILETIME`'s two halves as one count of 100 ns ticks.
fn ticks(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

/// The WSL VM's cumulative CPU time: the kernel and user time of every `vmmemWSL` process (image name compared ignoring
/// case), in 100 ns ticks, read from the system's process list, which needs no access to the VM's process; `None` when
/// no VM runs.
///
/// # Errors
///
/// An error naming the call and its status when the process list cannot be read, or naming the entry when the list
/// does not read as the layout this reader expects: an entry that leaves the buffer, or a VM entry whose creation time
/// is zero or more than a day past now, or whose CPU times are negative.
pub fn wsl_vm_cpu() -> io::Result<Option<u64>> {
    let list = process_list()?;
    let vms = walk(as_bytes(&list), |entry| is_vm_image(entry.name))?;
    vm_cpu_in(vms.iter().map(|vm| (vm.create, vm.kernel, vm.user)), file_time_now())
}

/// The WSL VM's CPU from its process list `entries`, each the creation, kernel and user times of one as the list gives
/// them, read at `now`: their kernel and user times summed, or `None` when there is no entry.
fn vm_cpu_in(entries: impl IntoIterator<Item = (i64, i64, i64)>, now: u64) -> io::Result<Option<u64>> {
    let latest = now.saturating_add(CLOCK_STEP_MARGIN);
    let mut total: Option<u64> = None;
    for (create, kernel, user) in entries {
        let created = u64::try_from(create).unwrap_or(0);
        if created == 0 || created > latest {
            return Err(io::Error::other(format!(
                "{PROCESS_LIST}: the {VM_IMAGE} entry's creation time {create} is not between 1601 and a day past now \
                 ({now}); the entry layout is not the one expected"
            )));
        }
        let (Ok(kernel), Ok(user)) = (u64::try_from(kernel), u64::try_from(user)) else {
            return Err(io::Error::other(format!(
                "{PROCESS_LIST}: the {VM_IMAGE} entry's CPU times ({kernel} kernel, {user} user) are negative; \
                 the entry layout is not the one expected"
            )));
        };
        total = Some(total.unwrap_or(0).saturating_add(kernel).saturating_add(user));
    }
    Ok(total)
}

/// Whether the UTF-16 image name `name`, in the list's byte order, is [`VM_IMAGE`], ignoring case.
fn is_vm_image(name: &[u8]) -> bool {
    let (units, rest) = name.as_chunks::<2>();
    rest.is_empty()
        && units.len() == VM_IMAGE.len()
        && units
            .iter()
            .zip(VM_IMAGE.bytes())
            .all(|(unit, expected)| u8::try_from(u16::from_le_bytes(*unit)).is_ok_and(|byte| byte.eq_ignore_ascii_case(&expected)))
}

/// The system's process list, as `NtQuerySystemInformation` writes it, in a buffer aligned for its entries; the read
/// starts at the size the last list fit in.
fn process_list() -> io::Result<Vec<u64>> {
    let mut list: Vec<u64> = vec![0; LIST_SIZE.load(Ordering::Relaxed).div_ceil(size_of::<u64>())];
    for _ in 0..LIST_ATTEMPTS {
        let bytes = size_of_val(list.as_slice());
        let size = u32::try_from(bytes).map_err(io::Error::other)?;
        let mut needed = 0_u32;
        // SAFETY: the buffer is writable for `size` bytes, aligned for the entries the call writes, and `needed` is
        // writable; the call writes at most `size` bytes.
        let status = unsafe { NtQuerySystemInformation(SystemProcessInformation, list.as_mut_ptr().cast(), size, &raw mut needed) };
        if status == STATUS_INFO_LENGTH_MISMATCH {
            let bytes = usize::try_from(needed).map_err(io::Error::other)? + LIST_SLACK;
            list.resize(bytes.div_ceil(size_of::<u64>()), 0);
            continue;
        }
        if status < 0 {
            return Err(io::Error::other(format!("{PROCESS_LIST} failed with status 0x{status:08X}")));
        }
        LIST_SIZE.store(bytes, Ordering::Relaxed);
        return Ok(list);
    }
    Err(io::Error::other(format!(
        "{PROCESS_LIST} kept outgrowing its buffer (status 0x{STATUS_INFO_LENGTH_MISMATCH:08X}) after {LIST_ATTEMPTS} attempts"
    )))
}

/// The bytes of the process list `list`.
fn as_bytes(list: &[u64]) -> &[u8] {
    // SAFETY: the bytes are those of `list`, which outlives the slice, and any byte is a valid `u8`.
    unsafe { std::slice::from_raw_parts(list.as_ptr().cast::<u8>(), size_of_val(list)) }
}

/// The entries of the process list `bytes` that `keep` keeps, walked by each entry's link to the next and checked
/// against the buffer at every step.
fn walk<'a>(bytes: &'a [u8], keep: impl Fn(&Entry<'a>) -> bool) -> io::Result<Vec<Entry<'a>>> {
    let mut kept = Vec::new();
    let mut at = 0_usize;
    loop {
        let next = usize::try_from(read_u32(bytes, at + NEXT_AT)?).map_err(io::Error::other)?;
        if next != 0 && next < ENTRY_READ {
            return Err(outside(at, bytes.len()));
        }
        let entry = entry_at(bytes, at)?;
        if keep(&entry) {
            kept.push(entry);
        }
        if next == 0 {
            return Ok(kept);
        }
        at = at.checked_add(next).ok_or_else(|| outside(at, bytes.len()))?;
    }
}

/// The entry at `at` of the process list `bytes`.
fn entry_at(bytes: &[u8], at: usize) -> io::Result<Entry<'_>> {
    Ok(Entry {
        pid: read_usize(bytes, at + PID_AT)?,
        name: image_name(bytes, at)?,
        create: read_i64(bytes, at + CREATE_TIME_AT)?,
        kernel: read_i64(bytes, at + KERNEL_TIME_AT)?,
        user: read_i64(bytes, at + USER_TIME_AT)?,
    })
}

/// The image name of the entry at `at` of the process list `bytes`, whose characters the system wrote into the same
/// buffer: read from there, bounds-checked, and empty for the idle process, which has none.
fn image_name(bytes: &[u8], at: usize) -> io::Result<&[u8]> {
    let length = usize::from(read_u16(bytes, at + NAME_AT)?);
    if length == 0 {
        return Ok(&[]);
    }
    let start = read_usize(bytes, at + NAME_BUFFER_AT)?.wrapping_sub(bytes.as_ptr().addr());
    start
        .checked_add(length)
        .and_then(|end| bytes.get(start..end))
        .ok_or_else(|| outside(at, bytes.len()))
}

/// The `N` bytes at `at` of the process list `bytes`, or the error of a read that would leave it.
fn read_bytes<const N: usize>(bytes: &[u8], at: usize) -> io::Result<[u8; N]> {
    at.checked_add(N)
        .and_then(|end| bytes.get(at..end))
        .and_then(|slice| <[u8; N]>::try_from(slice).ok())
        .ok_or_else(|| outside(at, bytes.len()))
}

/// The `u16` at `at` of the process list `bytes`.
fn read_u16(bytes: &[u8], at: usize) -> io::Result<u16> {
    read_bytes(bytes, at).map(u16::from_le_bytes)
}

/// The `u32` at `at` of the process list `bytes`.
fn read_u32(bytes: &[u8], at: usize) -> io::Result<u32> {
    read_bytes(bytes, at).map(u32::from_le_bytes)
}

/// The `usize` (a pointer or a process id) at `at` of the process list `bytes`.
fn read_usize(bytes: &[u8], at: usize) -> io::Result<usize> {
    read_bytes::<{ size_of::<usize>() }>(bytes, at).map(usize::from_le_bytes)
}

/// The `i64` at `at` of the process list `bytes`.
fn read_i64(bytes: &[u8], at: usize) -> io::Result<i64> {
    read_bytes(bytes, at).map(i64::from_le_bytes)
}

/// The error of a walk of the process list that would leave its `size`-byte buffer at `at`.
fn outside(at: usize, size: usize) -> io::Error {
    io::Error::other(format!(
        "{PROCESS_LIST}: the entry at byte {at} leaves the {size}-byte buffer; the list layout is not the one expected"
    ))
}

#[cfg(test)]
mod tests {
    use super::{as_bytes, file_time_now, is_vm_image, process_list, process_times, vm_cpu_in, walk, wsl_vm_cpu};

    /// The time now the pure tests read entries against, in 100 ns ticks since 1601.
    const NOW: i64 = 1_000_000;
    /// An hour, in 100 ns ticks.
    const HOUR: i64 = 60 * 60 * 10_000_000;

    /// `name` as the process list holds an image name: UTF-16, little-endian.
    fn utf16(name: &str) -> Vec<u8> {
        name.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    /// The VM's CPU from `entries` read at [`NOW`].
    fn vm_cpu(entries: &[(i64, i64, i64)]) -> std::io::Result<Option<u64>> {
        vm_cpu_in(entries.iter().copied(), NOW.unsigned_abs())
    }

    #[test]
    fn a_process_list_without_vmmem_wsl_reads_as_no_vm() {
        for name in ["vmwp.exe", "vmmem", "vmmemWSL.exe", "vmmemWS", ""] {
            assert!(!is_vm_image(&utf16(name)), "{name:?} is not the WSL VM");
        }
        assert_eq!(vm_cpu(&[]).ok(), Some(None), "no VM entry is no VM");
    }

    #[test]
    fn the_vm_cpu_is_kernel_and_user_of_every_vmmem_wsl_entry() {
        for name in ["VmmemWSL", "vmmemWSL", "VMMEMWSL"] {
            assert!(is_vm_image(&utf16(name)), "{name:?} is the WSL VM, ignoring case");
        }
        assert_eq!(vm_cpu(&[(100, 10, 1), (200, 5, 0)]).ok(), Some(Some(16)), "both entries are summed");
    }

    #[test]
    fn a_vm_entry_with_no_create_time_is_an_error() {
        assert!(vm_cpu(&[(0, 10, 1)]).is_err(), "a zero create time reads as a changed layout");
        assert!(vm_cpu(&[(-1, 10, 1)]).is_err(), "a negative create time reads as a changed layout");
    }

    #[test]
    fn a_vm_entry_with_negative_cpu_time_is_an_error() {
        assert!(vm_cpu(&[(100, -1, 1)]).is_err(), "negative kernel time");
        assert!(vm_cpu(&[(100, 1, -1)]).is_err(), "negative user time");
    }

    #[test]
    fn a_vm_entry_created_more_than_a_day_in_the_future_is_an_error() {
        assert!(
            vm_cpu(&[(NOW + 25 * HOUR, 10, 1)]).is_err(),
            "a create time 25 h ahead reads as a changed layout"
        );
    }

    #[test]
    fn a_vm_entry_created_within_a_day_ahead_reads_as_a_clock_step() {
        let minute = HOUR / 60;
        assert_eq!(
            vm_cpu(&[(NOW + minute, 10, 1)]).ok(),
            Some(Some(11)),
            "a clock stepped back a minute is no error"
        );
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a list that cannot be read must fail the test loudly")]
    fn the_walk_decodes_this_process_from_the_real_list() {
        let list = process_list().expect("the process list reads unelevated");
        let pid = usize::try_from(std::process::id()).expect("a pid fits a usize");
        let found = walk(as_bytes(&list), |entry| entry.pid == pid).expect("the list walks");
        let now = file_time_now();
        assert_eq!(found.len(), 1, "this process is listed once");
        let me = found.first().expect("this process is listed");

        let exe = std::env::current_exe().expect("the test executable has a path");
        let file = exe.file_name().expect("the path names a file").to_string_lossy().into_owned();
        let units: Vec<u16> = me.name.as_chunks::<2>().0.iter().map(|unit| u16::from_le_bytes(*unit)).collect();
        let name = String::from_utf16_lossy(&units);
        assert!(
            name.eq_ignore_ascii_case(&file),
            "the image name {name:?} is the test executable's {file:?}"
        );

        let created = u64::try_from(me.create).expect("a create time is not negative");
        assert!(created > 0 && created <= now, "created at {created}, now {now}");
        // CPU is charged in whole 15.625 ms clock ticks, so a young process can hold more CPU than wall age: the list is
        // checked against `GetProcessTimes` of this process instead, read after it.
        let own = process_times(std::process::id())
            .expect("this process's times read")
            .expect("this process runs");
        assert_eq!(created, own.created, "the list's create time is the one GetProcessTimes reads");
        let cpu = u64::try_from(me.kernel + me.user).expect("CPU time is not negative");
        assert!(
            cpu <= own.cpu,
            "the list's {cpu} ticks of CPU, read first, exceed the {} read after",
            own.cpu
        );
    }

    #[test]
    fn wsl_vm_cpu_reads_without_error() {
        let read = wsl_vm_cpu();
        assert!(read.is_ok(), "the process list reads unelevated: {read:?}");
    }
}
