//! The unix sockets behind the vsock routes of sandboxes.
//!
//! A vsock route connects a guest vsock port on host CID 2 to a host unix socket, and is fixed
//! when the sandbox is created (ADR 0038). Each sandbox gets its own socket per channel, named
//! after the sandbox, so the socket tells fbkd which sandbox a connection comes from without
//! trusting the guest to say so. The sockets live in a directory private to the user and never
//! fall back to a shared temp directory.

use microsandbox::sandbox::{SandboxSpec, VsockSocketType};
use std::io;
use std::path::{Path, PathBuf};
use tokio::net::{UnixListener, UnixStream};

/// Returns the socket of the sandbox in the directory, or `None` when there's no directory or
/// the path is too long for a unix socket, since microsandbox would then refuse to boot the
/// sandbox.
pub fn socket_path(dir: Option<&Path>, sandbox: &str) -> Option<PathBuf> {
    let path = dir?.join(format!("{sandbox}.sock"));

    std::os::unix::net::SocketAddr::from_pathname(&path)
        .is_ok()
        .then_some(path)
}

/// Whether the sandbox has a stream route from the guest port to the socket.
pub fn has_route(spec: &SandboxSpec, port: u32, socket: &Path) -> bool {
    spec.vsock.routes.iter().any(|route| {
        route.port == port
            && route.socket_type == VsockSocketType::Stream
            && route.host_socket == socket
    })
}

/// Returns the socket directory when it's private to the user, creating it when it's missing.
/// Logs a warning that sandboxes get no `channel` when there's no directory or it isn't private.
pub fn private_dir(dir: Option<PathBuf>, channel: &str) -> Option<PathBuf> {
    let Some(dir) = dir else {
        tracing::warn!("XDG_RUNTIME_DIR isn't an absolute path, so sandboxes get no {channel}");
        return None;
    };

    match firebrick_utils::create_private_dir(&dir) {
        Ok(()) => {
            tracing::info!("{channel} sockets are in {}", dir.display());
            Some(dir)
        }
        Err(err) => {
            tracing::warn!(
                "sandboxes get no {channel}: {err}, or set XDG_RUNTIME_DIR to a private directory"
            );
            None
        }
    }
}

/// Binds the socket at `path` in the private directory `dir`. Checks the directory again first,
/// since it may have been removed since fbkd started, for example by the cleanup of `/tmp`.
/// Removes a stale socket file, but fails with [`io::ErrorKind::AddrInUse`] when another process
/// listens on it.
pub async fn bind(dir: &Path, path: &Path) -> io::Result<UnixListener> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || firebrick_utils::create_private_dir(&dir))
        .await
        .map_err(io::Error::other)??;

    if UnixStream::connect(path).await.is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("another process listens on {}", path.display()),
        ));
    }

    remove_if_exists(path).await?;

    UnixListener::bind(path)
}

/// Removes the file, if there's one, logging a warning when that fails.
pub async fn remove_logged(path: Option<PathBuf>) {
    let Some(path) = path else {
        return;
    };

    if let Err(err) = remove_if_exists(&path).await {
        tracing::warn!("failed to remove {}: {err}", path.display());
    }
}

/// Removes the file, succeeding when it doesn't exist.
async fn remove_if_exists(path: &Path) -> io::Result<()> {
    match tokio::fs::remove_file(path).await {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn socket_path_is_none_when_too_long_or_without_a_directory() {
        let dir = PathBuf::from("/run/user/1000/firebrick/netlog");

        assert_eq!(
            socket_path(Some(&dir), "short"),
            Some(dir.join("short.sock"))
        );
        assert!(socket_path(Some(&dir), &"a".repeat(200)).is_none());
        assert!(socket_path(None, "short").is_none());
    }

    #[tokio::test]
    async fn bind_leaves_a_live_socket_alone_and_replaces_a_stale_one() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir = temp.path().join("sockets");
        let path = dir.join("dev.sock");

        let first = bind(&dir, &path).await.unwrap();
        let busy = bind(&dir, &path).await.unwrap_err();
        drop(first);
        let second = bind(&dir, &path).await;

        assert_eq!(busy.kind(), io::ErrorKind::AddrInUse);
        assert!(second.is_ok());
    }

    #[tokio::test]
    async fn bind_refuses_a_directory_with_group_or_other_access() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir = temp.path().join("sockets");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        let error = bind(&dir, &dir.join("dev.sock")).await.unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn private_dir_keeps_only_a_private_directory() {
        let temp = tempfile::TempDir::new().unwrap();
        let private = temp.path().join("private");
        let open = temp.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            private_dir(Some(private.clone()), "test"),
            Some(private.clone())
        );
        assert_eq!(private_dir(Some(open), "test"), None);
        assert_eq!(private_dir(None, "test"), None);
    }
}
