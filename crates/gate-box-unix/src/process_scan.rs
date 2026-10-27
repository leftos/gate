//! The drained test's process scan: the box user's own processes, each with its pid, its parent, its start, its image
//! name and its command line, for [`gate_wire::drain::counted_processes`]. Only the box's own user's processes are
//! listed, since no other account can open a root at mode 0700. On macOS it reads `proc_listallpids`, each process's
//! `proc_pidinfo(PROC_PIDTBSDINFO)` and its arguments from `sysctl` `KERN_PROCARGS2`; elsewhere it is not built.

use gate_wire::drain::ProcessInfo;

/// The running processes of the box's effective user. A process whose arguments cannot be read has no command line.
/// Else why they could not be listed.
#[cfg(target_os = "macos")]
pub(crate) fn own_processes() -> Result<Vec<ProcessInfo>, String> {
    // SAFETY: `geteuid` takes nothing, touches no memory and always succeeds.
    let euid = unsafe { libc::geteuid() };
    let most = argument_bytes()?;
    let mut found = Vec::new();
    for pid in all_pids()? {
        if let Some(process) = own_process(pid, euid, most)? {
            found.push(process);
        }
    }
    Ok(found)
}

/// The processes cannot be listed on a Unix other than macOS, so the drained test cannot tell.
#[cfg(not(target_os = "macos"))]
pub(crate) fn own_processes() -> Result<Vec<ProcessInfo>, String> {
    Err(format!("listing the processes is not built for {}", std::env::consts::OS))
}

/// How many times the pid list is read again with more room when it filled the room it had.
#[cfg(target_os = "macos")]
const LIST_TRIES: usize = 4;

/// Every pid the system lists ([`libc::proc_listallpids`]). Else why not.
#[cfg(target_os = "macos")]
fn all_pids() -> Result<Vec<i32>, String> {
    // SAFETY: a null buffer of no bytes asks only for the count, writing nothing.
    let counted = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let mut room = usize::try_from(counted).map_err(|_| format!("proc_listallpids failed: {}", std::io::Error::last_os_error()))?;
    for _ in 0..LIST_TRIES {
        room = room.saturating_mul(2).max(64);
        let mut pids = vec![0_i32; room];
        let bytes = i32::try_from(room * std::mem::size_of::<i32>()).map_err(|_| "the pid list is too long to read".to_owned())?;
        // SAFETY: `pids` holds `bytes` bytes and lives across the call, which writes at most that much into it.
        let filled = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        let filled = usize::try_from(filled).map_err(|_| format!("proc_listallpids failed: {}", std::io::Error::last_os_error()))?;
        // A list that filled its room may have left pids out: read again with more.
        if filled < room {
            pids.truncate(filled);
            return Ok(pids);
        }
    }
    Err(format!("proc_listallpids filled {room} places each of {LIST_TRIES} times"))
}

/// The process `pid` when it is running and the box user `euid`'s own, its arguments read in at most `most` bytes;
/// `None` for a process that is gone, exited and not yet reaped, or another user's, one the system does not let the box
/// look at included. Else why it could not be looked at.
#[cfg(target_os = "macos")]
fn own_process(pid: i32, euid: u32, most: usize) -> Result<Option<ProcessInfo>, String> {
    // SAFETY: an all-zero `proc_bsdinfo` is a valid value of a plain C struct.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).map_err(|_| "proc_bsdinfo is too large".to_owned())?;
    // SAFETY: `info` is `size` bytes and lives across the call, which writes at most that much into it. The argument 0
    // looks at a running process only.
    let filled = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    if filled != size {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            // The box user may look at each of its own processes; one it may not look at is another user's.
            Some(libc::ESRCH | libc::EPERM) if filled <= 0 => Ok(None),
            _ => Err(format!("cannot look at the process {pid}: {error}")),
        };
    }
    if info.pbi_uid != euid {
        return Ok(None);
    }
    let name = match c_text(&info.pbi_name) {
        name if name.is_empty() => c_text(&info.pbi_comm),
        name => name,
    };
    let created = i64::try_from(info.pbi_start_tvsec)
        .ok()
        .zip(i32::try_from(info.pbi_start_tvusec).ok())
        .and_then(|(seconds, micros)| jiff::Timestamp::new(seconds, micros.saturating_mul(1000)).ok());
    Ok(Some(ProcessInfo {
        pid: pid.unsigned_abs(),
        parent_pid: info.pbi_ppid,
        name,
        command_line: arguments(pid, most),
        created,
    }))
}

/// The text of a C string held in `chars`, up to its first NUL.
#[cfg(target_os = "macos")]
fn c_text(chars: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = chars
        .iter()
        .map(|&unit| u8::from_ne_bytes(unit.to_ne_bytes()))
        .take_while(|&byte| byte != 0)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The most bytes a process's arguments and environment take (`sysctl` `KERN_ARGMAX`). Else why not.
#[cfg(target_os = "macos")]
fn argument_bytes() -> Result<usize, String> {
    let mut name = [libc::CTL_KERN, libc::KERN_ARGMAX];
    let mut most: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    // SAFETY: `name` is two ints, the old value and its size point at an int and its size, which `sysctl` fills no
    // further than that size; no new value is given.
    let read = unsafe { libc::sysctl(name.as_mut_ptr(), 2, (&raw mut most).cast(), &raw mut size, std::ptr::null_mut(), 0) };
    if read != 0 {
        return Err(format!("sysctl kern.argmax failed: {}", std::io::Error::last_os_error()));
    }
    usize::try_from(most).map_err(|_| format!("sysctl kern.argmax gave {most}"))
}

/// The arguments of the process `pid` joined by spaces, read in at most `most` bytes (`sysctl` `KERN_PROCARGS2`);
/// `None` when they could not be read.
#[cfg(target_os = "macos")]
fn arguments(pid: i32, most: usize) -> Option<String> {
    let mut name = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut bytes = vec![0_u8; most];
    let mut size = most;
    // SAFETY: `name` is three ints, the old value and its size point at `bytes` and its length, which `sysctl` fills no
    // further than that length; no new value is given.
    let read = unsafe { libc::sysctl(name.as_mut_ptr(), 3, bytes.as_mut_ptr().cast(), &raw mut size, std::ptr::null_mut(), 0) };
    if read != 0 {
        return None;
    }
    bytes.truncate(size);
    arguments_of(&bytes)
}

/// The arguments a `KERN_PROCARGS2` buffer `bytes` holds, joined by spaces: its argument count, a native-endian `int`,
/// then the program's path, NULs, then that many NUL-terminated arguments. `None` for a buffer of any other form.
#[cfg(target_os = "macos")]
fn arguments_of(bytes: &[u8]) -> Option<String> {
    let (count, rest) = bytes.split_first_chunk::<4>()?;
    let count = usize::try_from(i32::from_ne_bytes(*count)).ok()?;
    let path_end = rest.iter().position(|&byte| byte == 0)?;
    let mut rest = rest.get(path_end..)?;
    let start = rest.iter().position(|&byte| byte != 0)?;
    rest = rest.get(start..)?;
    let mut words = Vec::with_capacity(count);
    for _ in 0..count {
        let end = rest.iter().position(|&byte| byte == 0)?;
        words.push(String::from_utf8_lossy(rest.get(..end)?).into_owned());
        rest = rest.get(end + 1..).unwrap_or_default();
    }
    Some(words.join(" "))
}
