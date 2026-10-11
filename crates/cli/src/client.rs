use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use crate::api::sandbox_management_service_client::SandboxManagementServiceClient;
use anyhow::{Context, Result, bail};
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;
use tokio::time::{Instant, sleep};
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

const DAEMON_BINARY: &str = "fbkd";
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(5);
const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Connects to the daemon over its unix socket, starting the daemon when needed.
pub async fn connect() -> Result<SandboxManagementServiceClient<Channel>> {
    let path = firebrick_utils::socket_path();

    // Note: the HTTP endpoint isn't actually used. It will only show up in the authority header.
    let channel = Endpoint::try_from("http://localhost")
        .unwrap()
        .connect_with_connector(service_fn(move |_: Uri| {
            let path = path.clone();
            async move {
                let stream = ensure_daemon(&path).await.map_err(std::io::Error::other)?;
                Ok::<_, std::io::Error>(TokioIo::new(stream))
            }
        }))
        .await?;

    Ok(SandboxManagementServiceClient::new(channel))
}

/// Connects to the daemon socket, spawning the daemon and waiting for it when it isn't running.
pub async fn ensure_daemon(socket_path: &Path) -> Result<UnixStream> {
    if let Ok(stream) = UnixStream::connect(socket_path).await {
        return verify_owner(socket_path, stream);
    }

    // The socket file may be left behind by a daemon that didn't shut down cleanly.
    // The daemon refuses to start when the socket exists, so clean it up first.
    if socket_path.exists() {
        std::fs::remove_file(socket_path)
            .with_context(|| format!("failed to remove stale socket {}", socket_path.display()))?;
    }

    let child = Command::new(daemon_binary())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .context("failed to start the firebrick daemon")?;

    let stream = wait_for_daemon(socket_path, child).await?;
    verify_owner(socket_path, stream)
}

/// Returns the stream when the process serving it runs as the current user or root, so `fbk`
/// never sends secrets or requests to a daemon another user planted on the socket path, for
/// example in the shared temp dir when `XDG_RUNTIME_DIR` isn't set.
fn verify_owner(socket_path: &Path, stream: UnixStream) -> Result<UnixStream> {
    let peer_uid = stream
        .peer_cred()
        .with_context(|| format!("failed to read the owner of {}", socket_path.display()))?
        .uid();
    if !is_trusted(peer_uid, nix::unistd::geteuid().as_raw()) {
        bail!(
            "refusing to use {}: it's served by UID {peer_uid}, not by you",
            socket_path.display()
        );
    }
    Ok(stream)
}

/// Returns whether a daemon running as `peer_uid` may serve a client running as `own_uid`. Root
/// is trusted because it can read the client's memory anyway, as the daemon's own check allows.
fn is_trusted(peer_uid: u32, own_uid: u32) -> bool {
    peer_uid == own_uid || peer_uid == 0
}

/// Waits until the spawned daemon listens on the socket, failing when it exits or times out.
async fn wait_for_daemon(socket_path: &Path, mut child: Child) -> Result<UnixStream> {
    let deadline = Instant::now() + DAEMON_START_TIMEOUT;

    loop {
        if let Ok(stream) = UnixStream::connect(socket_path).await {
            return Ok(stream);
        }

        if let Some(status) = child.try_wait()? {
            bail!("the firebrick daemon exited unexpectedly ({status})");
        }

        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for the firebrick daemon to listen on {}",
                socket_path.display()
            );
        }

        sleep(DAEMON_POLL_INTERVAL).await;
    }
}

/// Returns the daemon binary next to the current executable, or falls back to looking it up on `PATH`.
fn daemon_binary() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(DAEMON_BINARY)))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(DAEMON_BINARY))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusts_only_own_uid_and_root() {
        let cases = [(1000, true), (0, true), (1001, false), (65534, false)];
        for (peer_uid, expected) in cases {
            assert_eq!(is_trusted(peer_uid, 1000), expected, "peer uid {peer_uid}");
        }
    }

    #[tokio::test]
    async fn accepts_a_socket_served_by_the_current_user() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fbkd.sock");
        let _listener = tokio::net::UnixListener::bind(&path).unwrap();

        let stream = UnixStream::connect(&path).await.unwrap();

        assert!(verify_owner(&path, stream).is_ok());
    }
}
