use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use crate::api::sandbox_management_service_client::SandboxManagementServiceClient;
use crate::api::{GetVersionRequest, ShutdownRequest};
use anyhow::{Context, Result, bail};
use firebrick_utils::ipc::{self, Stream};
use hyper_util::rt::TokioIo;
use tokio::time::{Instant, sleep};
use tonic::Code;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

const DAEMON_BINARY: &str = "fbkd";
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(5);
const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// A client of the daemon's gRPC API.
type Client = SandboxManagementServiceClient<Channel>;

/// Connects to the daemon over its endpoint, a unix socket or a named pipe, starting the daemon
/// when needed. A daemon of another version than `fbk` is restarted, so `fbk` doesn't talk to
/// the daemon of an earlier install.
pub async fn connect() -> Result<Client> {
    let path =
        firebrick_utils::socket_path().context("can't name the firebrick daemon's endpoint")?;
    let lock_path = firebrick_utils::daemon_lock_path().context(firebrick_utils::NO_DATA_DIR)?;
    let client = connect_to(path.clone()).await?;
    let restarted = async move {
        wait_for_shutdown(&lock_path).await?;
        connect_to(path).await
    };

    matching_daemon(
        client,
        env!("CARGO_PKG_VERSION"),
        &daemon_binary(),
        restarted,
    )
    .await
}

/// Returns `client` when its daemon runs version `expected`. Otherwise asks the daemon to shut
/// down and returns the client from `restarted`, which connects to the daemon that replaces it,
/// when that one runs `expected`. Fails when the daemon can't shut down, or the one that
/// replaces it runs another version than `expected`, naming the `binary` it was started from.
async fn matching_daemon(
    mut client: Client,
    expected: &str,
    binary: &Path,
    restarted: impl Future<Output = Result<Client>>,
) -> Result<Client> {
    let running = daemon_version(&mut client).await?;

    if running.as_deref() == Some(expected) {
        return Ok(client);
    }

    let running = version_label(running.as_deref());
    eprintln!("note: restarting the firebrick daemon (running {running}, fbk is {expected})");
    shut_down(client, running, expected).await?;

    let mut client = restarted.await?;
    let restarted = daemon_version(&mut client).await?;

    if restarted.as_deref() != Some(expected) {
        bail!(
            "restarted the firebrick daemon but it runs version {}, not {expected}; make sure \
             the fbkd executable at {} matches fbk",
            version_label(restarted.as_deref()),
            binary.display()
        );
    }

    Ok(client)
}

/// Returns the daemon's version, or `None` for a daemon from before `GetVersion`.
async fn daemon_version(client: &mut Client) -> Result<Option<String>> {
    match client.get_version(GetVersionRequest {}).await {
        Ok(response) => Ok(Some(response.into_inner().version)),
        Err(status) if status.code() == Code::Unimplemented => Ok(None),
        Err(status) => Err(status).context("can't read the version of the firebrick daemon"),
    }
}

/// Describes a version from [`daemon_version`].
fn version_label(version: Option<&str>) -> &str {
    version.unwrap_or("an older version")
}

/// Asks the daemon, which runs version `running` instead of `expected`, to shut down. Closes
/// the client, so the daemon doesn't wait for its connection.
async fn shut_down(mut client: Client, running: &str, expected: &str) -> Result<()> {
    let Err(status) = client.shutdown(ShutdownRequest {}).await else {
        return Ok(());
    };

    match status.code() {
        Code::Unimplemented => bail!(
            "the firebrick daemon is older than fbk {expected} and can't be restarted by fbk; \
             stop the fbkd process and try again"
        ),
        Code::FailedPrecondition => bail!(
            "the firebrick daemon runs version {running} but fbk is {expected}, and it can't \
             restart while {}; close them and try again",
            status.message()
        ),
        _ => Err(status).context("failed to shut down the firebrick daemon"),
    }
}

/// Waits until the daemon that was asked to shut down has exited and released the lock at
/// `lock_path`, so the daemon that replaces it finds its endpoint, host ports and listeners
/// free.
async fn wait_for_shutdown(lock_path: &Path) -> Result<()> {
    let deadline = Instant::now() + DAEMON_START_TIMEOUT;

    while firebrick_utils::try_lock_daemon(lock_path)
        .with_context(|| format!("failed to lock {}", lock_path.display()))?
        .is_none()
    {
        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for the firebrick daemon to shut down and release {}; stop \
                 the fbkd process and try again",
                lock_path.display()
            );
        }

        sleep(DAEMON_POLL_INTERVAL).await;
    }

    Ok(())
}

/// Connects to the daemon on its endpoint, starting the daemon when needed.
async fn connect_to(path: PathBuf) -> Result<Client> {
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
        .await
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
/// to `fbkd` as well, so `fbk` then starts `fbkd` in the job and says so.
#[cfg(windows)]
fn spawn_daemon() -> std::io::Result<Child> {
    use windows_sys::Win32::System::Threading::CREATE_BREAKAWAY_FROM_JOB;

    match daemon_command(CREATE_BREAKAWAY_FROM_JOB).spawn() {
        Err(err) if err.kind() == ErrorKind::PermissionDenied => {
            eprintln!(
                "note: this terminal's job object keeps the firebrick daemon in it, so the \
                 daemon stops when the terminal closes and may not be able to start sandboxes"
            );
            daemon_command(0).spawn()
        }
        spawned => spawned,
    }
}

/// Connects to the daemon, returning `None` when nothing serves the endpoint: it doesn't exist,
/// or on Unix only a stale socket file is left. Fails on every other error, such as an endpoint
/// that another user serves or one that stays busy, instead of starting a second daemon.
async fn try_connect(socket_path: &Path) -> Result<Option<Stream>> {
    match ipc::connect(socket_path).await {
        Ok(stream) => Ok(Some(stream)),
        Err(err)
            if matches!(
                err.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(None)
        }
        Err(err) if err.kind() == ErrorKind::PermissionDenied => {
            bail!("refusing to use the firebrick daemon: {err}")
        }
        Err(err) => Err(err).with_context(|| {
            format!(
                "can't connect to the firebrick daemon at {}",
                socket_path.display()
            )
        }),
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
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | extra_flags);
}

/// Returns the daemon binary `fbk` starts: the one next to it, or else the one on `PATH`.
fn daemon_binary() -> PathBuf {
    let dir = std::env::current_exe().ok();
    daemon_binary_in(dir.as_deref().and_then(Path::parent))
}

/// Returns the daemon binary in `dir`, the directory of the `fbk` binary, or falls back to
/// looking it up on `PATH` when it isn't there.
fn daemon_binary_in(dir: Option<&Path>) -> PathBuf {
    let file_name = format!("{DAEMON_BINARY}{}", std::env::consts::EXE_SUFFIX);
    dir.map(|dir| dir.join(&file_name))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(DAEMON_BINARY))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_daemon;

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
    async fn try_connect_uses_an_endpoint_served_by_the_current_user() {
        let (_dir, path) = test_endpoint("served");
        let accepted = tokio::spawn(accept_one(ipc::Listener::bind(&path).unwrap()));

        let stream = try_connect(&path).await.unwrap();

        assert!(stream.is_some());
        accepted.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn try_connect_finds_no_daemon_at_a_missing_endpoint() {
        let (_dir, path) = test_endpoint("missing");

        assert!(try_connect(&path).await.unwrap().is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn try_connect_finds_no_daemon_at_a_stale_socket_file() {
        let (_dir, path) = test_endpoint("stale");
        drop(ipc::Listener::bind(&path).unwrap());

        assert!(path.exists());
        assert!(try_connect(&path).await.unwrap().is_none());
    }

    const FBKD: &str = "/opt/firebrick/fbkd";

    /// A restart that fails the test when `fbk` restarts the daemon.
    async fn no_restart() -> Result<Client> {
        bail!("restarted the daemon")
    }

    /// A restart to a daemon that runs `version`.
    async fn restart_to(version: &str) -> Result<Client> {
        Ok(fake_daemon::serve_version(version, 0).await)
    }

    /// Checks the version of `client` against 0.9.0, with `restarted` as the restart.
    async fn check(
        client: Client,
        restarted: impl Future<Output = Result<Client>>,
    ) -> Result<Client> {
        matching_daemon(client, "0.9.0", Path::new(FBKD), restarted).await
    }

    async fn version_of(mut client: Client) -> Option<String> {
        daemon_version(&mut client).await.unwrap()
    }

    #[tokio::test]
    async fn matching_daemon_keeps_a_daemon_of_the_same_version() {
        let client = check(fake_daemon::serve_version("0.9.0", 0).await, no_restart())
            .await
            .unwrap();

        assert_eq!(version_of(client).await.as_deref(), Some("0.9.0"));
    }

    #[tokio::test]
    async fn matching_daemon_restarts_a_daemon_of_another_version() {
        let client = check(
            fake_daemon::serve_version("0.8.0", 0).await,
            restart_to("0.9.0"),
        )
        .await
        .unwrap();

        assert_eq!(version_of(client).await.as_deref(), Some("0.9.0"));
    }

    #[tokio::test]
    async fn matching_daemon_refuses_to_restart_a_daemon_with_open_sessions() {
        let error = check(fake_daemon::serve_version("0.8.0", 2).await, no_restart())
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "the firebrick daemon runs version 0.8.0 but fbk is 0.9.0, and it can't restart \
             while 2 sessions are open; close them and try again"
        );
    }

    #[tokio::test]
    async fn matching_daemon_asks_to_stop_a_daemon_from_before_the_version_check() {
        let error = check(fake_daemon::serve_unversioned().await, no_restart())
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "the firebrick daemon is older than fbk 0.9.0 and can't be restarted by fbk; stop \
             the fbkd process and try again"
        );
    }

    #[tokio::test]
    async fn matching_daemon_fails_when_the_restarted_daemon_runs_another_version() {
        let error = check(
            fake_daemon::serve_version("0.8.0", 0).await,
            restart_to("0.8.0"),
        )
        .await
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            format!(
                "restarted the firebrick daemon but it runs version 0.8.0, not 0.9.0; make sure \
                 the fbkd executable at {} matches fbk",
                Path::new(FBKD).display()
            )
        );
    }

    /// Releases the lock after a moment, like a daemon that exits.
    async fn release_later(lock: std::fs::File) {
        sleep(Duration::from_millis(100)).await;
        drop(lock);
    }

    #[tokio::test]
    async fn wait_for_shutdown_returns_once_the_daemon_released_its_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fbkd.lock");
        let lock = firebrick_utils::try_lock_daemon(&path).unwrap().unwrap();

        let (waited, ()) = tokio::join!(wait_for_shutdown(&path), release_later(lock));

        waited.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_shutdown_times_out_while_the_daemon_holds_its_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fbkd.lock");
        let _lock = firebrick_utils::try_lock_daemon(&path).unwrap().unwrap();

        let error = wait_for_shutdown(&path).await.unwrap_err();

        assert_eq!(
            error.to_string(),
            format!(
                "timed out waiting for the firebrick daemon to shut down and release {}; stop \
                 the fbkd process and try again",
                path.display()
            )
        );
    }

    #[test]
    fn daemon_binary_is_the_one_next_to_fbk() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = dir
            .path()
            .join(format!("fbkd{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&daemon, "").unwrap();

        assert_eq!(daemon_binary_in(Some(dir.path())), daemon);
    }

    #[test]
    fn daemon_binary_falls_back_to_the_path_without_one_next_to_fbk() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(daemon_binary_in(Some(dir.path())), PathBuf::from("fbkd"));
        assert_eq!(daemon_binary_in(None), PathBuf::from("fbkd"));
    }
}
