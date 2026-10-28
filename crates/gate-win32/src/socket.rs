//! TCP sockets for a data channel: a listener bound to one address on a port of a range, with `SO_EXCLUSIVEADDRUSE`
//! set before the bind so no other socket can bind the same port beside it, and the stream it accepts or a client
//! connects. Every socket is created non-inheritable (`WSA_FLAG_NO_HANDLE_INHERIT`), an accepted one is marked so
//! again, and each is non-blocking: every accept, connect, read and write waits in `select` up to a deadline the caller
//! gives, and ends at it with nothing done.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::os::windows::io::{AsRawSocket, RawSocket};
use std::sync::OnceLock;
use std::time::Instant;
use std::{io, mem, ptr};

use windows_sys::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation};
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, FD_SET, FIONBIO, IN_ADDR, IN_ADDR_0, INVALID_SOCKET, IPPROTO_TCP, LINGER, SD_BOTH, SO_ERROR, SO_EXCLUSIVEADDRUSE, SO_LINGER, SO_SNDBUF,
    SOCK_STREAM, SOCKADDR, SOCKADDR_IN, SOCKET, SOCKET_ERROR, SOL_SOCKET, TIMEVAL, WSA_FLAG_NO_HANDLE_INHERIT, WSADATA, WSAEACCES, WSAEADDRINUSE,
    WSAECONNABORTED, WSAECONNRESET, WSAEWOULDBLOCK, WSAGetLastError, WSASocketW, WSAStartup, accept, bind, closesocket, connect, getpeername,
    getsockname, getsockopt, ioctlsocket, listen, recv, select, send, setsockopt, shutdown,
};

/// Winsock 2.2, the version every Windows the gate runs on has.
const WINSOCK_VERSION: u16 = 0x0202;

/// The connections a listener queues: one, the PC's.
const BACKLOG: i32 = 1;

/// The longest one `select` waits, so a deadline far off never overflows its seconds.
const MOST_SELECT_SECONDS: u64 = 3600;

/// Starts Winsock for this process once; every later call gives the first one's result. Winsock is never cleaned up:
/// it lives as long as the process.
fn started() -> io::Result<()> {
    static STARTED: OnceLock<i32> = OnceLock::new();
    let status = *STARTED.get_or_init(|| {
        let mut data = WSADATA::default();
        // SAFETY: `data` is a valid place for the description Winsock writes.
        unsafe { WSAStartup(WINSOCK_VERSION, &raw mut data) }
    });
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status))
    }
}

/// The last Winsock error of this thread.
fn last_error() -> io::Error {
    // SAFETY: no argument; it reads this thread's last Winsock error.
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

/// Whether `error` is Winsock's `WSAEWOULDBLOCK`: a non-blocking call that has nothing to do yet.
fn would_block(error: &io::Error) -> bool {
    error.raw_os_error() == Some(WSAEWOULDBLOCK)
}

/// The `SOCKADDR_IN` of `address`.
fn sockaddr(address: SocketAddrV4) -> SOCKADDR_IN {
    SOCKADDR_IN {
        sin_family: AF_INET,
        sin_port: address.port().to_be(),
        sin_addr: IN_ADDR {
            S_un: IN_ADDR_0 {
                S_addr: u32::from_ne_bytes(address.ip().octets()),
            },
        },
        sin_zero: [0; 8],
    }
}

/// The address `sockaddr` holds.
fn address_of(sockaddr: &SOCKADDR_IN) -> SocketAddrV4 {
    // SAFETY: every member of the union is the same four bytes of the address.
    let raw = unsafe { sockaddr.sin_addr.S_un.S_addr };
    SocketAddrV4::new(Ipv4Addr::from(raw.to_ne_bytes()), u16::from_be(sockaddr.sin_port))
}

/// The size of a `SOCKADDR_IN`, as Winsock takes it.
fn sockaddr_size() -> i32 {
    i32::try_from(mem::size_of::<SOCKADDR_IN>()).unwrap_or(i32::MAX)
}

/// One open socket, closed when dropped.
#[derive(Debug)]
struct Socket(SOCKET);

impl Socket {
    /// A new TCP socket over IPv4, non-inheritable and non-blocking.
    fn tcp() -> io::Result<Socket> {
        started()?;
        // SAFETY: no protocol info is given; the flags are Winsock's own.
        let raw = unsafe { WSASocketW(i32::from(AF_INET), SOCK_STREAM, IPPROTO_TCP, ptr::null(), 0, WSA_FLAG_NO_HANDLE_INHERIT) };
        if raw == INVALID_SOCKET {
            return Err(last_error());
        }
        let socket = Socket(raw);
        socket.set_nonblocking()?;
        Ok(socket)
    }

    /// Makes the socket non-blocking.
    fn set_nonblocking(&self) -> io::Result<()> {
        let mut on: u32 = 1;
        // SAFETY: the socket is open; `on` is a valid place for the argument.
        let status = unsafe { ioctlsocket(self.0, FIONBIO, &raw mut on) };
        if status == SOCKET_ERROR { Err(last_error()) } else { Ok(()) }
    }

    /// Marks the socket's handle not to be inherited by a process this one creates.
    fn set_not_inherited(&self) -> io::Result<()> {
        // A socket is a handle the system can mark like any other.
        let handle = self.0 as HANDLE;
        // SAFETY: the socket is open, and its value is its handle.
        let set = unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
        if set == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// Sets the `SOL_SOCKET` option `name` to the whole number `value`.
    fn set_option(&self, name: i32, value: i32) -> io::Result<()> {
        let bytes = value.to_ne_bytes();
        // SAFETY: the socket is open; the value is `bytes`, valid for its length.
        let status = unsafe { setsockopt(self.0, SOL_SOCKET, name, bytes.as_ptr(), 4) };
        if status == SOCKET_ERROR { Err(last_error()) } else { Ok(()) }
    }

    /// The `SOL_SOCKET` option `name`, a whole number.
    fn option(&self, name: i32) -> io::Result<i32> {
        let mut bytes = [0_u8; 4];
        let mut size: i32 = 4;
        // SAFETY: the socket is open; `bytes` is valid for the `size` bytes asked.
        let status = unsafe { getsockopt(self.0, SOL_SOCKET, name, bytes.as_mut_ptr(), &raw mut size) };
        if status == SOCKET_ERROR {
            Err(last_error())
        } else {
            Ok(i32::from_ne_bytes(bytes))
        }
    }

    /// Binds the socket to `address`.
    fn bind_to(&self, address: SocketAddrV4) -> io::Result<()> {
        let sockaddr = sockaddr(address);
        // SAFETY: the socket is open; `sockaddr` is a valid `SOCKADDR_IN` of the size given.
        let status = unsafe { bind(self.0, (&raw const sockaddr).cast::<SOCKADDR>(), sockaddr_size()) };
        if status == SOCKET_ERROR { Err(last_error()) } else { Ok(()) }
    }

    /// The address the socket is bound to.
    fn local(&self) -> io::Result<SocketAddrV4> {
        let mut sockaddr = SOCKADDR_IN::default();
        let mut size = sockaddr_size();
        // SAFETY: the socket is open; `sockaddr` is valid for `size` bytes.
        let status = unsafe { getsockname(self.0, (&raw mut sockaddr).cast::<SOCKADDR>(), &raw mut size) };
        if status == SOCKET_ERROR {
            Err(last_error())
        } else {
            Ok(address_of(&sockaddr))
        }
    }

    /// Waits until the socket is ready for `wait`, or until `deadline`; whether it is ready. Only a connect also waits
    /// on the failed set, where Winsock reports a connect that failed, so the caller reads why.
    fn ready(&self, wait: Wait, deadline: Instant) -> io::Result<bool> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(false);
            }
            let seconds = left.as_secs().min(MOST_SELECT_SECONDS);
            let timeout = TIMEVAL {
                tv_sec: i32::try_from(seconds).unwrap_or(i32::MAX),
                tv_usec: i32::try_from(left.subsec_micros()).unwrap_or(0),
            };
            let mut set = FD_SET {
                fd_count: 1,
                ..FD_SET::default()
            };
            set.fd_array[0] = self.0;
            let mut failed = FD_SET {
                fd_count: 1,
                ..FD_SET::default()
            };
            failed.fd_array[0] = self.0;
            let (reads, writes, fails) = match wait {
                Wait::Read => (&raw mut set, ptr::null_mut(), ptr::null_mut()),
                Wait::Write => (ptr::null_mut(), &raw mut set, ptr::null_mut()),
                Wait::Connect => (ptr::null_mut(), &raw mut set, &raw mut failed),
            };
            // SAFETY: each set given holds the one open socket; the timeout is valid; the first argument is ignored.
            let count = unsafe { select(0, reads, writes, fails, &raw const timeout) };
            match count {
                SOCKET_ERROR => return Err(last_error()),
                0 => {}
                _ => return Ok(true),
            }
        }
    }
}

/// What a socket's wait waits for.
#[derive(Clone, Copy)]
enum Wait {
    /// A byte to read, or a connection to accept.
    Read,
    /// Room to write.
    Write,
    /// A connect to end, made or failed.
    Connect,
}

impl Drop for Socket {
    fn drop(&mut self) {
        // SAFETY: the socket is open and closed once, here; a failed close leaves nothing to do.
        unsafe { closesocket(self.0) };
    }
}

/// The port to try first in `first..=last`: one drawn from the system's preferred random generator.
///
/// # Errors
///
/// An error of kind [`io::ErrorKind::InvalidInput`] when `first` is above `last`; the generator's error.
pub fn random_port(first: u16, last: u16) -> io::Result<u16> {
    if first > last {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("the port range {first}-{last} is empty"),
        ));
    }
    let [first_byte, second, third, fourth, ..] = crate::pipe::random_bytes()?;
    let drawn = u32::from_le_bytes([first_byte, second, third, fourth]);
    let span = u32::from(last) - u32::from(first) + 1;
    u16::try_from(u32::from(first) + drawn % span).map_err(io::Error::other)
}

/// The 16 bytes of a data channel's proof, drawn from the system's preferred random generator through the one draw
/// this crate makes every random value with.
///
/// # Errors
///
/// The generator's error.
pub fn random_proof() -> io::Result<[u8; 16]> {
    crate::pipe::random_bytes()
}

/// A TCP listener bound to one address and port, queueing one connection.
#[derive(Debug)]
pub struct Listener {
    socket: Socket,
    port: u16,
}

impl Listener {
    /// A listener on `address` at the first port of `first..=last` it can bind, trying from `start` upward and wrapping
    /// to `first` after `last`; a `start` outside the range starts at `first`. Each port is tried on a new socket with
    /// `SO_EXCLUSIVEADDRUSE` set before the bind; a port another socket holds (`WSAEADDRINUSE`) or that the system
    /// refuses (`WSAEACCES`) is passed over. `None` when every port of the range is passed over.
    ///
    /// # Errors
    ///
    /// An error of kind [`io::ErrorKind::InvalidInput`] when `first` is above `last`; the system's error when a
    /// socket cannot be made, a bind fails any other way, or the listen fails.
    pub fn bind_in_range(address: Ipv4Addr, first: u16, last: u16, start: u16) -> io::Result<Option<Listener>> {
        if first > last {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("the port range {first}-{last} is empty"),
            ));
        }
        let start = if (first..=last).contains(&start) { start } else { first };
        for port in (start..=last).chain(first..start) {
            let socket = Socket::tcp()?;
            socket.set_option(SO_EXCLUSIVEADDRUSE, 1)?;
            match socket.bind_to(SocketAddrV4::new(address, port)) {
                Ok(()) => {}
                Err(error) if matches!(error.raw_os_error(), Some(WSAEADDRINUSE | WSAEACCES)) => continue,
                Err(error) => return Err(error),
            }
            // SAFETY: the socket is open and bound.
            if unsafe { listen(socket.0, BACKLOG) } == SOCKET_ERROR {
                return Err(last_error());
            }
            return Ok(Some(Listener { socket, port }));
        }
        Ok(None)
    }

    /// The port the listener is bound to.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The next connection, waiting up to `deadline`; `None` when none came by then. A connection its peer reset or
    /// aborted before it was taken (`WSAECONNRESET`, `WSAECONNABORTED`) is passed over and the wait goes on. The
    /// accepted socket is made non-inheritable under the spawn lock, so no process this crate starts meanwhile inherits
    /// it, and non-blocking; a socket whose setup failed comes back as that failure with its peer's address, so the
    /// caller can pass over a peer it would not serve.
    ///
    /// # Errors
    ///
    /// The system's error when the wait or the accept fails any other way.
    pub fn accept(&self, deadline: Instant) -> io::Result<Option<Accepted>> {
        loop {
            let mut sockaddr = SOCKADDR_IN::default();
            let mut size = sockaddr_size();
            let taken = {
                let _spawning = crate::create::lock_spawns();
                // SAFETY: the socket is open and listening; `sockaddr` is valid for `size` bytes.
                let raw = unsafe { accept(self.socket.0, (&raw mut sockaddr).cast::<SOCKADDR>(), &raw mut size) };
                (raw != INVALID_SOCKET).then(|| {
                    let socket = Socket(raw);
                    socket.set_not_inherited().map(|()| socket)
                })
            };
            if let Some(taken) = taken {
                let stream = taken.and_then(|socket| socket.set_nonblocking().map(|()| Stream { socket }));
                return Ok(Some(Accepted {
                    stream,
                    peer: address_of(&sockaddr),
                }));
            }
            let error = last_error();
            // Winsock documents `WSAECONNRESET` for a connection its peer ended before the accept took it; a reset
            // over loopback comes back as an accepted connection instead. The skip still ends at the deadline, so a
            // run of resets never holds the wait past it.
            if matches!(error.raw_os_error(), Some(WSAECONNRESET | WSAECONNABORTED)) {
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                continue;
            }
            if !would_block(&error) {
                return Err(error);
            }
            if !self.socket.ready(Wait::Read, deadline)? {
                return Ok(None);
            }
        }
    }
}

/// A connection a [`Listener`] took: its peer's address, and the connection set up, or why its setup failed (the
/// socket then closed).
#[derive(Debug)]
pub struct Accepted {
    /// The connection, non-inheritable and non-blocking; else why it could not be made so.
    pub stream: io::Result<Stream>,
    /// The address of the connection's peer.
    pub peer: SocketAddrV4,
}

impl AsRawSocket for Listener {
    fn as_raw_socket(&self) -> RawSocket {
        self.socket.0 as RawSocket
    }
}

/// One TCP connection, accepted by a [`Listener`] or connected by [`Stream::connect`].
#[derive(Debug)]
pub struct Stream {
    socket: Socket,
}

impl Stream {
    /// A connection from `local`, at a port the system picks, to `remote`, waiting up to `deadline`; `None` when it
    /// was not made by then.
    ///
    /// # Errors
    ///
    /// The system's error when the socket cannot be made or bound, or the connection is refused or fails.
    pub fn connect(local: Ipv4Addr, remote: SocketAddrV4, deadline: Instant) -> io::Result<Option<Stream>> {
        match Stream::connecting(local, remote)? {
            Connect::Made(stream) => Ok(Some(stream)),
            Connect::Waiting(connecting) => match connecting.made(deadline)? {
                Connect::Made(stream) => Ok(Some(stream)),
                Connect::Waiting(_) => Ok(None),
            },
        }
    }

    /// Starts a connection from `local`, at a port the system picks, to `remote`, waiting for nothing: the connection
    /// when it was made at once, else the connect under way, which [`Connecting::made`] waits on.
    ///
    /// # Errors
    ///
    /// The system's error when the socket cannot be made or bound, or the connection is refused or fails at once.
    pub fn connecting(local: Ipv4Addr, remote: SocketAddrV4) -> io::Result<Connect> {
        let socket = Socket::tcp()?;
        socket.bind_to(SocketAddrV4::new(local, 0))?;
        let sockaddr = sockaddr(remote);
        // SAFETY: the socket is open and bound; `sockaddr` is a valid `SOCKADDR_IN` of the size given.
        if unsafe { connect(socket.0, (&raw const sockaddr).cast::<SOCKADDR>(), sockaddr_size()) } == SOCKET_ERROR {
            let error = last_error();
            if !would_block(&error) {
                return Err(error);
            }
            return Ok(Connect::Waiting(Connecting { socket }));
        }
        Ok(Connect::Made(Stream { socket }))
    }

    /// Reads into `buffer` what has come, waiting up to `deadline` for a byte: `Some(0)` once the peer closed its end,
    /// `None` when nothing came by `deadline`.
    ///
    /// # Errors
    ///
    /// The system's error when the wait or the read fails.
    pub fn read(&self, buffer: &mut [u8], deadline: Instant) -> io::Result<Option<usize>> {
        let length = i32::try_from(buffer.len()).unwrap_or(i32::MAX);
        loop {
            // SAFETY: the socket is open; `buffer` is writable for `length` bytes.
            let read = unsafe { recv(self.socket.0, buffer.as_mut_ptr(), length, 0) };
            if let Ok(read) = usize::try_from(read) {
                return Ok(Some(read));
            }
            let error = last_error();
            if !would_block(&error) {
                return Err(error);
            }
            if !self.socket.ready(Wait::Read, deadline)? {
                return Ok(None);
            }
        }
    }

    /// Writes what of `bytes` the connection takes, waiting up to `deadline` for room: how many bytes, `None` when no
    /// room came by `deadline`.
    ///
    /// # Errors
    ///
    /// The system's error when the wait or the write fails.
    pub fn write(&self, bytes: &[u8], deadline: Instant) -> io::Result<Option<usize>> {
        let length = i32::try_from(bytes.len()).unwrap_or(i32::MAX);
        loop {
            // SAFETY: the socket is open; `bytes` is readable for `length` bytes.
            let wrote = unsafe { send(self.socket.0, bytes.as_ptr(), length, 0) };
            if let Ok(wrote) = usize::try_from(wrote) {
                return Ok(Some(wrote));
            }
            let error = last_error();
            if !would_block(&error) {
                return Err(error);
            }
            if !self.socket.ready(Wait::Write, deadline)? {
                return Ok(None);
            }
        }
    }

    /// Makes the connection's close abortive (`SO_LINGER` on, 0 s): closing it resets the connection and leaves
    /// nothing behind on its port.
    ///
    /// # Errors
    ///
    /// The system's error when the option cannot be set.
    pub fn set_linger_abort(&self) -> io::Result<()> {
        let linger = LINGER { l_onoff: 1, l_linger: 0 };
        let size = i32::try_from(mem::size_of::<LINGER>()).unwrap_or(i32::MAX);
        // SAFETY: the socket is open; the value is `linger`, valid for its size.
        let status = unsafe { setsockopt(self.socket.0, SOL_SOCKET, SO_LINGER, (&raw const linger).cast::<u8>(), size) };
        if status == SOCKET_ERROR { Err(last_error()) } else { Ok(()) }
    }

    /// Shuts both directions of the connection down, so its peer reads the end; the socket is closed when dropped.
    ///
    /// # Errors
    ///
    /// The system's error when the shutdown fails, as for a connection its peer already reset.
    pub fn shut_down(&self) -> io::Result<()> {
        // SAFETY: the socket is open.
        let status = unsafe { shutdown(self.socket.0, SD_BOTH) };
        if status == SOCKET_ERROR { Err(last_error()) } else { Ok(()) }
    }

    /// Sets the connection's send buffer to `bytes`.
    ///
    /// # Errors
    ///
    /// The system's error when the option cannot be set.
    pub fn set_send_buffer(&self, bytes: i32) -> io::Result<()> {
        self.socket.set_option(SO_SNDBUF, bytes)
    }

    /// The address of the connection's peer.
    ///
    /// # Errors
    ///
    /// The system's error when it cannot be read.
    pub fn peer(&self) -> io::Result<SocketAddrV4> {
        let mut sockaddr = SOCKADDR_IN::default();
        let mut size = sockaddr_size();
        // SAFETY: the socket is open; `sockaddr` is valid for `size` bytes.
        let status = unsafe { getpeername(self.socket.0, (&raw mut sockaddr).cast::<SOCKADDR>(), &raw mut size) };
        if status == SOCKET_ERROR {
            Err(last_error())
        } else {
            Ok(address_of(&sockaddr))
        }
    }

    /// The address the connection's own end is bound to.
    ///
    /// # Errors
    ///
    /// The system's error when it cannot be read.
    pub fn local(&self) -> io::Result<SocketAddrV4> {
        self.socket.local()
    }
}

/// Where a connect [`Stream::connecting`] started stands: made, or still under way.
#[derive(Debug)]
pub enum Connect {
    /// The connection, made.
    Made(Stream),
    /// The connect, still under way.
    Waiting(Connecting),
}

/// A connect under way; dropping it abandons the connect and closes its socket.
#[derive(Debug)]
pub struct Connecting {
    socket: Socket,
}

impl Connecting {
    /// Waits up to `deadline` for the connect to end: the connection once it was made, else the connect still under
    /// way, to wait on again or drop.
    ///
    /// # Errors
    ///
    /// The system's error when the wait fails, or the connection is refused or fails.
    pub fn made(self, deadline: Instant) -> io::Result<Connect> {
        if !self.socket.ready(Wait::Connect, deadline)? {
            return Ok(Connect::Waiting(self));
        }
        let failure = self.socket.option(SO_ERROR)?;
        if failure != 0 {
            return Err(io::Error::from_raw_os_error(failure));
        }
        Ok(Connect::Made(Stream { socket: self.socket }))
    }
}

impl AsRawSocket for Stream {
    fn as_raw_socket(&self) -> RawSocket {
        self.socket.0 as RawSocket
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its sockets must fail loudly")]
mod tests {
    use std::io::Write;
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Networking::WinSock::{SO_REUSEADDR, WSAEACCES};

    use super::{Listener, Socket, Stream, random_port};

    /// How long a case waits for an event before it calls the socket hung.
    const GUARD: Duration = Duration::from_secs(30);

    /// A deadline already near, for a wait that must end without a peer.
    const SHORT: Duration = Duration::from_millis(50);

    const LOOPBACK: Ipv4Addr = Ipv4Addr::LOCALHOST;

    /// A loopback port free a moment ago: the system's pick for a listener closed at once.
    fn free_port() -> u16 {
        let listener = TcpListener::bind((LOOPBACK, 0)).expect("a loopback listener binds");
        listener.local_addr().expect("the listener's address is read").port()
    }

    /// A listener on the loopback port `port` alone.
    fn listener_on(port: u16) -> Listener {
        Listener::bind_in_range(LOOPBACK, port, port, port)
            .expect("the bind does not fail")
            .expect("the free port is bound")
    }

    #[test]
    fn a_second_bind_with_socket_reuseaddr_on_the_listeners_port_is_refused_access() {
        let listener = listener_on(free_port());
        let other = Socket::tcp().expect("a socket is made");
        other.set_option(SO_REUSEADDR, 1).expect("SO_REUSEADDR is set");

        let bound = other.bind_to(SocketAddrV4::new(LOOPBACK, listener.port()));

        assert_eq!(bound.expect_err("the bind is refused").raw_os_error(), Some(WSAEACCES));
    }

    #[test]
    fn socket_bind_in_range_passes_a_held_port_wrapping_to_the_start_of_the_range() {
        let held = TcpListener::bind((LOOPBACK, 0)).expect("a loopback listener binds");
        let port = held.local_addr().expect("the listener's address is read").port();
        let first = port - 8;

        let listener = Listener::bind_in_range(LOOPBACK, first, port, port)
            .expect("the bind does not fail")
            .expect("a port of the range is free");

        assert!((first..port).contains(&listener.port()), "{} in {first}-{port}", listener.port());
    }

    #[test]
    fn socket_bind_in_range_finds_none_when_every_port_is_held() {
        let held = TcpListener::bind((LOOPBACK, 0)).expect("a loopback listener binds");
        let port = held.local_addr().expect("the listener's address is read").port();
        let ours = listener_on(free_port());

        assert!(
            Listener::bind_in_range(LOOPBACK, port, port, port)
                .expect("the bind does not fail")
                .is_none()
        );
        assert!(
            Listener::bind_in_range(LOOPBACK, ours.port(), ours.port(), ours.port())
                .expect("the bind does not fail")
                .is_none()
        );
    }

    #[test]
    fn socket_random_port_lies_in_its_range() {
        for _ in 0..64 {
            let port = random_port(47100, 47131).expect("a port is drawn");
            assert!((47100..=47131).contains(&port), "{port}");
        }
        assert_eq!(random_port(5, 5).expect("a port is drawn"), 5);
        assert!(random_port(6, 5).is_err());
    }

    /// A child created with handle inheritance on while the listener and an accepted connection are open holds
    /// neither: once both are closed the port binds again with `SO_EXCLUSIVEADDRUSE` while the child still runs.
    #[test]
    fn socket_accepted_connection_is_not_inherited_by_a_child() {
        let listener = listener_on(free_port());
        let port = listener.port();
        let client = TcpStream::connect((LOOPBACK, port)).expect("the client connects");
        let (accepted, peer) = taken(&listener);
        assert_eq!(SocketAddr::V4(peer), client.local_addr().expect("the client's address is read"));
        // std creates every child with inheritance on, so its pipes reach it.
        let mut child = crate::spawn(
            Command::new("cmd.exe")
                .args(["/d", "/c", "set /p x="])
                .stdin(Stdio::piped())
                .stdout(Stdio::null()),
        )
        .expect("the child starts");
        // An abortive close: the connection leaves nothing behind on the port once no handle holds it.
        accepted.set_linger_abort().expect("the linger is set");
        drop(accepted);
        drop(listener);
        drop(client);

        let again = Listener::bind_in_range(LOOPBACK, port, port, port).expect("the bind does not fail");

        child
            .stdin
            .take()
            .expect("the child's stdin")
            .write_all(b"\n")
            .expect("the child is told to end");
        child.wait().expect("the child ends");
        assert!(again.is_some(), "the port is free again while the child runs");
    }

    /// The next connection `listener` takes within the guard, set up, and its peer.
    fn taken(listener: &Listener) -> (Stream, SocketAddrV4) {
        let accepted = listener
            .accept(Instant::now() + GUARD)
            .expect("the accept does not fail")
            .expect("a client is accepted");
        (accepted.stream.expect("the connection is set up"), accepted.peer)
    }

    #[test]
    fn socket_accept_ends_at_its_deadline_without_a_peer() {
        let listener = listener_on(free_port());

        assert!(listener.accept(Instant::now() + SHORT).expect("the accept does not fail").is_none());
    }

    #[test]
    fn socket_read_ends_at_its_deadline_without_a_byte_and_reads_what_comes() {
        let listener = listener_on(free_port());
        let mut client = TcpStream::connect((LOOPBACK, listener.port())).expect("the client connects");
        let (accepted, _) = taken(&listener);
        let mut buffer = [0_u8; 8];

        assert!(
            accepted
                .read(&mut buffer, Instant::now() + SHORT)
                .expect("the read does not fail")
                .is_none()
        );

        client.write_all(b"abc").expect("the client writes");
        let read = accepted.read(&mut buffer, Instant::now() + GUARD).expect("the read does not fail");
        assert_eq!(read, Some(3));
        assert_eq!(&buffer[..3], b"abc");
        drop(client);
        assert_eq!(
            accepted.read(&mut buffer, Instant::now() + GUARD).expect("the read does not fail"),
            Some(0)
        );
    }
}
