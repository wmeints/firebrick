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
    ForwardPortResponse, GetSandboxRequest, GetSandboxResponse, ImagePullProgress,
    ListSandboxesRequest, Mount, NetworkPolicy, PortForward, PortForwards, RemovePortRequest,
    RemovePortResponse, RemoveSandboxRequest, RemoveSecretRequest, SandboxResources,
    SandboxStarted, SandboxStatus, SandboxVolumes, SetSecretRequest, StartSandboxRequest,
    StartSandboxResponse, StopSandboxRequest, UpdateNetworkRequest, attach_request,
    attach_response, start_sandbox_response,
};
use firebrick_daemon::open::{self, Opener};
use firebrick_daemon::secrets::{self, Secret, SecretStore};
use firebrick_daemon::{mise, sandboxes, server};
use hyper_util::rt::TokioIo;
use microsandbox::Sandbox;
use microsandbox::sandbox::{
    OwnedVolumeStorage, RootfsSource, SandboxHandle, SandboxSpec, VolumeMount,
};
use microsandbox::snapshot::{Snapshot, SnapshotHandle};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixStream};
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

struct TestDaemon {
    socket_path: PathBuf,
    opener: Arc<RecordingOpener>,
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
        init_logging();
        STALE_SWEEP.get_or_init(sweep_stale_sandboxes).await;

        let socket_path =
            std::env::temp_dir().join(format!("fbk-it-{}-{}.sock", std::process::id(), name));
        let _ = std::fs::remove_file(&socket_path);

        let (tx, rx) = oneshot::channel();
        let opener = Arc::new(RecordingOpener::default());
        let handle = tokio::spawn(serve_until(
            socket_path.clone(),
            secrets,
            opener.clone(),
            rx,
        ));

        wait_for_socket(&socket_path).await;

        Self {
            socket_path,
            opener,
            shutdown: Some(tx),
            handle,
        }
    }

    async fn client(&self) -> SandboxManagementServiceClient<Channel> {
        let path = self.socket_path.clone();

        // The HTTP endpoint isn't used; it only shows up in the authority header.
        let channel = Endpoint::try_from("http://localhost")
            .unwrap()
            .connect_with_connector(service_fn(move |_: Uri| connect_unix(path.clone())))
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

/// Serves the daemon on the socket until `stop` fires, opening URLs with `opener`.
async fn serve_until(
    path: PathBuf,
    secrets: SecretStore,
    opener: Arc<RecordingOpener>,
    stop: oneshot::Receiver<()>,
) -> Result<(), server::ServerError> {
    let service = server::FirebrickServer::new(secrets, opener);

    server::serve(&path, service, async {
        let _ = stop.await;
    })
    .await
}

/// Connects to the daemon socket for a gRPC channel.
async fn connect_unix(path: PathBuf) -> std::io::Result<TokioIo<UnixStream>> {
    Ok(TokioIo::new(UnixStream::connect(path).await?))
}

async fn wait_for_socket(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);

    while UnixStream::connect(path).await.is_err() {
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

/// Returns the id of the test process that created the sandbox, if the name has one.
fn sandbox_pid(name: &str) -> Option<u32> {
    let (pid, _) = name.strip_prefix(SANDBOX_PREFIX)?.split_once('-')?;
    pid.parse().ok()
}

/// Whether a process with the id still runs. `kill -0` works on both Linux and macOS.
fn process_is_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
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
/// processes left in the temp directory. They're all named `fbk-it-<pid>-...`, also when the
/// process was killed before it created the sandbox.
fn remove_stale_host_files() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };

    for entry in entries.flatten() {
        if is_stale(&entry.file_name().to_string_lossy()) {
            let path = entry.path();
            let _ = std::fs::remove_dir_all(&path).or_else(|_| std::fs::remove_file(&path));
        }
    }
}

/// Stops and removes a sandbox and its workspace, including ones left over from an earlier
/// (failed) run.
async fn remove_sandbox(name: &str) {
    if let Ok(sb) = Sandbox::get(name).await {
        let _ = sb.stop().await;
        let _ = sb.remove().await;
    }

    let _ = std::fs::remove_dir_all(workspace_path(name));
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

/// Returns the host directory used as the workspace of a test sandbox. It's named after the
/// sandbox, so `remove_sandbox` and the sweep of stale sandboxes can find it again.
fn workspace_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{name}-ws"))
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
    let mut child = std::process::Command::new("true").spawn().unwrap();
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
async fn get_sandbox_returns_name_status_and_workspace_path() {
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
        std::fs::canonicalize(workspace_path(name)).unwrap()
    );

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

/// Sends the start request and reads its stream to the end. Returns the pull progress and the
/// started sandbox, or the status the stream ended with. Fails when a message follows the
/// started sandbox.
async fn start_stream(
    client: &mut SandboxManagementServiceClient<Channel>,
    request: StartSandboxRequest,
) -> Result<(Vec<ImagePullProgress>, SandboxStarted), Status> {
    let mut stream = client.start_sandbox(request).await?.into_inner();
    let mut progress = vec![];

    while let Some(StartSandboxResponse { message }) = stream.message().await? {
        match message.expect("empty start response") {
            start_sandbox_response::Message::PullProgress(message) => progress.push(message),
            start_sandbox_response::Message::Started(started) => {
                let rest = stream.message().await?;
                assert_eq!(rest, None, "the stream continued after the start");
                return Ok((progress, started));
            }
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
    let dir = std::env::temp_dir().join(format!("{name}-{suffix}"));
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

#[tokio::test]
async fn start_sandbox_skips_mise_when_image_has_none() {
    let name: &str = &sandbox_name("mise-missing");
    remove_sandbox(name).await;

    let daemon = TestDaemon::start("mise-missing").await;
    let mut client = daemon.client().await;
    // The test image has no mise, so both the create and the restart only log a warning.
    std::fs::write(
        test_workspace(name).join("mise.toml"),
        "[tools]\nnode = \"22\"\n",
    )
    .unwrap();

    start_running_sandbox(&mut client, name).await;
    restart_sandbox(&mut client, name).await;

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
    }
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

/// Returns the snapshots that updating the sandbox's network rules left behind.
async fn network_snapshots(name: &str) -> Vec<SnapshotHandle> {
    let prefix = format!("firebrick-network-{name}-");

    Snapshot::list()
        .await
        .expect("failed to list snapshots")
        .into_iter()
        .filter(|snapshot| snapshot.name().is_some_and(|n| n.starts_with(&prefix)))
        .collect()
}

/// Returns the names of the snapshots that updating the sandbox's network rules left behind.
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

#[tokio::test]
async fn update_network_of_running_sandbox_keeps_its_disk_and_settings() {
    let name: &str = &sandbox_name("update-running");
    remove_sandbox_and_snapshots(name).await;

    let dir = tempfile::tempdir().unwrap();
    let store = store_with_test_secret(dir.path());
    let daemon = TestDaemon::start_with_secrets("update-running", store).await;
    let mut client = daemon.client().await;
    start_enforced_sandbox(&mut client, name, &["example.com"], &[]).await;
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
    assert_eq!(after.settings, before);
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
    let snapshots = network_snapshots(name).await;
    let paths: Vec<String> = snapshots
        .iter()
        .filter_map(|snapshot| snapshot.path().ok())
        .map(|path| path.to_string_lossy().into_owned())
        .collect();

    daemon.stop().await;
    remove_sandbox_and_snapshots(name).await;

    assert_eq!(status.code(), Code::Internal);
    assert!(!exists, "the half-created sandbox should be removed");
    assert_eq!(paths.len(), 1, "{paths:?}");
    assert_eq!(
        status.message(),
        format!(
            "failed to update the network rules of {name}; the sandbox was kept as snapshot {}",
            paths[0]
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
    sb.pause().await.unwrap();

    let status = update_network(&mut client, name, allow_policy(true, &["example.org"])).await;
    let paused = sandbox_status(&mut client, name).await;

    sb.resume().await.unwrap();
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
