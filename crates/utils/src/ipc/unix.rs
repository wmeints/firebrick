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
    let (peer_uid, peer_pid) = peer(stream)?;
    let own_uid = nix::unistd::geteuid().as_raw();

    if is_trusted_peer(peer_uid, own_uid) {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("the {role} runs as UID {peer_uid} (PID {peer_pid:?}), not as you"),
    ))
}

/// Returns the UID and, when known, the PID of the stream's peer.
#[cfg(not(target_os = "macos"))]
fn peer(stream: &UnixStream) -> io::Result<(u32, Option<i32>)> {
    let cred = stream.peer_cred()?;
    Ok((cred.uid(), cred.pid()))
}

/// Returns the UID of the stream's peer, which the kernel saved when it connected. On macOS
/// `peer_cred` also asks for the PID, which fails with `ENOTCONN` once the peer has closed, so a
/// client that wrote and closed before the listener accepted it would be refused.
#[cfg(target_os = "macos")]
fn peer(stream: &UnixStream) -> io::Result<(u32, Option<i32>)> {
    let (uid, _) = nix::unistd::getpeereid(stream)?;
    Ok((uid.as_raw(), None))
}

/// Returns the error with the socket's path in front of its message.
fn with_path(err: io::Error, path: &Path) -> io::Error {
    io::Error::new(err.kind(), format!("{}: {err}", path.display()))
}

/// Returns whether a peer on the daemon socket running as `peer_uid` is trusted by a process
/// running as `own_uid`: only the same user and root are. The daemon applies this to its
/// clients and the CLI to the daemon. Root is trusted because it can bypass the check anyway,
/// for example by reading the other process's memory. [`super`] applies it on Unix.
fn is_trusted_peer(peer_uid: u32, own_uid: u32) -> bool {
    peer_uid == own_uid || peer_uid == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusts_only_own_uid_and_root() {
        let cases = [(1000, true), (0, true), (1001, false), (u32::MAX, false)];
        for (peer_uid, expected) in cases {
            assert_eq!(
                is_trusted_peer(peer_uid, 1000),
                expected,
                "peer uid {peer_uid}"
            );
        }
    }
}
