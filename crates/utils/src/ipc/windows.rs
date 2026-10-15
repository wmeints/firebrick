//! Named pipes for [`super`], restricted to the current user and checked with the user of the
//! process at the other end of each connection.

use super::Stream;
use crate::win32::{self, Sid};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer};
use tokio::time::Instant;
use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;

/// The longest pipe name Windows accepts, `\\.\pipe\` included.
const MAX_PIPE_NAME: usize = 256;

/// How long [`connect`] keeps trying while every instance of the pipe is busy.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long [`connect`] waits before it tries a busy pipe again.
const BUSY_RETRY: Duration = Duration::from_millis(10);

/// A named pipe with a free instance waiting for the next client.
pub struct Listener {
    path: PathBuf,
    owner: Sid,
    next: NamedPipeServer,
}

impl Listener {
    /// Creates the first instance of the pipe, failing with [`io::ErrorKind::AddrInUse`] when
    /// the pipe exists already, so a pipe that another process created first is never served.
    pub fn bind(path: &Path) -> io::Result<Self> {
        let owner = Sid::current_user()?;
        let next = win32::create_pipe(path, &owner, true).map_err(|err| in_use(err, path))?;

        Ok(Self {
            path: path.to_path_buf(),
            owner,
            next,
        })
    }

    /// Waits for a client on the free instance, puts a new free instance in its place, and
    /// checks that the client is trusted. The instance is only replaced once a client
    /// connected, so a cancelled call loses nothing. When the client is gone before it's
    /// accepted, or no new instance can be created, the client is disconnected and the instance
    /// waits for the next one, so a failure that persists can't make every call return at once.
    pub async fn accept(&mut self) -> io::Result<Stream> {
        let next = match self.next.connect().await {
            Ok(()) => win32::create_pipe(&self.path, &self.owner, false),
            Err(err) => Err(err),
        };
        let next = next.inspect_err(|_| {
            // The instance is free again for the next client; a disconnect error leaves it
            // as it was, which the next `connect` reports.
            let _ = self.next.disconnect();
        })?;
        let pipe = std::mem::replace(&mut self.next, next);

        check_peer(win32::pipe_client_pid(&pipe)?, &self.owner, "client")?;
        Ok(Stream::new(pipe))
    }
}

/// Connects to the pipe and checks that the process serving it is trusted.
pub async fn connect(path: &Path) -> io::Result<Stream> {
    let pipe = open(path).await?;
    let owner = Sid::current_user()?;

    check_peer(win32::pipe_server_pid(&pipe)?, &owner, "server")
        .map_err(|err| io::Error::new(err.kind(), format!("{}: {err}", path.display())))?;
    Ok(Stream::new(pipe))
}

/// Opens the pipe, trying again while all its instances are busy.
async fn open(path: &Path) -> io::Result<NamedPipeClient> {
    let deadline = Instant::now() + BUSY_TIMEOUT;

    loop {
        match ClientOptions::new().open(path) {
            Err(err) if is_busy(&err) && Instant::now() < deadline => {}
            opened => return opened,
        }
        tokio::time::sleep(BUSY_RETRY).await;
    }
}

/// Whether a process serves the pipe, also when all its instances are busy.
pub async fn is_served(path: &Path) -> bool {
    match ClientOptions::new().open(path) {
        Ok(_) => true,
        Err(err) => is_busy(&err),
    }
}

/// Whether the pipe exists, which is when a process serves it.
pub async fn exists(path: &Path) -> bool {
    is_served(path).await
}

/// Does nothing: a pipe disappears with its last server instance.
pub async fn remove(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Returns the pipe `<dir>-<name>`, or `None` when it's longer than a pipe name can be.
pub fn endpoint(dir: &Path, name: &str) -> Option<PathBuf> {
    let mut pipe = OsString::from(dir);
    pipe.push("-");
    pipe.push(name);

    (pipe.len() <= MAX_PIPE_NAME).then(|| pipe.into())
}

/// Does nothing: pipes have no directory to prepare.
pub fn prepare_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Whether the error says that every instance of the pipe is busy.
fn is_busy(err: &io::Error) -> bool {
    err.raw_os_error() == i32::try_from(ERROR_PIPE_BUSY).ok()
}

/// Turns the access denied error of creating a first instance that exists into
/// [`io::ErrorKind::AddrInUse`].
fn in_use(err: io::Error, path: &Path) -> io::Error {
    if err.kind() != io::ErrorKind::PermissionDenied {
        return err;
    }

    io::Error::new(
        io::ErrorKind::AddrInUse,
        format!("another process serves {}", path.display()),
    )
}

/// Fails with [`io::ErrorKind::PermissionDenied`] when the process `pid` at the other end, the
/// `role` of the connection, runs as another user than `owner` and isn't `LocalSystem`.
fn check_peer(pid: u32, owner: &Sid, role: &str) -> io::Result<()> {
    let user = Sid::of_process(pid)?;

    if user == *owner || user.is_local_system() {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("the {role} runs as {user} (PID {pid}), not as you"),
    ))
}
