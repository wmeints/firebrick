use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use crate::api::sandbox_management_service_client::SandboxManagementServiceClient;
use anyhow::{Context, Result, bail};
use firebrick_utils::ipc::{self, Stream};
use hyper_util::rt::TokioIo;
use tokio::time::{Instant, sleep};
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

const DAEMON_BINARY: &str = "fbkd";
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(5);
const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Connects to the daemon over its endpoint, a unix socket or a named pipe, starting the daemon
/// when needed.
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

/// Connects to the daemon's endpoint, spawning the daemon and waiting for it when it isn't
/// running. Refuses an endpoint that a process of another user serves, so `fbk` never sends
/// secrets or requests to a daemon another user planted there, for example in the shared temp
/// dir when `XDG_RUNTIME_DIR` isn't set, or under the predictable pipe name on Windows.
pub async fn ensure_daemon(socket_path: &Path) -> Result<Stream> {
    if let Some(stream) = try_connect(socket_path).await? {
        return Ok(stream);
    }

    // The socket file may be left behind by a daemon that didn't shut down cleanly.
    // The daemon refuses to start when the socket exists, so clean it up first.
    ipc::remove(socket_path)
        .with_context(|| format!("failed to remove stale socket {}", socket_path.display()))?;

    // The daemon's stderr is discarded, so report the errors it would exit with here.
    if let Some(err) = firebrick_utils::missing_dir() {
        bail!("can't start the firebrick daemon: {err}");
    }

    // The daemon outlives `fbk`, so it must not keep the pipes `fbk` was started with open.
    firebrick_utils::keep_stdio_from_children();
    let child = spawn_daemon().context("failed to start the firebrick daemon")?;

    wait_for_daemon(socket_path, child).await
}

/// Starts the daemon detached from the terminal.
#[cfg(unix)]
fn spawn_daemon() -> std::io::Result<Child> {
    daemon_command(0).spawn()
}

/// Starts the daemon detached from the terminal and, when the job object `fbk` runs in allows
/// it, outside that job. Terminals such as the ones of IDEs can run `fbk` in a job that kills
/// its processes when it closes, and microsandbox starts each VM outside the job of `fbkd`,
/// which a job without the breakaway right refuses. A job that refuses breakaway refuses it
/// to `fbkd` as well, so `fbk` then starts `fbkd` in the job.
#[cfg(windows)]
fn spawn_daemon() -> std::io::Result<Child> {
    /// `CREATE_BREAKAWAY_FROM_JOB` from the Win32 API.
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    match daemon_command(CREATE_BREAKAWAY_FROM_JOB).spawn() {
        Err(err) if err.kind() == ErrorKind::PermissionDenied => daemon_command(0).spawn(),
        spawned => spawned,
    }
}

/// Connects to the daemon, returning `None` when nothing serves the endpoint and an error when
/// another user serves it.
async fn try_connect(socket_path: &Path) -> Result<Option<Stream>> {
    match ipc::connect(socket_path).await {
        Ok(stream) => Ok(Some(stream)),
        Err(err) if err.kind() == ErrorKind::PermissionDenied => {
            bail!("refusing to use the firebrick daemon: {err}")
        }
        Err(_) => Ok(None),
    }
}

/// Waits until the spawned daemon listens on the endpoint, failing when it exits or times out.
async fn wait_for_daemon(socket_path: &Path, mut child: Child) -> Result<Stream> {
    let deadline = Instant::now() + DAEMON_START_TIMEOUT;

    loop {
        if let Some(stream) = try_connect(socket_path).await? {
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

/// Returns the command that starts the daemon detached from the terminal, without stdio. On
/// Windows it's created with the `extra_flags` too.
fn daemon_command(extra_flags: u32) -> Command {
    let mut command = Command::new(daemon_binary());
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut command, extra_flags);
    command
}

/// Starts the daemon in a process group of its own, so the terminal's signals don't reach it.
/// Unix has no creation flags, so `_extra_flags` is always 0 there.
#[cfg(unix)]
fn detach(command: &mut Command, _extra_flags: u32) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

/// Starts the daemon without a console and in a process group of its own, so it outlives the
/// console `fbk` runs in and Ctrl-C there doesn't reach it, with the `extra_flags` too.
#[cfg(windows)]
fn detach(command: &mut Command, extra_flags: u32) {
    use std::os::windows::process::CommandExt;
    /// `DETACHED_PROCESS` from the Win32 API.
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    /// `CREATE_NEW_PROCESS_GROUP` from the Win32 API.
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | extra_flags);
}

/// Returns the daemon binary next to the current executable, or falls back to looking it up on `PATH`.
fn daemon_binary() -> PathBuf {
    let file_name = format!("{DAEMON_BINARY}{}", std::env::consts::EXE_SUFFIX);
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(&file_name)))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(DAEMON_BINARY))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns an endpoint unique to this test, in the directory it returns, if any.
    fn test_endpoint(test: &str) -> (Option<tempfile::TempDir>, PathBuf) {
        if cfg!(windows) {
            let pipe = format!(r"\\.\pipe\fbk-cli-test-{}-{test}", std::process::id());
            return (None, PathBuf::from(pipe));
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fbkd.sock");
        (Some(dir), path)
    }

    /// Accepts one client on the listener.
    async fn accept_one(mut listener: ipc::Listener) -> std::io::Result<()> {
        listener.accept().await.map(drop)
    }

    #[tokio::test]
    async fn uses_an_endpoint_served_by_the_current_user() {
        let (_dir, path) = test_endpoint("served");
        let accepted = tokio::spawn(accept_one(ipc::Listener::bind(&path).unwrap()));

        ensure_daemon(&path).await.unwrap();

        accepted.await.unwrap().unwrap();
    }

    #[test]
    fn daemon_binary_has_the_platforms_executable_name() {
        let name = daemon_binary();
        let name = name.file_name().unwrap().to_string_lossy();

        assert!(name.starts_with(DAEMON_BINARY), "{name}");
    }
}
