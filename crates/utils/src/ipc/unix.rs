//! Unix sockets for [`super`], checked with the peer credentials of each connection.

use super::Stream;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tokio::net::{UnixListener, UnixStream};

/// A listening unix socket.
pub struct Listener(UnixListener);

impl Listener {
    /// Binds the socket and gives it mode `0600`, whatever the umask. Removes the socket again
    /// when that fails.
    pub fn bind(path: &Path) -> io::Result<Self> {
        let listener = UnixListener::bind(path)?;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).inspect_err(|_| {
            let _ = fs::remove_file(path);
        })?;

        Ok(Self(listener))
    }

    /// Accepts the next connection and checks that its peer is trusted.
    pub async fn accept(&mut self) -> io::Result<Stream> {
        let (stream, _) = self.0.accept().await?;
        check_peer(&stream, "client")?;
        Ok(Stream::new(stream))
    }
}

/// Connects to the socket and checks that the process serving it is trusted.
pub async fn connect(path: &Path) -> io::Result<Stream> {
    let stream = UnixStream::connect(path).await?;
    check_peer(&stream, "server").map_err(|err| with_path(err, path))?;
    Ok(Stream::new(stream))
}

/// Whether a process listens on the socket.
pub async fn is_served(path: &Path) -> bool {
    UnixStream::connect(path).await.is_ok()
}

/// Whether the socket file exists, live or stale.
pub async fn exists(path: &Path) -> bool {
    tokio::fs::try_exists(path).await.unwrap_or(false)
}

/// Removes the socket file, succeeding when there's none.
pub async fn remove(path: &Path) -> io::Result<()> {
    match tokio::fs::remove_file(path).await {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

/// Returns the socket `<dir>/<name>.sock`, or `None` when its path is too long for a unix socket
/// address.
pub fn endpoint(dir: &Path, name: &str) -> Option<PathBuf> {
    let path = dir.join(format!("{name}.sock"));

    std::os::unix::net::SocketAddr::from_pathname(&path)
        .is_ok()
        .then_some(path)
}

/// Creates the socket directory private to the user.
pub fn prepare_dir(dir: &Path) -> io::Result<()> {
    crate::create_private_dir(dir)
}

/// Fails with [`io::ErrorKind::PermissionDenied`] when the peer of the stream, the `role` of the
/// connection, runs as another user than this process and isn't root.
fn check_peer(stream: &UnixStream, role: &str) -> io::Result<()> {
    let peer = stream.peer_cred()?;
    let own_uid = nix::unistd::geteuid().as_raw();

    if crate::is_trusted_peer(peer.uid(), own_uid) {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "the {role} runs as UID {} (PID {:?}), not as you",
            peer.uid(),
            peer.pid()
        ),
    ))
}

/// Returns the error with the socket's path in front of its message.
fn with_path(err: io::Error, path: &Path) -> io::Error {
    io::Error::new(err.kind(), format!("{}: {err}", path.display()))
}
