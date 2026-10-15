//! Integration test for the microsandbox home `fbkd` picks when `MSB_HOME` isn't set.
//!
//! The test runs the `fbkd` binary with `MSB_HOME` removed and the home and XDG directories
//! pointed at a fresh directory under `/tmp`, or the temp directory on Windows, so it never
//! touches the user's state. On Windows its pipes get a prefix of their own. It boots a real VM;
//! run it with `cargo test -p firebrick-daemon --features vm-tests`.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use firebrick_daemon::api::sandbox_management_service_client::SandboxManagementServiceClient;
use firebrick_daemon::api::{
    AttachRequest, AttachResize, AttachStart, ListSandboxesRequest, NetworkPolicy,
    RemoveSandboxRequest, SandboxStatus, StartSandboxRequest, attach_request, attach_response,
};
use firebrick_utils::ipc;
use hyper_util::rt::TokioIo;
use tokio::time::{Instant, sleep};
use tonic::codegen::tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

const TIMEOUT: Duration = Duration::from_secs(300);
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const NAME: &str = "fbk-it-msb-home";
/// Image with `curl`, so the sandbox can make an HTTPS request through TLS interception.
const CURL_IMAGE: &str = "buildpack-deps:trixie-curl";

/// An `fbkd` process with its own `HOME`, XDG directories and socket under `root`.
struct Daemon {
    root: PathBuf,
    child: Child,
}

impl Daemon {
    fn start() -> Self {
        let root = test_root();
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();

        let mut command = Command::new(env!("CARGO_BIN_EXE_fbkd"));
        command
            .env_remove("MSB_HOME")
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"));
        isolate_home_and_endpoints(&mut command, &root);
        let child = command.spawn().expect("failed to start fbkd");

        Self { root, child }
    }

    /// Waits until fbkd listens on its socket, failing as soon as it exits.
    async fn wait_for_socket(&mut self, socket: &Path) {
        let deadline = Instant::now() + TIMEOUT;
        while !ipc::is_served(socket).await {
            let exited = self.child.try_wait().unwrap();
            assert!(exited.is_none(), "fbkd exited before listening: {exited:?}");
            assert!(Instant::now() < deadline, "fbkd did not start listening");
            sleep(POLL_INTERVAL).await;
        }
    }

    async fn client(&mut self) -> SandboxManagementServiceClient<Channel> {
        let socket = daemon_endpoint(&self.root);
        self.wait_for_socket(&socket).await;

        // The HTTP endpoint isn't used; it only shows up in the authority header.
        let channel = Endpoint::try_from("http://localhost")
            .unwrap()
            .connect_with_connector(service_fn(move |_: Uri| connect(socket.clone())))
            .await
            .expect("failed to connect to fbkd");

        SandboxManagementServiceClient::new(channel)
    }
}

/// Returns the directory the test keeps the daemon's state in. Short on Unix, because
/// microsandbox creates unix sockets under the home.
#[cfg(unix)]
fn test_root() -> PathBuf {
    PathBuf::from(format!("/tmp/fbk-mh-{}", std::process::id()))
}

/// Returns the directory the test keeps the daemon's state in, canonical without the `\\?\`
/// prefix that microsandbox refuses in mounts.
#[cfg(windows)]
fn test_root() -> PathBuf {
    let temp = dunce::canonicalize(std::env::temp_dir()).unwrap();
    temp.join(format!("fbk-mh-{}", std::process::id()))
}

/// Points the daemon's home at `root/home` and puts its socket in `root`.
#[cfg(unix)]
fn isolate_home_and_endpoints(command: &mut Command, root: &Path) {
    command
        .env("HOME", root.join("home"))
        .env("XDG_RUNTIME_DIR", root);
}

/// Points the daemon's home at `root/home` and gives its pipes a prefix of their own.
#[cfg(windows)]
fn isolate_home_and_endpoints(command: &mut Command, root: &Path) {
    command
        .env("USERPROFILE", root.join("home"))
        .env("FIREBRICK_PIPE_PREFIX", pipe_prefix());
}

/// Returns the daemon's endpoint for the state in `root`.
#[cfg(unix)]
fn daemon_endpoint(root: &Path) -> PathBuf {
    root.join("fbkd.sock")
}

/// Returns the daemon's endpoint, which [`isolate_home_and_endpoints`] named.
#[cfg(windows)]
fn daemon_endpoint(_root: &Path) -> PathBuf {
    PathBuf::from(format!("{}-fbkd", pipe_prefix()))
}

/// Returns the prefix of the test daemon's pipes.
#[cfg(windows)]
fn pipe_prefix() -> String {
    format!(r"\\.\pipe\fbk-mh-{}", std::process::id())
}

/// Connects to the daemon socket for a gRPC channel.
async fn connect(path: PathBuf) -> std::io::Result<TokioIo<ipc::Stream>> {
    Ok(TokioIo::new(ipc::connect(&path).await?))
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn wait_until_running(client: &mut SandboxManagementServiceClient<Channel>) {
    let deadline = Instant::now() + TIMEOUT;

    loop {
        let status = client
            .list_sandboxes(ListSandboxesRequest {})
            .await
            .expect("list_sandboxes failed")
            .into_inner()
            .sandboxes
            .into_iter()
            .find(|sb| sb.name == NAME)
            .map(|sb| sb.status);

        if status == Some(SandboxStatus::Running.into()) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "sandbox did not start: {status:?}"
        );
        sleep(POLL_INTERVAL).await;
    }
}

/// Returns the message that starts `command` with `args` in the sandbox.
fn attach_start(command: &str, args: &[&str]) -> AttachRequest {
    AttachRequest {
        message: Some(attach_request::Message::Start(AttachStart {
            name: NAME.to_string(),
            command: command.to_string(),
            args: args.iter().map(ToString::to_string).collect(),
            size: Some(AttachResize {
                width: 80,
                height: 24,
            }),
        })),
    }
}

/// Runs a command in the sandbox and returns its exit code.
async fn run_command(
    client: &mut SandboxManagementServiceClient<Channel>,
    command: &str,
    args: &[&str],
) -> i32 {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tx.send(attach_start(command, args)).await.unwrap();

    let mut responses = client
        .attach(ReceiverStream::new(rx))
        .await
        .expect("attach failed")
        .into_inner();

    loop {
        let response = tokio::time::timeout(TIMEOUT, responses.message())
            .await
            .expect("command did not finish in time")
            .expect("attach stream failed")
            .expect("attach stream closed before the command exited");

        if let Some(attach_response::Message::ExitCode(code)) = response.message {
            drop(tx);
            return code;
        }
    }
}

fn assert_exists(path: &Path) {
    assert!(path.exists(), "{} does not exist", path.display());
}

#[tokio::test]
async fn sandbox_without_msb_home_lands_in_firebrick_home() {
    let mut daemon = Daemon::start();
    let mut client = daemon.client().await;

    // The VM outlives fbkd, so remove the sandbox even when a check fails.
    let checks = tokio::spawn(start_and_check(daemon.root.clone(), client.clone())).await;
    let _ = client
        .remove_sandbox(RemoveSandboxRequest {
            name: NAME.to_string(),
            force: true,
        })
        .await;
    if let Err(err) = checks {
        std::panic::resume_unwind(err.into_panic());
    }
}

/// Returns a request for a sandbox with the workspace that only allows `example.com`. An
/// enforced policy turns on TLS interception, whose CA the VM process keeps in the home.
fn start_request(workspace: &Path) -> StartSandboxRequest {
    StartSandboxRequest {
        name: NAME.to_string(),
        workspace: workspace.to_string_lossy().into_owned(),
        image: CURL_IMAGE.to_string(),
        init: Some(false),
        network: Some(NetworkPolicy {
            enforce: true,
            allow: vec!["example.com".to_string()],
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Starts a sandbox through the daemon in `root` and checks that its state is in firebrick's
/// microsandbox home.
async fn start_and_check(root: PathBuf, mut client: SandboxManagementServiceClient<Channel>) {
    let workspace = root.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    // `/tmp` is a symlink on macOS, and microsandbox refuses to mount a path through one.
    let workspace = dunce::canonicalize(workspace).unwrap();
    start(&mut client, start_request(&workspace)).await;
    wait_until_running(&mut client).await;

    let args = ["-4", "-sS", "-m", "30", "https://example.com"];
    let code = run_command(&mut client, "curl", &args).await;
    assert_eq!(code, 0, "curl through TLS interception failed");

    assert_in_firebrick_home(&root);
}

/// Sends the start request and reads its stream to the end, failing on the status a failed
/// start ends it with.
async fn start(client: &mut SandboxManagementServiceClient<Channel>, request: StartSandboxRequest) {
    let mut stream = client
        .start_sandbox(request)
        .await
        .expect("failed to start sandbox")
        .into_inner();

    while stream
        .message()
        .await
        .expect("failed to start sandbox")
        .is_some()
    {}
}

/// Checks that microsandbox keeps its state in firebrick's home under `root`, not in
/// `~/.microsandbox`.
fn assert_in_firebrick_home(root: &Path) {
    let home = root.join("state/firebrick/msb");
    let msb = format!("msb{}", std::env::consts::EXE_SUFFIX);
    assert_exists(&home.join("bin").join(msb));
    assert_exists(&home.join("db/msb.db"));
    assert_exists(&home.join("sandboxes").join(NAME));
    assert_exists(&home.join("tls/ca.crt"));
    let default_home = root.join("home/.microsandbox");
    assert!(
        !default_home.exists(),
        "fbkd created {}",
        default_home.display()
    );
}
