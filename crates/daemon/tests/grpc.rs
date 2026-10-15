//! Integration tests for the gRPC handlers.
//!
//! These tests talk to a real daemon over a unix socket and use the local microsandbox
//! runtime, so they boot real VMs and pull the sandbox image. They only build with the
//! `vm-tests` feature; run them with `cargo test -p firebrick-daemon --features vm-tests`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use firebrick_daemon::api::sandbox_management_service_client::SandboxManagementServiceClient;
use firebrick_daemon::api::{
    AttachInput, AttachRequest, AttachResize, AttachResponse, AttachStart, ForwardPortRequest,
    ForwardPortResponse, GetSandboxRequest, GetSandboxResponse, ListNetworkActivityRequest,
    ListSandboxesRequest, MiseInstallProgress, Mount, NetworkActivity, NetworkActivityKind,
    NetworkPolicy, PortForward, PortForwards, RemovePortRequest, RemovePortResponse,
    RemoveSandboxRequest, RemoveSecretRequest, SandboxResources, SandboxStarted, SandboxStatus,
    SandboxUpgraded, SandboxVolumes, SetSecretRequest, StartSandboxRequest, StartSandboxResponse,
    StopSandboxRequest, UpdateNetworkRequest, UpgradeSandboxRequest, UpgradeSandboxResponse,
    attach_request, attach_response, start_sandbox_response, upgrade_sandbox_response,
};
use firebrick_daemon::clipboard::{self, Cause, Clipboards, HostClipboard, HostError, Kind};
use firebrick_daemon::open::{self, Opener};
use firebrick_daemon::secrets::{self, Secret, SecretStore};
use firebrick_daemon::{mise, netlog, sandboxes, server};
use firebrick_utils::ipc;
use hyper_util::rt::TokioIo;
use microsandbox::sandbox::{
    OwnedVolumeStorage, RootfsSource, SandboxHandle, SandboxSpec, VolumeMount, VsockSocketType,
};
use microsandbox::snapshot::{Snapshot, SnapshotHandle};
use microsandbox::{MicrosandboxError, MicrosandboxResult, Sandbox};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OnceCell, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep};
use tonic::codegen::tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tonic::{Code, Status, Streaming};
use tower::service_fn;
use tracing_subscriber::EnvFilter;

const STATUS_TIMEOUT: Duration = Duration::from_secs(120);
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const DEFAULT_SIZE: AttachResize = AttachResize {
    width: 80,
    height: 24,
};

/// Records the URLs the daemon opens instead of launching a browser.
#[derive(Default)]
struct RecordingOpener {
    urls: Mutex<Vec<String>>,
}

impl Opener for RecordingOpener {
    fn open(&self, url: &str) -> std::io::Result<()> {
        self.urls.lock().unwrap().push(url.to_string());
        Ok(())
    }
}

impl RecordingOpener {
    fn urls(&self) -> Vec<String> {
        self.urls.lock().unwrap().clone()
    }
}

/// Text on the fake host clipboard of the test daemons.
const CLIPBOARD_TEXT: &[u8] = b"hello from the host";

/// PNG on the fake host clipboard of the test daemons: the PNG signature and a few bytes that
/// aren't valid UTF-8, so a stand-in that mangles binary output fails the test.
const CLIPBOARD_PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\xff\xfe";

/// What the fake host clipboard of a test daemon holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FakeContent {
    /// [`CLIPBOARD_TEXT`] and [`CLIPBOARD_PNG`].
    Full,
    /// Nothing.
    Empty,
    /// Text and a PNG one byte over their limits, which the host clipboard hands over anyway.
    Large,
    /// It can't be read, for the cause.
    Unavailable(Cause),
}

/// A host clipboard whose content the test sets, so the tests don't need a display. It starts
/// [`FakeContent::Full`], and records what the sandbox copies.
struct FakeClipboard {
    content: Mutex<FakeContent>,
    written: Mutex<Vec<(Kind, Vec<u8>)>>,
}

impl FakeClipboard {
    fn content(&self) -> FakeContent {
        *self.content.lock().unwrap()
    }
}

#[async_trait::async_trait]
impl HostClipboard for FakeClipboard {
    async fn kinds(&self) -> Result<Vec<Kind>, HostError> {
        match self.content() {
            FakeContent::Full | FakeContent::Large => Ok(vec![Kind::Text, Kind::Png]),
            FakeContent::Empty => Ok(vec![]),
            FakeContent::Unavailable(cause) => {
                Err(HostError::Unavailable(cause, "fake".to_string()))
            }
        }
    }

    async fn read(&self, kind: Kind, _limit: usize) -> Result<Vec<u8>, HostError> {
        match (self.content(), kind) {
            (FakeContent::Full, Kind::Text) => Ok(CLIPBOARD_TEXT.to_vec()),
            (FakeContent::Full, Kind::Png) => Ok(CLIPBOARD_PNG.to_vec()),
            (FakeContent::Large, kind) => Ok(vec![b'a'; kind.limit() + 1]),
            (FakeContent::Empty, _) => Err(HostError::Empty),
            (FakeContent::Unavailable(cause), _) => {
                Err(HostError::Unavailable(cause, "fake".to_string()))
            }
        }
    }

    async fn write(&self, kind: Kind, content: Vec<u8>) -> Result<(), HostError> {
        if let FakeContent::Unavailable(cause) = self.content() {
            return Err(HostError::Unavailable(cause, "fake".to_string()));
        }

        self.written.lock().unwrap().push((kind, content));
        Ok(())
    }
}

struct TestDaemon {
    socket_path: PathBuf,
    opener: Arc<RecordingOpener>,
    clipboard: Arc<FakeClipboard>,
    shutdown: Option<oneshot::Sender<()>>,
    handle: JoinHandle<Result<(), server::ServerError>>,
}

impl TestDaemon {
    async fn start(name: &str) -> Self {
        let secrets_path = std::env::temp_dir().join(format!(
            "fbk-it-{}-{}-secrets.yml",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_file(&secrets_path);

        Self::start_with_secrets(name, SecretStore::new(secrets_path)).await
    }

    async fn start_with_secrets(name: &str, secrets: SecretStore) -> Self {
        Self::start_with(name, secrets, false).await
    }

    /// Starts a daemon that listens for the network logs and clipboards of the existing
    /// sandboxes first, like `fbkd` does when it starts.
    async fn start_restoring_listeners(name: &str) -> Self {
        let secrets_path = std::env::temp_dir().join(format!(
            "fbk-it-{}-{}-secrets.yml",
            std::process::id(),
            name
        ));

        Self::start_with(name, SecretStore::new(secrets_path), true).await
    }

    async fn start_with(name: &str, secrets: SecretStore, restore_listeners: bool) -> Self {
        init_logging();
        STALE_SWEEP.get_or_init(sweep_stale_sandboxes).await;

        let socket_path = daemon_endpoint(name);
        let _ = ipc::remove(&socket_path).await;

        let (tx, rx) = oneshot::channel();
        let opener = Arc::new(RecordingOpener::default());
        let clipboard = Arc::new(FakeClipboard {
            content: Mutex::new(FakeContent::Full),
            written: Mutex::new(Vec::new()),
        });
        let service = server::FirebrickServer::new(secrets, opener.clone(), clipboard.clone());

        if restore_listeners {
            service.restore_listeners().await;
        }

        let handle = tokio::spawn(serve_until(socket_path.clone(), service, rx));

        wait_for_socket(&socket_path).await;

        Self {
            socket_path,
            opener,
            clipboard,
            shutdown: Some(tx),
            handle,
        }
    }

    /// Sets what the daemon's fake host clipboard holds.
    fn set_clipboard(&self, content: FakeContent) {
        *self.clipboard.content.lock().unwrap() = content;
    }

    /// Returns what the sandboxes copied to the daemon's fake host clipboard, in order.
    fn copied(&self) -> Vec<(Kind, Vec<u8>)> {
        self.clipboard.written.lock().unwrap().clone()
    }

    async fn client(&self) -> SandboxManagementServiceClient<Channel> {
        let path = self.socket_path.clone();

        // The HTTP endpoint isn't used; it only shows up in the authority header.
        let channel = Endpoint::try_from("http://localhost")
            .unwrap()
            .connect_with_connector(service_fn(move |_: Uri| connect(path.clone())))
            .await
            .expect("failed to connect to test daemon");

        SandboxManagementServiceClient::new(channel)
    }

    async fn stop(mut self) {
        self.shutdown.take().unwrap().send(()).unwrap();
        self.handle
            .await
            .unwrap()
            .expect("daemon exited with an error");
    }
}

/// Sends the daemon's logs through libtest's output capture, so a failing test prints them.
///
/// The tests use the current-thread runtime, so the daemon logs on the test's own thread and
/// its lines end up under the right test. Only the first call installs the subscriber.
fn init_logging() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .try_init();
}

/// Serves the daemon on the socket until `stop` fires.
async fn serve_until(
    path: PathBuf,
    service: server::FirebrickServer,
    stop: oneshot::Receiver<()>,
) -> Result<(), server::ServerError> {
    server::serve(&path, service, async {
        let _ = stop.await;
    })
    .await
}

/// Connects to the daemon socket for a gRPC channel.
async fn connect(path: PathBuf) -> std::io::Result<TokioIo<ipc::Stream>> {
    Ok(TokioIo::new(ipc::connect(&path).await?))
}

async fn wait_for_socket(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);

    while !ipc::is_served(path).await {
        assert!(Instant::now() < deadline, "daemon did not start listening");
        sleep(Duration::from_millis(10)).await;
    }
}

/// Prefix of every sandbox the tests create, followed by the id of the test process.
const SANDBOX_PREFIX: &str = "fbk-it-";

/// Runs the sweep of sandboxes left behind by killed test runs once per test process.
static STALE_SWEEP: OnceCell<()> = OnceCell::const_new();

/// Returns the name of a test sandbox: `fbk-it-<pid>-<test>`. The process id keeps concurrent
/// test runs that share the microsandbox home from touching each other's sandboxes.
fn sandbox_name(test: &str) -> String {
    format!("{SANDBOX_PREFIX}{}-{test}", std::process::id())
}

/// Start of the names of the probe sandboxes the daemon pulls images with for an upgrade,
/// followed by the id of the process that created them.
const PROBE_PREFIX: &str = "fbk-upgrade-";

/// Returns the id of the test process that created the sandbox or upgrade probe, if the name
/// has one.
fn sandbox_pid(name: &str) -> Option<u32> {
    let rest = name
        .strip_prefix(SANDBOX_PREFIX)
        .or_else(|| name.strip_prefix(PROBE_PREFIX))?;
    let (pid, _) = rest.split_once('-')?;
    pid.parse().ok()
}

/// Returns the endpoint of the test daemon `name`: a socket in the temp directory, or a pipe on
/// Windows.
fn daemon_endpoint(name: &str) -> PathBuf {
    let file = format!("fbk-it-{}-{}", std::process::id(), name);
    if cfg!(windows) {
        return PathBuf::from(format!(r"\\.\pipe\{file}"));
    }
    std::env::temp_dir().join(format!("{file}.sock"))
}

/// Whether a process with the id still runs. `kill -0` works on both Linux and macOS.
#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Whether a process with the id still runs, by whether `tasklist` lists it. Its CSV quotes the
/// PID, which no message for a missing process does, in any language.
#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .stderr(std::process::Stdio::null())
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\"")))
}

/// Removes the sandboxes, and their host directories, of test processes that no longer run.
///
/// Panics when the microsandbox home can't be listed, because no test can run against it.
async fn sweep_stale_sandboxes() {
    let all = sandboxes::list_all()
        .await
        .unwrap_or_else(|err| panic_unusable_home(err));

    for sb in all.iter().filter(|sb| is_stale(sb.name())) {
        remove_stale_sandbox(sb).await;
    }

    remove_stale_host_files();
}

/// Whether the name belongs to a test sandbox, or its host files, of a process that no longer
/// runs. Names without a process id are never stale.
fn is_stale(name: &str) -> bool {
    sandbox_pid(name).is_some_and(|pid| !process_is_alive(pid))
}

fn panic_unusable_home(err: impl std::fmt::Display) -> ! {
    let home = microsandbox::config::config()
        .map(|config| config.home().display().to_string())
        .unwrap_or_else(|_| "$MSB_HOME".to_string());

    panic!("microsandbox home {home} is unusable: {err}; remove it and run the tests again")
}

/// Kills and removes a sandbox of a dead test process. A failure is logged, so one broken
/// leftover doesn't fail the run.
async fn remove_stale_sandbox(sb: &SandboxHandle) {
    let removed = match sb.kill().await {
        Ok(()) => sb.remove().await,
        Err(err) => Err(err),
    };

    if let Err(err) = removed {
        tracing::warn!("failed to remove stale test sandbox {}: {err}", sb.name());
    }
}

/// Removes the workspaces, mount directories, sockets and secrets files that dead test
/// processes left in the temp directory, and the network log sockets and records and the
/// clipboard sockets of their sandboxes. They're all named `fbk-it-<pid>-...`, also when the
/// process was killed before it created the sandbox.
fn remove_stale_host_files() {
    let dirs = [
        Some(std::env::temp_dir()),
        firebrick_utils::netlog_socket_dir().ok().flatten(),
        firebrick_utils::network_log_dir(),
        firebrick_utils::clipboard_socket_dir().ok().flatten(),
    ];

    for dir in dirs.iter().flatten() {
        remove_stale_files_in(dir);
    }
}

/// Removes the files and directories of dead test processes in the directory.
fn remove_stale_files_in(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        if is_stale(&entry.file_name().to_string_lossy()) {
            let path = entry.path();
            let _ = std::fs::remove_dir_all(&path).or_else(|_| std::fs::remove_file(&path));
        }
    }
}

/// Stops and removes a sandbox, its workspace, its network log and its clipboard socket,
/// including ones left over from an earlier (failed) run.
async fn remove_sandbox(name: &str) {
    if let Ok(sb) = Sandbox::get(name).await {
        let _ = sb.stop().await;
        let _ = sb.remove().await;
    }

    let _ = std::fs::remove_dir_all(workspace_path(name));
    let _ = ipc::remove(&netlog_socket(name)).await;
    let _ = std::fs::remove_file(netlog_record(name));
    let _ = ipc::remove(&clipboard_socket(name)).await;
}

async fn sandbox_status(
    client: &mut SandboxManagementServiceClient<Channel>,
    name: &str,
) -> Option<SandboxStatus> {
    client
        .list_sandboxes(ListSandboxesRequest {})
        .await
        .expect("list_sandboxes failed")
        .into_inner()
        .sandboxes
        .into_iter()
        .find(|sb| sb.name == name)
        .map(|sb| SandboxStatus::try_from(sb.status).expect("invalid sandbox status"))
}

async fn wait_for_status(
    client: &mut SandboxManagementServiceClient<Channel>,
    name: &str,
    expected: SandboxStatus,
) {
    let deadline = Instant::now() + STATUS_TIMEOUT;

    loop {
        let status = sandbox_status(client, name).await;

        if status == Some(expected) {
            return;
        }

        assert!(
            Instant::now() < deadline,
            "sandbox {name} did not reach {expected:?}, last status: {status:?}"
        );

        sleep(POLL_INTERVAL).await;
    }
}

/// Returns the resolved temp directory for the host directories the tests mount. On macOS the
/// temp directory goes through the `/var` symlink, and microsandbox refuses to bind mount a
/// host path that goes through a symlink.
fn test_temp_dir() -> PathBuf {
    dunce::canonicalize(std::env::temp_dir()).expect("failed to resolve the temp directory")
}

/// Returns the host directory used as the workspace of a test sandbox. It's named after the
/// sandbox, so `remove_sandbox` and the sweep of stale sandboxes can find it again.
fn workspace_path(name: &str) -> PathBuf {
    test_temp_dir().join(format!("{name}-ws"))
}

/// Creates an empty host directory to mount as the workspace of a test sandbox.
fn test_workspace(name: &str) -> PathBuf {
    let workspace = workspace_path(name);
    let _ = std::fs::remove_dir_all(&workspace);
    std::fs::create_dir_all(&workspace).expect("failed to create test workspace");

    workspace
}

/// Returns the path the daemon mounts a workspace at in the guest.
fn guest_workspace(workspace: &Path) -> String {
    let leaf = workspace.file_name().unwrap().to_string_lossy();

    format!("/workspaces/{leaf}")
}

/// Image the tests boot, independent of the default image.
const TEST_IMAGE: &str = "ubuntu:26.04";

fn start_request(name: &str) -> StartSandboxRequest {
    StartSandboxRequest {
        name: name.to_string(),
        workspace: test_workspace(name).to_string_lossy().into_owned(),
        // The default image is only published on release, so it doesn't exist for unreleased versions.
        image: TEST_IMAGE.to_string(),
        // The test image has no /sbin/init.
        init: Some(false),
        ..Default::default()
    }
}

/// Returns the sandbox as the daemon reports it.
async fn get_sandbox(
    client: &mut SandboxManagementServiceClient<Channel>,
    name: &str,
) -> GetSandboxResponse {
    client
        .get_sandbox(GetSandboxRequest {
            name: name.to_string(),
        })
        .await
        .expect("failed to get sandbox")
        .into_inner()
}

/// Stops the sandbox and waits until it has stopped.
async fn stop_sandbox(client: &mut SandboxManagementServiceClient<Channel>, name: &str) {
    client
        .stop_sandbox(StopSandboxRequest {
            name: name.to_string(),
        })
        .await
        .expect("failed to stop sandbox");
    wait_for_status(client, name, SandboxStatus::Stopped).await;
}

/// Returns the configuration microsandbox stores for the sandbox.
async fn sandbox_spec(name: &str) -> SandboxSpec {
    Sandbox::get(name)
        .await
        .expect("failed to get sandbox")
        .config()
        .expect("failed to read sandbox config")
        .spec
}

/// Returns the capacity in MiB of the sandbox-owned disk mounted at `/var/lib/docker`.
fn docker_disk_mib(spec: &SandboxSpec) -> Option<u32> {
    spec.mounts.iter().find_map(|mount| match mount {
        VolumeMount::Owned {
            guest,
            storage: OwnedVolumeStorage::Disk { capacity_mib },
            ..
        } if guest == "/var/lib/docker" => Some(*capacity_mib),
        _ => None,
    })
}

/// Returns the host directory that holds the sandbox's owned volumes, such as its Docker disk.
fn owned_volumes_dir(name: &str) -> PathBuf {
    microsandbox::config::config()
        .expect("failed to read microsandbox config")
        .sandboxes_dir()
        .join(name)
        .join("owned-volumes")
}

#[test]
fn sandbox_pid_parses_the_pid_of_test_sandboxes_only() {
    assert_eq!(sandbox_pid("fbk-it-48211-get"), Some(48211));
    assert_eq!(sandbox_pid(&sandbox_name("get")), Some(std::process::id()));
    assert_eq!(sandbox_pid("fbk-it-does-not-exist"), None);
    assert_eq!(sandbox_pid("other-48211-get"), None);
}

#[test]
fn process_is_alive_tells_live_from_dead_processes() {
    let (program, args): (&str, &[&str]) = if cfg!(windows) {
        ("cmd", &["/C", "exit"])
    } else {
        ("true", &[])
    };
    let mut child = std::process::Command::new(program)
        .args(args)
        .spawn()
        .unwrap();
    let pid = child.id();
    child.wait().unwrap();

    assert!(process_is_alive(std::process::id()));
    assert!(!process_is_alive(pid));
}

#[tokio::test]
async fn list_sandboxes_succeeds() {
    let daemon = TestDaemon::start("list").await;
    let mut client = daemon.client().await;

    client
        .list_sandboxes(ListSandboxesRequest {})
        .await
        .expect("list_sandboxes failed");

    daemon.stop().await;
}

#[tokio::test]
async fn stop_sandbox_returns_not_found_for_unknown_sandbox() {
    let daemon = TestDaemon::start("stop-unknown").await;
    let mut client = daemon.client().await;

    let status = client
        .stop_sandbox(StopSandboxRequest {
            name: "fbk-it-does-not-exist".to_string(),
        })
        .await
        .expect_err("stopping an unknown sandbox should fail");

    assert_eq!(status.code(), Code::NotFound);

    daemon.stop().await;
}

#[tokio::test]
async fn get_sandbox_returns_not_found_for_unknown_sandbox() {
    let daemon = TestDaemon::start("get-unknown").await;
    let mut client = daemon.client().await;

    let status = client
        .get_sandbox(GetSandboxRequest {
            name: "fbk-it-does-not-exist".to_string(),
        })
        .await
        .expect_err("getting an unknown sandbox should fail");

    assert_eq!(status.code(), Code::NotFound);

    daemon.stop().await;
}

#[tokio::test]
async fn get_sandbox_returns_name_status_workspace_path_and_image() {
    let name: &str = &sandbox_name("get");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("get").await;
    let mut client = daemon.client().await;

    start_running_sandbox(&mut client, name).await;

    let sandbox = get_sandbox(&mut client, name).await;

    assert_eq!(sandbox.name, name);
    assert_eq!(sandbox.status(), SandboxStatus::Running);
    assert_eq!(
        sandbox.workspace_path,
        guest_workspace(&workspace_path(name))
    );
    assert_eq!(
        Path::new(&sandbox.workspace_host_path),
        dunce::canonicalize(workspace_path(name)).unwrap()
    );
    assert_eq!(
        sandbox_spec(name).await.labels.get(sandboxes::IMAGE_LABEL),
        Some(&TEST_IMAGE.to_string())
    );
    assert_eq!(sandbox.image, TEST_IMAGE);
    // The test image isn't a firebrick-base image, so it's never outdated.
    assert_eq!(sandbox.upgrade_image, "");

    stop_sandbox(&mut client, name).await;

    assert_eq!(
        get_sandbox(&mut client, name).await.status(),
        SandboxStatus::Stopped
    );

    daemon.stop().await;
    remove_sandbox(name).await;
}

#[tokio::test]
async fn sandbox_lifecycle() {
    let name: &str = &sandbox_name("lifecycle");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("lifecycle").await;
    let mut client = daemon.client().await;

    // Starting an unknown sandbox creates it.
    start(&mut client, start_request(name))
        .await
        .expect("failed to create sandbox");
    wait_for_status(&mut client, name, SandboxStatus::Running).await;

    client
        .stop_sandbox(StopSandboxRequest {
            name: name.to_string(),
        })
        .await
        .expect("failed to stop sandbox");
    wait_for_status(&mut client, name, SandboxStatus::Stopped).await;

    // Starting an existing sandbox restarts it.
    start(&mut client, start_request(name))
        .await
        .expect("failed to restart sandbox");
    wait_for_status(&mut client, name, SandboxStatus::Running).await;

    daemon.stop().await;
    remove_sandbox(name).await;
}

#[tokio::test]
async fn start_sandbox_is_a_no_op_for_running_sandbox() {
    let name: &str = &sandbox_name("start-twice");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("start-twice").await;
    let mut client = daemon.client().await;

    let request = start_request(name);
    start_and_wait(&mut client, request.clone()).await;

    start(&mut client, request)
        .await
        .expect("failed to start running sandbox");

    assert_eq!(
        get_sandbox(&mut client, name).await.status(),
        SandboxStatus::Running
    );

    daemon.stop().await;
    remove_sandbox(name).await;
}

#[tokio::test]
async fn start_sandbox_sends_no_pull_progress_for_cached_image_or_existing_sandbox() {
    let first: &str = &sandbox_name("pull-cached-1");
    let second: &str = &sandbox_name("pull-cached-2");
    remove_sandbox(first).await;
    remove_sandbox(second).await;

    let daemon = TestDaemon::start("pull-cached").await;
    let mut client = daemon.client().await;

    // Creating the first sandbox caches the image, if it wasn't already.
    start_and_wait(&mut client, start_request(first)).await;

    let (created_progress, _) = start_stream(&mut client, start_request(second))
        .await
        .expect("failed to create sandbox from cached image");
    stop_sandbox(&mut client, first).await;
    let (restarted_progress, _) = start_stream(&mut client, start_request(first))
        .await
        .expect("failed to restart sandbox");

    assert_eq!(created_progress, vec![]);
    assert_eq!(restarted_progress, vec![]);

    daemon.stop().await;
    remove_sandbox(first).await;
    remove_sandbox(second).await;
}

#[tokio::test]
async fn concurrent_starts_of_stopped_sandbox_both_succeed() {
    let name: &str = &sandbox_name("start-race");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("start-race").await;
    let mut client = daemon.client().await;

    let request = start_request(name);
    start_and_wait(&mut client, request.clone()).await;
    stop_sandbox(&mut client, name).await;

    let mut other_client = client.clone();
    let (first, second) = tokio::join!(
        start(&mut client, request.clone()),
        start(&mut other_client, request),
    );

    first.expect("first start failed");
    second.expect("second start failed");
    wait_for_status(&mut client, name, SandboxStatus::Running).await;

    daemon.stop().await;
    remove_sandbox(name).await;
}

#[tokio::test]
async fn start_sandbox_uses_requested_image_and_resources() {
    let name: &str = &sandbox_name("resources");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("resources").await;
    let mut client = daemon.client().await;

    let request = StartSandboxRequest {
        image: "alpine:3.22".to_string(),
        resources: Some(SandboxResources {
            cpu: 1,
            memory: "1 GiB".to_string(),
        }),
        volumes: Some(SandboxVolumes {
            docker: "1 GiB".to_string(),
        }),
        ..start_request(name)
    };
    start_and_wait(&mut client, request).await;

    let spec = sandbox_spec(name).await;

    assert!(
        matches!(&spec.image, RootfsSource::Oci(oci) if oci.reference.contains("alpine")),
        "unexpected image: {:?}",
        spec.image
    );
    assert_eq!(spec.resources.cpus, 1);
    assert_eq!(spec.resources.memory_mib, 1024);
    assert_eq!(docker_disk_mib(&spec), Some(1024));
    assert!(spec.init.is_none(), "unexpected init: {:?}", spec.init);

    daemon.stop().await;
    remove_sandbox(name).await;
}

#[tokio::test]
async fn sandbox_gets_default_docker_disk_that_survives_restart() {
    let name: &str = &sandbox_name("docker-disk");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("docker-disk").await;
    let mut client = daemon.client().await;
    // The test request has no resources or volumes and init disabled, like a non-firebrick image.
    start_running_sandbox(&mut client, name).await;

    assert_eq!(docker_disk_mib(&sandbox_spec(name).await), Some(20 * 1024));

    let script = "mount | grep ' /var/lib/docker '; echo kept > /var/lib/docker/marker";
    let (output, code) = run_command(&mut client, name, "sh", &["-c", script]).await;
    assert_eq!(code, 0, "unexpected output: {output:?}");
    assert!(output.contains("type ext4"), "unexpected mount: {output:?}");

    restart_sandbox(&mut client, name).await;

    let (output, code) = run_command(&mut client, name, "cat", &["/var/lib/docker/marker"]).await;
    assert_eq!(code, 0, "unexpected output: {output:?}");
    assert!(output.contains("kept"), "marker was lost: {output:?}");

    daemon.stop().await;
    remove_sandbox(name).await;
}

#[tokio::test]
async fn remove_sandbox_removes_docker_disk() {
    let name: &str = &sandbox_name("docker-disk-rm");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("docker-disk-rm").await;
    let mut client = daemon.client().await;
    start_running_sandbox(&mut client, name).await;
    stop_sandbox(&mut client, name).await;

    let disk_dir = owned_volumes_dir(name);
    assert!(disk_dir.exists(), "no disk at {}", disk_dir.display());

    client
        .remove_sandbox(RemoveSandboxRequest {
            name: name.to_string(),
            ..Default::default()
        })
        .await
        .expect("failed to remove sandbox");

    assert!(!disk_dir.exists(), "disk kept at {}", disk_dir.display());

    daemon.stop().await;
    remove_sandbox(name).await;
}

#[tokio::test]
async fn start_sandbox_with_init_explains_missing_init() {
    let name: &str = &sandbox_name("missing-init");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("missing-init").await;
    let mut client = daemon.client().await;

    let status = start(
        &mut client,
        StartSandboxRequest {
            init: None,
            ..start_request(name)
        },
    )
    .await
    .expect_err("an image without /sbin/init should fail to boot with init");

    assert_eq!(status.code(), Code::FailedPrecondition);
    assert!(
        status.message().contains("init: false"),
        "{}",
        status.message()
    );
    assert!(Sandbox::get(name).await.is_err(), "failed sandbox was kept");

    daemon.stop().await;
    remove_sandbox(name).await;
}

#[tokio::test]
async fn start_sandbox_with_unknown_image_tag_names_the_image() {
    let name: &str = &sandbox_name("unknown-image");
    const IMAGE: &str = "alpine:0.0.404";
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("unknown-image").await;
    let mut client = daemon.client().await;

    let status = start(
        &mut client,
        StartSandboxRequest {
            image: IMAGE.to_string(),
            ..start_request(name)
        },
    )
    .await
    .expect_err("an image tag that doesn't exist can't be pulled");

    assert_eq!(status.code(), Code::NotFound, "{}", status.message());
    assert_eq!(
        status.message(),
        format!("failed to create sandbox: image {IMAGE} doesn't exist")
    );
    assert!(Sandbox::get(name).await.is_err(), "failed sandbox was kept");

    daemon.stop().await;
    remove_sandbox(name).await;
}

#[tokio::test]
async fn start_sandbox_rejects_invalid_resources() {
    let name: &str = &sandbox_name("bad-resources");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("bad-resources").await;
    let mut client = daemon.client().await;

    for (memory, docker) in [("lots", ""), ("1 GiB", "20 GB")] {
        let status = start(
            &mut client,
            StartSandboxRequest {
                resources: Some(SandboxResources {
                    cpu: 2,
                    memory: memory.to_string(),
                }),
                volumes: Some(SandboxVolumes {
                    docker: docker.to_string(),
                }),
                ..start_request(name)
            },
        )
        .await
        .expect_err("start_sandbox should reject invalid resources");

        assert_eq!(
            status.code(),
            Code::InvalidArgument,
            "{memory:?}, {docker:?}"
        );
        assert!(Sandbox::get(name).await.is_err(), "sandbox was created");
    }

    daemon.stop().await;
    remove_sandbox(name).await;
}

#[tokio::test]
async fn remove_sandbox_returns_not_found_for_unknown_sandbox() {
    let daemon = TestDaemon::start("remove-unknown").await;
    let mut client = daemon.client().await;

    let status = client
        .remove_sandbox(RemoveSandboxRequest {
            name: "fbk-it-does-not-exist".to_string(),
            force: false,
        })
        .await
        .expect_err("removing an unknown sandbox should fail");

    assert_eq!(status.code(), Code::NotFound);

    daemon.stop().await;
}

#[tokio::test]
async fn remove_sandbox_removes_stopped_sandbox() {
    let name: &str = &sandbox_name("remove");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("remove").await;
    let mut client = daemon.client().await;

    start(&mut client, start_request(name))
        .await
        .expect("failed to create sandbox");
    wait_for_status(&mut client, name, SandboxStatus::Running).await;

    client
        .stop_sandbox(StopSandboxRequest {
            name: name.to_string(),
        })
        .await
        .expect("failed to stop sandbox");
    wait_for_status(&mut client, name, SandboxStatus::Stopped).await;

    client
        .remove_sandbox(RemoveSandboxRequest {
            name: name.to_string(),
            force: false,
        })
        .await
        .expect("failed to remove sandbox");

    assert_eq!(sandbox_status(&mut client, name).await, None);

    daemon.stop().await;
    remove_sandbox(name).await;
}

#[tokio::test]
async fn remove_sandbox_refuses_running_sandbox() {
    let name: &str = &sandbox_name("remove-running");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("remove-running").await;
    let mut client = daemon.client().await;
    start_running_sandbox(&mut client, name).await;

    let status = client
        .remove_sandbox(RemoveSandboxRequest {
            name: name.to_string(),
            force: false,
        })
        .await
        .expect_err("removing a running sandbox without force should fail");

    assert_eq!(status.code(), Code::FailedPrecondition, "{status:?}");
    assert!(status.message().contains("running"), "{status:?}");
    assert_eq!(
        sandbox_status(&mut client, name).await,
        Some(SandboxStatus::Running)
    );

    daemon.stop().await;
    remove_sandbox(name).await;
}

#[tokio::test]
async fn remove_sandbox_with_force_stops_and_removes_running_sandbox() {
    let name: &str = &sandbox_name("remove-force");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("remove-force").await;
    let mut client = daemon.client().await;
    start_running_sandbox(&mut client, name).await;

    client
        .remove_sandbox(RemoveSandboxRequest {
            name: name.to_string(),
            force: true,
        })
        .await
        .expect("failed to force-remove running sandbox");

    let status = client
        .get_sandbox(GetSandboxRequest {
            name: name.to_string(),
        })
        .await
        .expect_err("the removed sandbox should be gone");
    assert_eq!(status.code(), Code::NotFound);

    daemon.stop().await;
    remove_sandbox(name).await;
}

#[tokio::test]
async fn remove_sandbox_with_force_removes_stopped_sandbox() {
    let name: &str = &sandbox_name("remove-force-stopped");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("remove-force-stopped").await;
    let mut client = daemon.client().await;
    start_running_sandbox(&mut client, name).await;
    stop_sandbox(&mut client, name).await;

    client
        .remove_sandbox(RemoveSandboxRequest {
            name: name.to_string(),
            force: true,
        })
        .await
        .expect("failed to force-remove stopped sandbox");

    assert_eq!(sandbox_status(&mut client, name).await, None);

    daemon.stop().await;
    remove_sandbox(name).await;
}

#[tokio::test]
async fn stop_or_kill_kills_sandbox_that_misses_the_timeout() {
    let name: &str = &sandbox_name("stop-or-kill");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("stop-or-kill").await;
    let mut client = daemon.client().await;
    start_running_sandbox(&mut client, name).await;

    // A zero budget makes the graceful stop time out before it asks the guest to shut down,
    // so only the kill can stop the sandbox.
    let sb = Sandbox::get(name).await.expect("failed to get sandbox");
    sandboxes::stop_or_kill(&sb, Duration::ZERO)
        .await
        .expect("failed to stop or kill sandbox");

    assert_eq!(
        sandbox_status(&mut client, name).await,
        Some(SandboxStatus::Stopped)
    );

    daemon.stop().await;
    remove_sandbox(name).await;
}

fn attach_start(name: &str, command: &str, args: &[&str], size: AttachResize) -> AttachRequest {
    AttachRequest {
        message: Some(attach_request::Message::Start(AttachStart {
            name: name.to_string(),
            command: command.to_string(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            size: Some(size),
        })),
    }
}

fn attach_input(data: &[u8]) -> AttachRequest {
    AttachRequest {
        message: Some(attach_request::Message::Input(AttachInput {
            data: data.to_vec(),
        })),
    }
}

fn attach_resize(width: u32, height: u32) -> AttachRequest {
    AttachRequest {
        message: Some(attach_request::Message::Resize(AttachResize {
            width,
            height,
        })),
    }
}

/// Collects session output until the session reports its exit code.
async fn collect_session(mut responses: Streaming<AttachResponse>) -> (String, i32) {
    let mut output = Vec::new();

    loop {
        let response = tokio::time::timeout(STATUS_TIMEOUT, responses.message())
            .await
            .expect("session did not finish in time")
            .expect("attach stream failed")
            .expect("attach stream closed before the session exited");

        match response.message {
            Some(attach_response::Message::Output(data)) => output.extend(data),
            Some(attach_response::Message::ExitCode(code)) => {
                return (String::from_utf8_lossy(&output).into_owned(), code);
            }
            None => {}
        }
    }
}

/// Sends the start message and returns the sender for further messages and the session output.
async fn open_session(
    client: &mut SandboxManagementServiceClient<Channel>,
    start: AttachRequest,
) -> (mpsc::Sender<AttachRequest>, Streaming<AttachResponse>) {
    let (tx, rx) = mpsc::channel(4);
    tx.send(start).await.unwrap();

    let responses = client
        .attach(ReceiverStream::new(rx))
        .await
        .expect("attach failed")
        .into_inner();

    (tx, responses)
}

/// Runs a command in the sandbox without input and returns its output and exit code.
async fn run_command(
    client: &mut SandboxManagementServiceClient<Channel>,
    sandbox: &str,
    command: &str,
    args: &[&str],
) -> (String, i32) {
    let (tx, responses) =
        open_session(client, attach_start(sandbox, command, args, DEFAULT_SIZE)).await;
    let result = collect_session(responses).await;

    // Closing the input ends the session, so keep it open until the session has exited.
    drop(tx);

    result
}

async fn start_running_sandbox(client: &mut SandboxManagementServiceClient<Channel>, name: &str) {
    start_and_wait(client, start_request(name)).await;
}

/// Sends the start request and reads its stream to the end. Returns the progress messages and
/// the started sandbox, or the status the stream ended with. Fails when a message follows the
/// started sandbox.
async fn start_stream(
    client: &mut SandboxManagementServiceClient<Channel>,
    request: StartSandboxRequest,
) -> Result<(Vec<start_sandbox_response::Message>, SandboxStarted), Status> {
    let mut stream = client.start_sandbox(request).await?.into_inner();
    let mut progress = vec![];

    while let Some(StartSandboxResponse { message }) = stream.message().await? {
        match message.expect("empty start response") {
            start_sandbox_response::Message::Started(started) => {
                let rest = stream.message().await?;
                assert_eq!(rest, None, "the stream continued after the start");
                return Ok((progress, started));
            }
            message => progress.push(message),
        }
    }

    panic!("the start stream ended without starting the sandbox");
}

/// Sends the start request and returns the started sandbox, or the status the stream ended
/// with.
async fn start(
    client: &mut SandboxManagementServiceClient<Channel>,
    request: StartSandboxRequest,
) -> Result<SandboxStarted, Status> {
    start_stream(client, request)
        .await
        .map(|(_, started)| started)
}

/// Starts a sandbox with the request and waits until it runs.
async fn start_and_wait(
    client: &mut SandboxManagementServiceClient<Channel>,
    request: StartSandboxRequest,
) {
    let name = request.name.clone();

    start(client, request)
        .await
        .expect("failed to create sandbox");
    wait_for_status(client, &name, SandboxStatus::Running).await;
}

#[tokio::test]
async fn attach_rejects_missing_start() {
    let daemon = TestDaemon::start("attach-no-start").await;
    let mut client = daemon.client().await;

    let (tx, rx) = mpsc::channel(4);
    tx.send(attach_input(b"hi\n")).await.unwrap();

    let status = client
        .attach(ReceiverStream::new(rx))
        .await
        .expect_err("attach without start should fail");

    assert_eq!(status.code(), Code::InvalidArgument);

    daemon.stop().await;
}

#[tokio::test]
async fn attach_returns_not_found_for_unknown_sandbox() {
    let daemon = TestDaemon::start("attach-unknown").await;
    let mut client = daemon.client().await;

    let (tx, rx) = mpsc::channel(4);
    tx.send(attach_start(
        "fbk-it-does-not-exist",
        "sh",
        &[],
        DEFAULT_SIZE,
    ))
    .await
    .unwrap();

    let status = client
        .attach(ReceiverStream::new(rx))
        .await
        .expect_err("attaching to an unknown sandbox should fail");

    assert_eq!(status.code(), Code::NotFound);

    daemon.stop().await;
}

#[tokio::test]
async fn attach_runs_command_and_streams_output() {
    let name: &str = &sandbox_name("attach");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("attach").await;
    let mut client = daemon.client().await;
    start_running_sandbox(&mut client, name).await;

    let start = attach_start(name, "sh", &["-c", "read x; echo got:$x"], DEFAULT_SIZE);
    let (tx, responses) = open_session(&mut client, start).await;
    tx.send(attach_input(b"hi\n")).await.unwrap();

    let (output, code) = collect_session(responses).await;
    drop(tx);

    assert!(output.contains("got:hi"), "unexpected output: {output:?}");
    assert_eq!(code, 0);

    daemon.stop().await;
    remove_sandbox(name).await;
}

#[tokio::test]
async fn attach_uses_generic_terminal_type() {
    let name: &str = &sandbox_name("attach-term");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("attach-term").await;
    let mut client = daemon.client().await;
    start_running_sandbox(&mut client, name).await;

    // Without an explicit TERM, the session would get the daemon's own, which the guest may
    // not have a terminfo entry for (for example `xterm-ghostty`).
    let (output, code) = run_print_env(&mut client, name, "TERM").await;

    assert_eq!(output.trim(), "xterm-256color");
    assert_eq!(code, 0);

    daemon.stop().await;
    remove_sandbox(name).await;
}

#[tokio::test]
async fn attach_applies_window_size_and_resize() {
    let name: &str = &sandbox_name("attach-resize");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("attach-resize").await;
    let mut client = daemon.client().await;
    start_running_sandbox(&mut client, name).await;

    let start = attach_start(
        name,
        "sh",
        &["-c", "stty size; read x; stty size"],
        AttachResize {
            width: 100,
            height: 40,
        },
    );
    let (tx, responses) = open_session(&mut client, start).await;

    // Resize before releasing the `read` so the second `stty size` sees the new size.
    sleep(Duration::from_millis(500)).await;
    tx.send(attach_resize(120, 50)).await.unwrap();
    sleep(Duration::from_millis(200)).await;
    tx.send(attach_input(b"\n")).await.unwrap();

    let (output, code) = collect_session(responses).await;
    drop(tx);

    assert!(
        output.contains("40 100"),
        "initial size not applied: {output:?}"
    );
    assert!(output.contains("50 120"), "resize not applied: {output:?}");
    assert_eq!(code, 0);

    daemon.stop().await;
    remove_sandbox(name).await;
}

/// Waits until no process with the name runs in the sandbox.
async fn wait_for_process_exit(sandbox: &str, process: &str) {
    let sb = Sandbox::get(sandbox)
        .await
        .expect("sandbox disappeared")
        .connect()
        .await
        .expect("failed to connect to sandbox");
    let deadline = Instant::now() + Duration::from_secs(10);
    let check = format!("pgrep -x {process} || true");

    while !process_output(&sb, &check).await.is_empty() {
        assert!(Instant::now() < deadline, "{process} is still running");
        sleep(POLL_INTERVAL).await;
    }
}

/// Runs a shell command in the sandbox and returns its standard output.
async fn process_output(sb: &Sandbox, command: &str) -> Vec<u8> {
    sb.exec("sh", ["-c", command])
        .await
        .expect("failed to check for session process")
        .stdout_bytes()
        .to_vec()
}

#[tokio::test]
async fn attach_disconnect_ends_session() {
    let name: &str = &sandbox_name("attach-disconnect");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("attach-disconnect").await;
    let mut client = daemon.client().await;
    start_running_sandbox(&mut client, name).await;

    let start = attach_start(name, "sleep", &["300"], DEFAULT_SIZE);
    let (tx, responses) = open_session(&mut client, start).await;

    sleep(Duration::from_millis(500)).await;
    drop(tx);
    drop(responses);

    wait_for_process_exit(name, "sleep").await;

    daemon.stop().await;
    remove_sandbox(name).await;
}

#[tokio::test]
async fn start_sandbox_rejects_relative_workspace() {
    let name: &str = &sandbox_name("relative-workspace");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("relative-workspace").await;
    let mut client = daemon.client().await;

    let status = start(
        &mut client,
        StartSandboxRequest {
            name: name.to_string(),
            workspace: "project".to_string(),
            ..Default::default()
        },
    )
    .await
    .expect_err("a relative workspace should be rejected");

    assert_eq!(status.code(), Code::InvalidArgument);

    daemon.stop().await;
}

#[tokio::test]
async fn workspace_is_mounted_read_write() {
    let name: &str = &sandbox_name("workspace");
    remove_sandbox(name).await;

    let request = start_request(name);
    let workspace = PathBuf::from(&request.workspace);
    let guest_path = guest_workspace(&workspace);
    std::fs::write(workspace.join("from-host.txt"), "hello from host").unwrap();

    let daemon = TestDaemon::start("workspace").await;
    let mut client = daemon.client().await;
    start_and_wait(&mut client, request).await;

    let script = format!(
        "pwd; cat {guest_path}/from-host.txt; echo; echo hello from guest > {guest_path}/from-guest.txt"
    );
    let (output, code) = run_command(&mut client, name, "sh", &["-c", &script]).await;

    assert_eq!(code, 0, "unexpected output: {output:?}");
    assert!(
        output.contains(&guest_path),
        "session didn't start in the workspace: {output:?}"
    );
    assert!(
        output.contains("hello from host"),
        "unexpected output: {output:?}"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("from-guest.txt")).unwrap(),
        "hello from guest\n"
    );

    daemon.stop().await;
    remove_sandbox(name).await;
}

#[tokio::test]
async fn workspace_is_owned_by_agent_user() {
    let name: &str = &sandbox_name("workspace-owner");
    remove_sandbox(name).await;

    let request = start_request(name);
    let workspace = PathBuf::from(&request.workspace);
    let guest_path = guest_workspace(&workspace);
    std::fs::write(workspace.join("from-host.txt"), "hello from host").unwrap();

    let daemon = TestDaemon::start("workspace-owner").await;
    let mut client = daemon.client().await;
    start_and_wait(&mut client, request).await;

    // The test image runs as root, so switch to UID/GID 1000 the way the agent user would run.
    let script = format!(
        "stat -c %u:%g {guest_path} {guest_path}/from-host.txt; \
         setpriv --reuid=1000 --regid=1000 --clear-groups touch {guest_path}/from-agent.txt"
    );
    let (output, code) = run_command(&mut client, name, "sh", &["-c", &script]).await;

    assert_eq!(code, 0, "unexpected output: {output:?}");
    assert_eq!(
        output.matches("1000:1000").count(),
        2,
        "unexpected output: {output:?}"
    );
    assert!(workspace.join("from-agent.txt").exists());

    daemon.stop().await;
    remove_sandbox(name).await;
}

/// Creates an empty host directory to mount next to the workspace of a test sandbox.
fn test_mount_dir(name: &str, suffix: &str) -> PathBuf {
    let dir = test_temp_dir().join(format!("{name}-{suffix}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("failed to create test mount directory");

    dir
}

/// Returns the request message that mounts the host directory at the guest path.
fn mount(host: &Path, guest: &str, readonly: bool) -> Mount {
    Mount {
        host: host.to_string_lossy().into_owned(),
        guest: guest.to_string(),
        readonly,
    }
}

#[tokio::test]
async fn extra_mounts_are_writable_unless_readonly() {
    let name: &str = &sandbox_name("extra-mounts");
    remove_sandbox(name).await;

    let writable = test_mount_dir(name, "rw");
    let readonly = test_mount_dir(name, "ro");
    std::fs::write(readonly.join("from-host.txt"), "hello from host").unwrap();
    let request = StartSandboxRequest {
        mounts: vec![
            mount(&writable, "/mnt/rw", false),
            mount(&readonly, "/mnt/ro", true),
        ],
        ..start_request(name)
    };

    let daemon = TestDaemon::start("extra-mounts").await;
    let mut client = daemon.client().await;
    start_and_wait(&mut client, request).await;

    // The test image runs as root, so switch to UID/GID 1000 the way the agent user would run.
    let script = "setpriv --reuid=1000 --regid=1000 --clear-groups touch /mnt/rw/from-agent.txt \
                  && cat /mnt/ro/from-host.txt \
                  && ! touch /mnt/ro/from-guest.txt";
    let (output, code) = run_command(&mut client, name, "sh", &["-c", script]).await;

    daemon.stop().await;
    remove_sandbox(name).await;

    assert_eq!(code, 0, "unexpected output: {output:?}");
    assert!(output.contains("hello from host"), "{output:?}");
    assert!(output.contains("Read-only file system"), "{output:?}");
    assert!(writable.join("from-agent.txt").exists());
    assert!(!readonly.join("from-guest.txt").exists());

    let _ = std::fs::remove_dir_all(writable);
    let _ = std::fs::remove_dir_all(readonly);
}

#[tokio::test]
async fn start_sandbox_rejects_mount_at_workspace_guest_path() {
    let name: &str = &sandbox_name("mount-conflict");
    remove_sandbox(name).await;

    let extra = test_mount_dir(name, "extra");
    let request = start_request(name);
    let guest = guest_workspace(Path::new(&request.workspace));
    let request = StartSandboxRequest {
        mounts: vec![mount(&extra, &guest, false)],
        ..request
    };

    let daemon = TestDaemon::start("mount-conflict").await;
    let mut client = daemon.client().await;
    let status = start(&mut client, request).await.unwrap_err();
    let exists = Sandbox::get(name).await.is_ok();

    daemon.stop().await;
    remove_sandbox(name).await;
    let _ = std::fs::remove_dir_all(extra);

    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(
        status.message(),
        format!("mount {guest} conflicts with the workspace mount")
    );
    assert!(!exists, "the sandbox should not have been created");
}

fn test_secret(name: &str) -> Secret {
    Secret::new(
        name.to_string(),
        "fbk-it-secret-value".to_string(),
        vec!["example.com".to_string()],
    )
    .unwrap()
}

/// Prints the environment variable in the sandbox and returns the output.
async fn print_env(
    client: &mut SandboxManagementServiceClient<Channel>,
    sandbox: &str,
    var: &str,
) -> String {
    let (output, code) = run_print_env(client, sandbox, var).await;
    assert_eq!(code, 0, "printenv {var} failed: {output:?}");

    output
}

/// Runs `printenv` for the variable in the sandbox and returns its output and exit code.
async fn run_print_env(
    client: &mut SandboxManagementServiceClient<Channel>,
    sandbox: &str,
    var: &str,
) -> (String, i32) {
    run_command(client, sandbox, "printenv", &[var]).await
}

/// Stops the sandbox and starts it again.
async fn restart_sandbox(client: &mut SandboxManagementServiceClient<Channel>, name: &str) {
    stop_sandbox(client, name).await;
    start_running_sandbox(client, name).await;
}

#[tokio::test]
async fn new_sandbox_sees_secret_placeholder_only() {
    let name: &str = &sandbox_name("secret-new");
    remove_sandbox(name).await;

    let dir = tempfile::tempdir().unwrap();
    let store = SecretStore::new(dir.path().join("secrets.yml"));
    store.set(test_secret("FIREBRICK_IT_TOKEN")).unwrap();

    let daemon = TestDaemon::start_with_secrets("secret-new", store).await;
    let mut client = daemon.client().await;
    start_running_sandbox(&mut client, name).await;

    let output = print_env(&mut client, name, "FIREBRICK_IT_TOKEN").await;

    assert!(
        output.contains("$MSB_FIREBRICK_IT_TOKEN"),
        "unexpected output: {output:?}"
    );
    assert!(!output.contains("fbk-it-secret-value"));

    daemon.stop().await;
    remove_sandbox(name).await;
}

#[tokio::test]
async fn existing_sandbox_sees_added_secret_after_restart() {
    let name: &str = &sandbox_name("secret-existing");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("secret-existing").await;
    let mut client = daemon.client().await;
    start_running_sandbox(&mut client, name).await;

    let handle = Sandbox::get(name).await.unwrap();
    secrets::add_to_sandbox(&handle, &test_secret("FIREBRICK_IT_TOKEN"))
        .await
        .expect("failed to add secret");

    restart_sandbox(&mut client, name).await;

    let output = print_env(&mut client, name, "FIREBRICK_IT_TOKEN").await;

    assert!(
        output.contains("$MSB_FIREBRICK_IT_TOKEN"),
        "unexpected output: {output:?}"
    );
    assert!(!output.contains("fbk-it-secret-value"));

    daemon.stop().await;
    remove_sandbox(name).await;
}

#[tokio::test]
async fn existing_sandbox_loses_removed_secret_after_restart() {
    let name: &str = &sandbox_name("secret-removed");
    remove_sandbox(name).await;

    let dir = tempfile::tempdir().unwrap();
    let store = SecretStore::new(dir.path().join("secrets.yml"));
    store.set(test_secret("FIREBRICK_IT_TOKEN")).unwrap();

    let daemon = TestDaemon::start_with_secrets("secret-removed", store).await;
    let mut client = daemon.client().await;
    start_running_sandbox(&mut client, name).await;

    // The RemoveSecret RPC would change every firebrick sandbox on the host, so remove the secret
    // from the test sandbox only.
    let handle = Sandbox::get(name).await.unwrap();
    secrets::remove_from_sandbox(&handle, "FIREBRICK_IT_TOKEN")
        .await
        .expect("failed to remove secret");
    restart_sandbox(&mut client, name).await;

    let (output, code) = run_print_env(&mut client, name, "FIREBRICK_IT_TOKEN").await;

    assert_ne!(code, 0, "secret is still set: {output:?}");

    daemon.stop().await;
    remove_sandbox(name).await;
}

/// Returns the allowed hosts microsandbox stores for the sandbox's secret, as debug text. The
/// guest sees the same placeholder for every value, so the hosts tell which secret it got.
async fn stored_secret_hosts(sandbox: &str, var: &str) -> String {
    let spec = sandbox_spec(sandbox).await;
    let entry = spec
        .network
        .secrets
        .iter()
        .flat_map(|config| &config.secrets)
        .find(|entry| entry.env_var == var)
        .unwrap_or_else(|| panic!("sandbox {sandbox} has no secret {var}"));

    format!("{:?}", entry.allowed_hosts)
}

/// Returns a secret with the name for the host.
fn secret_for_host(name: &str, host: &str) -> Secret {
    Secret::new(
        name.to_string(),
        "fbk-it-secret-value".to_string(),
        vec![host.to_string()],
    )
    .unwrap()
}

/// Sets a secret for `host` scoped to the sandbox and checks it was added to the sandbox.
async fn set_scoped_secret(
    client: &mut SandboxManagementServiceClient<Channel>,
    sandbox: &str,
    var: &str,
    host: &str,
) {
    let response = client
        .set_secret(SetSecretRequest {
            name: var.to_string(),
            value: "fbk-it-scoped-value".to_string(),
            allowed_hosts: vec![host.to_string()],
            sandbox: Some(sandbox.to_string()),
        })
        .await
        .expect("failed to set scoped secret")
        .into_inner();

    assert!(response.failed_sandboxes.is_empty(), "{response:?}");
}

/// Removes the secret scoped to the sandbox and checks it was removed from the sandbox.
async fn remove_scoped_secret(
    client: &mut SandboxManagementServiceClient<Channel>,
    sandbox: &str,
    var: &str,
) {
    let response = client
        .remove_secret(RemoveSecretRequest {
            name: var.to_string(),
            sandbox: Some(sandbox.to_string()),
        })
        .await
        .expect("failed to remove scoped secret")
        .into_inner();

    assert!(response.failed_sandboxes.is_empty(), "{response:?}");
}

/// Checks that the running sandbox sees the secret's placeholder, and that microsandbox stores
/// the secret for `host`.
async fn assert_sees_secret_for_host(
    client: &mut SandboxManagementServiceClient<Channel>,
    sandbox: &str,
    var: &str,
    host: &str,
) {
    let output = print_env(client, sandbox, var).await;
    assert!(output.contains(&format!("$MSB_{var}")), "{output:?}");

    let hosts = stored_secret_hosts(sandbox, var).await;
    assert!(hosts.contains(&format!("{host:?}")), "{sandbox}: {hosts}");
}

#[tokio::test]
async fn sandbox_scoped_secret_overrides_global_secret_after_restart() {
    let scoped: &str = &sandbox_name("secret-scoped");
    let other: &str = &sandbox_name("secret-unscoped");
    const VAR: &str = "FIREBRICK_IT_SCOPED_TOKEN";
    remove_sandbox(scoped).await;
    remove_sandbox(other).await;

    // The global secret is stored up front, because the global SetSecret RPC would change every
    // firebrick sandbox on the host.
    let dir = tempfile::tempdir().unwrap();
    let store = SecretStore::new(dir.path().join("secrets.yml"));
    store
        .set(secret_for_host(VAR, "global.example.com"))
        .unwrap();

    let daemon = TestDaemon::start_with_secrets("secret-scoped", store).await;
    let mut client = daemon.client().await;
    start_running_sandbox(&mut client, scoped).await;
    start_running_sandbox(&mut client, other).await;

    set_scoped_secret(&mut client, scoped, VAR, "scoped.example.com").await;
    restart_sandbox(&mut client, scoped).await;
    restart_sandbox(&mut client, other).await;

    assert_sees_secret_for_host(&mut client, scoped, VAR, "scoped.example.com").await;
    assert_sees_secret_for_host(&mut client, other, VAR, "global.example.com").await;

    remove_scoped_secret(&mut client, scoped, VAR).await;
    restart_sandbox(&mut client, scoped).await;

    assert_sees_secret_for_host(&mut client, scoped, VAR, "global.example.com").await;

    daemon.stop().await;
    remove_sandbox(scoped).await;
    remove_sandbox(other).await;
}

#[tokio::test]
async fn remove_sandbox_removes_its_scoped_secrets() {
    let name: &str = &sandbox_name("secret-scoped-rm");
    remove_sandbox(name).await;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secrets.yml");
    let global = secret_for_host("FIREBRICK_IT_TOKEN", "example.com");
    SecretStore::new(&path).set(global.clone()).unwrap();

    let daemon = TestDaemon::start_with_secrets("secret-scoped-rm", SecretStore::new(&path)).await;
    let mut client = daemon.client().await;
    start_running_sandbox(&mut client, name).await;

    set_scoped_secret(&mut client, name, "FIREBRICK_IT_TOKEN", "example.com").await;
    assert_eq!(SecretStore::new(&path).load().unwrap().len(), 2);

    client
        .remove_sandbox(RemoveSandboxRequest {
            name: name.to_string(),
            force: true,
        })
        .await
        .expect("failed to remove sandbox");

    assert_eq!(SecretStore::new(&path).load().unwrap(), [global]);

    daemon.stop().await;
    remove_sandbox(name).await;
}

/// Creates the sandbox with `mise_toml` as the workspace's `mise.toml`, if any, then stops and
/// restarts it. Returns the mise progress messages that each start sent.
async fn mise_progress_of_create_and_restart(
    client: &mut SandboxManagementServiceClient<Channel>,
    name: &str,
    mise_toml: Option<&str>,
) -> [Vec<MiseInstallProgress>; 2] {
    // `start_request` empties the workspace, so the config is written after it and the restart
    // reuses the request.
    let request = start_request(name);
    if let Some(config) = mise_toml {
        std::fs::write(workspace_path(name).join("mise.toml"), config).unwrap();
    }
    let created = start_stream(client, request.clone())
        .await
        .expect("failed to create sandbox");
    wait_for_status(client, name, SandboxStatus::Running).await;
    stop_sandbox(client, name).await;
    let restarted = start_stream(client, request)
        .await
        .expect("failed to restart sandbox");
    wait_for_status(client, name, SandboxStatus::Running).await;

    [created.0, restarted.0].map(mise_progress)
}

/// Returns the mise progress messages among the progress messages of a start.
fn mise_progress(messages: Vec<start_sandbox_response::Message>) -> Vec<MiseInstallProgress> {
    messages
        .into_iter()
        .filter_map(|message| match message {
            start_sandbox_response::Message::MiseProgress(progress) => Some(progress),
            _ => None,
        })
        .collect()
}

/// Creates the sandbox and gives it a fake `mise` that runs `install` for `mise install` and
/// does nothing for `mise trust`. Then adds a `mise.toml` to the workspace and restarts the
/// sandbox, so the restart installs the tools. Returns the result of the restart.
async fn restart_with_fake_mise(
    client: &mut SandboxManagementServiceClient<Channel>,
    name: &str,
    install: &str,
) -> Result<(Vec<start_sandbox_response::Message>, SandboxStarted), Status> {
    // `start_request` empties the workspace, so the restart reuses the request.
    let request = start_request(name);
    start_and_wait(client, request.clone()).await;
    let workspace = workspace_path(name);
    let script = format!("#!/bin/sh\n[ \"$1\" = install ] || exit 0\n{install}\n");
    std::fs::write(workspace.join("fake-mise"), script).unwrap();
    let copy = format!(
        "cp {}/fake-mise /usr/local/bin/mise && chmod +x /usr/local/bin/mise",
        guest_workspace(&workspace)
    );
    let (output, code) = run_command(client, name, "sh", &["-c", &copy]).await;
    assert_eq!(code, 0, "failed to install the fake mise: {output}");

    std::fs::write(workspace.join("mise.toml"), "[tools]\n").unwrap();
    stop_sandbox(client, name).await;

    start_stream(client, request).await
}

#[tokio::test]
async fn start_sandbox_streams_mise_progress_without_control_characters() {
    let name: &str = &sandbox_name("mise-progress");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("mise-progress").await;
    let mut client = daemon.client().await;
    // Only the first line within a progress interval is reported right away, and a line still
    // waiting when mise exits is dropped, so the escaped line is the fake mise's only output.
    let install = r"printf 'mise \033[1;32minstalling\033[0m node\n'";

    let (messages, _) = restart_with_fake_mise(&mut client, name, install)
        .await
        .expect("failed to restart sandbox");
    let progress = mise_progress(messages);

    let ends =
        [progress.first(), progress.last()].map(|p| p.map(|p| (p.line.as_str(), p.complete)));
    assert_eq!(ends, [Some(("", false)), Some(("", true))], "{progress:?}");
    assert!(
        progress.iter().any(|p| p.line.contains("installing")),
        "{progress:?}"
    );
    assert!(
        progress
            .iter()
            .all(|p| !p.line.chars().any(char::is_control)),
        "{progress:?}"
    );

    daemon.stop().await;
    remove_sandbox(name).await;
}

#[tokio::test]
async fn start_sandbox_reports_the_stderr_of_a_failed_mise_install() {
    let name: &str = &sandbox_name("mise-failed");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("mise-failed").await;
    let mut client = daemon.client().await;
    let install = r"printf 'mise ERROR \033]52;c;ZXZpbA==\007no such version\n' >&2; exit 1";

    let status = restart_with_fake_mise(&mut client, name, install)
        .await
        .expect_err("the failed install started the sandbox");

    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert_eq!(
        status.message(),
        format!("mise install failed in sandbox {name}: mise ERROR no such version")
    );

    daemon.stop().await;
    remove_sandbox(name).await;
}

#[tokio::test]
async fn start_sandbox_skips_mise_when_image_has_none() {
    let name: &str = &sandbox_name("mise-missing");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("mise-missing").await;
    let mut client = daemon.client().await;
    // The test image has no mise, so both the create and the restart only log a warning.
    let config = "[tools]\nnode = \"22\"\n";

    let progress = mise_progress_of_create_and_restart(&mut client, name, Some(config)).await;

    assert_eq!(progress, [vec![], vec![]]);

    daemon.stop().await;
    remove_sandbox(name).await;
}

#[tokio::test]
async fn start_sandbox_sends_no_mise_progress_without_a_mise_config() {
    let name: &str = &sandbox_name("mise-no-config");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("mise-no-config").await;
    let mut client = daemon.client().await;

    let progress = mise_progress_of_create_and_restart(&mut client, name, None).await;

    assert_eq!(progress, [vec![], vec![]]);

    daemon.stop().await;
    remove_sandbox(name).await;
}

/// Returns the mise label microsandbox stores for the sandbox.
async fn mise_label(name: &str) -> Option<String> {
    sandbox_spec(name)
        .await
        .labels
        .get(mise::ENABLED_LABEL)
        .cloned()
}

#[tokio::test]
async fn start_sandbox_stores_the_mise_setting() {
    let enabled: &str = &sandbox_name("mise-enabled");
    let disabled: &str = &sandbox_name("mise-disabled");
    remove_sandbox(enabled).await;
    remove_sandbox(disabled).await;

    let daemon = TestDaemon::start("mise-setting").await;
    let mut client = daemon.client().await;

    start_running_sandbox(&mut client, enabled).await;
    let request = StartSandboxRequest {
        mise: Some(false),
        ..start_request(disabled)
    };
    start_and_wait(&mut client, request).await;

    assert_eq!(mise_label(enabled).await.as_deref(), Some("true"));
    assert_eq!(mise_label(disabled).await.as_deref(), Some("false"));

    daemon.stop().await;
    remove_sandbox(enabled).await;
    remove_sandbox(disabled).await;
}

/// Image with `curl` for the network tests; the default test image has no HTTP client.
const CURL_IMAGE: &str = "buildpack-deps:trixie-curl";

/// Requests the URL over IPv4 from inside the sandbox and returns the body followed by the
/// HTTP status code on its own line.
///
/// curl writes the status code as it exits, and microsandbox drops terminal output that the
/// guest agent hasn't read when it reports the exit. The shell waits a second after curl exits
/// so the status code is read first.
async fn fetch(
    client: &mut SandboxManagementServiceClient<Channel>,
    name: &str,
    url: &str,
) -> String {
    let script = "curl -4 -sS -m 30 -w '\\n%{http_code}' \"$1\"; code=$?; sleep 1; exit $code";
    let (output, _) = run_command(client, name, "sh", &["-c", script, "fetch", url]).await;

    output
}

/// Starts a sandbox from the curl image that enforces the allow and deny rules.
async fn start_enforced_sandbox(
    client: &mut SandboxManagementServiceClient<Channel>,
    name: &str,
    allow: &[&str],
    deny: &[&str],
) {
    let rules = |rules: &[&str]| rules.iter().map(ToString::to_string).collect();
    let request = StartSandboxRequest {
        image: CURL_IMAGE.to_string(),
        network: Some(NetworkPolicy {
            enforce: true,
            allow: rules(allow),
            deny: rules(deny),
            ..Default::default()
        }),
        ..start_request(name)
    };

    start_and_wait(client, request).await;
}

#[tokio::test]
async fn enforced_network_policy_answers_hosts_it_does_not_allow_with_deny_page() {
    let name: &str = &sandbox_name("network-allow");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("network-allow").await;
    let mut client = daemon.client().await;
    start_enforced_sandbox(&mut client, name, &["example.com"], &[]).await;

    let allowed = fetch(&mut client, name, "https://example.com").await;
    let not_allowed = fetch(&mut client, name, "https://example.org").await;

    daemon.stop().await;
    remove_sandbox(name).await;

    assert!(allowed.trim_end().ends_with("200"), "{allowed:?}");
    assert!(
        not_allowed.contains(
            "firebrick blocked the connection to example.org: the network policy of this \
             sandbox doesn't allow it. To allow it, run `fbk network allow example.org` on the \
             host, outside the sandbox."
        ),
        "{not_allowed:?}"
    );
    assert!(not_allowed.trim_end().ends_with("403"), "{not_allowed:?}");
}

#[tokio::test]
async fn enforced_network_policy_denies_host_that_is_also_allowed() {
    let name: &str = &sandbox_name("network-deny");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("network-deny").await;
    let mut client = daemon.client().await;
    start_enforced_sandbox(&mut client, name, &["*.example.net"], &["example.net"]).await;

    let allowed = fetch(&mut client, name, "https://www.example.net").await;
    let denied = fetch(&mut client, name, "https://example.net").await;

    daemon.stop().await;
    remove_sandbox(name).await;

    // The allowed subdomain proves the rules apply, so the failure comes from the deny rule.
    assert!(!allowed.trim_end().ends_with("000"), "{allowed:?}");
    assert!(denied.trim_end().ends_with("000"), "{denied:?}");
}

#[tokio::test]
async fn enforced_network_policy_allows_and_denies_ip_addresses() {
    let name: &str = &sandbox_name("network-ip");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("network-ip").await;
    let mut client = daemon.client().await;
    // Cloudflare's resolvers answer plain HTTP on their addresses.
    start_enforced_sandbox(
        &mut client,
        name,
        &["1.0.0.0/24", "1.1.1.0/24"],
        &["1.1.1.1"],
    )
    .await;

    let allowed = fetch(&mut client, name, "http://1.0.0.1").await;
    let denied = fetch(&mut client, name, "http://1.1.1.1").await;

    daemon.stop().await;
    remove_sandbox(name).await;

    assert!(!allowed.trim_end().ends_with("000"), "{allowed:?}");
    assert!(denied.trim_end().ends_with("000"), "{denied:?}");
}

#[tokio::test]
async fn start_sandbox_rejects_invalid_network_rule() {
    let daemon = TestDaemon::start("network-invalid").await;
    let mut client = daemon.client().await;
    let request = StartSandboxRequest {
        network: Some(NetworkPolicy {
            enforce: true,
            allow: vec!["github.com:443".to_string()],
            ..Default::default()
        }),
        ..start_request(&sandbox_name("network-invalid"))
    };

    let status = start(&mut client, request).await.unwrap_err();

    daemon.stop().await;

    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(status.message().contains("github.com:443"), "{status:?}");
}

/// Asks the daemon to replace the egress rules of the sandbox. Returns whether it recreated the
/// sandbox.
async fn update_network(
    client: &mut SandboxManagementServiceClient<Channel>,
    name: &str,
    network: NetworkPolicy,
) -> Result<bool, tonic::Status> {
    client
        .update_network(UpdateNetworkRequest {
            name: name.to_string(),
            network: Some(network),
        })
        .await
        .map(|response| response.into_inner().updated)
}

/// Returns the id of the sandbox's current boot, which changes when it is recreated.
async fn boot_id(client: &mut SandboxManagementServiceClient<Channel>, name: &str) -> String {
    let (output, _) = run_command(client, name, "cat", &["/proc/sys/kernel/random/boot_id"]).await;

    output
}

/// Returns a network policy with the allow rules and no deny rules.
fn allow_policy(enforce: bool, allow: &[&str]) -> NetworkPolicy {
    NetworkPolicy {
        enforce,
        allow: allow.iter().map(ToString::to_string).collect(),
        ..Default::default()
    }
}

/// Returns the network settings that remove the sandbox's network device.
fn disabled_network() -> NetworkPolicy {
    NetworkPolicy {
        enabled: Some(false),
        ..Default::default()
    }
}

/// Writes a marker file to the root disk and one to the Docker disk of the sandbox.
async fn write_markers(client: &mut SandboxManagementServiceClient<Channel>, name: &str) {
    let script = "echo kept > /var/tmp/marker && echo kept > /var/lib/docker/marker";
    let (output, code) = run_command(client, name, "sh", &["-c", script]).await;

    assert_eq!(code, 0, "failed to write markers: {output:?}");
}

/// Returns the contents of the marker files written by `write_markers`.
async fn read_markers(client: &mut SandboxManagementServiceClient<Channel>, name: &str) -> String {
    let (output, _) = run_command(
        client,
        name,
        "cat",
        &["/var/tmp/marker", "/var/lib/docker/marker"],
    )
    .await;

    output
}

/// The settings of a sandbox that updating its network rules must keep.
#[derive(Debug, PartialEq)]
struct KeptSettings {
    hostname: String,
    workspace_host_path: String,
    workspace_path: String,
    cpus: u8,
    memory_mib: u32,
    init: bool,
    docker_disk_mib: Option<u32>,
    clipboard_route: bool,
}

async fn kept_settings(
    client: &mut SandboxManagementServiceClient<Channel>,
    name: &str,
) -> KeptSettings {
    let sandbox = get_sandbox(client, name).await;
    let spec = sandbox_spec(name).await;

    KeptSettings {
        hostname: sandbox.hostname,
        workspace_host_path: sandbox.workspace_host_path,
        workspace_path: sandbox.workspace_path,
        cpus: spec.resources.cpus,
        memory_mib: spec.resources.memory_mib,
        init: spec.init.is_some(),
        docker_disk_mib: docker_disk_mib(&spec),
        clipboard_route: has_clipboard_route(name, &spec),
    }
}

/// Asserts that a recreated sandbox kept its settings, including its clipboard route.
fn assert_kept_with_clipboard(after: &KeptSettings, before: &KeptSettings) {
    assert!(before.clipboard_route, "the sandbox had no clipboard route");
    assert_eq!(after, before);
}

/// What a sandbox looks like right after its network rules were updated.
struct AfterUpdate {
    status: Option<SandboxStatus>,
    settings: KeptSettings,
    stored_network: String,
    snapshots: Vec<String>,
}

async fn after_update(
    client: &mut SandboxManagementServiceClient<Channel>,
    name: &str,
) -> AfterUpdate {
    AfterUpdate {
        status: sandbox_status(client, name).await,
        settings: kept_settings(client, name).await,
        stored_network: format!("{:?}", sandbox_spec(name).await.network),
        snapshots: leftover_snapshots(name).await,
    }
}

/// Returns the snapshots that updating the sandbox's network rules or upgrading it left behind.
async fn network_snapshots(name: &str) -> Vec<SnapshotHandle> {
    let prefixes = ["network", "upgrade"].map(|kind| format!("firebrick-{kind}-{name}-"));
    let recreated = |n: &str| prefixes.iter().any(|prefix| n.starts_with(prefix));

    Snapshot::list()
        .await
        .expect("failed to list snapshots")
        .into_iter()
        .filter(|snapshot| snapshot.name().is_some_and(recreated))
        .collect()
}

/// Returns the names of the snapshots that updating the sandbox's network rules or upgrading it
/// left behind.
async fn leftover_snapshots(name: &str) -> Vec<String> {
    network_snapshots(name)
        .await
        .iter()
        .filter_map(|snapshot| snapshot.name().map(ToString::to_string))
        .collect()
}

/// Removes the sandbox like `remove_sandbox`, and the snapshots a failed update of its network
/// rules kept.
async fn remove_sandbox_and_snapshots(name: &str) {
    remove_sandbox(name).await;

    for snapshot in network_snapshots(name).await {
        let _ = snapshot.remove(true).await;
    }
}

/// Returns a store in the directory with the test secret `FIREBRICK_IT_TOKEN`.
fn store_with_test_secret(dir: &Path) -> SecretStore {
    let store = SecretStore::new(dir.join("secrets.yml"));
    store.set(test_secret("FIREBRICK_IT_TOKEN")).unwrap();

    store
}

/// Starts the existing sandbox and waits until it runs.
async fn start_existing(client: &mut SandboxManagementServiceClient<Channel>, name: &str) {
    let request = StartSandboxRequest {
        name: name.to_string(),
        ..Default::default()
    };

    start_and_wait(client, request).await;
}

/// Returns the request of a sandbox with curl that allows `example.com` and shares the host
/// clipboard.
fn clipboard_curl_request(name: &str) -> StartSandboxRequest {
    StartSandboxRequest {
        image: CURL_IMAGE.to_string(),
        network: Some(allow_policy(true, &["example.com"])),
        clipboard: Some(true),
        ..start_request(name)
    }
}

#[tokio::test]
async fn update_network_of_running_sandbox_keeps_its_disk_and_settings() {
    let name: &str = &sandbox_name("update-running");
    remove_sandbox_and_snapshots(name).await;

    let dir = tempfile::tempdir().unwrap();
    let store = store_with_test_secret(dir.path());
    let daemon = TestDaemon::start_with_secrets("update-running", store).await;
    let mut client = daemon.client().await;
    start_and_wait(&mut client, clipboard_curl_request(name)).await;
    write_markers(&mut client, name).await;
    let before = kept_settings(&mut client, name).await;
    let denied = fetch(&mut client, name, "https://example.org").await;

    let network = allow_policy(true, &["example.com", "example.org"]);
    assert!(update_network(&mut client, name, network).await.unwrap());

    let after = after_update(&mut client, name).await;
    let markers = read_markers(&mut client, name).await;
    let allowed = fetch(&mut client, name, "https://example.org").await;
    let secret = print_env(&mut client, name, "FIREBRICK_IT_TOKEN").await;

    daemon.stop().await;
    remove_sandbox(name).await;

    assert!(denied.trim_end().ends_with("403"), "{denied:?}");
    assert_eq!(after.status, Some(SandboxStatus::Running));
    assert_kept_with_clipboard(&after.settings, &before);
    assert!(
        after.stored_network.contains("example.org"),
        "{}",
        after.stored_network
    );
    assert!(after.snapshots.is_empty(), "{:?}", after.snapshots);
    assert_eq!(markers, "kept\r\nkept\r\n");
    assert!(allowed.trim_end().ends_with("200"), "{allowed:?}");
    assert!(secret.contains("$MSB_FIREBRICK_IT_TOKEN"), "{secret:?}");
}

#[tokio::test]
async fn update_network_removes_its_snapshot_after_a_reader_releases_it() {
    let name: &str = &sandbox_name("update-leased");
    remove_sandbox_and_snapshots(name).await;

    let daemon = TestDaemon::start("update-leased").await;
    let mut client = daemon.client().await;
    start_running_sandbox(&mut client, name).await;
    let reader = hold_snapshot_leases(snapshot_group(name), Duration::from_secs(3));

    let updated = update_network(&mut client, name, allow_policy(true, &["example.com"])).await;
    let held = reader.join();
    let snapshots = leftover_snapshots(name).await;

    daemon.stop().await;
    remove_sandbox_and_snapshots(name).await;

    assert!(held.is_ok(), "the reader failed to hold the snapshot");
    assert!(matches!(updated, Ok(true)), "{updated:?}");
    assert!(snapshots.is_empty(), "{snapshots:?}");
}

/// Returns the directory microsandbox keeps the snapshots of the sandbox in.
fn snapshot_group(name: &str) -> PathBuf {
    microsandbox::config::config()
        .unwrap()
        .home()
        .join("snapshots")
        .join(name)
}

/// Waits in a thread until the snapshot group holds a snapshot, then holds a reader lease on
/// each of its entries for `hold`, like a concurrent microsandbox operation that reads it.
fn hold_snapshot_leases(group: PathBuf, hold: Duration) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + STATUS_TIMEOUT;

        while !has_snapshot(&group) {
            assert!(std::time::Instant::now() < deadline, "no snapshot appeared");
            std::thread::sleep(Duration::from_millis(5));
        }

        let leases: Vec<std::fs::File> = std::fs::read_dir(group.join(".msb-leases"))
            .unwrap()
            .map(|entry| std::fs::File::open(entry.unwrap().path()).unwrap())
            .collect();

        for lease in &leases {
            lease.lock_shared().unwrap();
        }

        std::thread::sleep(hold);
    })
}

/// Returns whether the snapshot group holds a snapshot.
fn has_snapshot(group: &Path) -> bool {
    std::fs::read_dir(group).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with("snap_"))
    })
}

#[tokio::test]
async fn update_network_of_stopped_sandbox_keeps_it_stopped() {
    let name: &str = &sandbox_name("update-stopped");
    remove_sandbox_and_snapshots(name).await;

    let daemon = TestDaemon::start("update-stopped").await;
    let mut client = daemon.client().await;
    start_enforced_sandbox(&mut client, name, &["example.com"], &[]).await;
    write_markers(&mut client, name).await;
    let before = kept_settings(&mut client, name).await;
    stop_sandbox(&mut client, name).await;

    let network = allow_policy(false, &["example.com"]);
    assert!(update_network(&mut client, name, network).await.unwrap());

    let after = after_update(&mut client, name).await;
    start_existing(&mut client, name).await;
    let markers = read_markers(&mut client, name).await;
    // Without enforcement, the rules no longer deny anything.
    let unrestricted = fetch(&mut client, name, "https://example.org").await;

    daemon.stop().await;
    remove_sandbox(name).await;

    assert_eq!(after.status, Some(SandboxStatus::Stopped));
    assert_eq!(after.settings, before);
    // Without enforcement, microsandbox gets no policy with the rules.
    assert!(
        !after.stored_network.contains("example.com"),
        "{}",
        after.stored_network
    );
    assert!(after.snapshots.is_empty(), "{:?}", after.snapshots);
    assert_eq!(markers, "kept\r\nkept\r\n");
    assert!(unrestricted.trim_end().ends_with("200"), "{unrestricted:?}");
}

#[tokio::test]
async fn update_network_returns_not_found_for_unknown_sandbox() {
    let daemon = TestDaemon::start("update-unknown").await;
    let mut client = daemon.client().await;

    let status = update_network(
        &mut client,
        "fbk-it-does-not-exist",
        NetworkPolicy::default(),
    )
    .await
    .unwrap_err();

    daemon.stop().await;

    assert_eq!(status.code(), Code::NotFound);
    assert_eq!(status.message(), "couldn't find specified sandbox");
}

/// Returns the resolved paths of the snapshots that the network updates of the sandbox kept.
async fn network_snapshot_paths(name: &str) -> Vec<PathBuf> {
    network_snapshots(name)
        .await
        .iter()
        .filter_map(|snapshot| snapshot.path().ok())
        .filter_map(|path| dunce::canonicalize(path).ok())
        .collect()
}

/// Returns the message of a failed update with the snapshot it names resolved. The daemon names
/// it under the unresolved `MSB_HOME`, which goes through the `/tmp` symlink on macOS, while
/// `Snapshot::path` returns the resolved path.
fn resolve_kept_snapshot(message: &str) -> String {
    const KEPT: &str = "; the sandbox was kept as snapshot ";

    match message.split_once(KEPT) {
        Some((head, path)) => match dunce::canonicalize(path) {
            Ok(path) => format!("{head}{KEPT}{}", path.display()),
            Err(_) => message.to_string(),
        },
        None => message.to_string(),
    }
}

#[tokio::test]
async fn failed_recreate_keeps_the_snapshot_and_names_it() {
    let name: &str = &sandbox_name("update-failed");
    remove_sandbox_and_snapshots(name).await;

    let dir = tempfile::tempdir().unwrap();
    let store = store_with_test_secret(dir.path());
    let daemon = TestDaemon::start_with_secrets("update-failed", store).await;
    let mut client = daemon.client().await;
    start_running_sandbox(&mut client, name).await;
    // The recreate loads the secrets, so a store it can't read makes it fail.
    std::fs::write(dir.path().join("secrets.yml"), "not: [valid").unwrap();

    let status = update_network(&mut client, name, allow_policy(true, &["example.org"]))
        .await
        .unwrap_err();

    let exists = Sandbox::get(name).await.is_ok();
    let paths = network_snapshot_paths(name).await;
    let message = resolve_kept_snapshot(status.message());

    daemon.stop().await;
    remove_sandbox_and_snapshots(name).await;

    assert_eq!(status.code(), Code::Internal);
    assert!(!exists, "the half-created sandbox should be removed");
    assert_eq!(paths.len(), 1, "{paths:?}");
    assert_eq!(
        message,
        format!(
            "failed to update the network rules of {name}; the sandbox was kept as snapshot {}",
            paths[0].display()
        )
    );
}

#[tokio::test]
async fn update_network_finishes_when_the_client_disconnects() {
    let name: &str = &sandbox_name("update-disconnect");
    remove_sandbox_and_snapshots(name).await;

    let daemon = TestDaemon::start("update-disconnect").await;
    let mut client = daemon.client().await;
    start_running_sandbox(&mut client, name).await;
    let network = allow_policy(true, &["example.com"]);

    // Recreating the sandbox takes seconds, so the client gives up while it runs.
    let cancelled = tokio::time::timeout(
        Duration::from_millis(100),
        update_network(&mut client.clone(), name, network.clone()),
    )
    .await;
    // Waits for the cancelled update, which holds the sandbox's lock.
    let again = update_network(&mut client, name, network).await;
    let snapshots = leftover_snapshots(name).await;

    daemon.stop().await;
    remove_sandbox_and_snapshots(name).await;

    assert!(cancelled.is_err(), "the update finished within 100 ms");
    assert_eq!(again.map_err(|status| status.to_string()), Ok(false));
    assert!(snapshots.is_empty(), "{snapshots:?}");
}

#[tokio::test]
async fn update_network_can_update_the_same_sandbox_again() {
    let name: &str = &sandbox_name("update-twice");
    remove_sandbox_and_snapshots(name).await;

    let daemon = TestDaemon::start("update-twice").await;
    let mut client = daemon.client().await;
    start_running_sandbox(&mut client, name).await;

    let first = update_network(&mut client, name, allow_policy(true, &["example.org"])).await;
    let second = update_network(&mut client, name, allow_policy(true, &["example.com"])).await;
    let after = after_update(&mut client, name).await;

    daemon.stop().await;
    remove_sandbox_and_snapshots(name).await;

    assert!(matches!(first, Ok(true)), "{first:?}");
    assert!(matches!(second, Ok(true)), "{second:?}");
    assert_eq!(after.status, Some(SandboxStatus::Running));
    assert!(
        after.stored_network.contains("example.com"),
        "{}",
        after.stored_network
    );
    assert!(
        !after.stored_network.contains("example.org"),
        "{}",
        after.stored_network
    );
    assert!(after.snapshots.is_empty(), "{:?}", after.snapshots);
}

#[tokio::test]
async fn update_network_with_the_rules_the_sandbox_has_leaves_it_alone() {
    let name: &str = &sandbox_name("update-unchanged");
    remove_sandbox_and_snapshots(name).await;

    let daemon = TestDaemon::start("update-unchanged").await;
    let mut client = daemon.client().await;
    start_enforced_sandbox(&mut client, name, &["example.com"], &[]).await;
    let boot = boot_id(&mut client, name).await;

    let same = update_network(&mut client, name, allow_policy(true, &["example.com"])).await;
    let same_boot = boot_id(&mut client, name).await;
    let disabled = update_network(&mut client, name, allow_policy(false, &["example.com"])).await;
    // Rules that aren't enforced don't change the sandbox.
    let still_disabled =
        update_network(&mut client, name, allow_policy(false, &["example.org"])).await;

    daemon.stop().await;
    remove_sandbox_and_snapshots(name).await;

    assert!(matches!(same, Ok(false)), "{same:?}");
    assert_eq!(same_boot, boot);
    assert!(matches!(disabled, Ok(true)), "{disabled:?}");
    assert!(matches!(still_disabled, Ok(false)), "{still_disabled:?}");
}

/// Starts a sandbox from the curl image with the network settings and waits until it runs.
async fn start_curl_sandbox(
    client: &mut SandboxManagementServiceClient<Channel>,
    name: &str,
    network: Option<NetworkPolicy>,
) {
    let request = StartSandboxRequest {
        image: CURL_IMAGE.to_string(),
        network,
        ..start_request(name)
    };

    start_and_wait(client, request).await;
}

#[tokio::test]
async fn disabled_network_blocks_connections_until_it_is_enabled() {
    let name: &str = &sandbox_name("network-disabled");
    remove_sandbox_and_snapshots(name).await;

    let dir = tempfile::tempdir().unwrap();
    let store = store_with_test_secret(dir.path());
    let daemon = TestDaemon::start_with_secrets("network-disabled", store).await;
    let mut client = daemon.client().await;
    start_curl_sandbox(&mut client, name, Some(disabled_network())).await;
    write_markers(&mut client, name).await;
    let before = kept_settings(&mut client, name).await;

    let (_, resolved) = run_command(&mut client, name, "getent", &["hosts", "example.com"]).await;
    let offline = fetch(&mut client, name, "http://1.1.1.1").await;
    let enabled = update_network(&mut client, name, NetworkPolicy::default()).await;
    let after = after_update(&mut client, name).await;
    let markers = read_markers(&mut client, name).await;
    let online = fetch(&mut client, name, "https://example.com").await;
    let secret = print_env(&mut client, name, "FIREBRICK_IT_TOKEN").await;

    daemon.stop().await;
    remove_sandbox_and_snapshots(name).await;

    assert_ne!(resolved, 0, "example.com resolved without a network");
    assert!(offline.trim_end().ends_with("000"), "{offline:?}");
    assert!(matches!(enabled, Ok(true)), "{enabled:?}");
    assert_eq!(after.status, Some(SandboxStatus::Running));
    assert_eq!(after.settings, before);
    assert!(after.snapshots.is_empty(), "{:?}", after.snapshots);
    assert_eq!(markers, "kept\r\nkept\r\n");
    assert!(online.trim_end().ends_with("200"), "{online:?}");
    assert!(secret.contains("$MSB_FIREBRICK_IT_TOKEN"), "{secret:?}");
}

#[tokio::test]
async fn update_network_disables_the_network_once() {
    let name: &str = &sandbox_name("network-disable");
    remove_sandbox_and_snapshots(name).await;

    let daemon = TestDaemon::start("network-disable").await;
    let mut client = daemon.client().await;
    start_curl_sandbox(&mut client, name, None).await;

    let disabled = update_network(&mut client, name, disabled_network()).await;
    let offline = fetch(&mut client, name, "http://1.1.1.1").await;
    // A disabled network ignores its rules, so changing them doesn't recreate the sandbox.
    let still_disabled = update_network(
        &mut client,
        name,
        NetworkPolicy {
            enforce: true,
            ..disabled_network()
        },
    )
    .await;

    daemon.stop().await;
    remove_sandbox_and_snapshots(name).await;

    assert!(matches!(disabled, Ok(true)), "{disabled:?}");
    assert!(offline.trim_end().ends_with("000"), "{offline:?}");
    assert!(matches!(still_disabled, Ok(false)), "{still_disabled:?}");
}

#[tokio::test]
async fn update_network_refuses_a_paused_sandbox() {
    let name: &str = &sandbox_name("update-paused");
    remove_sandbox_and_snapshots(name).await;

    let daemon = TestDaemon::start("update-paused").await;
    let mut client = daemon.client().await;
    start_running_sandbox(&mut client, name).await;
    let sb = Sandbox::get(name).await.unwrap();
    pause(&sb).await.unwrap();

    let status = update_network(&mut client, name, allow_policy(true, &["example.org"])).await;
    let paused = sandbox_status(&mut client, name).await;

    resume(&sb).await.unwrap();
    daemon.stop().await;
    remove_sandbox_and_snapshots(name).await;

    let status = status.unwrap_err();
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert_eq!(
        status.message(),
        format!("sandbox {name} is paused; resume it before updating its network rules")
    );
    assert_eq!(paused, Some(SandboxStatus::Paused));
}

// Windows has no `kill -STOP`, which these tests freeze the runtime of a sandbox with.
#[cfg(unix)]
#[tokio::test]
async fn paused_sandbox_with_a_slow_runtime_is_not_reported_running() {
    let name: &str = &sandbox_name("paused-slow");
    remove_sandbox_and_snapshots(name).await;

    let daemon = TestDaemon::start("paused-slow").await;
    let mut client = daemon.client().await;
    let (sb, pid) = start_paused_sandbox(&mut client, name).await;

    let listed = slowed(name, sandbox_status(&mut client, name)).await;
    let got = slowed(name, get_status(&mut client, name)).await;
    let policy = allow_policy(true, &["example.org"]);
    let updated = slowed(name, update_network(&mut client, name, policy)).await;
    let upgraded = slowed(name, upgrade(&mut client, name, UPGRADE_IMAGE)).await;
    let kept = runtime_pid(name).await;

    resume(&sb).await.unwrap();
    daemon.stop().await;
    remove_sandbox_and_snapshots(name).await;

    assert_eq!(listed.0, Some(SandboxStatus::Paused));
    assert_eq!(got.0, Ok(SandboxStatus::Paused));
    assert_eq!(code(updated.0), Code::FailedPrecondition);
    assert_eq!(code(upgraded.0), Code::FailedPrecondition);
    // Each request waited for the runtime, so it asked while the runtime was stopped.
    for elapsed in [listed.1, got.1, updated.1, upgraded.1] {
        assert!(elapsed >= SLOW_RUNTIME, "answered in {elapsed:?}");
    }
    assert_eq!(kept, pid, "the paused sandbox was replaced");
}

#[cfg(unix)]
#[tokio::test]
async fn paused_sandbox_with_a_hung_runtime_is_listed_as_unknown_and_not_replaced() {
    let name: &str = &sandbox_name("paused-hung");
    remove_sandbox_and_snapshots(name).await;

    let daemon = TestDaemon::start("paused-hung").await;
    let (mut list_client, mut get_client) = (daemon.client().await, daemon.client().await);
    let mut client = daemon.client().await;
    let (sb, pid) = start_paused_sandbox(&mut client, name).await;

    let requests = async {
        tokio::join!(
            sandbox_status(&mut list_client, name),
            get_status(&mut get_client, name),
            update_network(&mut client, name, allow_policy(true, &["example.org"])),
        )
    };
    let ((listed, got, updated), _) = frozen(name, HUNG_RUNTIME, requests).await;
    let kept = runtime_pid(name).await;

    resume(&sb).await.unwrap();
    daemon.stop().await;
    remove_sandbox_and_snapshots(name).await;

    assert_eq!(listed, Some(SandboxStatus::Unknown));
    assert_eq!(got, Err(Code::Internal));
    assert_eq!(code(updated), Code::Internal);
    assert_eq!(kept, pid, "the paused sandbox was replaced");
}

/// Starts a sandbox and pauses it, and returns it with the id of its runtime process.
#[cfg(unix)]
async fn start_paused_sandbox(
    client: &mut SandboxManagementServiceClient<Channel>,
    name: &str,
) -> (SandboxHandle, i32) {
    start_running_sandbox(client, name).await;
    let sb = Sandbox::get(name).await.unwrap();
    pause(&sb).await.unwrap();

    (sb, runtime_pid(name).await)
}

/// How long a test stops a sandbox's runtime to make it miss the 250 ms microsandbox gives it
/// to report whether the sandbox is paused, as on a busy host.
#[cfg(unix)]
const SLOW_RUNTIME: Duration = Duration::from_secs(1);

/// How long a test stops a sandbox's runtime to make it miss the 10 s the daemon gives it to
/// report whether the sandbox is paused.
#[cfg(unix)]
const HUNG_RUNTIME: Duration = Duration::from_secs(12);

/// Returns the status the daemon reports for the sandbox, or the code it fails with.
#[cfg(unix)]
async fn get_status(
    client: &mut SandboxManagementServiceClient<Channel>,
    name: &str,
) -> Result<SandboxStatus, Code> {
    client
        .get_sandbox(GetSandboxRequest {
            name: name.to_string(),
        })
        .await
        .map(|response| response.into_inner().status())
        .map_err(|status| status.code())
}

/// Returns the code of a failed request.
#[cfg(unix)]
fn code<T: std::fmt::Debug>(result: Result<T, Status>) -> Code {
    result.expect_err("the request succeeded").code()
}

/// Returns the id of the process that runs the sandbox, which changes when it's replaced.
#[cfg(unix)]
async fn runtime_pid(name: &str) -> i32 {
    Sandbox::get(name)
        .await
        .unwrap()
        .local()
        .and_then(|local| local.pid)
        .expect("sandbox has no runtime process")
}

/// How often a test sends a pause or resume that microsandbox didn't send.
const CONTROL_ATTEMPTS: u32 = 5;

/// Pauses the sandbox, see [`send_control`].
async fn pause(sb: &SandboxHandle) -> MicrosandboxResult<()> {
    send_control(|| sb.pause()).await
}

/// Resumes the sandbox, see [`send_control`].
async fn resume(sb: &SandboxHandle) -> MicrosandboxResult<()> {
    send_control(|| sb.resume()).await
}

/// Sends a pause or resume to the sandbox's runtime, again when microsandbox didn't send it.
/// A failed request from another task, such as the daemon's, closes microsandbox's shared
/// connection to the runtime, and a request on it then fails with `Closed` before it's sent.
/// Only such unsent requests are retried, so the runtime never gets one twice. microsandbox
/// doesn't export the delivery of the error, so it's read from the message.
async fn send_control<F, Fut>(mut request: F) -> MicrosandboxResult<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = MicrosandboxResult<()>>,
{
    let mut attempt = 1;

    loop {
        match request().await {
            Err(err @ MicrosandboxError::ControlClient(_))
                if attempt < CONTROL_ATTEMPTS && err.to_string().contains("delivery: NotSent") =>
            {
                attempt += 1;
                sleep(POLL_INTERVAL).await;
            }
            result => return result,
        }
    }
}

/// Runs the request while the sandbox's runtime is stopped for [`SLOW_RUNTIME`], see [`frozen`].
#[cfg(unix)]
async fn slowed<T>(name: &str, request: impl Future<Output = T>) -> (T, Duration) {
    frozen(name, SLOW_RUNTIME, request).await
}

/// Runs the request while the sandbox's runtime process is stopped for the time, and returns
/// its result and how long it took.
#[cfg(unix)]
async fn frozen<T>(
    name: &str,
    freeze: Duration,
    request: impl Future<Output = T>,
) -> (T, Duration) {
    let pid = runtime_pid(name).await.to_string();
    signal_process("STOP", &pid).await;
    let start = Instant::now();

    let request = async { (request.await, start.elapsed()) };
    let thaw = async {
        sleep(freeze).await;
        signal_process("CONT", &pid).await;
    };
    let (result, ()) = tokio::join!(request, thaw);

    result
}

/// Sends the signal to the process with `kill`.
#[cfg(unix)]
async fn signal_process(signal: &str, pid: &str) {
    let status = tokio::process::Command::new("kill")
        .args([format!("-{signal}").as_str(), pid])
        .status()
        .await
        .expect("failed to run kill");

    assert!(status.success(), "kill -{signal} {pid} failed");
}

/// Image upgrade tests create their sandboxes from, before they upgrade them to
/// [`UPGRADE_IMAGE`].
const OLD_IMAGE: &str = "alpine:3.21";

/// Image upgrade tests upgrade their sandboxes to. busybox in alpine has `nc`.
const UPGRADE_IMAGE: &str = "alpine:3.22";

/// Asks the daemon to upgrade the sandbox to the image and reads the stream to the end. Returns
/// the result of the upgrade, or the status the stream ended with. Fails when a message follows
/// the result.
async fn upgrade(
    client: &mut SandboxManagementServiceClient<Channel>,
    name: &str,
    image: &str,
) -> Result<SandboxUpgraded, Status> {
    let request = UpgradeSandboxRequest {
        name: name.to_string(),
        image: image.to_string(),
    };
    let mut stream = client.upgrade_sandbox(request).await?.into_inner();

    while let Some(UpgradeSandboxResponse { message }) = stream.message().await? {
        if let Some(upgrade_sandbox_response::Message::Upgraded(upgraded)) = message {
            let rest = stream.message().await?;
            assert_eq!(rest, None, "the stream continued after the upgrade");
            return Ok(upgraded);
        }
    }

    panic!("the upgrade stream ended without a result");
}

/// Succeeds when neither marker file of `write_markers` exists.
const NO_MARKERS: &str = "test ! -e /var/tmp/marker && test ! -e /var/lib/docker/marker";

/// Returns the names of the probe sandboxes an upgrade of the sandbox left behind.
async fn leftover_probes(name: &str) -> Vec<String> {
    Sandbox::list_with(|opt| opt.label(sandboxes::PROBE_LABEL, name))
        .await
        .expect("failed to list probe sandboxes")
        .sandboxes
        .iter()
        .map(|sb| sb.name().to_string())
        .collect()
}

/// Returns the version of alpine the sandbox runs.
async fn alpine_release(
    client: &mut SandboxManagementServiceClient<Channel>,
    name: &str,
) -> String {
    let (output, _) = run_command(client, name, "cat", &["/etc/alpine-release"]).await;

    output
}

/// Returns the start request of a sandbox from [`OLD_IMAGE`].
fn old_image_request(name: &str) -> StartSandboxRequest {
    StartSandboxRequest {
        image: OLD_IMAGE.to_string(),
        ..start_request(name)
    }
}

/// Returns the labels firebrick stores with the sandbox, such as its host name, ports, mise
/// setting and egress rules.
async fn firebrick_labels(name: &str) -> Vec<(String, String)> {
    sandbox_spec(name)
        .await
        .labels
        .into_iter()
        .filter(|(key, _)| key.starts_with("firebrick."))
        .collect()
}

/// What a sandbox looks like right after an upgrade.
struct AfterUpgrade {
    update: AfterUpdate,
    labels: Vec<(String, String)>,
    release: String,
    /// The output and exit code of a check that the marker files are gone.
    fresh_disks: (String, i32),
    secret: String,
    probes: Vec<String>,
}

async fn after_upgrade(
    client: &mut SandboxManagementServiceClient<Channel>,
    name: &str,
) -> AfterUpgrade {
    AfterUpgrade {
        update: after_update(client, name).await,
        labels: firebrick_labels(name).await,
        release: alpine_release(client, name).await,
        fresh_disks: run_command(client, name, "sh", &["-c", NO_MARKERS]).await,
        secret: print_env(client, name, "FIREBRICK_IT_TOKEN").await,
        probes: leftover_probes(name).await,
    }
}

/// Starts a sandbox from [`OLD_IMAGE`] that allows `example.com`, forwards the host port,
/// shares the host clipboard and has marker files on its root and Docker disks.
async fn start_old_sandbox(
    client: &mut SandboxManagementServiceClient<Channel>,
    name: &str,
    host: u16,
) {
    let request = StartSandboxRequest {
        network: Some(allow_policy(true, &["example.com"])),
        ports: Some(forward_ports(host, GUEST_PORT)),
        clipboard: Some(true),
        ..old_image_request(name)
    };

    start_with_ports(client, request).await;
    write_markers(client, name).await;
}

#[tokio::test]
async fn upgrade_of_running_sandbox_moves_it_to_the_new_image_and_keeps_its_settings() {
    let name: &str = &sandbox_name("upgrade-running");
    remove_sandbox_and_snapshots(name).await;

    let dir = tempfile::tempdir().unwrap();
    let store = store_with_test_secret(dir.path());
    let daemon = TestDaemon::start_with_secrets("upgrade-running", store).await;
    let mut client = daemon.client().await;
    let host = free_port().await;
    start_old_sandbox(&mut client, name, host).await;
    let (before, labels) = (
        kept_settings(&mut client, name).await,
        firebrick_labels(name).await,
    );

    let upgraded = upgrade(&mut client, name, UPGRADE_IMAGE).await;

    let after = after_upgrade(&mut client, name).await;
    check_forward_both_ways(&mut client, name, host).await;

    daemon.stop().await;
    remove_sandbox_and_snapshots(name).await;

    let expected = SandboxUpgraded {
        upgraded: true,
        old_image: OLD_IMAGE.to_string(),
        new_image: UPGRADE_IMAGE.to_string(),
    };
    assert_eq!(upgraded.unwrap(), expected);
    assert_upgraded_and_kept(&after, &before, &labels);
}

/// Asserts that the sandbox runs the new image with fresh disks after the upgrade, and kept
/// its settings, labels, egress rules and secrets.
fn assert_upgraded_and_kept(
    after: &AfterUpgrade,
    before: &KeptSettings,
    labels: &[(String, String)],
) {
    assert_eq!(after.update.status, Some(SandboxStatus::Running));
    assert_kept_with_clipboard(&after.update.settings, before);
    let image_label = |image: &str| (sandboxes::IMAGE_LABEL.to_string(), image.to_string());
    assert!(labels.contains(&image_label(OLD_IMAGE)), "{labels:?}");
    let clipboard_label = (clipboard::ENABLED_LABEL.to_string(), "true".to_string());
    assert!(labels.contains(&clipboard_label), "{labels:?}");
    assert!(
        after.labels.contains(&image_label(UPGRADE_IMAGE)),
        "{:?}",
        after.labels
    );
    assert_eq!(after.labels, with_image_label(labels, UPGRADE_IMAGE));
    let network = &after.update.stored_network;
    assert!(network.contains("example.com"), "{network}");
    assert!(
        after.update.snapshots.is_empty(),
        "{:?}",
        after.update.snapshots
    );
    assert!(after.release.starts_with("3.22"), "{:?}", after.release);
    // The root filesystem and the Docker disk start out fresh.
    assert_eq!(after.fresh_disks.1, 0, "{:?}", after.fresh_disks.0);
    assert!(after.probes.is_empty(), "{:?}", after.probes);
    assert!(
        after.secret.contains("$MSB_FIREBRICK_IT_TOKEN"),
        "{:?}",
        after.secret
    );
}

/// Returns the labels with the image label set to the image, as an upgrade to it leaves them.
fn with_image_label(labels: &[(String, String)], image: &str) -> Vec<(String, String)> {
    labels
        .iter()
        .map(|(key, value)| match key.as_str() {
            sandboxes::IMAGE_LABEL => (key.clone(), image.to_string()),
            _ => (key.clone(), value.clone()),
        })
        .collect()
}

#[tokio::test]
async fn upgrade_of_stopped_sandbox_keeps_it_stopped() {
    let name: &str = &sandbox_name("upgrade-stopped");
    remove_sandbox_and_snapshots(name).await;

    let daemon = TestDaemon::start("upgrade-stopped").await;
    let mut client = daemon.client().await;
    start_and_wait(&mut client, old_image_request(name)).await;
    let before = kept_settings(&mut client, name).await;
    stop_sandbox(&mut client, name).await;

    let upgraded = upgrade(&mut client, name, UPGRADE_IMAGE).await;

    let after = after_update(&mut client, name).await;
    start_existing(&mut client, name).await;
    let release = alpine_release(&mut client, name).await;

    daemon.stop().await;
    remove_sandbox_and_snapshots(name).await;

    assert!(upgraded.unwrap().upgraded);
    assert_eq!(after.status, Some(SandboxStatus::Stopped));
    assert_eq!(after.settings, before);
    assert!(after.snapshots.is_empty(), "{:?}", after.snapshots);
    assert!(release.starts_with("3.22"), "{release:?}");
}

#[tokio::test]
async fn upgrade_to_the_image_the_sandbox_has_leaves_it_alone() {
    let name: &str = &sandbox_name("upgrade-same");
    remove_sandbox_and_snapshots(name).await;

    let daemon = TestDaemon::start("upgrade-same").await;
    let mut client = daemon.client().await;
    let request = StartSandboxRequest {
        image: UPGRADE_IMAGE.to_string(),
        ..start_request(name)
    };
    start_and_wait(&mut client, request).await;
    let boot_before = boot_id(&mut client, name).await;

    let upgraded = upgrade(&mut client, name, UPGRADE_IMAGE).await;

    let boot_after = boot_id(&mut client, name).await;
    let probes = leftover_probes(name).await;

    daemon.stop().await;
    remove_sandbox_and_snapshots(name).await;

    assert_eq!(
        upgraded.unwrap(),
        SandboxUpgraded {
            upgraded: false,
            old_image: UPGRADE_IMAGE.to_string(),
            new_image: UPGRADE_IMAGE.to_string(),
        }
    );
    assert_eq!(boot_after, boot_before);
    assert!(probes.is_empty(), "{probes:?}");
}

#[tokio::test]
async fn upgrade_to_an_unknown_image_leaves_the_sandbox_running() {
    let name: &str = &sandbox_name("upgrade-unknown");
    remove_sandbox_and_snapshots(name).await;

    let daemon = TestDaemon::start("upgrade-unknown").await;
    let mut client = daemon.client().await;
    start_and_wait(&mut client, old_image_request(name)).await;
    let boot_before = boot_id(&mut client, name).await;

    let status = upgrade(&mut client, name, "alpine:0.0.404").await;

    let running = sandbox_status(&mut client, name).await;
    let boot_after = boot_id(&mut client, name).await;
    let release = alpine_release(&mut client, name).await;
    let probes = leftover_probes(name).await;

    daemon.stop().await;
    remove_sandbox_and_snapshots(name).await;

    assert!(probes.is_empty(), "{probes:?}");
    let status = status.unwrap_err();
    assert_eq!(status.code(), Code::NotFound);
    assert_eq!(
        status.message(),
        "failed to create sandbox: image alpine:0.0.404 doesn't exist"
    );
    assert_eq!(running, Some(SandboxStatus::Running));
    assert_eq!(boot_after, boot_before);
    assert!(release.starts_with("3.21"), "{release:?}");
}

#[tokio::test]
async fn upgrade_refuses_a_paused_sandbox() {
    let name: &str = &sandbox_name("upgrade-paused");
    remove_sandbox_and_snapshots(name).await;

    let daemon = TestDaemon::start("upgrade-paused").await;
    let mut client = daemon.client().await;
    start_and_wait(&mut client, old_image_request(name)).await;
    let sb = Sandbox::get(name).await.unwrap();
    pause(&sb).await.unwrap();

    let status = upgrade(&mut client, name, UPGRADE_IMAGE).await;
    let paused = sandbox_status(&mut client, name).await;

    resume(&sb).await.unwrap();
    daemon.stop().await;
    remove_sandbox_and_snapshots(name).await;

    let status = status.unwrap_err();
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert_eq!(
        status.message(),
        format!("sandbox {name} is paused; resume or stop it before upgrading")
    );
    assert_eq!(paused, Some(SandboxStatus::Paused));
}

#[tokio::test]
async fn upgrade_returns_not_found_for_unknown_sandbox() {
    let daemon = TestDaemon::start("upgrade-missing").await;
    let mut client = daemon.client().await;

    let status = upgrade(&mut client, &sandbox_name("upgrade-missing"), UPGRADE_IMAGE).await;

    daemon.stop().await;

    assert_eq!(status.unwrap_err().code(), Code::NotFound);
}

/// Returns a host port that nothing listens on at the moment.
async fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();

    listener.local_addr().unwrap().port()
}

/// Returns the ports message that forwards the host port to the guest port.
fn forward_ports(host: u16, guest: u16) -> PortForwards {
    PortForwards {
        ports: vec![PortForward {
            host: host.into(),
            guest: guest.into(),
        }],
    }
}

/// Connects to the forwarded host port until the guest listens, sends `sent` and returns once
/// the guest answers with `expected`. A connection closes right away while nothing listens in
/// the guest.
async fn exchange_through_forward(host: u16, sent: &[u8], expected: &str) {
    let deadline = Instant::now() + STATUS_TIMEOUT;

    while Instant::now() < deadline {
        if received_reply(host, sent, expected).await {
            return;
        }

        sleep(POLL_INTERVAL).await;
    }

    panic!("the guest never answered {expected:?} on localhost:{host}");
}

/// Sends the bytes over a new connection to the host port and returns whether the reply
/// contains `expected` before the connection closes.
async fn received_reply(host: u16, sent: &[u8], expected: &str) -> bool {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", host)).await else {
        return false;
    };
    let _ = stream.write_all(sent).await;
    let mut received = Vec::new();
    let mut buffer = [0; 256];

    while let Ok(Ok(count @ 1..)) =
        tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buffer)).await
    {
        received.extend_from_slice(&buffer[..count]);

        if String::from_utf8_lossy(&received).contains(expected) {
            return true;
        }
    }

    false
}

/// Reads session output until it contains `expected`.
async fn wait_for_output(responses: &mut Streaming<AttachResponse>, expected: &str) {
    let mut output = Vec::new();

    while !String::from_utf8_lossy(&output).contains(expected) {
        let response = tokio::time::timeout(STATUS_TIMEOUT, responses.message())
            .await
            .expect("the session printed nothing in time")
            .expect("attach stream failed")
            .expect("the session ended before printing the expected output");

        if let Some(attach_response::Message::Output(data)) = response.message {
            output.extend(data);
        }
    }
}

/// Port the guest server listens on in the port forwarding test.
const GUEST_PORT: u16 = 4000;

/// Runs `nc -l` on the guest port and checks that bytes go both ways between it and a
/// connection to the forwarded host port.
async fn check_forward_both_ways(
    client: &mut SandboxManagementServiceClient<Channel>,
    sandbox: &str,
    host: u16,
) {
    // nc sends its input to the connection and prints what it receives.
    let port = GUEST_PORT.to_string();
    let (tx, mut responses) = open_session(
        client,
        attach_start(sandbox, "nc", &["-l", "-p", &port], DEFAULT_SIZE),
    )
    .await;
    tx.send(attach_input(b"from-guest\n")).await.unwrap();

    exchange_through_forward(host, b"from-host\n", "from-guest").await;
    wait_for_output(&mut responses, "from-host").await;
}

/// Starts the sandbox with the request, waits until it runs and returns the daemon's response.
async fn start_with_ports(
    client: &mut SandboxManagementServiceClient<Channel>,
    request: StartSandboxRequest,
) -> SandboxStarted {
    let name = request.name.clone();
    let started = start(client, request)
        .await
        .expect("failed to start sandbox");
    wait_for_status(client, &name, SandboxStatus::Running).await;

    started
}

#[tokio::test]
async fn forwards_host_port_to_sandbox_and_closes_removed_port() {
    let name: &str = &sandbox_name("port-forward");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("port-forward").await;
    let mut client = daemon.client().await;
    let host = free_port().await;
    let request = StartSandboxRequest {
        // busybox in alpine has `nc`.
        image: "alpine:3.22".to_string(),
        ports: Some(forward_ports(host, GUEST_PORT)),
        ..start_request(name)
    };

    let started = start_with_ports(&mut client, request.clone()).await;
    check_forward_both_ways(&mut client, name, host).await;
    let restarted = start_with_ports(
        &mut client,
        StartSandboxRequest {
            ports: Some(PortForwards::default()),
            ..request
        },
    )
    .await;
    let closed = TcpStream::connect(("127.0.0.1", host)).await;

    daemon.stop().await;
    remove_sandbox(name).await;

    assert_eq!(started.forwards, forward_ports(host, GUEST_PORT).ports);
    assert!(started.failed_forwards.is_empty());
    assert!(restarted.forwards.is_empty());
    assert!(closed.is_err(), "localhost:{host} should be closed");
}

#[tokio::test]
async fn forwards_ports_to_sandbox_without_network() {
    let name: &str = &sandbox_name("port-forward-offline");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("port-forward-offline").await;
    let mut client = daemon.client().await;
    let host = free_port().await;
    let request = StartSandboxRequest {
        // busybox in alpine has `nc`.
        image: "alpine:3.22".to_string(),
        network: Some(disabled_network()),
        ports: Some(forward_ports(host, GUEST_PORT)),
        ..start_request(name)
    };

    let started = start_with_ports(&mut client, request).await;
    // The forwards go through microsandbox's SSH server, which needs no guest network.
    check_forward_both_ways(&mut client, name, host).await;

    daemon.stop().await;
    remove_sandbox(name).await;

    assert!(started.failed_forwards.is_empty());
}

#[tokio::test]
async fn update_network_keeps_extra_mounts_and_port_forwards() {
    let name: &str = &sandbox_name("update-mounts-ports");
    remove_sandbox_and_snapshots(name).await;

    let readonly = test_mount_dir(name, "ro");
    std::fs::write(readonly.join("from-host.txt"), "hello from host").unwrap();
    let daemon = TestDaemon::start("update-mounts-ports").await;
    let mut client = daemon.client().await;
    let host = free_port().await;
    let request = StartSandboxRequest {
        // busybox in alpine has `nc`.
        image: "alpine:3.22".to_string(),
        mounts: vec![mount(&readonly, "/mnt/ro", true)],
        ports: Some(forward_ports(host, GUEST_PORT)),
        ..start_request(name)
    };
    start_with_ports(&mut client, request).await;

    let updated = update_network(&mut client, name, allow_policy(true, &["example.org"])).await;
    let script = "cat /mnt/ro/from-host.txt && ! touch /mnt/ro/from-guest.txt";
    let (output, code) = run_command(&mut client, name, "sh", &["-c", script]).await;
    check_forward_both_ways(&mut client, name, host).await;

    daemon.stop().await;
    remove_sandbox_and_snapshots(name).await;
    let _ = std::fs::remove_dir_all(readonly);

    assert!(matches!(updated, Ok(true)), "{updated:?}");
    assert_eq!(code, 0, "unexpected output: {output:?}");
    assert!(output.contains("hello from host"), "{output:?}");
    assert!(output.contains("Read-only file system"), "{output:?}");
}

/// Writes a line to the URL relay's FIFO in the sandbox, waiting until the relay created it.
/// The test image has no `firebrick-open` stand-in, so this writes to the FIFO directly.
async fn write_to_relay(
    client: &mut SandboxManagementServiceClient<Channel>,
    sandbox: &str,
    line: &str,
) {
    let script = format!(
        "for i in $(seq 100); do [ -p {fifo} ] && break; sleep 0.1; done; \
         timeout 5 sh -c 'printf \"%s\\n\" \"$1\" > {fifo}' _ \"$1\"",
        fifo = open::FIFO_PATH
    );
    let (output, code) = run_command(client, sandbox, "sh", &["-c", &script, "_", line]).await;

    assert_eq!(code, 0, "failed to write to the relay: {output:?}");
}

/// Waits until the daemon opened the URLs.
async fn wait_for_opened(daemon: &TestDaemon, expected: &[&str]) {
    let deadline = Instant::now() + Duration::from_secs(10);

    while daemon.opener.urls() != expected {
        assert!(
            Instant::now() < deadline,
            "expected {expected:?} to be opened, got {:?}",
            daemon.opener.urls()
        );
        sleep(POLL_INTERVAL).await;
    }
}

/// Counts the URL relays that run in the sandbox.
async fn relay_count(client: &mut SandboxManagementServiceClient<Channel>, sandbox: &str) -> usize {
    // Matches open::RELAY_NAME; the bracket keeps it from matching this command's own grep.
    let script = "grep -l -a -- 'firebrick-open-rela[y]' /proc/[0-9]*/cmdline 2>/dev/null | wc -l";
    let (output, code) = run_command(client, sandbox, "sh", &["-c", script]).await;

    assert_eq!(code, 0, "failed to count relays: {output:?}");
    output.trim().parse().expect("relay count isn't a number")
}

#[tokio::test]
async fn opens_urls_from_the_sandbox_on_the_host() {
    let name: &str = &sandbox_name("open-url");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("open-url").await;
    let mut client = daemon.client().await;
    start_running_sandbox(&mut client, name).await;

    write_to_relay(&mut client, name, "file:///etc/passwd").await;
    write_to_relay(&mut client, name, "https://example.com").await;
    wait_for_opened(&daemon, &["https://example.com/"]).await;

    // Starting the sandbox again and attaching more sessions reuses the relay.
    start_running_sandbox(&mut client, name).await;
    run_command(&mut client, name, "true", &[]).await;
    assert_eq!(relay_count(&mut client, name).await, 1);

    daemon.stop().await;
    remove_sandbox(name).await;
}

#[tokio::test]
async fn url_relay_comes_back_after_restart() {
    let name: &str = &sandbox_name("open-url-restart");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("open-url-restart").await;
    let mut client = daemon.client().await;
    start_running_sandbox(&mut client, name).await;

    restart_sandbox(&mut client, name).await;
    write_to_relay(&mut client, name, "https://example.com/after-restart").await;
    wait_for_opened(&daemon, &["https://example.com/after-restart"]).await;
    assert_eq!(relay_count(&mut client, name).await, 1);

    daemon.stop().await;
    remove_sandbox(name).await;
}

#[tokio::test]
async fn url_relay_follows_updated_network_rules() {
    let name: &str = &sandbox_name("open-url-network");
    remove_sandbox_and_snapshots(name).await;

    let daemon = TestDaemon::start("open-url-network").await;
    let mut client = daemon.client().await;
    start_running_sandbox(&mut client, name).await;

    let updated = update_network(&mut client, name, allow_policy(true, &["example.org"])).await;
    write_to_relay(&mut client, name, "https://example.com/not-allowed").await;
    write_to_relay(&mut client, name, "http://10.0.0.1:3000/").await;
    write_to_relay(&mut client, name, "https://example.org/allowed").await;
    wait_for_opened(&daemon, &["https://example.org/allowed"]).await;
    let relays = relay_count(&mut client, name).await;

    daemon.stop().await;
    remove_sandbox_and_snapshots(name).await;

    assert!(matches!(updated, Ok(true)), "{updated:?}");
    assert_eq!(relays, 1);
}

/// Returns a URL that sends the browser back to `localhost:<port>`, as an OAuth login does.
fn login_url(port: u16) -> String {
    format!("https://example.com/login?redirect_uri=http%3A%2F%2Flocalhost%3A{port}%2Fcb")
}

#[tokio::test]
async fn forwards_the_callback_port_of_an_opened_url() {
    let name: &str = &sandbox_name("open-url-callback");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("open-url-callback").await;
    let mut client = daemon.client().await;
    start_alpine_sandbox(&mut client, name).await;
    let port = free_port().await;
    let (tx, mut responses) = open_session(
        &mut client,
        attach_start(name, "nc", &["-l", "-p", &port.to_string()], DEFAULT_SIZE),
    )
    .await;
    tx.send(attach_input(b"from-guest\n")).await.unwrap();

    write_to_relay(&mut client, name, &login_url(port)).await;
    wait_for_opened(&daemon, &[&login_url(port)]).await;
    exchange_through_forward(port, b"from-host\n", "from-guest").await;
    wait_for_output(&mut responses, "from-host").await;
    stop_sandbox(&mut client, name).await;
    let closed = TcpStream::connect(("127.0.0.1", port)).await;

    daemon.stop().await;
    remove_sandbox(name).await;

    assert!(
        closed.is_err(),
        "localhost:{port} should close with the sandbox"
    );
}

/// Asks the daemon to forward the host port to the guest port of the sandbox.
async fn forward_port(
    client: &mut SandboxManagementServiceClient<Channel>,
    name: &str,
    host: u16,
    guest: u16,
) -> Result<ForwardPortResponse, tonic::Status> {
    let request = ForwardPortRequest {
        name: name.to_string(),
        port: Some(PortForward {
            host: host.into(),
            guest: guest.into(),
        }),
    };

    client.forward_port(request).await.map(|r| r.into_inner())
}

/// Asks the daemon to remove the forward of the host port from the sandbox.
async fn remove_port(
    client: &mut SandboxManagementServiceClient<Channel>,
    name: &str,
    host: u16,
) -> Result<RemovePortResponse, tonic::Status> {
    let request = RemovePortRequest {
        name: name.to_string(),
        host: host.into(),
    };

    client.remove_port(request).await.map(|r| r.into_inner())
}

/// Returns the ports label microsandbox stores for the sandbox.
async fn ports_label(name: &str) -> Option<String> {
    sandbox_spec(name)
        .await
        .labels
        .get(firebrick_daemon::forward::PORTS_LABEL)
        .cloned()
}

/// Starts an alpine sandbox, whose busybox has `nc`, and waits until it runs.
async fn start_alpine_sandbox(
    client: &mut SandboxManagementServiceClient<Channel>,
    name: &str,
) -> StartSandboxRequest {
    let request = StartSandboxRequest {
        image: "alpine:3.22".to_string(),
        ..start_request(name)
    };
    start_with_ports(client, request.clone()).await;

    request
}

#[tokio::test]
async fn forward_port_and_remove_port_change_forwards_of_running_sandbox() {
    let name: &str = &sandbox_name("port-live");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("port-live").await;
    let mut client = daemon.client().await;
    let host = free_port().await;
    start_alpine_sandbox(&mut client, name).await;

    let forwarded = forward_port(&mut client, name, host, GUEST_PORT).await;
    check_forward_both_ways(&mut client, name, host).await;
    let removed = remove_port(&mut client, name, host).await;
    let closed = TcpStream::connect(("127.0.0.1", host)).await;
    let label = ports_label(name).await;

    daemon.stop().await;
    remove_sandbox(name).await;

    assert!(forwarded.unwrap().running);
    assert!(removed.unwrap().running);
    assert!(closed.is_err(), "localhost:{host} should be closed");
    assert_eq!(label.as_deref(), Some(""));
}

#[tokio::test]
async fn port_rpcs_reject_busy_and_unknown_ports_without_changing_the_label() {
    let name: &str = &sandbox_name("port-errors");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("port-errors").await;
    let mut client = daemon.client().await;
    let busy = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let busy_port = busy.local_addr().unwrap().port();
    start_alpine_sandbox(&mut client, name).await;

    let busy_status = forward_port(&mut client, name, busy_port, GUEST_PORT).await;
    let unknown_status = remove_port(&mut client, name, busy_port).await;
    let label = ports_label(name).await;

    daemon.stop().await;
    remove_sandbox(name).await;

    let busy_status = busy_status.unwrap_err();
    assert_eq!(
        busy_status.code(),
        Code::FailedPrecondition,
        "{busy_status:?}"
    );
    let unknown_status = unknown_status.unwrap_err();
    assert_eq!(unknown_status.code(), Code::NotFound, "{unknown_status:?}");
    assert_eq!(
        unknown_status.message(),
        format!("port {busy_port} isn't forwarded for sandbox {name}")
    );
    assert_eq!(label.as_deref(), Some(""));
}

#[tokio::test]
async fn forward_port_on_stopped_sandbox_opens_when_it_starts() {
    let name: &str = &sandbox_name("port-stopped");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("port-stopped").await;
    let mut client = daemon.client().await;
    let host = free_port().await;
    let request = start_alpine_sandbox(&mut client, name).await;
    stop_sandbox(&mut client, name).await;

    let forwarded = forward_port(&mut client, name, host, GUEST_PORT).await;
    let closed_while_stopped = TcpStream::connect(("127.0.0.1", host)).await;
    // Without ports, starting keeps the stored ones.
    let started = start_with_ports(&mut client, request).await;
    check_forward_both_ways(&mut client, name, host).await;

    daemon.stop().await;
    remove_sandbox(name).await;

    assert!(!forwarded.unwrap().running);
    assert!(closed_while_stopped.is_err());
    assert_eq!(started.forwards, forward_ports(host, GUEST_PORT).ports);
}

#[tokio::test]
async fn forward_port_fails_for_a_stored_port_that_is_still_busy() {
    let name: &str = &sandbox_name("port-stored-busy");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("port-stored-busy").await;
    let mut client = daemon.client().await;
    let busy = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let busy_port = busy.local_addr().unwrap().port();
    let request = StartSandboxRequest {
        image: "alpine:3.22".to_string(),
        ports: Some(forward_ports(busy_port, GUEST_PORT)),
        ..start_request(name)
    };
    let started = start_with_ports(&mut client, request).await;

    let status = forward_port(&mut client, name, busy_port, GUEST_PORT).await;

    daemon.stop().await;
    remove_sandbox(name).await;

    assert_eq!(started.failed_forwards.len(), 1);
    let status = status.unwrap_err();
    assert_eq!(status.code(), Code::FailedPrecondition, "{status:?}");
}

/// Returns the host socket fbkd listens on for the sandbox's network log, a pipe on Windows.
fn netlog_socket(name: &str) -> PathBuf {
    let dir = firebrick_utils::netlog_socket_dir()
        .expect("can't name the network log endpoints")
        .expect("set XDG_RUNTIME_DIR to an absolute path for the network log tests on Linux");
    ipc::endpoint(&dir, name).expect("the sandbox name is too long for an endpoint")
}

/// Returns the file fbkd records the sandbox's network activity in.
fn netlog_record(name: &str) -> PathBuf {
    firebrick_utils::network_log_dir()
        .expect("no state directory for network records")
        .join(format!("{name}.json"))
}

/// Whether the sandbox has a stream route from the network log port to its socket.
async fn has_netlog_route(name: &str) -> bool {
    sandbox_spec(name).await.vsock.routes.iter().any(|route| {
        route.port == netlog::GUEST_PORT
            && route.socket_type == VsockSocketType::Stream
            && route.host_socket == netlog_socket(name)
    })
}

/// Writes the lines to the sandbox's network log socket, as the logger in the guest does.
async fn write_netlog_lines(name: &str, lines: &[&str]) {
    let mut stream = ipc::connect(&netlog_socket(name))
        .await
        .expect("fbkd doesn't listen on the network log socket");

    stream
        .write_all(format!("{}\n", lines.join("\n")).as_bytes())
        .await
        .unwrap();
}

/// Waits until the sandbox's record file has the expected number of DNS and connection
/// entries, and returns it.
async fn wait_for_record(name: &str, dns: usize, connections: usize) -> netlog::Record {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut record = read_record(name);

    while record.as_ref().map(|r| (r.dns.len(), r.connections.len())) != Some((dns, connections)) {
        assert!(Instant::now() < deadline, "record of {name}: {record:?}");
        sleep(POLL_INTERVAL).await;
        record = read_record(name);
    }

    record.unwrap()
}

/// Reads the sandbox's record file, if it exists and parses.
fn read_record(name: &str) -> Option<netlog::Record> {
    let content = std::fs::read(netlog_record(name)).ok()?;

    serde_json::from_slice(&content).ok()
}

const DNS_LINE: &str = r#"{"kind":"dns","name":"example.com","addresses":["93.184.215.14"],"at":"2026-10-11T14:02:11Z"}"#;
const CONNECT_LINE: &str = r#"{"kind":"connect","protocol":"tcp","ip":"93.184.215.14","port":443}"#;

#[tokio::test]
async fn network_log_records_lines_across_restarts_until_the_sandbox_is_removed() {
    let name: &str = &sandbox_name("netlog");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("netlog").await;
    let mut client = daemon.client().await;
    start_running_sandbox(&mut client, name).await;
    let routed = has_netlog_route(name).await;
    write_netlog_lines(
        name,
        &[DNS_LINE, "not json", &"x".repeat(5000), CONNECT_LINE],
    )
    .await;
    let before = wait_for_record(name, 1, 1).await;
    daemon.stop().await;

    let daemon = TestDaemon::start_restoring_listeners("netlog-restarted").await;
    let mut client = daemon.client().await;
    write_netlog_lines(name, &[DNS_LINE]).await;
    let after = wait_for_record_count(name, 2).await;
    remove_with_force(&mut client, name).await;
    let socket_left = netlog_socket(name).exists();
    let record_left = netlog_record(name).exists();

    daemon.stop().await;
    remove_sandbox(name).await;

    assert!(routed, "sandbox has no network log route");
    assert_eq!(before.dns[0].name, "example.com");
    assert_eq!(before.connections[0].ports, [443].into());
    assert_eq!(after.dns[0].first_seen, before.dns[0].first_seen);
    assert!(!socket_left, "the network log socket is left");
    assert!(!record_left, "the network record is left");
}

/// Waits until the DNS entry of the sandbox's record has the count.
async fn wait_for_record_count(name: &str, count: u64) -> netlog::Record {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut record = read_record(name);

    while record.as_ref().and_then(|r| r.dns.first()).map(|e| e.count) != Some(count) {
        assert!(Instant::now() < deadline, "record of {name}: {record:?}");
        sleep(POLL_INTERVAL).await;
        record = read_record(name);
    }

    record.unwrap()
}

/// Removes the sandbox through the daemon, stopping it first.
async fn remove_with_force(client: &mut SandboxManagementServiceClient<Channel>, name: &str) {
    client
        .remove_sandbox(RemoveSandboxRequest {
            name: name.to_string(),
            force: true,
        })
        .await
        .expect("failed to remove sandbox");
}

#[tokio::test]
async fn update_network_adds_the_network_log_route_to_a_sandbox_without_one() {
    let name: &str = &sandbox_name("netlog-update");
    remove_sandbox_and_snapshots(name).await;
    // Like a sandbox fbkd created before network logging, with a Docker disk but no route.
    Sandbox::builder(name)
        .image(TEST_IMAGE)
        .volume("/var/lib/docker", |m| m.owned_with(|v| v.disk().size(1024)))
        .detached(true)
        .create()
        .await
        .expect("failed to create sandbox without a route");
    let routed_before = has_netlog_route(name).await;

    let daemon = TestDaemon::start("netlog-update").await;
    let mut client = daemon.client().await;
    let activity_before = list_network_activity(&mut client, name).await;
    let updated = update_network(&mut client, name, allow_policy(true, &["example.com"])).await;
    let routed_after = has_netlog_route(name).await;
    let listening = ipc::is_served(&netlog_socket(name)).await;
    let activity_after = list_network_activity(&mut client, name).await;

    daemon.stop().await;
    remove_sandbox_and_snapshots(name).await;

    assert!(!routed_before);
    let status = activity_before.unwrap_err();
    assert_eq!(status.code(), Code::FailedPrecondition, "{status:?}");
    assert_eq!(activity_after.unwrap(), []);
    assert!(updated.unwrap());
    assert!(routed_after, "recreated sandbox has no network log route");
    assert!(listening, "fbkd doesn't listen for the recreated sandbox");
}

/// Returns the network activity the daemon recorded for the sandbox.
async fn list_network_activity(
    client: &mut SandboxManagementServiceClient<Channel>,
    name: &str,
) -> Result<Vec<NetworkActivity>, Status> {
    client
        .list_network_activity(ListNetworkActivityRequest {
            name: name.to_string(),
        })
        .await
        .map(|response| response.into_inner().entries)
}

/// Waits until the daemon reports `count` entries of network activity for the sandbox, and
/// returns them.
async fn wait_for_activity(
    client: &mut SandboxManagementServiceClient<Channel>,
    name: &str,
    count: usize,
) -> Vec<NetworkActivity> {
    let deadline = Instant::now() + Duration::from_secs(10);

    loop {
        let entries = list_network_activity(client, name)
            .await
            .expect("failed to list the network activity");

        if entries.len() == count {
            return entries;
        }

        assert!(Instant::now() < deadline, "activity of {name}: {entries:?}");
        sleep(POLL_INTERVAL).await;
    }
}

/// Returns the entry of the kind, failing when there's none.
fn entry_of_kind(entries: &[NetworkActivity], kind: NetworkActivityKind) -> &NetworkActivity {
    entries
        .iter()
        .find(|entry| entry.kind() == kind)
        .unwrap_or_else(|| panic!("no {kind:?} entry in {entries:?}"))
}

#[tokio::test]
async fn list_network_activity_returns_not_found_for_unknown_sandbox() {
    let daemon = TestDaemon::start("activity-unknown").await;
    let mut client = daemon.client().await;

    let status = list_network_activity(&mut client, &sandbox_name("activity-unknown")).await;

    daemon.stop().await;
    assert_eq!(status.unwrap_err().code(), Code::NotFound);
}

#[tokio::test]
async fn list_network_activity_returns_the_recorded_events_with_the_policy() {
    let name: &str = &sandbox_name("activity");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("activity").await;
    let mut client = daemon.client().await;
    let request = StartSandboxRequest {
        network: Some(allow_policy(true, &["example.com"])),
        ..start_request(name)
    };
    start_and_wait(&mut client, request).await;
    let empty = list_network_activity(&mut client, name).await;
    write_netlog_lines(name, &[DNS_LINE, CONNECT_LINE]).await;
    let entries = wait_for_activity(&mut client, name, 2).await;

    daemon.stop().await;
    remove_sandbox(name).await;

    assert_eq!(empty.unwrap(), []);
    let dns = entry_of_kind(&entries, NetworkActivityKind::Dns);
    let tcp = entry_of_kind(&entries, NetworkActivityKind::Tcp);
    assert_eq!(dns.destination, "example.com");
    assert_eq!(dns.addresses, ["93.184.215.14"]);
    assert_eq!(dns.count, 1);
    assert!(dns.allowed, "example.com is allowed by a domain rule");
    assert_eq!(tcp.destination, "93.184.215.14");
    assert_eq!(tcp.ports, [443]);
    assert!(!tcp.allowed, "no IP rule allows 93.184.215.14");
}

/// Returns the host socket fbkd listens on for the sandbox's clipboard, a pipe on Windows.
fn clipboard_socket(name: &str) -> PathBuf {
    let dir = firebrick_utils::clipboard_socket_dir()
        .expect("can't name the clipboard endpoints")
        .expect("set XDG_RUNTIME_DIR to an absolute path for the clipboard tests on Linux");
    ipc::endpoint(&dir, name).expect("the sandbox name is too long for an endpoint")
}

/// Whether the sandbox has a stream route from the clipboard port to its socket.
fn has_clipboard_route(name: &str, spec: &SandboxSpec) -> bool {
    spec.vsock.routes.iter().any(|route| {
        route.port == clipboard::GUEST_PORT
            && route.socket_type == VsockSocketType::Stream
            && route.host_socket == clipboard_socket(name)
    })
}

/// An image with `python3`, which the `wl-paste` and `xclip` stand-ins run on.
const PYTHON_IMAGE: &str = "python:3.13-slim-trixie";

/// The stand-in of the `firebrick-base` image.
const CLIPBOARD_STAND_IN: &[u8] = include_bytes!("../../../image/firebrick-clipboard");

/// Installs the stand-in as `wl-paste`, `wl-copy` and `xclip`, like the `firebrick-base` image
/// does.
async fn install_clipboard_stand_in(sb: &Sandbox) {
    let script = "cat > /usr/local/bin/firebrick-clipboard \
        && chmod 755 /usr/local/bin/firebrick-clipboard \
        && ln -sf firebrick-clipboard /usr/local/bin/wl-paste \
        && ln -sf firebrick-clipboard /usr/local/bin/wl-copy \
        && ln -sf firebrick-clipboard /usr/local/bin/xclip";
    let output = sb
        .exec_with("sh", |exec| {
            exec.args(["-c", script]).stdin_bytes(CLIPBOARD_STAND_IN)
        })
        .await
        .expect("failed to install the stand-in");

    assert!(output.status().success, "{:?}", output.stderr());
}

/// The exit code, stdout and stderr of a command.
type Pasted = (i32, Vec<u8>, String);

/// Runs the shell command in the sandbox without a terminal, so binary output stays intact.
async fn paste(sb: &Sandbox, command: &str) -> Pasted {
    let output = sb
        .exec("sh", ["-c", command])
        .await
        .expect("failed to run the stand-in");

    (
        output.status().code,
        output.stdout_bytes().to_vec(),
        String::from_utf8_lossy(output.stderr_bytes()).into_owned(),
    )
}

/// Returns a successful paste of the output.
fn pasted(stdout: &[u8]) -> Pasted {
    (0, stdout.to_vec(), String::new())
}

/// Creates a sandbox from [`PYTHON_IMAGE`] with the clipboard setting and the stand-in.
async fn start_clipboard_sandbox(
    client: &mut SandboxManagementServiceClient<Channel>,
    name: &str,
    clipboard: bool,
) -> Sandbox {
    let request = StartSandboxRequest {
        image: PYTHON_IMAGE.to_string(),
        clipboard: Some(clipboard),
        ..start_request(name)
    };
    start_and_wait(client, request).await;
    let sb = Sandbox::get(name)
        .await
        .expect("failed to get the sandbox")
        .connect()
        .await
        .expect("failed to connect to the sandbox");
    install_clipboard_stand_in(&sb).await;

    sb
}

#[tokio::test]
async fn stand_ins_paste_the_host_clipboard_in_a_sandbox_with_the_setting() {
    let name: &str = &sandbox_name("clipboard");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("clipboard").await;
    let mut client = daemon.client().await;
    let sb = start_clipboard_sandbox(&mut client, name, true).await;
    let (spec, label) = (sandbox_spec(name).await, firebrick_labels(name).await);

    let mut pastes = Vec::new();
    for (command, _) in PASTES {
        pastes.push(paste(&sb, command).await);
    }
    let primary = paste(&sb, "xclip -selection primary -o").await;
    daemon.stop().await;
    // The route stays, but nothing listens on it anymore.
    let unreachable = paste(&sb, "wl-paste").await;
    remove_sandbox(name).await;

    let enabled = (clipboard::ENABLED_LABEL.to_string(), "true".to_string());
    assert!(label.contains(&enabled), "{label:?}");
    assert!(has_clipboard_route(name, &spec));
    let expected: Vec<Pasted> = PASTES.iter().map(|(_, out)| pasted(out)).collect();
    assert_eq!(pastes, expected);
    assert_eq!(primary, (2, vec![], XCLIP_USAGE.to_string()));
    let message = "wl-paste: couldn't reach the host clipboard\n";
    assert_eq!(unreachable, (1, vec![], message.to_string()));
}

/// What `xclip` prints for a form the stand-in doesn't support.
const XCLIP_USAGE: &str = "xclip: only these forms are supported: xclip -selection clipboard|c \
                           -o [-t UTF8_STRING|STRING|text/plain|image/png|TARGETS], or xclip \
                           -selection clipboard|c [-i] [-t UTF8_STRING|STRING|text/plain|\
                           image/png]\n";

/// What `wl-copy` prints for a form the stand-in doesn't support.
const WL_COPY_USAGE: &str = "wl-copy: only these forms are supported: wl-copy [--type|-t \
                             text/plain|text/plain;charset=utf-8|image/png] [text...]\n";

/// Runs the shell command in the sandbox with the input on its stdin, without a terminal.
async fn copy(sb: &Sandbox, command: &str, input: &[u8]) -> Pasted {
    let output = sb
        .exec_with("sh", |exec| {
            exec.args(["-c", command]).stdin_bytes(input.to_vec())
        })
        .await
        .expect("failed to run the stand-in");

    (
        output.status().code,
        output.stdout_bytes().to_vec(),
        String::from_utf8_lossy(output.stderr_bytes()).into_owned(),
    )
}

/// The stand-in commands that copy, their input, and what they set the host clipboard to.
const COPIES: [(&str, &[u8], Kind, &[u8]); 6] = [
    (
        "wl-copy",
        b"copied in the sandbox\n",
        Kind::Text,
        b"copied in the sandbox\n",
    ),
    ("wl-copy argument text", b"", Kind::Text, b"argument text"),
    (
        "wl-copy --type image/png",
        CLIPBOARD_PNG,
        Kind::Png,
        CLIPBOARD_PNG,
    ),
    ("xclip -selection clipboard", b"notes", Kind::Text, b"notes"),
    (
        "xclip -selection clipboard -i -t UTF8_STRING",
        "héllo".as_bytes(),
        Kind::Text,
        "héllo".as_bytes(),
    ),
    (
        "xclip -selection c -t image/png",
        CLIPBOARD_PNG,
        Kind::Png,
        CLIPBOARD_PNG,
    ),
];

#[tokio::test]
async fn stand_ins_copy_to_the_host_clipboard_in_a_sandbox_with_the_setting() {
    let name: &str = &sandbox_name("clipboard-copy");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("clipboard-copy").await;
    let mut client = daemon.client().await;
    let sb = start_clipboard_sandbox(&mut client, name, true).await;
    let labels = firebrick_labels(name).await;

    let mut copies = Vec::new();
    for (command, input, _, _) in COPIES {
        copies.push(copy(&sb, command, input).await);
    }
    let copied = daemon.copied();
    daemon.stop().await;
    let unreachable = copy(&sb, "wl-copy", b"text").await;
    remove_sandbox(name).await;

    let copy_label = (clipboard::COPY_LABEL.to_string(), "true".to_string());
    assert!(labels.contains(&copy_label), "{labels:?}");
    assert_eq!(copies, vec![pasted(b""); COPIES.len()]);
    let expected: Vec<(Kind, Vec<u8>)> = COPIES
        .iter()
        .map(|(_, _, kind, content)| (*kind, content.to_vec()))
        .collect();
    assert_eq!(copied, expected);
    let message = "wl-copy: couldn't reach the host clipboard\n";
    assert_eq!(unreachable, (1, vec![], message.to_string()));
}

#[tokio::test]
async fn sandbox_created_before_copying_can_paste_but_not_copy_after_fbkd_restarts() {
    let name: &str = &sandbox_name("clipboard-paste-only");
    remove_sandbox(name).await;
    // Like a sandbox fbkd created with `clipboard: true` before copying existed: the clipboard
    // label, route and marker, but no copy label.
    let routes = Clipboards::from_env(Arc::new(FakeClipboard {
        content: Mutex::new(FakeContent::Empty),
        written: Mutex::new(Vec::new()),
    }));
    let builder = Sandbox::builder(name)
        .image(PYTHON_IMAGE)
        .label(clipboard::ENABLED_LABEL, "true");
    let sb = routes
        .add_route(builder, name)
        .detached(true)
        .create()
        .await
        .expect("failed to create a paste-only sandbox");
    install_clipboard_stand_in(&sb).await;

    let daemon = TestDaemon::start_restoring_listeners("clipboard-paste-only").await;
    let copy_attempt = copy(&sb, "wl-copy", b"text").await;
    let pasted_text = paste(&sb, "wl-paste --no-newline").await;
    let copied = daemon.copied();

    daemon.stop().await;
    remove_sandbox(name).await;

    let message = "wl-copy: this sandbox was created before copying to the host clipboard was \
                   supported; recreate the sandbox to copy\n";
    assert_eq!(copy_attempt, (1, vec![], message.to_string()));
    assert_eq!(pasted_text, pasted(CLIPBOARD_TEXT));
    assert!(copied.is_empty(), "{copied:?}");
}

/// The stand-in commands that can't copy, their input, and what they exit with and print.
const COPY_ERRORS: [(&str, &[u8], i32, &str); 7] = [
    // The stand-in stops reading at the limit, so the content comes from a pipe in the
    // guest that may be left unread.
    (
        "head -c 1048577 /dev/zero | wl-copy",
        b"",
        1,
        "wl-copy: the content is larger than 1 MiB\n",
    ),
    (
        "head -c 10485761 /dev/zero | wl-copy -t image/png",
        b"",
        1,
        "wl-copy: the content is larger than 10 MiB\n",
    ),
    (
        "wl-copy",
        b"\xff\xfe",
        1,
        "wl-copy: the content isn't valid UTF-8 text\n",
    ),
    (
        "xclip -selection clipboard -t image/png",
        b"GIF89a",
        1,
        "xclip: the content isn't valid image/png\n",
    ),
    ("wl-copy --clear", b"", 2, WL_COPY_USAGE),
    ("wl-copy --type image/jpeg", b"", 2, WL_COPY_USAGE),
    ("xclip -selection primary -i", b"text", 2, XCLIP_USAGE),
];

#[tokio::test]
async fn stand_ins_report_what_keeps_them_from_copying() {
    let name: &str = &sandbox_name("clipboard-copy-errors");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("clipboard-copy-errors").await;
    let mut client = daemon.client().await;
    let sb = start_clipboard_sandbox(&mut client, name, true).await;
    let mut errors = Vec::new();
    for (command, input, _, _) in COPY_ERRORS {
        errors.push(copy(&sb, command, input).await);
    }
    daemon.set_clipboard(FakeContent::Unavailable(Cause::Failed));
    let unavailable = copy(&sb, "wl-copy", b"text").await;
    daemon.set_clipboard(FakeContent::Full);
    // More copies than fbkd takes within its window, counting the ones above that reached it.
    let flood = "for i in 1 2 3 4 5 6 7 8 9 10 11; do echo $i | wl-copy; done";
    let (_, _, flooded) = copy(&sb, flood, b"").await;
    let copied = daemon.copied();

    daemon.stop().await;
    remove_sandbox(name).await;

    let expected: Vec<Pasted> = COPY_ERRORS
        .iter()
        .map(|(_, _, status, message)| (*status, vec![], message.to_string()))
        .collect();
    assert_eq!(errors, expected);
    let message = "wl-copy: the host clipboard isn't available\n";
    assert_eq!(unavailable, (1, vec![], message.to_string()));
    let busy = "wl-copy: too many copies; try again later";
    assert!(!flooded.is_empty(), "every copy of the flood was taken");
    assert!(flooded.lines().all(|line| line == busy), "{flooded}");
    assert!(copied.len() <= clipboard::MAX_COPIES, "{}", copied.len());
}

/// The stand-in commands that paste the [`FakeClipboard`], and what they print.
const PASTES: [(&str, &[u8]); 7] = [
    ("wl-paste", b"hello from the host\n"),
    ("wl-paste --no-newline", CLIPBOARD_TEXT),
    (
        "wl-paste --list-types",
        b"text/plain;charset=utf-8\nimage/png\n",
    ),
    ("wl-paste --type image/png", CLIPBOARD_PNG),
    (
        "xclip -selection clipboard -t TARGETS -o",
        b"TARGETS\nUTF8_STRING\nimage/png\n",
    ),
    ("xclip -selection c -o", CLIPBOARD_TEXT),
    ("xclip -selection clipboard -t image/png -o", CLIPBOARD_PNG),
];

/// What `wl-paste` prints in a sandbox without the clipboard setting.
const OFF_MESSAGE: &str = "wl-paste: clipboard sharing is off for this sandbox; set clipboard: \
                           true in .firebrick.yml and recreate the sandbox\n";

#[tokio::test]
async fn sandbox_without_the_setting_has_no_clipboard_route() {
    let name: &str = &sandbox_name("clipboard-off");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("clipboard-off").await;
    let mut client = daemon.client().await;
    let sb = start_clipboard_sandbox(&mut client, name, false).await;
    let (spec, labels) = (sandbox_spec(name).await, firebrick_labels(name).await);

    let off = paste(&sb, "wl-paste").await;
    let copy_off = copy(&sb, "wl-copy", b"text").await;
    // Root in the guest can't ask fbkd either: the sandbox has no route to it.
    let direct = paste(&sb, ASK_FBKD_DIRECTLY).await;
    let copied = daemon.copied();

    daemon.stop().await;
    remove_sandbox(name).await;

    let labeled = labels
        .iter()
        .any(|(key, _)| key == clipboard::ENABLED_LABEL);
    assert!(!labeled, "{labels:?}");
    assert!(!has_clipboard_route(name, &spec));
    assert_eq!(off, (1, vec![], OFF_MESSAGE.to_string()));
    let copy_off_message = OFF_MESSAGE.replacen("wl-paste", "wl-copy", 1);
    assert_eq!(copy_off, (1, vec![], copy_off_message));
    assert!(copied.is_empty(), "{copied:?}");
    assert_eq!(
        (direct.0, direct.1.as_slice()),
        (1, &b"connection failed"[..]),
        "{direct:?}"
    );
}

/// Asks for the clipboard text on the clipboard port without the stand-in. Prints the answer,
/// or `connection failed` and exits 1 when the connection fails or closes without one.
const ASK_FBKD_DIRECTLY: &str = "python3 -c '
import socket, sys
try:
    s = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
    s.settimeout(5)
    s.connect((2, 52001))
    s.sendall(b\"text\\n\")
    answer = s.recv(100)
except OSError:
    answer = b\"\"
sys.stdout.buffer.write(answer or b\"connection failed\")
sys.exit(0 if answer else 1)
'";

/// What the stand-in prints for each content of the fake clipboard that it can't paste.
const PASTE_ERRORS: [(FakeContent, &str, &str); 5] = [
    (
        FakeContent::Empty,
        "wl-paste",
        "wl-paste: the clipboard holds no text\n",
    ),
    (
        FakeContent::Empty,
        "wl-paste --list-types",
        "wl-paste: the clipboard is empty\n",
    ),
    (
        FakeContent::Large,
        "wl-paste --type image/png",
        "wl-paste: the clipboard content is larger than 10 MiB\n",
    ),
    (
        FakeContent::Unavailable(Cause::NoDisplay),
        "xclip -selection clipboard -o",
        "xclip: the host clipboard isn't available; fbkd has no display, so stop it and run \
         fbk from your desktop session\n",
    ),
    (
        FakeContent::Unavailable(Cause::Failed),
        "wl-paste",
        "wl-paste: the host clipboard isn't available\n",
    ),
];

#[tokio::test]
async fn stand_ins_report_what_keeps_them_from_pasting() {
    let name: &str = &sandbox_name("clipboard-errors");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("clipboard-errors").await;
    let mut client = daemon.client().await;
    let sb = start_clipboard_sandbox(&mut client, name, true).await;
    let mut errors = Vec::new();
    for (content, command, _) in PASTE_ERRORS {
        daemon.set_clipboard(content);
        errors.push(paste(&sb, command).await);
    }

    daemon.stop().await;
    remove_sandbox(name).await;

    let expected: Vec<Pasted> = PASTE_ERRORS
        .iter()
        .map(|(_, _, message)| (1, vec![], message.to_string()))
        .collect();
    assert_eq!(errors, expected);
}
