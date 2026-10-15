//! A named pipe for a hand-off between two local processes of one user: a server that admits only that user's local
//! clients at medium integrity or above, one at a time, under a name with a fresh random part, and a client that lends
//! the server no identity and opens only a pipe that user owns with the server's own DACL and label. Each side can read the
//! other's process id, for the caller to check against the process it expects, and reads and writes bytes with a
//! deadline and handles that end the wait early.

use std::io;
use std::os::windows::io::{AsRawHandle, BorrowedHandle};
use std::time::Instant;

use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_NOT_FOUND, ERROR_OPERATION_ABORTED, ERROR_PIPE_CONNECTED, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE, LocalFree, WAIT_ABANDONED_0, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1, SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, DACL_SECURITY_INFORMATION, GetAce, LABEL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_ATTRIBUTES,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_CREATE_PIPE_INSTANCE, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX, READ_CONTROL, ReadFile, SECURITY_ANONYMOUS, SECURITY_SQOS_PRESENT, WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId, GetNamedPipeServerProcessId, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::SystemServices::{
    ACCESS_ALLOWED_ACE_TYPE, MAXIMUM_WAIT_OBJECTS, SYSTEM_MANDATORY_LABEL_ACE_TYPE, SYSTEM_MANDATORY_LABEL_NO_EXECUTE_UP,
    SYSTEM_MANDATORY_LABEL_NO_READ_UP, SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForMultipleObjects};

use crate::tree::is_error;
use crate::{own_handle, wait_millis, wide};

/// The access the pipe's first entry grants its user, and all a [`PipeClient`] asks for: reading and writing data and
/// attributes, reading its security, and waiting on it. `FILE_GENERIC_WRITE` also holds `FILE_CREATE_PIPE_INSTANCE`,
/// which is left out, so a client cannot make an instance of its own.
const CLIENT_ACCESS: u32 = (FILE_GENERIC_READ | FILE_GENERIC_WRITE) & !FILE_CREATE_PIPE_INSTANCE;

/// The OWNER RIGHTS SID: an entry for it replaces the rights an object's owner has without one.
const OWNER_RIGHTS: &str = "S-1-3-4";

/// The medium mandatory label's SID, the integrity level of the pipe's label.
const MEDIUM_LABEL: &str = "S-1-16-8192";

/// The policy of the pipe's label: a client below medium integrity can neither write, read nor execute it.
const LABEL_POLICY: u32 = SYSTEM_MANDATORY_LABEL_NO_WRITE_UP | SYSTEM_MANDATORY_LABEL_NO_READ_UP | SYSTEM_MANDATORY_LABEL_NO_EXECUTE_UP;

/// The digits of a pipe name's random part, lowercase hex.
const DIGITS: &[u8; 16] = b"0123456789abcdef";

/// The size, in bytes, of each of the pipe's two buffers.
const BUFFER_BYTES: u32 = 64 * 1024;

/// The server end of a named pipe of one instance, which admits only local clients of the user it was made for.
pub struct PipeServer {
    handle: OwnedHandle,
    name: String,
}

/// How a wait for a client ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectWait {
    /// A client is connected; [`PipeServer::client_pid`] says which process it is.
    Connected,
    /// The handle at this index of the wake list was signalled first, and no client had connected.
    Woken(usize),
    /// The deadline passed with no client connected.
    TimedOut,
}

/// How a [`PipeServer::read`] or a [`PipeClient::read`] ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipeRead {
    /// This many bytes, at least one, were read into the start of the buffer.
    Read(usize),
    /// No more data will be read on this connection: the other end closed its handle, or a read completed with no
    /// bytes, as a zero-byte write by the other end can also make it. It does not say the other end's process is
    /// gone: a caller still bounds and ends that process by its process handle.
    Closed,
    /// The deadline passed with nothing read.
    TimedOut,
    /// The handle at this index of the wake list was signalled first, and nothing was read.
    Woken(usize),
}

/// How a [`PipeServer::write_all`] or a [`PipeClient::write_all`] ended. Any result but [`PipeWrite::Written`] leaves
/// the byte stream out of step, an unknown part of the bytes having been written: the caller must disconnect or close
/// the pipe, and never retry the write or write another frame on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipeWrite {
    /// Every byte was written.
    Written,
    /// No more data will be taken on this connection: the other end closed its handle before every byte was written.
    /// It does not say the other end's process is gone: a caller still bounds and ends that process by its process
    /// handle.
    Closed,
    /// The deadline passed before every byte was written; some may have been.
    TimedOut,
    /// The handle at this index of the wake list was signalled before every byte was written; some may have been.
    Woken(usize),
}

/// What ended a wait on an overlapped operation's event and the wake handles.
enum Signalled {
    /// The operation's event: the operation has ended.
    Event,
    /// The wait ended before the operation did.
    Early(Early),
}

/// Why a wait ended before its operation did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Early {
    /// The deadline passed.
    TimedOut,
    /// The handle at this index of the wake list was signalled.
    Woken(usize),
}

/// How an overlapped operation on a pipe ended.
#[derive(Debug)]
enum Outcome {
    /// It completed, moving this many bytes (none for a connect).
    Completed(u32),
    /// It failed with this error.
    Failed(io::Error),
    /// The wait ended first, and the operation was cancelled.
    Stopped(Early),
}

/// One overlapped read into, or write from, a buffer lent for the whole operation.
enum Transfer<'a> {
    /// A read into the buffer.
    Read(&'a mut [u8]),
    /// A write of the buffer.
    Write(&'a [u8]),
}

/// How a read or a write started.
enum Begun {
    /// It completed at once.
    Completed,
    /// It runs on, its end signalled through its `OVERLAPPED`'s event.
    Pending,
    /// It failed at once.
    Failed(io::Error),
}

/// How a connect started.
enum Started {
    /// A client is connected.
    Connected,
    /// The connect waits for a client, its end signalled through its `OVERLAPPED`'s event.
    Pending,
    /// A client connected and closed its end before the connect (`ERROR_NO_DATA`): the pipe must be disconnected first.
    ClientGone,
}

impl PipeServer {
    /// Creates the pipe `\\.\pipe\<prefix>-<32 hex digits>`, its last part 128 random bits from the system's
    /// generator, so no other process can know the name before this one has made it.
    ///
    /// The pipe has one instance (`FILE_FLAG_FIRST_PIPE_INSTANCE`, a maximum of one), so a pipe of the name made first
    /// by any other process fails the call rather than being joined. It refuses remote clients
    /// (`PIPE_REJECT_REMOTE_CLIENTS`), reads and writes bytes, and is opened for overlapped I/O. Its owner is
    /// `user_sid` (a SID in its string form, `S-1-5-21-…`), which this process's token must be able to make an owner:
    /// its own user. Its DACL is protected and holds two entries, in this order: one for `user_sid`, granting reading
    /// and writing and nothing that changes the pipe's security or makes another instance, and one for OWNER RIGHTS
    /// granting only `READ_CONTROL`, so the owner keeps no implicit right to change the DACL. Its mandatory label is
    /// medium, refusing a client below medium integrity reading, writing and executing (`NO_READ_UP`, `NO_WRITE_UP`,
    /// `NO_EXECUTE_UP`).
    ///
    /// # Errors
    ///
    /// An error of kind [`io::ErrorKind::InvalidInput`] for a `user_sid` that is not a SID's string form; the
    /// system's error when the name cannot be drawn, the security descriptor cannot be built, or the pipe cannot be
    /// created, `ERROR_ACCESS_DENIED` when a pipe of the name exists already, `ERROR_INVALID_OWNER` when this process
    /// cannot make `user_sid` an owner.
    pub fn create(prefix: &str, user_sid: &str) -> io::Result<PipeServer> {
        let name = format!(r"\\.\pipe\{prefix}-{}", random_hex()?);
        PipeServer::create_named(name, user_sid)
    }

    /// Creates the pipe `name` as [`PipeServer::create`] does.
    fn create_named(name: String, user_sid: &str) -> io::Result<PipeServer> {
        PipeServer::create_with(name, &server_sddl(user_sid)?)
    }

    /// Creates the pipe `name` as [`PipeServer::create`] does, with the security descriptor `sddl` describes.
    fn create_with(name: String, sddl: &str) -> io::Result<PipeServer> {
        let descriptor = SecurityDescriptor::from_sddl(sddl)?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: crate::launch::struct_size::<SECURITY_ATTRIBUTES>(),
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        let wide_name = wide(&name);
        let open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED;
        let pipe_mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
        // SAFETY: `wide_name` is NUL-terminated and `attributes` points to a whole security descriptor; both outlive the
        // call. A failure returns `INVALID_HANDLE_VALUE`, checked before the handle is owned.
        let handle = unsafe {
            CreateNamedPipeW(
                wide_name.as_ptr(),
                open_mode,
                pipe_mode,
                1,
                BUFFER_BYTES,
                BUFFER_BYTES,
                0,
                &raw const attributes,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a success is a fresh handle nothing else owns.
        let handle = unsafe { own_handle(handle) }?;
        Ok(PipeServer { handle, name })
    }

    /// The pipe's full name, as a [`PipeClient`] opens it.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Waits until a client connects, `deadline` passes or one of `wake` is signalled, whichever comes first; a client
    /// that connects at the same moment as either of the others wins. A client that connected before the call is
    /// [`ConnectWait::Connected`] at once; one that connected and closed its end before the call is dropped
    /// ([`PipeServer::disconnect`]) and the wait goes on. A wait that ends without a client leaves the pipe waiting for
    /// none, and a later call waits again.
    ///
    /// A handle of `wake` must stay signalled once set: a manual-reset event or a process handle. An auto-reset event,
    /// a semaphore or a mutex can lose its signal, since the wait takes it and a connect that completes despite the
    /// cancel is [`ConnectWait::Connected`]. When the cancel of a connect fails, the call still waits until the connect
    /// ends, which lasts until a client connects.
    ///
    /// # Errors
    ///
    /// An error of kind [`io::ErrorKind::InvalidInput`] for more than 63 handles in `wake`; the system's error when the
    /// connect, a disconnect or the wait fails, or a handle of `wake` cannot be waited on; an error naming the handle
    /// for an abandoned mutex in `wake`; an error naming both failures, its source the cancel's system error, when a
    /// wait that ended without a client could not cancel its connect.
    pub fn wait_for_client(&self, deadline: Instant, wake: &[BorrowedHandle<'_>]) -> io::Result<ConnectWait> {
        check_wake(wake, "connect")?;
        let event = new_event()?;
        // Built before the connect starts, so nothing allocates while it is pending.
        let handles = wait_list(&event, wake);
        let mut overlapped = OVERLAPPED {
            hEvent: event.raw(),
            ..OVERLAPPED::default()
        };
        loop {
            // SAFETY: `overlapped` stays where it is, unmoved and alive, until the operation has ended: every path below
            // that leaves with it pending first cancels it and waits for its end.
            match unsafe { self.start_connect(&raw mut overlapped) }? {
                Started::Connected => return Ok(ConnectWait::Connected),
                Started::Pending => break,
                Started::ClientGone => self.disconnect()?,
            }
        }
        let waited = wait_any(&handles, deadline);
        // SAFETY: `overlapped` is the one the pending connect was given, still alive and unmoved; it is passed as a raw
        // pointer, since the kernel may still write it.
        match unsafe { finish(self.handle.raw(), &raw const overlapped, waited, "connect") }? {
            Outcome::Completed(_) => Ok(ConnectWait::Connected),
            Outcome::Failed(error) => Err(error),
            Outcome::Stopped(Early::TimedOut) => Ok(ConnectWait::TimedOut),
            Outcome::Stopped(Early::Woken(index)) => Ok(ConnectWait::Woken(index)),
        }
    }

    /// Reads what the client has written into the start of `buf`, returning as soon as at least one byte has arrived,
    /// so a caller that wants an exact count loops. The read ends early when `deadline` passes or one of `wake` is
    /// signalled, whichever comes first; bytes that arrive at the same moment win, and a read that ends early has read
    /// nothing. A read that completes at once returns without looking at `deadline` or `wake`, so a peer that keeps the
    /// pipe full is read on: the caller's loop checks its own deadline and wake handles between reads.
    ///
    /// A handle of `wake` must stay signalled once set: a manual-reset event or a process handle. An auto-reset event,
    /// a semaphore or a mutex can lose its signal, since the wait takes it and a read that completes despite the cancel
    /// is [`PipeRead::Read`]. When the cancel of a read that ended early fails, the call still waits until the read
    /// completes, which against a silent peer lasts until the other end is closed or its process ended (another thread
    /// can end it).
    ///
    /// # Errors
    ///
    /// An error of kind [`io::ErrorKind::InvalidInput`] for an empty `buf` or more than 63 handles in `wake`; the
    /// system's error, its code kept, when the read or the wait fails; an error naming the handle for an abandoned mutex
    /// in `wake`; an error naming both failures, its source the cancel's system error, when a read that ended early
    /// could not be cancelled.
    pub fn read(&self, buf: &mut [u8], deadline: Instant, wake: &[BorrowedHandle<'_>]) -> io::Result<PipeRead> {
        read_pipe(self.handle.raw(), buf, deadline, wake)
    }

    /// Writes every byte of `bytes` to the client, waiting while the pipe's buffer is full until the client reads. The
    /// write ends early when `deadline` passes or one of `wake` is signalled, whichever comes first, having written
    /// some of the bytes or none; a write that completes at the same moment wins. Nothing at all is written for an
    /// empty `bytes`. A write that completes at once goes on to the next without looking at `deadline` or `wake`. Any
    /// result but [`PipeWrite::Written`], and any error, leaves the stream out of step: the caller must disconnect or
    /// close the pipe, and never retry or write another frame on it.
    ///
    /// A handle of `wake` must stay signalled once set: a manual-reset event or a process handle. An auto-reset event,
    /// a semaphore or a mutex can lose its signal, since the wait takes it and a write that completes despite the
    /// cancel goes on. When the cancel of a write that ended early fails, the call still waits until the write
    /// completes, which against a peer that never reads lasts until the other end is closed or its process ended
    /// (another thread can end it).
    ///
    /// # Errors
    ///
    /// An error of kind [`io::ErrorKind::InvalidInput`] for more than 63 handles in `wake`; the system's error, its
    /// code kept, when a write or the wait fails; an error naming the handle for an abandoned mutex in `wake`; an error
    /// naming both failures, its source the cancel's system error, when a write that ended early could not be
    /// cancelled; an error when the system reports writing more bytes than it was asked to.
    pub fn write_all(&self, bytes: &[u8], deadline: Instant, wake: &[BorrowedHandle<'_>]) -> io::Result<PipeWrite> {
        write_pipe(self.handle.raw(), bytes, deadline, wake)
    }

    /// Starts a connect (`ConnectNamedPipe`) with `overlapped`, and says how it started.
    ///
    /// # Safety
    ///
    /// `overlapped` points to a whole `OVERLAPPED` with its own event, used by no other operation, that stays alive and
    /// unmoved until a pending connect has ended.
    unsafe fn start_connect(&self, overlapped: *mut OVERLAPPED) -> io::Result<Started> {
        // SAFETY: the pipe is open for overlapped I/O, and the caller guarantees `overlapped`.
        if unsafe { ConnectNamedPipe(self.handle.raw(), overlapped) } != 0 {
            return Ok(Started::Connected);
        }
        // SAFETY: `GetLastError` takes no pointers; it is read before anything else can set it.
        match unsafe { GetLastError() } {
            ERROR_PIPE_CONNECTED => Ok(Started::Connected),
            ERROR_IO_PENDING => Ok(Started::Pending),
            ERROR_NO_DATA => Ok(Started::ClientGone),
            code => Err(io::Error::from_raw_os_error(code.cast_signed())),
        }
    }

    /// Drops the client at the other end (`DisconnectNamedPipe`), whether it is still connected or has closed its end,
    /// so the next [`PipeServer::wait_for_client`] can admit another. The dropped client's handle no longer reaches the
    /// pipe.
    ///
    /// # Errors
    ///
    /// The system's error when the pipe cannot be disconnected.
    pub fn disconnect(&self) -> io::Result<()> {
        // SAFETY: the pipe is open for the whole call.
        if unsafe { DisconnectNamedPipe(self.handle.raw()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// The id of the process at the client end of the connected pipe (`GetNamedPipeClientProcessId`).
    ///
    /// # Errors
    ///
    /// The system's error when no client is connected or the id cannot be read.
    pub fn client_pid(&self) -> io::Result<u32> {
        let mut pid = 0;
        // SAFETY: the pipe is open for the whole call and `pid` is writable.
        if unsafe { GetNamedPipeClientProcessId(self.handle.raw(), &raw mut pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(pid)
    }
}

/// Refuses a wake list longer than a wait on an operation's event can watch, naming the operation, `what`.
fn check_wake(wake: &[BorrowedHandle<'_>], what: &str) -> io::Result<()> {
    if wake.len() >= MAXIMUM_WAIT_OBJECTS as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("a {what} wait wakes on at most {} handles, not {}", MAXIMUM_WAIT_OBJECTS - 1, wake.len()),
        ));
    }
    Ok(())
}

/// A fresh event for one overlapped operation: manual-reset and unset, as an overlapped call wants it.
fn new_event() -> io::Result<OwnedHandle> {
    // SAFETY: null security attributes and a null name are allowed; the 1 and 0 make the event manual-reset and unset.
    // A success is a fresh handle nothing else owns.
    unsafe { own_handle(CreateEventW(std::ptr::null(), 1, 0, std::ptr::null())) }
}

/// The handles a wait on an operation watches: its event first, then `wake` in its order.
fn wait_list(event: &OwnedHandle, wake: &[BorrowedHandle<'_>]) -> Vec<HANDLE> {
    let mut handles = vec![event.raw()];
    handles.extend(wake.iter().map(AsRawHandle::as_raw_handle));
    handles
}

/// Waits until `deadline` for any of `handles`, the operation's event first: [`Signalled::Event`] for the event, an
/// [`Early::Woken`] with the index into the wake list for another, an [`Early::TimedOut`] past the deadline. The event
/// wins over a wake handle signalled at the same moment.
fn wait_any(handles: &[HANDLE], deadline: Instant) -> io::Result<Signalled> {
    let count = u32::try_from(handles.len()).map_err(io::Error::other)?;
    let timeout = wait_millis(deadline.saturating_duration_since(Instant::now()));
    // SAFETY: `handles` holds `count` handles, each open for the whole call: the event this module made, and the wake
    // handles the caller borrows to it.
    let result = unsafe { WaitForMultipleObjects(count, handles.as_ptr(), 0, timeout) };
    match result {
        WAIT_OBJECT_0 => Ok(Signalled::Event),
        WAIT_TIMEOUT => Ok(Signalled::Early(Early::TimedOut)),
        WAIT_FAILED => Err(io::Error::last_os_error()),
        _ if result > WAIT_OBJECT_0 && result < WAIT_OBJECT_0 + count => Ok(Signalled::Early(Early::Woken((result - WAIT_OBJECT_0 - 1) as usize))),
        _ if result > WAIT_ABANDONED_0 && result < WAIT_ABANDONED_0 + count => Err(io::Error::other(format!(
            "the wake handle at index {} is a mutex its owner abandoned; a wait on a pipe wakes on no mutex",
            result - WAIT_ABANDONED_0 - 1
        ))),
        _ => Err(io::Error::other(format!(
            "the wait on a pipe ended with the unexpected result 0x{result:x}"
        ))),
    }
}

/// Ends the operation `overlapped` belongs to, on the pipe `handle`, as the wait `waited` found it: completed when its
/// event was signalled, else cancelled and then waited for, so the operation never outlives `overlapped` or its buffer.
/// An operation that completed despite the cancel is [`Outcome::Completed`]. So is one that completed while the wait
/// itself failed: see the comment where that is decided. A cancel that failed makes the result a [`CancelFailed`]
/// naming the operation, `what`.
///
/// # Safety
///
/// `overlapped` points to the `OVERLAPPED` a started operation on the open pipe `handle` was given, alive and unmoved
/// until this returns. It is a raw pointer, never a reference, because the kernel writes it while the operation is
/// pending, which a shared reference would forbid.
unsafe fn finish(handle: HANDLE, overlapped: *const OVERLAPPED, waited: io::Result<Signalled>, what: &str) -> io::Result<Outcome> {
    let failed_cancel = if matches!(waited, Ok(Signalled::Event)) {
        None
    } else {
        // SAFETY: the caller guarantees `overlapped`.
        unsafe { cancel(handle, overlapped) }
    };
    let mut transferred = 0;
    // SAFETY: the caller guarantees `overlapped` belongs to an operation on this open pipe; `transferred` is writable,
    // and the call waits for the operation's end, so nothing writes the `OVERLAPPED` once it returns.
    let completed = unsafe { GetOverlappedResult(handle, overlapped, &raw mut transferred, 1) } != 0;
    // A completion wins over a failed wait, deliberately: the bytes it moved are already out of the pipe or in it, and
    // an error in their place would leave the caller's stream out of step. The wait's failure comes back on the
    // caller's next call, which waits on the same handles.
    let ended = if completed {
        Ok(Outcome::Completed(transferred))
    } else {
        let error = io::Error::last_os_error();
        match waited {
            Ok(Signalled::Early(early)) if is_error(&error, ERROR_OPERATION_ABORTED) => Ok(Outcome::Stopped(early)),
            Ok(_) => Ok(Outcome::Failed(error)),
            Err(failed) => Err(failed),
        }
    };
    match failed_cancel {
        None => ended,
        Some(cancel) => Err(io::Error::new(
            cancel.kind(),
            CancelFailed {
                what: what.to_owned(),
                ended: format!("{ended:?}"),
                cancel,
            },
        )),
    }
}

/// A pending operation whose cancel failed, and how it then ended. Its source is the cancel's own error, which keeps
/// the system's code.
#[derive(Debug)]
struct CancelFailed {
    /// The operation: a connect, a read or a write.
    what: String,
    /// How the operation ended once waited for.
    ended: String,
    /// Why the cancel failed.
    cancel: io::Error,
}

impl std::fmt::Display for CancelFailed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "the pending {} could not be cancelled ({}), and then ended as {}",
            self.what, self.cancel, self.ended
        )
    }
}

impl std::error::Error for CancelFailed {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cancel)
    }
}

/// Cancels the operation `overlapped` belongs to (`CancelIoEx`); the error when the cancel failed, except for
/// `ERROR_NOT_FOUND`, which says the operation had already ended.
///
/// # Safety
///
/// `overlapped` points to the `OVERLAPPED` an operation on the open pipe `handle` was given, alive and unmoved for the
/// whole call; a raw pointer, as the kernel may write it meanwhile.
unsafe fn cancel(handle: HANDLE, overlapped: *const OVERLAPPED) -> Option<io::Error> {
    // SAFETY: the caller guarantees `overlapped` belongs to an operation on this open pipe; the call only names it.
    if unsafe { CancelIoEx(handle, overlapped) } != 0 {
        return None;
    }
    let error = io::Error::last_os_error();
    (!is_error(&error, ERROR_NOT_FOUND)).then_some(error)
}

/// Reads into `buf` from the pipe `handle`, as [`PipeServer::read`] says.
fn read_pipe(handle: HANDLE, buf: &mut [u8], deadline: Instant, wake: &[BorrowedHandle<'_>]) -> io::Result<PipeRead> {
    if buf.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a pipe read needs room for at least one byte",
        ));
    }
    Ok(match transfer(handle, Transfer::Read(buf), deadline, wake)? {
        Outcome::Completed(0) => PipeRead::Closed,
        Outcome::Completed(read) => PipeRead::Read(read as usize),
        Outcome::Failed(error) if is_error(&error, ERROR_BROKEN_PIPE) => PipeRead::Closed,
        Outcome::Failed(error) => return Err(error),
        Outcome::Stopped(Early::TimedOut) => PipeRead::TimedOut,
        Outcome::Stopped(Early::Woken(index)) => PipeRead::Woken(index),
    })
}

/// The most bytes one `WriteFile` is asked to write: its count is a `u32`.
const LONGEST_WRITE: usize = u32::MAX as usize;

/// Writes all of `bytes` to the pipe `handle`, as [`PipeServer::write_all`] says.
fn write_pipe(handle: HANDLE, bytes: &[u8], deadline: Instant, wake: &[BorrowedHandle<'_>]) -> io::Result<PipeWrite> {
    write_pipe_in_pieces(handle, bytes, LONGEST_WRITE, deadline, wake)
}

/// Writes all of `bytes` to the pipe `handle`, as [`PipeServer::write_all`] says, one write of at most `piece` bytes
/// after another until every byte is written, each going on from where the one before stopped.
fn write_pipe_in_pieces(handle: HANDLE, bytes: &[u8], piece: usize, deadline: Instant, wake: &[BorrowedHandle<'_>]) -> io::Result<PipeWrite> {
    let mut rest = bytes;
    while !rest.is_empty() {
        let (asked, _) = rest.split_at(rest.len().min(piece));
        let count = match written(transfer(handle, Transfer::Write(asked), deadline, wake)?)? {
            Ok(count) => count,
            Err(ended) => return Ok(ended),
        };
        rest = rest.get(count..).filter(|_| count <= asked.len()).ok_or_else(|| {
            io::Error::other(format!(
                "a pipe write reported writing {count} bytes of the {} it was asked to",
                asked.len()
            ))
        })?;
    }
    Ok(PipeWrite::Written)
}

/// The count of bytes one write moved, or how [`write_pipe`] ends without writing the rest.
fn written(outcome: Outcome) -> io::Result<Result<usize, PipeWrite>> {
    match outcome {
        Outcome::Completed(0) => Err(io::Error::other("a pipe write completed without writing a byte")),
        Outcome::Completed(count) => Ok(Ok(count as usize)),
        Outcome::Failed(error) if is_error(&error, ERROR_BROKEN_PIPE) || is_error(&error, ERROR_NO_DATA) => Ok(Err(PipeWrite::Closed)),
        Outcome::Failed(error) => Err(error),
        Outcome::Stopped(Early::TimedOut) => Ok(Err(PipeWrite::TimedOut)),
        Outcome::Stopped(Early::Woken(index)) => Ok(Err(PipeWrite::Woken(index))),
    }
}

/// Runs one overlapped read or write, `op`, on the pipe `handle`, which is open for overlapped I/O, until it ends,
/// `deadline` passes or one of `wake` is signalled. An operation the wait leaves pending is cancelled and waited for,
/// so it never outlives its `OVERLAPPED` or its buffer.
fn transfer(handle: HANDLE, mut op: Transfer<'_>, deadline: Instant, wake: &[BorrowedHandle<'_>]) -> io::Result<Outcome> {
    let what = match op {
        Transfer::Read(_) => "read",
        Transfer::Write(_) => "write",
    };
    check_wake(wake, what)?;
    let event = new_event()?;
    // Built before the operation starts, so nothing allocates while it is pending.
    let handles = wait_list(&event, wake);
    let mut overlapped = OVERLAPPED {
        hEvent: event.raw(),
        ..OVERLAPPED::default()
    };
    // SAFETY: `handle` is open for overlapped I/O; `overlapped` has its own event, and it and the buffer `op` borrows
    // stay where they are, alive and untouched, until the operation has ended: a started operation always reaches
    // `finish`, which waits for its end. Only raw pointers to `overlapped` are made while it may be pending.
    let waited = match unsafe { start_transfer(handle, &mut op, &raw mut overlapped) } {
        Begun::Completed => Ok(Signalled::Event),
        Begun::Pending => wait_any(&handles, deadline),
        Begun::Failed(error) => return Ok(Outcome::Failed(error)),
    };
    // SAFETY: `overlapped` is the one the started operation was given, still alive and unmoved, passed as a raw
    // pointer since the kernel may still write it.
    unsafe { finish(handle, &raw const overlapped, waited, what) }
}

/// Starts `op` (`ReadFile` or `WriteFile`) on the pipe `handle` with `overlapped`, for at most `u32::MAX` bytes, and
/// says how it started.
///
/// # Safety
///
/// `handle` is a pipe open for overlapped I/O; `overlapped` points to a whole `OVERLAPPED` with its own event, used by
/// no other operation; it and the buffer `op` borrows stay alive and unmoved, and the buffer untouched, until a started
/// operation has ended.
unsafe fn start_transfer(handle: HANDLE, op: &mut Transfer<'_>, overlapped: *mut OVERLAPPED) -> Begun {
    let started = match op {
        Transfer::Read(buf) => {
            let room = u32::try_from(buf.len()).unwrap_or(u32::MAX);
            // SAFETY: the caller guarantees `handle` and `overlapped`; `buf` is writable for `room` bytes, and no count
            // is asked for, as an overlapped read reports it through `overlapped`.
            unsafe { ReadFile(handle, buf.as_mut_ptr(), room, std::ptr::null_mut(), overlapped) }
        }
        Transfer::Write(bytes) => {
            let length = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
            // SAFETY: as for the read; `bytes` is readable for `length` bytes.
            unsafe { WriteFile(handle, bytes.as_ptr(), length, std::ptr::null_mut(), overlapped) }
        }
    };
    if started != 0 {
        return Begun::Completed;
    }
    // SAFETY: `GetLastError` takes no pointers; it is read before anything else can set it.
    match unsafe { GetLastError() } {
        ERROR_IO_PENDING => Begun::Pending,
        code => Begun::Failed(io::Error::from_raw_os_error(code.cast_signed())),
    }
}

/// A security descriptor the system built from a string, freed when dropped.
struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

/// The descriptor string of a [`PipeServer`]'s pipe for `user_sid`: owned by it, a protected DACL allowing it
/// [`CLIENT_ACCESS`] and OWNER RIGHTS `READ_CONTROL` alone, and a medium label refusing reads, writes and execution
/// from below.
fn server_sddl(user_sid: &str) -> io::Result<String> {
    if !is_sid_string(user_sid) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{user_sid:?} is not a SID in its string form"),
        ));
    }
    Ok(format!(
        "O:{user_sid}D:P(A;;0x{CLIENT_ACCESS:x};;;{user_sid})(A;;0x{READ_CONTROL:x};;;{OWNER_RIGHTS})S:(ML;;NWNRNX;;;ME)"
    ))
}

impl SecurityDescriptor {
    /// The descriptor `sddl` describes.
    fn from_sddl(sddl: &str) -> io::Result<SecurityDescriptor> {
        let sddl = wide(sddl);
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated and outlives the call, `descriptor` is writable, and no size is asked for.
        let built = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &raw mut descriptor, std::ptr::null_mut())
        };
        if built == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(SecurityDescriptor(descriptor))
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: the system allocated the descriptor with `LocalAlloc`; it is freed once, here.
        unsafe { LocalFree(self.0) };
    }
}

/// Whether `text` has the shape of a SID's string form, `S-` and then numbers joined by `-`, so it adds nothing but a
/// SID to the descriptor string it goes into.
fn is_sid_string(text: &str) -> bool {
    text.strip_prefix("S-").is_some_and(|rest| {
        rest.split('-').count() >= 2
            && rest
                .split('-')
                .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    })
}

/// 128 bits from the system's preferred random generator, as 32 lowercase hex digits.
fn random_hex() -> io::Result<String> {
    let mut bytes = [0_u8; 16];
    let size = u32::try_from(bytes.len()).map_err(io::Error::other)?;
    // SAFETY: `bytes` is writable for `size` bytes; with `BCRYPT_USE_SYSTEM_PREFERRED_RNG` no algorithm handle is passed.
    let status = unsafe { BCryptGenRandom(std::ptr::null_mut(), bytes.as_mut_ptr(), size, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    if status != 0 {
        return Err(io::Error::other(format!(
            "BCryptGenRandom failed with NTSTATUS 0x{:08x}",
            status.cast_unsigned()
        )));
    }
    let hex = bytes
        .iter()
        .flat_map(|&byte| [DIGITS[usize::from(byte >> 4)], DIGITS[usize::from(byte & 0xf)]]);
    Ok(hex.map(char::from).collect())
}

/// The client end of a [`PipeServer`]'s pipe, opened for overlapped I/O.
pub struct PipeClient {
    handle: OwnedHandle,
}

impl PipeClient {
    /// Opens the pipe `name` with the access its server grants, for overlapped I/O, at the anonymous impersonation
    /// level (`SECURITY_SQOS_PRESENT | SECURITY_ANONYMOUS`): whatever process made the pipe cannot act as this one.
    /// It then reads the pipe's owner, DACL and mandatory label and keeps the pipe only when the owner is `owner_sid` (a
    /// SID in its string form) and the DACL and label are the ones a [`PipeServer`] for `owner_sid` writes, entry for
    /// entry: a pipe of the name made by another identity, or by `owner_sid` below medium integrity, after the expected
    /// one's has closed, is refused before anything is sent.
    ///
    /// # Errors
    ///
    /// The system's error when the pipe cannot be opened: `ERROR_FILE_NOT_FOUND` when no pipe has the name,
    /// `ERROR_ACCESS_DENIED` when its DACL refuses this process's user, `ERROR_PIPE_BUSY` when another client holds it;
    /// the system's error when its security cannot be read; an error of kind [`io::ErrorKind::PermissionDenied`],
    /// with no system error code, naming what differs when its owner, DACL or label is not the expected one. A refused pipe
    /// is closed.
    pub fn open(name: &str, owner_sid: &str) -> io::Result<PipeClient> {
        let client = PipeClient::open_unchecked(name)?;
        client.check_security(owner_sid)?;
        Ok(client)
    }

    /// Opens the pipe `name` as [`PipeClient::open`] does, without reading its security.
    fn open_unchecked(name: &str) -> io::Result<PipeClient> {
        let wide_name = wide(name);
        let flags = FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_ANONYMOUS;
        // SAFETY: `wide_name` is NUL-terminated and outlives the call; no security attributes are passed, so the handle
        // is not inheritable, and no template file is passed.
        let handle = unsafe {
            CreateFileW(
                wide_name.as_ptr(),
                CLIENT_ACCESS,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                flags,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a success is a fresh handle nothing else owns.
        Ok(PipeClient {
            handle: unsafe { own_handle(handle) }?,
        })
    }

    /// The id of the process that made the pipe (`GetNamedPipeServerProcessId`).
    ///
    /// # Errors
    ///
    /// The system's error when the id cannot be read.
    pub fn server_pid(&self) -> io::Result<u32> {
        let mut pid = 0;
        // SAFETY: the pipe is open for the whole call and `pid` is writable.
        if unsafe { GetNamedPipeServerProcessId(self.handle.raw(), &raw mut pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(pid)
    }

    /// The client end's own handle, to tell it apart from a handle another process sent, as
    /// [`crate::check_received_output`] does.
    #[must_use]
    pub fn handle(&self) -> BorrowedHandle<'_> {
        // SAFETY: the handle is open for as long as `self` lives, which the borrow cannot outlive.
        unsafe { BorrowedHandle::borrow_raw(self.handle.raw()) }
    }

    /// Reads what the server has written into the start of `buf`, as [`PipeServer::read`] reads what its client wrote.
    ///
    /// # Errors
    ///
    /// As for [`PipeServer::read`].
    pub fn read(&self, buf: &mut [u8], deadline: Instant, wake: &[BorrowedHandle<'_>]) -> io::Result<PipeRead> {
        read_pipe(self.handle.raw(), buf, deadline, wake)
    }

    /// Writes every byte of `bytes` to the server, as [`PipeServer::write_all`] writes to its client.
    ///
    /// # Errors
    ///
    /// As for [`PipeServer::write_all`].
    pub fn write_all(&self, bytes: &[u8], deadline: Instant, wake: &[BorrowedHandle<'_>]) -> io::Result<PipeWrite> {
        write_pipe(self.handle.raw(), bytes, deadline, wake)
    }

    /// Refuses the pipe unless its owner is `owner_sid`, its DACL is the one a [`PipeServer`] for `owner_sid` writes
    /// (an entry allowing it [`CLIENT_ACCESS`], then one allowing OWNER RIGHTS `READ_CONTROL`, neither with flags), and
    /// its SACL holds the server's label alone: medium, [`LABEL_POLICY`], no flags.
    fn check_security(&self, owner_sid: &str) -> io::Result<()> {
        let security = ObjectSecurity::of(self.handle.raw())?;
        // SAFETY: the owner points into the descriptor `security` holds, alive for the whole call.
        let owner = unsafe { crate::token::sid_string(security.owner) }?;
        if !owner.eq_ignore_ascii_case(owner_sid) {
            return Err(refusal(format!("the pipe is owned by {owner}, not {owner_sid}")));
        }
        if security.dacl.is_null() {
            return Err(refusal("the pipe has a null DACL, which grants everyone everything".to_owned()));
        }
        // SAFETY: the DACL is not null and points into the descriptor `security` holds, alive for the whole call.
        let entries = unsafe { entries_of(security.dacl) }?;
        let expected = [(owner_sid, CLIENT_ACCESS), (OWNER_RIGHTS, READ_CONTROL)];
        let same = entries.len() == expected.len() && entries.iter().zip(expected).all(|(entry, (sid, mask))| entry.allows_only(sid, mask));
        if !same {
            return Err(refusal(format!("the pipe's DACL is {entries:?}, not the server's {expected:x?}")));
        }
        check_label(security.sacl)
    }
}

/// Refuses a pipe whose SACL `sacl` is not the server's: one mandatory label entry, medium, [`LABEL_POLICY`], no flags.
/// A pipe with no label, one labelled below medium (as a below-medium creator's is), or a SACL other than the server's
/// label is refused.
fn check_label(sacl: *const ACL) -> io::Result<()> {
    if sacl.is_null() {
        return Err(refusal("the pipe has no mandatory label, not the server's medium one".to_owned()));
    }
    // SAFETY: the SACL is not null and points into the descriptor the caller holds, alive for the whole call.
    let entries = unsafe { entries_of(sacl) }?;
    if !matches!(entries.as_slice(), [only] if only.labels_only(MEDIUM_LABEL, LABEL_POLICY)) {
        return Err(refusal(format!(
            "the pipe's mandatory label is {entries:?}, not the server's {MEDIUM_LABEL} with policy 0x{LABEL_POLICY:x}"
        )));
    }
    Ok(())
}

/// A refusal of a pipe whose security is not its server's, saying why.
fn refusal(why: String) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, why)
}

/// The owner, DACL and SACL of an object, pointing into the security descriptor that holds them.
struct ObjectSecurity {
    owner: PSID,
    dacl: *mut ACL,
    /// The SACL as far as `LABEL_SECURITY_INFORMATION` reads it: the mandatory label alone, null when there is none.
    sacl: *mut ACL,
    _descriptor: SecurityDescriptor,
}

impl ObjectSecurity {
    /// The owner, DACL and mandatory label of the kernel object behind `handle`, which is open with `READ_CONTROL`
    /// (`GetSecurityInfo`).
    fn of(handle: HANDLE) -> io::Result<ObjectSecurity> {
        let mut owner: PSID = std::ptr::null_mut();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: the caller keeps the handle open for the whole call; the owner, DACL, SACL and descriptor pointers are
        // writable, and the group is not asked for.
        let code = unsafe {
            GetSecurityInfo(
                handle,
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION,
                &raw mut owner,
                std::ptr::null_mut(),
                &raw mut dacl,
                &raw mut sacl,
                &raw mut descriptor,
            )
        };
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code.cast_signed()));
        }
        Ok(ObjectSecurity {
            owner,
            dacl,
            sacl,
            _descriptor: SecurityDescriptor(descriptor),
        })
    }
}

/// An entry of a DACL or SACL, as far as a pipe's check needs it.
#[derive(Debug)]
enum Entry {
    /// An access-allowed entry: its flags, its mask and its SID's string form.
    Allowed { flags: u8, mask: u32, sid: String },
    /// A mandatory label entry: its flags, its policy and its label SID's string form.
    Label { flags: u8, policy: u32, sid: String },
    /// An entry of any other type.
    Other {
        #[expect(dead_code, reason = "read only through `Debug`, in a refusal's message")]
        kind: u8,
    },
}

impl Entry {
    /// Whether the entry allows `sid` exactly `mask`, with no flags.
    fn allows_only(&self, sid: &str, mask: u32) -> bool {
        matches!(self, Entry::Allowed { flags: 0, mask: allowed, sid: named } if *allowed == mask && named.eq_ignore_ascii_case(sid))
    }

    /// Whether the entry is the mandatory label `sid` with exactly `policy`, with no flags.
    fn labels_only(&self, sid: &str, policy: u32) -> bool {
        matches!(self, Entry::Label { flags: 0, policy: held, sid: named } if *held == policy && named.eq_ignore_ascii_case(sid))
    }
}

/// The entries of `acl`, in their order.
///
/// # Safety
///
/// `acl` points to a valid ACL for the whole call.
unsafe fn entries_of(acl: *const ACL) -> io::Result<Vec<Entry>> {
    // SAFETY: the caller guarantees `acl`.
    let count = unsafe { (*acl).AceCount };
    // SAFETY: as above, and each index is below the ACL's count of entries.
    (0..u32::from(count)).map(|index| unsafe { entry_at(acl, index) }).collect()
}

/// The entry `index` of `acl`.
///
/// # Safety
///
/// `acl` points to a valid ACL holding more than `index` entries, for the whole call.
unsafe fn entry_at(acl: *const ACL, index: u32) -> io::Result<Entry> {
    let mut ace = std::ptr::null_mut();
    // SAFETY: the caller guarantees `acl` and `index`, and `ace` is writable.
    if unsafe { GetAce(acl, index, &raw mut ace) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // An access-allowed entry and a mandatory label entry share one layout: header, mask, then the SID from `SidStart`.
    let ace: *const ACCESS_ALLOWED_ACE = ace.cast_const().cast();
    // SAFETY: every entry starts with its header, within the ACL.
    let (kind, flags) = unsafe { ((*ace).Header.AceType, (*ace).Header.AceFlags) };
    if u32::from(kind) != ACCESS_ALLOWED_ACE_TYPE && u32::from(kind) != SYSTEM_MANDATORY_LABEL_ACE_TYPE {
        return Ok(Entry::Other { kind });
    }
    // SAFETY: an entry of either type, which the system checked whole when the ACL was set, holds its mask and, from
    // `SidStart`, its SID, both within the entry.
    let (mask, sid) = unsafe { ((*ace).Mask, (&raw const (*ace).SidStart).cast_mut().cast()) };
    // SAFETY: `sid` is the entry's SID, valid as the ACL is.
    let sid = unsafe { crate::token::sid_string(sid) }?;
    if u32::from(kind) == SYSTEM_MANDATORY_LABEL_ACE_TYPE {
        return Ok(Entry::Label { flags, policy: mask, sid });
    }
    Ok(Entry::Allowed { flags, mask, sid })
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose pipe cannot be made or opened must fail loudly")]
mod tests {
    use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_PIPE_NOT_CONNECTED, ERROR_PRIVILEGE_NOT_HELD, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::{
        CreateWellKnownSid, DuplicateTokenEx, RevertToSelf, SECURITY_MAX_SID_SIZE, SID_AND_ATTRIBUTES, SecurityImpersonation, SetTokenInformation,
        TOKEN_ADJUST_DEFAULT, TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenImpersonation, TokenIntegrityLevel,
        WinLowLabelSid,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_GENERIC_READ, FILE_READ_ATTRIBUTES, OPEN_EXISTING, READ_CONTROL, SECURITY_ANONYMOUS, SECURITY_SQOS_PRESENT, SYNCHRONIZE,
    };
    use windows_sys::Win32::System::Pipes::{PeekNamedPipe, WaitNamedPipeW};
    use windows_sys::Win32::System::SystemServices::SE_GROUP_INTEGRITY;
    use windows_sys::Win32::System::Threading::{CreateEventW, GetCurrentProcess, OpenProcessToken, SetThreadToken};

    use super::{CLIENT_ACCESS, ConnectWait, OWNER_RIGHTS, PipeClient, PipeRead, PipeServer, PipeWrite, random_hex, server_sddl};

    /// The longest a test waits for something that must happen: it never runs out unless the code is broken.
    const HANG_GUARD: Duration = Duration::from_secs(600);

    /// A fresh pipe's server and a client connected to it.
    fn connected() -> (PipeServer, PipeClient) {
        let server = PipeServer::create(&prefix(), &own_sid()).expect("the pipe is made");
        let client = PipeClient::open(server.name(), &own_sid()).expect("the client opens the pipe");
        let waited = server.wait_for_client(Instant::now() + HANG_GUARD, &[]).expect("the wait succeeds");
        assert_eq!(waited, ConnectWait::Connected);
        (server, client)
    }

    /// Exactly `count` bytes, read through `read`, which reads once into the buffer it is given.
    fn read_exactly(count: usize, mut read: impl FnMut(&mut [u8]) -> std::io::Result<PipeRead>) -> Vec<u8> {
        let mut bytes = vec![0_u8; count];
        let mut filled = 0;
        while filled < count {
            let ended = read(&mut bytes[filled..]).expect("the read succeeds");
            let read = if let PipeRead::Read(read) = ended { read } else { 0 };
            assert_ne!(read, 0, "the read ended as {ended:?} after {filled} of {count} bytes");
            filled += read;
        }
        bytes
    }

    #[test]
    fn a_message_written_by_the_client_is_read_by_the_server() {
        let (server, client) = connected();
        let message = b"a message from the client";

        let written = client.write_all(message, Instant::now() + HANG_GUARD, &[]).expect("the write succeeds");

        assert_eq!(written, PipeWrite::Written);
        let read = read_exactly(message.len(), |buf| server.read(buf, Instant::now() + HANG_GUARD, &[]));
        assert_eq!(read, message);
    }

    #[test]
    fn a_message_written_by_the_server_is_read_by_the_client() {
        let (server, client) = connected();
        let message = b"a message from the server";

        let written = server.write_all(message, Instant::now() + HANG_GUARD, &[]).expect("the write succeeds");

        assert_eq!(written, PipeWrite::Written);
        let read = read_exactly(message.len(), |buf| client.read(buf, Instant::now() + HANG_GUARD, &[]));
        assert_eq!(read, message);
    }

    #[test]
    fn a_read_returns_closed_when_the_other_end_closes() {
        let (server, client) = connected();
        let written = client.write_all(b"last", Instant::now() + HANG_GUARD, &[]).expect("the write succeeds");
        assert_eq!(written, PipeWrite::Written);
        drop(client);

        let before = read_exactly(4, |buf| server.read(buf, Instant::now() + HANG_GUARD, &[]));
        let mut buf = [0_u8; 16];
        let at_end = server.read(&mut buf, Instant::now() + HANG_GUARD, &[]).expect("the read succeeds");

        assert_eq!(before, b"last", "what was written before the close is read first");
        assert_eq!(at_end, PipeRead::Closed);
        let (server, client) = connected();
        drop(server);
        let at_client = client.read(&mut buf, Instant::now() + HANG_GUARD, &[]).expect("the read succeeds");
        assert_eq!(at_client, PipeRead::Closed);
    }

    #[test]
    fn a_read_times_out_at_its_deadline() {
        let (server, client) = connected();
        let mut buf = [0_u8; 16];

        let read = server.read(&mut buf, Instant::now(), &[]).expect("the read succeeds");

        assert_eq!(read, PipeRead::TimedOut);
        let written = client.write_all(b"after", Instant::now() + HANG_GUARD, &[]).expect("the write succeeds");
        assert_eq!(written, PipeWrite::Written);
        let after = read_exactly(5, |buf| server.read(buf, Instant::now() + HANG_GUARD, &[]));
        assert_eq!(after, b"after", "the cancelled read took nothing");
    }

    #[test]
    fn a_read_wakes_on_a_wake_handle() {
        let (_server, client) = connected();
        let unset = event(false);
        let set = event(true);
        let mut buf = [0_u8; 16];

        let read = client
            .read(&mut buf, Instant::now() + HANG_GUARD, &[unset.as_handle(), set.as_handle()])
            .expect("the read succeeds");

        assert_eq!(read, PipeRead::Woken(1));
    }

    #[test]
    fn a_write_larger_than_the_pipe_buffer_completes_while_the_other_end_reads() {
        let server = PipeServer::create(&prefix(), &own_sid()).expect("the pipe is made");
        let message: Vec<u8> = (0..=250_u8).cycle().take(1024 * 1024).collect();
        let (name, sid, size) = (server.name().to_owned(), own_sid(), message.len());
        let reader = std::thread::spawn(move || {
            let client = PipeClient::open(&name, &sid)?;
            Ok::<_, std::io::Error>(read_exactly(size, |buf| client.read(buf, Instant::now() + HANG_GUARD, &[])))
        });
        let waited = server.wait_for_client(Instant::now() + HANG_GUARD, &[]).expect("the wait succeeds");
        assert_eq!(waited, ConnectWait::Connected);

        let written = server.write_all(&message, Instant::now() + HANG_GUARD, &[]).expect("the write succeeds");

        let read = reader.join().expect("the reader's thread ends").expect("the reader opens the pipe");
        assert_eq!(written, PipeWrite::Written);
        assert!(read == message, "the {} bytes read are the {size} bytes written", read.len());
    }

    #[test]
    fn a_low_integrity_creator_cannot_make_a_pipe_with_the_server_s_medium_label() {
        let sid = own_sid();
        let sddl = server_sddl(&sid).expect("the descriptor string is built");

        let made = std::thread::spawn(move || {
            let server = {
                let _low = LowIntegrity::impersonate();
                PipeServer::create_with(test_name(), &sddl)
            };
            server.map(|server| PipeClient::open(server.name(), &sid).err())
        })
        .join()
        .expect("the thread ends");

        // Setting a medium label needs the relabel privilege, which a low-integrity creator lacks, so the system
        // refuses the creation itself. A pipe made after all would be a change in the system's behaviour, which this
        // test must be rewritten for (the client then refuses it for its label).
        let creation =
            made.expect_err("a low-integrity creator was refused a pipe with the server's medium label until now; the system's behaviour changed");
        assert_eq!(
            creation.raw_os_error(),
            Some(ERROR_PRIVILEGE_NOT_HELD.cast_signed()),
            "the creation is refused for the missing relabel privilege: {creation}"
        );
    }

    #[test]
    fn a_write_wakes_on_a_wake_handle_while_the_other_end_never_reads() {
        let (server, client) = connected();
        let set = event(true);
        let mut message: Vec<u8> = (0..=250_u8).cycle().take(1024 * 1024).collect();

        let written = server
            .write_all(&message, Instant::now() + HANG_GUARD, &[set.as_handle()])
            .expect("the write succeeds");

        assert_eq!(written, PipeWrite::Woken(0), "a write larger than the pipe's buffer waits for a reader");
        // The cancelled write is over, so its buffer is the caller's again: what is overwritten now never reaches the
        // pipe, and what the client finds there is a part of the message as it was.
        message.fill(0xFF);
        drop(message);
        let mut buf = vec![0_u8; 1024 * 1024];
        let read = client.read(&mut buf, Instant::now(), &[]).expect("the read succeeds");
        assert!(matches!(read, PipeRead::Read(_) | PipeRead::TimedOut), "{read:?}");
        let count = if let PipeRead::Read(count) = read { count } else { 0 };
        let expected = (0..=250_u8).cycle();
        assert!(
            buf.iter().take(count).zip(expected).all(|(got, want)| *got == want),
            "the {count} bytes read are the start of the message"
        );
    }

    #[test]
    fn a_write_returns_closed_when_the_other_end_has_closed() {
        let (server, client) = connected();
        drop(client);

        let written = server
            .write_all(b"after the close", Instant::now() + HANG_GUARD, &[])
            .expect("the write succeeds");

        assert_eq!(written, PipeWrite::Closed);
    }

    #[test]
    fn a_write_in_pieces_writes_every_byte_in_order() {
        let (server, client) = connected();
        // Three writes of at most 4096 bytes, each going on from where the one before stopped; all fit the buffer.
        let message: Vec<u8> = (0..=250_u8).cycle().take(10_000).collect();

        let written = super::write_pipe_in_pieces(server.handle.raw(), &message, 4096, Instant::now() + HANG_GUARD, &[]).expect("the write succeeds");

        assert_eq!(written, PipeWrite::Written);
        let read = read_exactly(message.len(), |buf| client.read(buf, Instant::now() + HANG_GUARD, &[]));
        assert!(read == message, "the {} bytes read are the {} bytes written", read.len(), message.len());
    }

    /// The prefix of every test pipe, with this process's id, so two test runs never share a name.
    fn prefix() -> String {
        format!("gate-win32-test-{}", std::process::id())
    }

    /// A fresh full pipe name under [`prefix`], for a pipe made by [`PipeServer::create_with`].
    fn test_name() -> String {
        format!(r"\\.\pipe\{}-{}", prefix(), random_hex().expect("the name is drawn"))
    }

    /// This process's user's SID, in its string form.
    fn own_sid() -> String {
        crate::current_user_sid().expect("this process's user is read")
    }

    /// A manual-reset event, set or unset as `set` says.
    fn event(set: bool) -> OwnedHandle {
        // SAFETY: null security attributes and a null name are allowed; the 1 makes it manual-reset.
        let event: HANDLE = unsafe { CreateEventW(std::ptr::null(), 1, i32::from(set), std::ptr::null()) };
        assert!(!event.is_null(), "the event is made: {}", std::io::Error::last_os_error());
        // SAFETY: a fresh handle nothing else owns.
        unsafe { OwnedHandle::from_raw_handle(event) }
    }

    #[test]
    fn a_pipe_admits_its_user_and_reports_the_client_pid() {
        let server = PipeServer::create(&prefix(), &own_sid()).expect("the pipe is made");
        let _client = PipeClient::open(server.name(), &own_sid()).expect("the pipe admits its user");

        let waited = server.wait_for_client(Instant::now() + HANG_GUARD, &[]).expect("the wait succeeds");

        assert_eq!(waited, ConnectWait::Connected);
        assert_eq!(server.client_pid().expect("the client pid is read"), std::process::id());

        // Only SYSTEM can make a pipe SYSTEM owns, so this one is this user's, its one entry for SYSTEM alone.
        let sddl = format!("O:{}D:P(A;;0x{CLIENT_ACCESS:x};;;S-1-5-18)", own_sid());
        let system_only = PipeServer::create_with(test_name(), &sddl).expect("a pipe for SYSTEM is made");
        let refused = PipeClient::open(system_only.name(), &own_sid())
            .err()
            .expect("a pipe for SYSTEM refuses this user");
        assert_eq!(refused.raw_os_error(), Some(ERROR_ACCESS_DENIED.cast_signed()), "{refused}");
    }

    #[test]
    fn a_server_drops_a_wrong_client_and_admits_the_next() {
        let server = PipeServer::create(&prefix(), &own_sid()).expect("the pipe is made");
        let wrong = PipeClient::open(server.name(), &own_sid()).expect("the first client opens the pipe");
        let first = server.wait_for_client(Instant::now() + HANG_GUARD, &[]).expect("the first wait succeeds");
        assert_eq!(first, ConnectWait::Connected);

        server.disconnect().expect("the first client is dropped");

        // SAFETY: `wrong`'s handle is open for the whole call, and no buffer or count is asked for.
        let peeked = unsafe {
            PeekNamedPipe(
                wrong.handle.raw(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        let cut_off = std::io::Error::last_os_error();
        assert_eq!(peeked, 0, "the dropped client no longer reaches the pipe");
        assert_eq!(cut_off.raw_os_error(), Some(ERROR_PIPE_NOT_CONNECTED.cast_signed()), "{cut_off}");

        // The next client waits until the pipe listens again, as a client that finds it busy does, then opens it.
        let (name, sid) = (server.name().to_owned(), own_sid());
        let next = std::thread::spawn(move || {
            let wide_name = crate::wide(&name);
            let guard = u32::try_from(HANG_GUARD.as_millis()).expect("the hang guard fits");
            // SAFETY: `wide_name` is NUL-terminated and outlives the call.
            if unsafe { WaitNamedPipeW(wide_name.as_ptr(), guard) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            PipeClient::open(&name, &sid).map(drop)
        });
        let second = server
            .wait_for_client(Instant::now() + HANG_GUARD, &[])
            .expect("the second wait succeeds");

        next.join()
            .expect("the next client's thread ends")
            .expect("the next client opens the pipe");
        assert_eq!(second, ConnectWait::Connected);
        drop(wrong);
    }

    #[test]
    fn a_client_refuses_a_pipe_whose_owner_is_not_the_expected_user() {
        let server = PipeServer::create(&prefix(), &own_sid()).expect("the pipe is made");

        let refused = PipeClient::open(server.name(), "S-1-5-18")
            .err()
            .expect("a pipe this user owns is not SYSTEM's");

        assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied, "{refused}");
        assert_eq!(refused.raw_os_error(), None, "a refusal of the pipe, not of the open: {refused}");
    }

    #[test]
    fn a_client_refuses_a_pipe_whose_dacl_differs() {
        let sid = own_sid();
        // A DACL without the OWNER RIGHTS entry, as a pipe made by another server might have: its owner keeps the right
        // to change the DACL.
        let sddl = format!("O:{sid}D:P(A;;0x{CLIENT_ACCESS:x};;;{sid})");
        let server = PipeServer::create_with(test_name(), &sddl).expect("the pipe is made");

        let refused = PipeClient::open(server.name(), &sid)
            .err()
            .expect("a DACL other than the server's is refused");

        assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied, "{refused}");
        assert_eq!(refused.raw_os_error(), None, "a refusal of the pipe, not of the open: {refused}");
    }

    #[test]
    fn a_client_refuses_a_pipe_whose_dacl_entry_has_a_flag() {
        let sid = own_sid();
        let sddl = server_sddl(&sid).expect("the descriptor string is built").replacen("(A;;", "(A;CI;", 1);
        let server = PipeServer::create_with(test_name(), &sddl).expect("the pipe is made");

        let refused = PipeClient::open(server.name(), &sid).err().expect("a DACL entry with a flag is refused");

        assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied, "{refused}");
        assert!(refused.to_string().contains("DACL is"), "refused for its DACL: {refused}");
    }

    #[test]
    fn a_client_refuses_a_pipe_with_a_null_dacl() {
        let sid = own_sid();
        let sddl = format!("O:{sid}D:NO_ACCESS_CONTROLS:(ML;;NWNRNX;;;ME)");
        let server = PipeServer::create_with(test_name(), &sddl).expect("the pipe is made");

        let refused = PipeClient::open(server.name(), &sid).err().expect("a null DACL is refused");

        assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied, "{refused}");
        assert!(refused.to_string().contains("null DACL"), "refused for its null DACL: {refused}");
    }

    #[test]
    fn a_client_refuses_a_medium_pipe_without_a_label() {
        let sid = own_sid();
        let sddl = format!("O:{sid}D:P(A;;0x{CLIENT_ACCESS:x};;;{sid})(A;;0x{READ_CONTROL:x};;;{OWNER_RIGHTS})");
        let server = PipeServer::create_with(test_name(), &sddl).expect("the pipe is made");

        let refused = PipeClient::open(server.name(), &sid).err().expect("a pipe with no label is refused");

        assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied, "{refused}");
        assert!(refused.to_string().contains("mandatory label"), "refused for its label: {refused}");
    }

    #[test]
    fn a_client_refuses_a_pipe_made_at_low_integrity() {
        let sid = own_sid();
        let sddl = format!("O:{sid}D:P(A;;0x{CLIENT_ACCESS:x};;;{sid})(A;;0x{READ_CONTROL:x};;;{OWNER_RIGHTS})");

        let refused = std::thread::spawn(move || {
            let server = {
                let _low = LowIntegrity::impersonate();
                PipeServer::create_with(test_name(), &sddl).expect("a low-integrity thread makes the pipe")
            };
            PipeClient::open(server.name(), &sid).err()
        })
        .join()
        .expect("the thread ends")
        .expect("a pipe made at low integrity is refused");

        assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied, "{refused}");
        assert!(refused.to_string().contains("mandatory label"), "refused for its label: {refused}");
    }

    #[test]
    fn a_second_instance_of_a_pipe_name_is_refused() {
        let first = PipeServer::create(&prefix(), &own_sid()).expect("the pipe is made");

        let second = PipeServer::create_named(first.name().to_owned(), &own_sid())
            .err()
            .expect("a second instance is refused");

        assert_eq!(second.raw_os_error(), Some(ERROR_ACCESS_DENIED.cast_signed()), "{second}");
    }

    #[test]
    fn the_client_sees_the_server_pid() {
        let server = PipeServer::create(&prefix(), &own_sid()).expect("the pipe is made");

        let client = PipeClient::open(server.name(), &own_sid()).expect("the client opens the pipe");

        assert_eq!(client.server_pid().expect("the server pid is read"), std::process::id());
    }

    #[test]
    fn a_connect_wait_ends_at_its_deadline() {
        let server = PipeServer::create(&prefix(), &own_sid()).expect("the pipe is made");

        let waited = server.wait_for_client(Instant::now(), &[]).expect("the wait succeeds");

        assert_eq!(waited, ConnectWait::TimedOut);
    }

    #[test]
    fn a_connect_wait_ends_when_a_given_handle_is_signalled() {
        let server = PipeServer::create(&prefix(), &own_sid()).expect("the pipe is made");
        let unset = event(false);
        let set = event(true);

        let waited = server
            .wait_for_client(Instant::now() + HANG_GUARD, &[unset.as_handle(), set.as_handle()])
            .expect("the wait succeeds");

        assert_eq!(waited, ConnectWait::Woken(1));
        let _client = PipeClient::open(server.name(), &own_sid()).expect("the pipe still takes a client after the cancelled wait");
        let again = server
            .wait_for_client(Instant::now() + HANG_GUARD, &[unset.as_handle()])
            .expect("the second wait succeeds");
        assert_eq!(again, ConnectWait::Connected);
    }

    #[test]
    fn a_client_that_closed_before_the_wait_does_not_end_it() {
        let server = PipeServer::create(&prefix(), &own_sid()).expect("the pipe is made");
        drop(PipeClient::open(server.name(), &own_sid()).expect("the first client opens the pipe"));

        let waited = server
            .wait_for_client(Instant::now(), &[])
            .expect("a client gone before the wait is no error");

        assert_eq!(waited, ConnectWait::TimedOut);
        let _client = PipeClient::open(server.name(), &own_sid()).expect("the pipe takes the next client");
        let again = server
            .wait_for_client(Instant::now() + HANG_GUARD, &[])
            .expect("the second wait succeeds");
        assert_eq!(again, ConnectWait::Connected);
    }

    #[test]
    fn a_low_integrity_client_cannot_open_the_pipe() {
        let server = PipeServer::create(&prefix(), &own_sid()).expect("the pipe is made");
        let name = server.name().to_owned();

        let accesses = [
            ("reading", FILE_GENERIC_READ),
            ("attributes", FILE_READ_ATTRIBUTES),
            ("synchronize", SYNCHRONIZE),
            ("no access", 0),
        ];

        let opened = std::thread::spawn(move || {
            let _low = LowIntegrity::impersonate();
            accesses.map(|(what, access)| (what, open_with(&name, access)))
        })
        .join()
        .expect("the low-integrity thread ends");

        for (what, result) in opened {
            let refused = result.expect_err(what);
            assert_eq!(refused.raw_os_error(), Some(ERROR_ACCESS_DENIED.cast_signed()), "{what}: {refused}");
        }
    }

    /// Opens the pipe `name` with `access` alone, as a client that wants only to take the pipe's one instance would.
    fn open_with(name: &str, access: u32) -> std::io::Result<OwnedHandle> {
        let wide_name = crate::wide(name);
        // SAFETY: `wide_name` is NUL-terminated and outlives the call; no security attributes or template are passed.
        let handle = unsafe {
            CreateFileW(
                wide_name.as_ptr(),
                access,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_ANONYMOUS,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a success is a fresh handle nothing else owns.
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }

    /// A low integrity label and the SID it points to, in one block, so the size the label is set with covers both.
    #[repr(C)]
    struct LowLabel {
        label: TOKEN_MANDATORY_LABEL,
        sid: [u32; (SECURITY_MAX_SID_SIZE as usize).div_ceil(size_of::<u32>())],
    }

    /// This thread impersonating a copy of this process's token lowered to low integrity, until dropped.
    struct LowIntegrity;

    impl LowIntegrity {
        fn impersonate() -> LowIntegrity {
            let mut own: HANDLE = std::ptr::null_mut();
            // SAFETY: this process's pseudo-handle is always open, and `own` is writable.
            let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY, &raw mut own) };
            assert_ne!(opened, 0, "this process's token opens: {}", std::io::Error::last_os_error());
            // SAFETY: a fresh token handle nothing else owns.
            let own = unsafe { OwnedHandle::from_raw_handle(own) };
            let mut copy: HANDLE = std::ptr::null_mut();
            let access = TOKEN_QUERY | TOKEN_IMPERSONATE | TOKEN_ADJUST_DEFAULT;
            // SAFETY: `own` is open with `TOKEN_DUPLICATE`, no security attributes are passed, and `copy` is writable.
            let duplicated = unsafe {
                DuplicateTokenEx(
                    own.as_raw_handle(),
                    access,
                    std::ptr::null(),
                    SecurityImpersonation,
                    TokenImpersonation,
                    &raw mut copy,
                )
            };
            assert_ne!(duplicated, 0, "the token is copied: {}", std::io::Error::last_os_error());
            // SAFETY: a fresh token handle nothing else owns.
            let copy = unsafe { OwnedHandle::from_raw_handle(copy) };
            let mut low = LowLabel {
                label: TOKEN_MANDATORY_LABEL::default(),
                sid: [0; (SECURITY_MAX_SID_SIZE as usize).div_ceil(size_of::<u32>())],
            };
            let mut size = SECURITY_MAX_SID_SIZE;
            // SAFETY: the buffer is writable for `size` bytes, the most a SID takes, and no domain SID is passed.
            let made = unsafe { CreateWellKnownSid(WinLowLabelSid, std::ptr::null_mut(), low.sid.as_mut_ptr().cast(), &raw mut size) };
            assert_ne!(made, 0, "the low label's SID is made: {}", std::io::Error::last_os_error());
            low.label.Label = SID_AND_ATTRIBUTES {
                Sid: low.sid.as_mut_ptr().cast(),
                Attributes: SE_GROUP_INTEGRITY.cast_unsigned(),
            };
            let length = u32::try_from(size_of::<LowLabel>()).expect("the label's size fits");
            // SAFETY: `copy` is open with `TOKEN_ADJUST_DEFAULT`, and `low` is a whole label followed by the SID it
            // points to, in the local, which is not moved while the call reads it.
            let lowered = unsafe { SetTokenInformation(copy.as_raw_handle(), TokenIntegrityLevel, (&raw const low).cast(), length) };
            assert_ne!(lowered, 0, "the copy is lowered: {}", std::io::Error::last_os_error());
            // SAFETY: a null thread is this one, and `copy` is an impersonation token open with `TOKEN_IMPERSONATE`.
            let set = unsafe { SetThreadToken(std::ptr::null(), copy.as_raw_handle()) };
            assert_ne!(set, 0, "the thread impersonates the copy: {}", std::io::Error::last_os_error());
            LowIntegrity
        }
    }

    impl Drop for LowIntegrity {
        fn drop(&mut self) {
            // SAFETY: `RevertToSelf` takes no arguments; it ends this thread's impersonation.
            let reverted = unsafe { RevertToSelf() } != 0;
            assert!(
                reverted || std::thread::panicking(),
                "the impersonation is reverted: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}
