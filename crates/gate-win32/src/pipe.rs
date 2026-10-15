//! A named pipe for a hand-off between two local processes of one user: a server that admits only that user's local
//! clients, one at a time, under a name with a fresh random part, and a client that lends the server no identity. Each
//! side can read the other's process id, for the caller to check against the process it expects.

use std::io;
use std::os::windows::io::{AsRawHandle, BorrowedHandle};
use std::time::Instant;

use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::{
    ERROR_IO_PENDING, ERROR_OPERATION_ABORTED, ERROR_PIPE_CONNECTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree, WAIT_FAILED,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows_sys::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_CREATE_PIPE_INSTANCE, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX, SECURITY_ANONYMOUS, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, GetNamedPipeServerProcessId, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::SystemServices::MAXIMUM_WAIT_OBJECTS;
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForMultipleObjects};

use crate::{own_handle, wait_millis, wide};

/// The access the pipe's one entry grants, and all a [`PipeClient`] asks for: reading and writing data and
/// attributes, reading its security, and waiting on it. `FILE_GENERIC_WRITE` also holds `FILE_CREATE_PIPE_INSTANCE`,
/// which is left out, so a client cannot make an instance of its own.
const CLIENT_ACCESS: u32 = (FILE_GENERIC_READ | FILE_GENERIC_WRITE) & !FILE_CREATE_PIPE_INSTANCE;

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

impl PipeServer {
    /// Creates the pipe `\\.\pipe\<prefix>-<32 hex digits>`, its last part 128 random bits from the system's
    /// generator, so no other process can know the name before this one has made it.
    ///
    /// The pipe has one instance (`FILE_FLAG_FIRST_PIPE_INSTANCE`, a maximum of one), so a pipe of the name made first
    /// by any other process fails the call rather than being joined. It refuses remote clients
    /// (`PIPE_REJECT_REMOTE_CLIENTS`), reads and writes bytes, and is opened for overlapped I/O. Its DACL is protected
    /// and holds one entry, for `user_sid` (a SID in its string form, `S-1-5-21-…`), granting reading and writing and
    /// nothing that changes the pipe's security or makes another instance.
    ///
    /// # Errors
    ///
    /// An error of kind [`io::ErrorKind::InvalidInput`] for a `user_sid` that is not a SID's string form; the
    /// system's error when the name cannot be drawn, the security descriptor cannot be built, or the pipe cannot be
    /// created, `ERROR_ACCESS_DENIED` when a pipe of the name exists already.
    pub fn create(prefix: &str, user_sid: &str) -> io::Result<PipeServer> {
        let name = format!(r"\\.\pipe\{prefix}-{}", random_hex()?);
        PipeServer::create_named(name, user_sid)
    }

    /// Creates the pipe `name` as [`PipeServer::create`] does.
    fn create_named(name: String, user_sid: &str) -> io::Result<PipeServer> {
        let descriptor = SecurityDescriptor::for_user(user_sid)?;
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
    /// [`ConnectWait::Connected`] at once. A wait that ends without a client leaves the pipe waiting for none, and a
    /// later call waits again.
    ///
    /// # Errors
    ///
    /// An error of kind [`io::ErrorKind::InvalidInput`] for more than 63 handles in `wake`; the system's error when the
    /// connect or the wait fails, or a handle of `wake` cannot be waited on.
    pub fn wait_for_client(&self, deadline: Instant, wake: &[BorrowedHandle<'_>]) -> io::Result<ConnectWait> {
        if wake.len() >= MAXIMUM_WAIT_OBJECTS as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("a connect wait wakes on at most {} handles, not {}", MAXIMUM_WAIT_OBJECTS - 1, wake.len()),
            ));
        }
        // SAFETY: null security attributes and a null name are allowed; the 1 and 0 make the event manual-reset and
        // unset, as an overlapped call wants it. A success is a fresh handle nothing else owns.
        let event = unsafe { own_handle(CreateEventW(std::ptr::null(), 1, 0, std::ptr::null())) }?;
        let mut overlapped = OVERLAPPED {
            hEvent: event.raw(),
            ..OVERLAPPED::default()
        };
        // SAFETY: the pipe is open for overlapped I/O, and `overlapped` stays where it is, unmoved and alive, until the
        // operation has ended: every path below that leaves with it pending first cancels it and waits for its end.
        if unsafe { ConnectNamedPipe(self.handle.raw(), &raw mut overlapped) } != 0 {
            return Ok(ConnectWait::Connected);
        }
        // SAFETY: `GetLastError` takes no pointers; it is read before anything else can set it.
        match unsafe { GetLastError() } {
            ERROR_PIPE_CONNECTED => return Ok(ConnectWait::Connected),
            ERROR_IO_PENDING => {}
            code => return Err(io::Error::from_raw_os_error(code.cast_signed())),
        }
        let mut handles = vec![event.raw()];
        handles.extend(wake.iter().map(AsRawHandle::as_raw_handle));
        let waited = wait_any(&handles, deadline);
        // SAFETY: `overlapped` is the one the pending connect was given, still alive and unmoved.
        unsafe { self.finish_connect(&overlapped, waited) }
    }

    /// Ends the connect that `overlapped` belongs to as the wait `waited` found it: completed when its event was
    /// signalled, else cancelled and then waited for, so the operation never outlives `overlapped`. A connect that
    /// completed despite the cancel is [`ConnectWait::Connected`].
    ///
    /// # Safety
    ///
    /// `overlapped` is the one a pending `ConnectNamedPipe` on this pipe was given, alive and unmoved until this returns.
    unsafe fn finish_connect(&self, overlapped: &OVERLAPPED, waited: io::Result<ConnectWait>) -> io::Result<ConnectWait> {
        if !matches!(waited, Ok(ConnectWait::Connected)) {
            // SAFETY: the caller guarantees `overlapped` belongs to an operation on this open pipe. A cancel that finds
            // the operation already ended does nothing, which the wait below then reports.
            unsafe { CancelIoEx(self.handle.raw(), overlapped) };
        }
        let mut transferred = 0;
        // SAFETY: as above; `transferred` is writable, and the call waits for the operation's end.
        let completed = unsafe { GetOverlappedResult(self.handle.raw(), overlapped, &raw mut transferred, 1) } != 0;
        if completed {
            return Ok(ConnectWait::Connected);
        }
        let error = io::Error::last_os_error();
        match waited {
            Ok(ConnectWait::Connected) => Err(error),
            Ok(ended) if error.raw_os_error() == Some(ERROR_OPERATION_ABORTED.cast_signed()) => Ok(ended),
            Ok(_) => Err(error),
            Err(failed) => Err(failed),
        }
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

/// Waits until `deadline` for any of `handles`, the connect's event first: [`ConnectWait::Connected`] for the event,
/// [`ConnectWait::Woken`] with the index into the wake list for another, [`ConnectWait::TimedOut`] past the deadline.
fn wait_any(handles: &[HANDLE], deadline: Instant) -> io::Result<ConnectWait> {
    let count = u32::try_from(handles.len()).map_err(io::Error::other)?;
    let timeout = wait_millis(deadline.saturating_duration_since(Instant::now()));
    // SAFETY: `handles` holds `count` handles, each open for the whole call: the event this module made, and the wake
    // handles the caller borrows to it.
    let result = unsafe { WaitForMultipleObjects(count, handles.as_ptr(), 0, timeout) };
    match result {
        WAIT_OBJECT_0 => Ok(ConnectWait::Connected),
        WAIT_TIMEOUT => Ok(ConnectWait::TimedOut),
        WAIT_FAILED => Err(io::Error::last_os_error()),
        _ if result > WAIT_OBJECT_0 && result < WAIT_OBJECT_0 + count => Ok(ConnectWait::Woken((result - WAIT_OBJECT_0 - 1) as usize)),
        _ => Err(io::Error::other(format!(
            "the wait for a pipe client ended with the unexpected result 0x{result:x}"
        ))),
    }
}

/// A security descriptor the system built from a string, freed when dropped.
struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    /// A descriptor whose DACL is protected and holds one entry, allowing `user_sid` [`CLIENT_ACCESS`].
    fn for_user(user_sid: &str) -> io::Result<SecurityDescriptor> {
        if !is_sid_string(user_sid) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{user_sid:?} is not a SID in its string form"),
            ));
        }
        let sddl = wide(&format!("D:P(A;;0x{CLIENT_ACCESS:x};;;{user_sid})"));
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
    ///
    /// # Errors
    ///
    /// The system's error when the pipe cannot be opened: `ERROR_FILE_NOT_FOUND` when no pipe has the name,
    /// `ERROR_ACCESS_DENIED` when its DACL refuses this process's user, `ERROR_PIPE_BUSY` when another client holds it.
    pub fn open(name: &str) -> io::Result<PipeClient> {
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
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose pipe cannot be made or opened must fail loudly")]
mod tests {
    use std::os::windows::io::{AsHandle, FromRawHandle, OwnedHandle};
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, HANDLE};
    use windows_sys::Win32::System::Threading::CreateEventW;

    use super::{ConnectWait, PipeClient, PipeServer};

    /// The longest a test waits for something that must happen: it never runs out unless the code is broken.
    const HANG_GUARD: Duration = Duration::from_secs(600);

    /// The prefix of every test pipe, with this process's id, so two test runs never share a name.
    fn prefix() -> String {
        format!("gate-win32-test-{}", std::process::id())
    }

    /// This process's user's SID, in its string form.
    fn own_sid() -> String {
        crate::token_facts().expect("this process's token is read").user
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
        let _client = PipeClient::open(server.name()).expect("the pipe admits its user");

        let waited = server.wait_for_client(Instant::now() + HANG_GUARD, &[]).expect("the wait succeeds");

        assert_eq!(waited, ConnectWait::Connected);
        assert_eq!(server.client_pid().expect("the client pid is read"), std::process::id());

        let system_only = PipeServer::create(&prefix(), "S-1-5-18").expect("a pipe for SYSTEM is made");
        let refused = PipeClient::open(system_only.name()).err().expect("a pipe for SYSTEM refuses this user");
        assert_eq!(refused.raw_os_error(), Some(ERROR_ACCESS_DENIED.cast_signed()), "{refused}");
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

        let client = PipeClient::open(server.name()).expect("the client opens the pipe");

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
        let _client = PipeClient::open(server.name()).expect("the pipe still takes a client after the cancelled wait");
        let again = server
            .wait_for_client(Instant::now() + HANG_GUARD, &[unset.as_handle()])
            .expect("the second wait succeeds");
        assert_eq!(again, ConnectWait::Connected);
    }
}
