//! The sessions of this machine: which one is attached to the console, and for a session whether it is connected,
//! who is logged on to it and whether it is locked.

use std::io;

use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
use windows_sys::Win32::System::RemoteDesktop::{
    WTS_CONNECTSTATE_CLASS, WTS_CURRENT_SERVER_HANDLE, WTS_SESSIONSTATE_LOCK, WTS_SESSIONSTATE_UNLOCK, WTSActive, WTSConnectQuery, WTSConnected,
    WTSDisconnected, WTSDown, WTSFreeMemory, WTSGetActiveConsoleSessionId, WTSINFOEXW, WTSIdle, WTSInit, WTSListen, WTSQuerySessionInformationW,
    WTSReset, WTSSessionInfoEx, WTSShadow,
};
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_READOBJECTS, GetUserObjectInformationW, HDESK, OpenInputDesktop, UOI_NAME,
};
use windows_sys::core::PWSTR;

/// The id `WTSGetActiveConsoleSessionId` gives when no session is attached to the console, as while one is being
/// attached or detached.
const NO_SESSION: u32 = 0xFFFF_FFFF;

/// The id of the session attached to the console (`WTSGetActiveConsoleSessionId`): the one whose desktop the screen
/// shows. `None` while no session is attached, as during a switch from one session to another.
#[must_use]
pub fn active_console_session() -> Option<u32> {
    // SAFETY: the call takes no argument and cannot fail.
    let session = unsafe { WTSGetActiveConsoleSessionId() };
    (session != NO_SESSION).then_some(session)
}

/// The name of the desktop that takes this session's input now (`OpenInputDesktop` with `DESKTOP_READOBJECTS`, then
/// `GetUserObjectInformationW` with `UOI_NAME`): `Default` for the user's desktop. A secure desktop, as the lock screen
/// or a UAC prompt shows, either has another name or cannot be opened at all.
///
/// # Errors
///
/// The system's error when the input desktop cannot be opened or its name cannot be read.
pub fn input_desktop_name() -> io::Result<String> {
    // SAFETY: `OpenInputDesktop` takes no pointers; a null result is a failure, read before anything else runs.
    let desktop = unsafe { OpenInputDesktop(0, 0, DESKTOP_READOBJECTS) };
    if desktop.is_null() {
        return Err(io::Error::last_os_error());
    }
    let name = desktop_name(desktop);
    // SAFETY: the handle was opened above, is used by nothing else and is closed once; a failed close loses nothing
    // the name needs.
    unsafe { CloseDesktop(desktop) };
    name
}

/// The name of the open desktop `desktop`.
fn desktop_name(desktop: HDESK) -> io::Result<String> {
    let mut buffer = vec![0_u16; 64];
    loop {
        let size = u32::try_from(buffer.len() * 2).map_err(io::Error::other)?;
        let mut needed = 0;
        // SAFETY: the desktop is open for the whole call, `buffer` is writable for `size` bytes and `needed` is writable.
        let read = unsafe { GetUserObjectInformationW(desktop, UOI_NAME, buffer.as_mut_ptr().cast(), size, &raw mut needed) };
        if read != 0 {
            let units = buffer.iter().position(|&unit| unit == 0).unwrap_or(buffer.len());
            return Ok(String::from_utf16_lossy(&buffer[..units]));
        }
        let error = io::Error::last_os_error();
        let wanted = (needed as usize).div_ceil(2);
        if error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER.cast_signed()) || wanted <= buffer.len() {
            return Err(error);
        }
        buffer.resize(wanted, 0);
    }
}

/// A session's connection state (`WTS_CONNECTSTATE_CLASS`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectState {
    /// `WTSActive`: a user is logged on and the session is connected to a display, the console or a remote client.
    Active,
    /// `WTSConnected`: connected to a client, no user logged on.
    Connected,
    /// `WTSConnectQuery`: connecting to a client.
    ConnectQuery,
    /// `WTSShadow`: shadowing another session.
    Shadow,
    /// `WTSDisconnected`: a user is logged on, but no client or display is connected.
    Disconnected,
    /// `WTSIdle`: waiting for a client to connect.
    Idle,
    /// `WTSListen`: listening for a connection.
    Listen,
    /// `WTSReset`: being reset.
    Reset,
    /// `WTSDown`: down because of an error.
    Down,
    /// `WTSInit`: being initialized.
    Init,
    /// A value this crate does not know.
    Other(i32),
}

/// Whether a session is locked, as its `SessionFlags` say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockState {
    /// The session is locked: its desktop is the lock screen.
    Locked,
    /// The session is unlocked.
    Unlocked,
    /// The system does not say (`WTS_SESSIONSTATE_UNKNOWN`), or gave a value this crate does not know. A caller must
    /// treat it as locked: nothing says the desktop is not the lock screen.
    Unknown,
}

/// What a session's `WTSSessionInfoEx` says of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionState {
    /// The session's id.
    pub id: u32,
    /// Its connection state.
    pub connect: ConnectState,
    /// The name of the user logged on to it, without its domain; empty when nobody is.
    pub user: String,
    /// The domain of that user, the machine's own name for a local account; empty when nobody is logged on.
    pub domain: String,
    /// Whether it is locked.
    pub lock: LockState,
}

/// What the session `id` is now: its connection state, the user logged on to it and whether it is locked
/// (`WTSQuerySessionInformationW` with `WTSSessionInfoEx`).
///
/// # Errors
///
/// The system's error when the session cannot be read, as for an id no session has; an error when the system answers
/// in a form other than the level-1 information asked for.
pub fn session_state(id: u32) -> io::Result<SessionState> {
    let mut buffer: PWSTR = std::ptr::null_mut();
    let mut bytes = 0;
    // SAFETY: the local server's handle needs no opening, and both out pointers are writable.
    let read = unsafe { WTSQuerySessionInformationW(WTS_CURRENT_SERVER_HANDLE, id, WTSSessionInfoEx, &raw mut buffer, &raw mut bytes) };
    if read == 0 {
        return Err(io::Error::last_os_error());
    }
    let memory = WtsMemory(buffer);
    if (bytes as usize) < size_of::<WTSINFOEXW>() {
        return Err(io::Error::other(format!(
            "the information of session {id} takes {bytes} bytes, fewer than the {} a WTSINFOEXW needs",
            size_of::<WTSINFOEXW>()
        )));
    }
    // SAFETY: the system wrote at least a whole `WTSINFOEXW` into the buffer, which lives until `memory` drops; the
    // read copies it without assuming the buffer's alignment.
    let info = unsafe { memory.0.cast::<WTSINFOEXW>().read_unaligned() };
    drop(memory);
    if info.Level != 1 {
        return Err(io::Error::other(format!("session {id} was described at level {}, not 1", info.Level)));
    }
    // SAFETY: level 1 says the union holds its level-1 member.
    let level1 = unsafe { info.Data.WTSInfoExLevel1 };
    Ok(SessionState {
        id,
        connect: connect_state(level1.SessionState),
        user: until_nul(&level1.UserName),
        domain: until_nul(&level1.DomainName),
        lock: lock_state(level1.SessionFlags),
    })
}

/// Memory a WTS call allocated, freed when dropped.
struct WtsMemory(PWSTR);

impl Drop for WtsMemory {
    fn drop(&mut self) {
        // SAFETY: the memory was allocated by `WTSQuerySessionInformationW` and is freed once, here.
        unsafe { WTSFreeMemory(self.0.cast()) };
    }
}

/// Each `WTS_CONNECTSTATE_CLASS` value this crate knows, and the state it names.
const CONNECT_STATES: [(WTS_CONNECTSTATE_CLASS, ConnectState); 10] = [
    (WTSActive, ConnectState::Active),
    (WTSConnected, ConnectState::Connected),
    (WTSConnectQuery, ConnectState::ConnectQuery),
    (WTSShadow, ConnectState::Shadow),
    (WTSDisconnected, ConnectState::Disconnected),
    (WTSIdle, ConnectState::Idle),
    (WTSListen, ConnectState::Listen),
    (WTSReset, ConnectState::Reset),
    (WTSDown, ConnectState::Down),
    (WTSInit, ConnectState::Init),
];

/// The connection state a `WTS_CONNECTSTATE_CLASS` value names.
fn connect_state(class: WTS_CONNECTSTATE_CLASS) -> ConnectState {
    CONNECT_STATES
        .iter()
        .find(|(known, _)| *known == class)
        .map_or(ConnectState::Other(class), |&(_, state)| state)
}

/// The lock state a session's `SessionFlags` say: `WTS_SESSIONSTATE_LOCK` (0) is locked and `WTS_SESSIONSTATE_UNLOCK`
/// (1) unlocked; any other value, `WTS_SESSIONSTATE_UNKNOWN` among them, is unknown. (Windows 7 and Windows Server
/// 2008 R2 give the two the other way round, per `WTSINFOEX_LEVEL1_W`; the gate runs only on Windows 10 and later.)
fn lock_state(flags: i32) -> LockState {
    match flags.cast_unsigned() {
        WTS_SESSIONSTATE_LOCK => LockState::Locked,
        WTS_SESSIONSTATE_UNLOCK => LockState::Unlocked,
        _ => LockState::Unknown,
    }
}

/// The text of a fixed UTF-16 field, up to its first NUL.
fn until_nul(field: &[u16]) -> String {
    let length = field.iter().position(|&unit| unit == 0).unwrap_or(field.len());
    String::from_utf16_lossy(&field[..length])
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose session cannot be read must fail loudly")]
mod tests {
    use windows_sys::Win32::System::RemoteDesktop::{
        WTSActive, WTSConnectQuery, WTSConnected, WTSDisconnected, WTSDown, WTSIdle, WTSInit, WTSListen, WTSReset, WTSShadow,
    };

    use super::{ConnectState, LockState, active_console_session, connect_state, lock_state, session_state};

    #[test]
    fn the_console_session_of_this_pc_is_read() {
        let id = active_console_session().expect("this PC's console has a session");

        let state = session_state(id).expect("the console session is read");

        assert_eq!(state.id, id);
    }

    #[test]
    fn each_connect_state_is_named() {
        let named = [
            (WTSActive, ConnectState::Active),
            (WTSConnected, ConnectState::Connected),
            (WTSConnectQuery, ConnectState::ConnectQuery),
            (WTSShadow, ConnectState::Shadow),
            (WTSDisconnected, ConnectState::Disconnected),
            (WTSIdle, ConnectState::Idle),
            (WTSListen, ConnectState::Listen),
            (WTSReset, ConnectState::Reset),
            (WTSDown, ConnectState::Down),
            (WTSInit, ConnectState::Init),
        ];
        for (class, state) in named {
            assert_eq!(connect_state(class), state, "class {class}");
        }
        assert_eq!(connect_state(10), ConnectState::Other(10));
    }

    #[test]
    fn the_lock_flag_reads_unlocked_as_one_and_locked_as_zero() {
        assert_eq!(lock_state(1), LockState::Unlocked);
        assert_eq!(lock_state(0), LockState::Locked);
    }

    #[test]
    fn an_unknown_lock_flag_is_unknown() {
        assert_eq!(lock_state(-1), LockState::Unknown, "WTS_SESSIONSTATE_UNKNOWN");
        assert_eq!(lock_state(2), LockState::Unknown);
    }
}
