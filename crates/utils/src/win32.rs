//! The Win32 calls behind the named pipes of [`crate::ipc`] and the private files of firebrick:
//! the user a process runs as, the processes at both ends of a pipe, and pipes, files and
//! directories only the current user and the system can open (ADR 0050, 0051).
//!
//! This is the only module with `unsafe` code. Each block calls one Win32 function with
//! pointers to live, correctly sized values and checks its result.

use std::fmt;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr;
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::{
    HANDLE, HANDLE_FLAG_INHERIT, LocalFree, SetHandleInformation,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT,
    SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetLengthSid, GetSecurityDescriptorDacl, GetTokenInformation,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// The string form of the `LocalSystem` account's SID, Windows' counterpart of root.
const LOCAL_SYSTEM: &str = "S-1-5-18";

/// A security identifier (SID) of a user, in its binary form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sid(Vec<u8>);

impl Sid {
    /// Returns the user the current process runs as.
    pub fn current_user() -> io::Result<Self> {
        // SAFETY: `GetCurrentProcess` has no preconditions and returns a pseudo handle that is
        // always valid and needs no closing.
        // HUMAN-APPROVED: Win32 FFI to identify the current user for the named pipe checks.
        let process = unsafe { GetCurrentProcess() };
        token_user(process)
    }

    /// Returns the user the process `pid` runs as. Fails when the process is gone or this
    /// process may not query it.
    pub fn of_process(pid: u32) -> io::Result<Self> {
        // SAFETY: `OpenProcess` has no preconditions; it returns null on failure, checked below.
        // HUMAN-APPROVED: Win32 FFI to identify the user at the other end of a named pipe.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `OpenProcess` succeeded, so `handle` is a process handle nothing else owns.
        // HUMAN-APPROVED: takes ownership of the handle so it is closed exactly once.
        let process = unsafe { OwnedHandle::from_raw_handle(handle) };
        token_user(process.as_raw_handle())
    }

    /// Whether this is the `LocalSystem` account.
    pub fn is_local_system(&self) -> bool {
        self.to_string() == LOCAL_SYSTEM
    }
}

impl fmt::Display for Sid {
    /// Writes the SID in its string form, such as `S-1-5-21-1004336348-1177238915-682003330-512`:
    /// the revision, the identifier authority and the sub-authorities, from a binary SID made
    /// of a revision byte, a sub-authority count, a 48-bit big-endian authority and 32-bit
    /// little-endian sub-authorities.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (Some(&revision), Some(authority)) = (self.0.first(), self.0.get(2..8)) else {
            return f.write_str("S-?");
        };
        let authority = authority
            .iter()
            .fold(0u64, |value, &byte| (value << 8) | u64::from(byte));

        write!(f, "S-{revision}-{authority}")?;
        for sub in self.0.get(8..).unwrap_or_default().as_chunks::<4>().0 {
            write!(f, "-{}", u32::from_le_bytes(*sub))?;
        }

        Ok(())
    }
}

/// Returns the user of the process behind `process`.
fn token_user(process: HANDLE) -> io::Result<Sid> {
    let mut token = ptr::null_mut();
    // SAFETY: `process` is a valid process handle and `token` a valid out pointer.
    // HUMAN-APPROVED: Win32 FFI to read the user of a process from its access token.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `OpenProcessToken` succeeded, so `token` is a handle nothing else owns.
    // HUMAN-APPROVED: takes ownership of the handle so it is closed exactly once.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };

    let buffer = token_information(&token)?;
    // SAFETY: `GetTokenInformation` filled `buffer`, which is aligned for `TOKEN_USER`, with a
    // `TOKEN_USER` whose SID points to a valid SID inside the same buffer, and `GetLengthSid`
    // returns that SID's length in bytes.
    // HUMAN-APPROVED: Win32 FFI to copy the user's SID out of the token information.
    let sid = unsafe {
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let len = GetLengthSid(user.User.Sid);
        std::slice::from_raw_parts(user.User.Sid.cast::<u8>(), to_usize(len)?).to_vec()
    };

    Ok(Sid(sid))
}

/// Returns the `TokenUser` information of the token, in a buffer of `u64`s so that it's aligned
/// for `TOKEN_USER`.
fn token_information(token: &OwnedHandle) -> io::Result<Vec<u64>> {
    let mut len = 0;
    // SAFETY: a null buffer of length 0 only asks for the length the information needs, which
    // the call writes to the valid out pointer `len`. It fails by design, so its result is
    // ignored and the length checked instead.
    // HUMAN-APPROVED: Win32 FFI to size the buffer for the token's user.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            ptr::null_mut(),
            0,
            &mut len,
        )
    };
    if len == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut buffer = vec![0u64; to_usize(len)?.div_ceil(8)];
    // SAFETY: `buffer` has at least `len` writable bytes.
    // HUMAN-APPROVED: Win32 FFI to read the token's user into the buffer.
    let read = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            len,
            &mut len,
        )
    };
    if read == 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(buffer)
}

/// Returns the ID of the process that serves the pipe the client is connected to.
pub fn pipe_server_pid(pipe: &impl AsRawHandle) -> io::Result<u32> {
    let mut pid = 0;
    // SAFETY: `pipe` is a pipe handle that stays open for the call and `pid` a valid out
    // pointer.
    // HUMAN-APPROVED: Win32 FFI to find the process serving a named pipe.
    if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(pid)
}

/// Returns the ID of the process connected to the server end of the pipe.
pub fn pipe_client_pid(pipe: &impl AsRawHandle) -> io::Result<u32> {
    let mut pid = 0;
    // SAFETY: `pipe` is a pipe handle that stays open for the call and `pid` a valid out
    // pointer.
    // HUMAN-APPROVED: Win32 FFI to find the process connected to a named pipe.
    if unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(pid)
}

/// Creates an instance of the pipe that rejects remote clients and whose access list grants
/// access only to `owner` and `LocalSystem`. With `first`, fails when the pipe exists already.
pub fn create_pipe(path: &Path, owner: &Sid, first: bool) -> io::Result<NamedPipeServer> {
    let descriptor = SecurityDescriptor::from_sddl(&format!("D:P(A;;GA;;;{owner})(A;;GA;;;SY)"))?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).map_err(io::Error::other)?,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first)
        .reject_remote_clients(true);

    // SAFETY: `attributes` is a valid `SECURITY_ATTRIBUTES` whose descriptor stays alive until
    // the call returns.
    // HUMAN-APPROVED: Win32 FFI to create a named pipe only the current user can open.
    unsafe { options.create_with_security_attributes_raw(path, (&raw mut attributes).cast()) }
}

/// Sets whether the processes this one starts inherit the handle. Windows hands a child every
/// inheritable handle of its parent, so a long-lived child such as `fbkd` would keep the pipe
/// behind an inherited handle open after the parent exits.
pub fn set_inheritable(handle: &impl AsRawHandle, inherit: bool) -> io::Result<()> {
    let flags = if inherit { HANDLE_FLAG_INHERIT } else { 0 };
    // SAFETY: `handle` is a handle that stays open for the call.
    // HUMAN-APPROVED: Win32 FFI to keep fbkd from inheriting the stdio handles of fbk.
    if unsafe { SetHandleInformation(handle.as_raw_handle(), HANDLE_FLAG_INHERIT, flags) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A self-relative security descriptor that Windows allocated, freed when dropped.
struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    /// Returns the descriptor that the string in Security Descriptor Definition Language
    /// describes.
    fn from_sddl(sddl: &str) -> io::Result<Self> {
        let sddl: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
        let mut descriptor = ptr::null_mut();
        // SAFETY: `sddl` is a NUL-terminated UTF-16 string and `descriptor` a valid out
        // pointer; the size out pointer may be null.
        // HUMAN-APPROVED: Win32 FFI to build the access list of the named pipes.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        if converted == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(descriptor))
    }
}

/// Replaces the access list of the file or directory at `path` with a protected one that grants
/// full access to `owner`, `LocalSystem` and the administrators only, like mode `0600` or `0700`
/// on Unix. A directory's list is inherited by the files and directories in it. Win32-OpenSSH
/// accepts keys with this list, and the administrators can take any file anyway.
pub fn restrict_to_owner(path: &Path, owner: &Sid) -> io::Result<()> {
    let inherit = if path.is_dir() { "OICI" } else { "" };
    let descriptor = SecurityDescriptor::from_sddl(&format!(
        "D:P(A;{inherit};FA;;;{owner})(A;{inherit};FA;;;SY)(A;{inherit};FA;;;BA)"
    ))?;
    let (mut present, mut defaulted, mut dacl) = (0, 0, ptr::null_mut());
    // SAFETY: `descriptor` is a valid security descriptor and the out pointers are valid. The
    // access list it returns points into the descriptor, which outlives its use below.
    // HUMAN-APPROVED: Win32 FFI to read the access list out of the descriptor.
    let read =
        unsafe { GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted) };
    if read == 0 || present == 0 {
        return Err(io::Error::last_os_error());
    }

    let name: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: `name` is a NUL-terminated UTF-16 path, `dacl` a valid access list, and the owner,
    // group and system list pointers may be null because only the access list is set.
    // HUMAN-APPROVED: Win32 FFI to make firebrick's private files accessible only to the user.
    let status = unsafe {
        SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            dacl,
            ptr::null(),
        )
    };
    if status != 0 {
        let code = i32::try_from(status).map_err(io::Error::other)?;
        return Err(io::Error::from_raw_os_error(code));
    }

    Ok(())
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: Windows allocated the descriptor with `LocalAlloc`, and it's freed only here.
        // HUMAN-APPROVED: Win32 FFI to free the access list once the pipe is created.
        unsafe { LocalFree(self.0) };
    }
}

/// Converts a length from Win32 to `usize`.
fn to_usize(len: u32) -> io::Result<usize> {
    usize::try_from(len).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sid_is_written_in_its_string_form() {
        let mut bytes = vec![1, 2, 0, 0, 0, 0, 0, 5];
        bytes.extend(21u32.to_le_bytes());
        bytes.extend(1_004_336_348u32.to_le_bytes());

        assert_eq!(Sid(bytes).to_string(), "S-1-5-21-1004336348");
    }

    #[test]
    fn local_system_is_recognized() {
        let mut bytes = vec![1, 1, 0, 0, 0, 0, 0, 5];
        bytes.extend(18u32.to_le_bytes());

        assert!(Sid(bytes).is_local_system());
    }

    #[test]
    fn current_user_is_the_user_of_the_current_process() {
        let user = Sid::current_user().unwrap();

        assert_eq!(Sid::of_process(std::process::id()).unwrap(), user);
        // Local and domain accounts have `S-1-5-` SIDs, Entra ID accounts `S-1-12-1-`.
        assert!(user.to_string().starts_with("S-1-"), "{user}");
        assert!(!user.is_local_system());
    }

    /// Starts a child that lives for a few seconds with its stdio closed, the way `fbk` starts
    /// `fbkd`, after giving the write end of a new pipe the inheritance `inherit`. Then closes
    /// that end and returns whether the read end sees the end of the pipe within a second, which
    /// it doesn't while the child holds an inherited copy.
    fn pipe_ends_with_a_child_running(inherit: bool) -> bool {
        use std::io::Read;
        use std::process::{Command, Stdio};

        let (mut reader, writer) = std::io::pipe().unwrap();
        set_inheritable(&writer, inherit).unwrap();
        let mut child = Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 5"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        drop(writer);

        let (done, ended) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(reader.read(&mut [0; 1]).ok() == Some(0));
        });
        let result = ended.recv_timeout(std::time::Duration::from_secs(1)) == Ok(true);
        let _ = child.kill();
        let _ = child.wait();
        result
    }

    #[test]
    fn a_child_keeps_an_inherited_pipe_open_unless_inheritance_is_off() {
        assert!(!pipe_ends_with_a_child_running(true));
        assert!(pipe_ends_with_a_child_running(false));
    }

    #[test]
    fn of_process_fails_for_a_process_that_does_not_exist() {
        assert!(Sid::of_process(u32::MAX - 3).is_err());
    }
}
