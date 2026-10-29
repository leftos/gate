//! The CPU times a watch samples: the system's, those of one process with the time it was created, and the WSL VM's,
//! all in the 100 ns ticks of a `FILETIME`.

use std::io;

use proc_table::system_list::{PROCESS_LIST, as_bytes, process_list, walk};
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows_sys::Win32::System::Threading::{GetProcessTimes, GetSystemTimes};

use crate::open_running;

/// The image name of the process whose CPU is the WSL VM's: Ubuntu and docker-desktop share it.
const VM_IMAGE: &str = "vmmemWSL";
/// How far past now a VM entry's creation time may lie, in 100 ns ticks, before it reads as a changed layout: a day,
/// so a clock stepped back since the VM started is no error.
const CLOCK_STEP_MARGIN: u64 = 24 * 60 * 60 * 10_000_000;

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

/// The time now on the coarse system clock (`GetSystemTimeAsFileTime`), in 100 ns ticks since 1601. The coarse clock
/// is never ahead of the precise one a process's creation is stamped from, so a process created after this call has a
/// creation time at or after it, unless the clock is stepped back meanwhile.
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

#[cfg(test)]
mod tests {
    use super::{is_vm_image, vm_cpu_in, wsl_vm_cpu};

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
    fn wsl_vm_cpu_reads_without_error() {
        let read = wsl_vm_cpu();
        assert!(read.is_ok(), "the process list reads unelevated: {read:?}");
    }
}
