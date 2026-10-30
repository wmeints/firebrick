//! The gRPC server: adapts `SandboxManagementService` requests to the sandbox management in
//! [`crate::sandboxes`]. Its submodules convert the messages, stream `Attach` sessions and
//! `SshTunnel` connections, accept the connections on the daemon's endpoint and stop the daemon
//! on a signal.

mod attach;
mod convert;
mod shutdown;
mod transport;
mod tunnel;

use attach::SessionCommand;

use crate::api::sandbox_management_service_server::SandboxManagementService;
use crate::api::{
    AttachRequest, AttachResponse, AttachStart, ForwardPortRequest, ForwardPortResponse,
    GetSandboxRequest, GetSandboxResponse, GetVersionRequest, GetVersionResponse,
    ListNetworkActivityRequest, ListNetworkActivityResponse, ListSandboxesRequest,
    ListSandboxesResponse, ListSecretsRequest, ListSecretsResponse, RemovePortRequest,
    RemovePortResponse, RemoveSandboxRequest, RemoveSandboxResponse, RemoveSecretRequest,
    RemoveSecretResponse, SetSecretRequest, SetSecretResponse, ShutdownRequest, ShutdownResponse,
    SshTunnelRequest, SshTunnelResponse, StartSandboxRequest, StartSandboxResponse,
    StopSandboxRequest, StopSandboxResponse, UpdateNetworkRequest, UpdateNetworkResponse,
    UpgradeSandboxRequest, UpgradeSandboxResponse, attach_request, ssh_tunnel_request,
    start_sandbox_response, upgrade_sandbox_response,
};
use crate::guest::clipboard::{HostClipboard, HostTools};
use crate::guest::open::{HostOpener, Opener};
use crate::sandboxes::{NewSecret, SandboxManager};
use crate::secrets::SecretStore;
use anyhow::Result;
use async_trait::async_trait;
use firebrick_spec::PortMapping;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{Notify, mpsc};
use tonic::codegen::tokio_stream::Stream;
use tonic::codegen::tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use convert::{
    ParsedStart, check_image, check_name, image_pull_progress, map_sandbox_status,
    network_activity, network_spec, port_number, sandbox_resources, sandbox_started,
    sandbox_summary, sandbox_upgraded, secret_summary, start_progress_response, start_sandbox_from,
    start_sandbox_response, upgrade_sandbox_response,
};
use shutdown::shutdown_or;
pub use transport::serve;

/// Errors that can occur while running the gRPC server.
#[derive(Error, Debug)]
pub enum ServerError {
    #[error("socket is already in use")]
    SocketAlreadyInUse(),
    #[error("server can't listen: {0}")]
    FailedToListen(#[from] tonic::transport::Error),
    #[error("invalid socket path")]
    InvalidSocketPath(#[from] std::io::Error),
}

/// gRPC service implementation that manages sandboxes.
pub struct FirebrickServer {
    // Shared with the tasks that start sandboxes, so a start finishes when its client is gone.
    sandboxes: Arc<SandboxManager>,
    sessions: Sessions,
    shutdown: Arc<Notify>,
}

impl FirebrickServer {
    /// Creates a server that adds the secrets from `secrets` to sandboxes, opens the URLs from
    /// sandboxes with `opener` and reads the host clipboard for them from `clipboard`.
    pub fn new(
        secrets: SecretStore,
        opener: Arc<dyn Opener>,
        clipboard: Arc<dyn HostClipboard>,
    ) -> Self {
        Self {
            sandboxes: Arc::new(SandboxManager::new(secrets, opener, clipboard)),
            sessions: Sessions::default(),
            shutdown: Arc::new(Notify::new()),
        }
    }

    /// Returns a future that completes when a client asks the daemon to shut down with the
    /// `Shutdown` request. It also completes for a request made before it's awaited.
    pub fn shutdown_requested(&self) -> impl Future<Output = ()> + use<> {
        let shutdown = Arc::clone(&self.shutdown);
        async move { shutdown.notified().await }
    }

    /// Listens for the network logs and clipboards of the existing sandboxes that have their
    /// routes. Run when the daemon starts, so it serves the sandboxes that kept running while
    /// it was down.
    pub async fn restore_listeners(&self) {
        self.sandboxes.restore_listeners().await;
    }
}

/// Counts the requests in progress that stopping the daemon would cut. Each of them holds a
/// [`Session`] from [`Sessions::open`]. It isn't `Clone`, so every other reference to its
/// `Arc` is an open session.
#[derive(Default)]
struct Sessions(Arc<()>);

/// An open session of [`Sessions`], closed when it's dropped.
type Session = Arc<()>;

impl Sessions {
    /// Opens a session, which counts until it's dropped.
    fn open(&self) -> Session {
        Arc::clone(&self.0)
    }

    /// Returns the number of open sessions.
    fn count(&self) -> usize {
        Arc::strong_count(&self.0) - 1
    }
}

/// A response stream that keeps its session open until the stream is dropped, which happens
/// when it ends or its client is gone.
struct Tracked<S> {
    stream: S,
    _session: Session,
}

impl<S: Stream + Unpin> Stream for Tracked<S> {
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<S::Item>> {
        Pin::new(&mut self.stream).poll_next(cx)
    }
}

/// Returns why the daemon refuses to shut down while `count` sessions are open.
fn open_sessions_message(count: usize) -> String {
    match count {
        1 => "1 session is open".to_string(),
        _ => format!("{count} sessions are open"),
    }
}

/// Stream of responses sent back to the client during an attached session.
type AttachStream = Pin<Box<dyn Stream<Item = Result<AttachResponse, Status>> + Send>>;

/// Stream of image pull progress that ends with the started sandbox.
type StartSandboxStream = Pin<Box<dyn Stream<Item = Result<StartSandboxResponse, Status>> + Send>>;

/// Stream of image pull progress that ends with the result of an upgrade.
type UpgradeSandboxStream =
    Pin<Box<dyn Stream<Item = Result<UpgradeSandboxResponse, Status>> + Send>>;

/// Stream of SSH protocol bytes sent back to the client through a tunnel.
type SshTunnelStream = Pin<Box<dyn Stream<Item = Result<SshTunnelResponse, Status>> + Send>>;

#[async_trait]
impl SandboxManagementService for FirebrickServer {
    type StartSandboxStream = StartSandboxStream;
    type AttachStream = AttachStream;
    type SshTunnelStream = SshTunnelStream;
    type UpgradeSandboxStream = UpgradeSandboxStream;

    /// Starts an existing sandbox or creates a new one when it doesn't exist, streaming the
    /// progress of the image pull and the mise install. The start runs in its own task, so it
    /// finishes even when the client disconnects.
    async fn start_sandbox(
        &self,
        request: Request<StartSandboxRequest>,
    ) -> Result<Response<Self::StartSandboxStream>, Status> {
        let request_data = request.into_inner();
        let parsed = ParsedStart::of(&request_data)?;
        let sandboxes = Arc::clone(&self.sandboxes);
        let session = self.sessions.open();
        let (tx, rx) = mpsc::channel(16);

        tokio::spawn(async move {
            let _session = session;
            let progress_tx = tx.clone();
            let result = sandboxes
                .start(
                    start_sandbox_from(&request_data, &parsed),
                    || sandbox_resources(&request_data),
                    // Progress is best-effort: drop it when the client is slow or gone.
                    |progress| {
                        let _ = progress_tx.try_send(Ok(start_progress_response(progress)));
                    },
                )
                .await;
            let last = result
                .map(|started| {
                    start_sandbox_response(start_sandbox_response::Message::Started(
                        sandbox_started(started),
                    ))
                })
                .map_err(Status::from);

            // The client may be gone; the sandbox started anyway.
            let _ = tx.send(last).await;
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    /// Replaces the egress rules of an existing sandbox, recreating it from a disk snapshot
    /// unless it already has them. The update runs in its own task, so a client that
    /// disconnects can't stop a recreate halfway and leave its snapshot behind.
    async fn update_network(
        &self,
        request: Request<UpdateNetworkRequest>,
    ) -> Result<Response<UpdateNetworkResponse>, Status> {
        let request_data = request.into_inner();
        check_name(&request_data.name)?;
        let network = network_spec(request_data.network.as_ref())?;
        let sandboxes = Arc::clone(&self.sandboxes);
        let session = self.sessions.open();

        let updated = tokio::spawn(async move {
            let _session = session;
            sandboxes.update_network(&request_data.name, &network).await
        })
        .await
        .map_err(|err| Status::internal(format!("failed to update the network rules: {err}")))??;

        Ok(Response::new(UpdateNetworkResponse { updated }))
    }

    /// Recreates an existing sandbox from the latest version of an image unless it already has
    /// it, streaming the progress of the image pull. The upgrade runs in its own task, so a
    /// client that disconnects can't stop a recreate halfway and leave its snapshot behind.
    async fn upgrade_sandbox(
        &self,
        request: Request<UpgradeSandboxRequest>,
    ) -> Result<Response<Self::UpgradeSandboxStream>, Status> {
        let request_data = request.into_inner();
        check_name(&request_data.name)?;
        check_image(&request_data.image)?;
        let sandboxes = Arc::clone(&self.sandboxes);
        let session = self.sessions.open();
        let (tx, rx) = mpsc::channel(16);

        tokio::spawn(async move {
            let _session = session;
            let result = sandboxes
                .upgrade(&request_data.name, &request_data.image, |progress| {
                    // Progress is best-effort: drop it when the client is slow or gone.
                    let message = upgrade_sandbox_response::Message::PullProgress(
                        image_pull_progress(progress),
                    );
                    let _ = tx.try_send(Ok(upgrade_sandbox_response(message)));
                })
                .await;

            // The client may be gone; the upgrade finished anyway.
            let _ = tx
                .send(result.map(sandbox_upgraded).map_err(Status::from))
                .await;
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    /// Stores a secret and adds it to the existing sandboxes in its scope. Running sandboxes
    /// pick it up the next time they start.
    async fn set_secret(
        &self,
        request: Request<SetSecretRequest>,
    ) -> Result<Response<SetSecretResponse>, Status> {
        let request_data = request.into_inner();
        request_data
            .sandbox
            .as_deref()
            .map(check_name)
            .transpose()?;
        let failed_sandboxes = self
            .sandboxes
            .set_secret(NewSecret {
                name: request_data.name,
                value: request_data.value,
                allowed_hosts: request_data.allowed_hosts,
                sandbox: request_data.sandbox,
            })
            .await?;

        Ok(Response::new(SetSecretResponse { failed_sandboxes }))
    }

    /// Lists the stored secrets by name and scope, without their values.
    async fn list_secrets(
        &self,
        _request: Request<ListSecretsRequest>,
    ) -> Result<Response<ListSecretsResponse>, Status> {
        let secrets = self
            .sandboxes
            .list_secrets()?
            .iter()
            .map(secret_summary)
            .collect();

        Ok(Response::new(ListSecretsResponse { secrets }))
    }

    /// Removes a stored secret and removes it from the existing sandboxes in its scope. Running
    /// sandboxes keep it until they restart.
    async fn remove_secret(
        &self,
        request: Request<RemoveSecretRequest>,
    ) -> Result<Response<RemoveSecretResponse>, Status> {
        let request = request.into_inner();
        request.sandbox.as_deref().map(check_name).transpose()?;
        let failed_sandboxes = self
            .sandboxes
            .remove_secret(&request.name, request.sandbox.as_deref())
            .await?;

        Ok(Response::new(RemoveSecretResponse { failed_sandboxes }))
    }

    /// Adds a forward to a sandbox's stored ports and opens it when the sandbox runs.
    async fn forward_port(
        &self,
        request: Request<ForwardPortRequest>,
    ) -> Result<Response<ForwardPortResponse>, Status> {
        let request = request.into_inner();
        check_name(&request.name)?;
        let port = request
            .port
            .ok_or_else(|| Status::invalid_argument("port is required"))?;
        let port = PortMapping {
            host: port_number(port.host)?,
            guest: port_number(port.guest)?,
        };
        let running = self.sandboxes.forward_port(&request.name, port).await?;

        Ok(Response::new(ForwardPortResponse { running }))
    }

    /// Removes a forward from a sandbox's stored ports and closes it when the sandbox runs.
    async fn remove_port(
        &self,
        request: Request<RemovePortRequest>,
    ) -> Result<Response<RemovePortResponse>, Status> {
        let request = request.into_inner();
        check_name(&request.name)?;
        let host = port_number(request.host)?;
        let running = self.sandboxes.remove_port(&request.name, host).await?;

        Ok(Response::new(RemovePortResponse { running }))
    }

    /// Returns the network activity recorded for a sandbox, with whether its current rules
    /// allow each destination.
    async fn list_network_activity(
        &self,
        request: Request<ListNetworkActivityRequest>,
    ) -> Result<Response<ListNetworkActivityResponse>, Status> {
        let name = request.into_inner().name;
        check_name(&name)?;
        let (record, rules) = self.sandboxes.network_activity(&name).await?;

        Ok(Response::new(ListNetworkActivityResponse {
            entries: network_activity(record, &rules),
        }))
    }

    /// Stops a running sandbox.
    async fn stop_sandbox(
        &self,
        request: Request<StopSandboxRequest>,
    ) -> Result<Response<StopSandboxResponse>, Status> {
        let name = request.into_inner().name;
        check_name(&name)?;
        self.sandboxes.stop(&name).await?;

        Ok(Response::new(StopSandboxResponse {}))
    }

    /// Lists all sandboxes with their status.
    async fn list_sandboxes(
        &self,
        _request: Request<ListSandboxesRequest>,
    ) -> Result<Response<ListSandboxesResponse>, Status> {
        let sandboxes = self
            .sandboxes
            .list()
            .await?
            .into_iter()
            .map(sandbox_summary)
            .collect();

        Ok(Response::new(ListSandboxesResponse { sandboxes }))
    }

    /// Returns the name, status, host name, workspace path and image of a single sandbox, with the
    /// image an upgrade moves it to when it runs an older `firebrick-base` release.
    async fn get_sandbox(
        &self,
        request: Request<GetSandboxRequest>,
    ) -> Result<Response<GetSandboxResponse>, Status> {
        let name = request.into_inner().name;
        check_name(&name)?;
        let sandbox = self.sandboxes.get(&name).await?;

        Ok(Response::new(GetSandboxResponse {
            name: sandbox.name,
            status: map_sandbox_status(sandbox.status),
            hostname: sandbox.hostname.unwrap_or_default(),
            workspace_path: sandbox.workspace_path.unwrap_or_default(),
            workspace_host_path: sandbox.workspace_host_path.unwrap_or_default(),
            image: sandbox.image.unwrap_or_default(),
            upgrade_image: sandbox.upgrade_image.unwrap_or_default(),
        }))
    }

    /// Removes a sandbox, stopping it first when the request forces it.
    async fn remove_sandbox(
        &self,
        request: Request<RemoveSandboxRequest>,
    ) -> std::result::Result<Response<RemoveSandboxResponse>, Status> {
        let request = request.into_inner();
        check_name(&request.name)?;
        self.sandboxes.remove(&request.name, request.force).await?;

        Ok(Response::new(RemoveSandboxResponse {}))
    }

    /// Starts an interactive session in a sandbox and streams its input and output.
    async fn attach(
        &self,
        request: Request<Streaming<AttachRequest>>,
    ) -> Result<Response<Self::AttachStream>, Status> {
        let opened = self.sessions.open();
        let mut inbound = request.into_inner();
        let start = attach_start(&mut inbound).await?;
        check_name(&start.name)?;
        let size = attach::window_size(start.size)?;
        let sb = self.sandboxes.connect(&start.name).await?;

        let command = SessionCommand {
            command: start.command,
            args: start.args,
            size,
        };
        let stream = attach::attach(sb, inbound, command).await?;

        Ok(Response::new(Box::pin(Tracked {
            stream,
            _session: opened,
        })))
    }

    /// Serves an SSH connection to a sandbox over the stream, starting the sandbox when needed.
    async fn ssh_tunnel(
        &self,
        request: Request<Streaming<SshTunnelRequest>>,
    ) -> Result<Response<Self::SshTunnelStream>, Status> {
        let opened = self.sessions.open();
        let mut inbound = request.into_inner();
        let hostname = tunnel_hostname(&mut inbound).await?;
        let sb = self.sandboxes.connect_by_hostname(&hostname).await?;
        let stream = tunnel::open(sb, inbound, hostname).await?;

        Ok(Response::new(Box::pin(Tracked {
            stream,
            _session: opened,
        })))
    }

    /// Returns the package version of the daemon.
    async fn get_version(
        &self,
        _: Request<GetVersionRequest>,
    ) -> Result<Response<GetVersionResponse>, Status> {
        Ok(Response::new(GetVersionResponse {
            version: env!("CARGO_PKG_VERSION").to_string(),
        }))
    }

    /// Completes [`FirebrickServer::shutdown_requested`], so the daemon stops serving once the
    /// requests in flight are done. Fails with `FailedPrecondition` while a session is open.
    async fn shutdown(
        &self,
        _: Request<ShutdownRequest>,
    ) -> Result<Response<ShutdownResponse>, Status> {
        let open = self.sessions.count();

        if open > 0 {
            return Err(Status::failed_precondition(open_sessions_message(open)));
        }

        tracing::info!("shutting down on request");
        self.shutdown.notify_one();

        Ok(Response::new(ShutdownResponse {}))
    }
}

/// Returns the start message, which must be the first message of an `Attach` stream.
async fn attach_start(inbound: &mut Streaming<AttachRequest>) -> Result<AttachStart, Status> {
    match inbound.message().await? {
        Some(AttachRequest {
            message: Some(attach_request::Message::Start(start)),
        }) => Ok(start),
        _ => Err(Status::invalid_argument("first message must be start")),
    }
}

/// Returns the host name, which must be the first message of an `SshTunnel` stream.
async fn tunnel_hostname(inbound: &mut Streaming<SshTunnelRequest>) -> Result<String, Status> {
    match inbound.message().await? {
        Some(SshTunnelRequest {
            message: Some(ssh_tunnel_request::Message::Hostname(hostname)),
        }) => Ok(hostname),
        _ => Err(Status::invalid_argument("first message must be hostname")),
    }
}

/// How long the daemon waits for the updates and upgrades of sandboxes when it shuts down.
const RECREATION_TIMEOUT: Duration = Duration::from_secs(60);

/// Reopens the port forwards of the sandboxes that are running, listens for the network logs
/// and clipboards of the sandboxes and removes the upgrade probes an earlier daemon left behind,
/// then serves the gRPC API on the socket until SIGINT or SIGTERM is received or a client sends
/// a `Shutdown` request, opening the URLs
/// from sandboxes in the host's browser and reading the host clipboard with its desktop's
/// tools. Afterwards, waits up to [`RECREATION_TIMEOUT`] for the updates and upgrades that are
/// still running and writes the network records a last time. The caller makes sure no other
/// daemon serves on the socket, so it doesn't take that daemon's host ports.
pub async fn run(socket_path: &Path, secrets: SecretStore) -> Result<(), ServerError> {
    let service = FirebrickServer::new(
        secrets,
        Arc::new(HostOpener),
        Arc::new(HostTools::from_env()),
    );
    let sandboxes = Arc::clone(&service.sandboxes);
    let requested = service.shutdown_requested();
    sandboxes.restore_forwards().await;
    sandboxes.restore_listeners().await;
    sandboxes.remove_stale_probes().await;

    let served = serve(socket_path, service, shutdown_or(requested)).await;

    if !sandboxes.finish_recreations(RECREATION_TIMEOUT).await {
        tracing::warn!("stopped while a sandbox was still being updated or upgraded");
    }

    sandboxes.close_listeners().await;

    served
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::sandbox_management_service_client::SandboxManagementServiceClient;
    use crate::api::{NetworkPolicy, PortForward, SecretSummary};
    use crate::secrets::Secret;
    use firebrick_utils::ipc;
    use hyper_util::rt::TokioIo;
    use std::path::PathBuf;
    use tonic::transport::{Channel, Endpoint, Uri};

    /// Returns a store for tests that never set a secret.
    fn unused_store() -> SecretStore {
        SecretStore::new(temp_path("unused-secrets.yml"))
    }

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("firebrick-test-{}-{}", std::process::id(), name))
    }

    /// Returns a store at `path` with a secret for `example.com` under each name.
    fn store_with_secrets(path: &Path, names: &[&str]) -> SecretStore {
        let store = SecretStore::new(path);
        let secrets = names
            .iter()
            .map(|name| Secret::new(name.to_string(), "value".into(), vec!["example.com".into()]));

        for secret in secrets {
            store.set(secret.unwrap()).unwrap();
        }

        store
    }

    /// Serves on the socket until `stop` fires.
    async fn serve_until(
        path: PathBuf,
        stop: tokio::sync::oneshot::Receiver<()>,
    ) -> Result<(), ServerError> {
        serve(
            &path,
            FirebrickServer::new(
                unused_store(),
                Arc::new(HostOpener),
                Arc::new(HostTools::new(None, None)),
            ),
            async {
                let _ = stop.await;
            },
        )
        .await
    }

    /// Returns an endpoint for the server unique to this test: a socket in the temp directory,
    /// or a pipe on Windows.
    fn test_endpoint(name: &str) -> PathBuf {
        if cfg!(windows) {
            let pipe = format!(r"\\.\pipe\firebrick-test-{}-{name}", std::process::id());
            // The pipe name is absolute; only the test name and process id vary.
            // foxguard: ignore[rs/no-path-traversal]
            return PathBuf::from(pipe);
        }
        temp_path(&format!("{name}.sock"))
    }

    /// Waits until the server accepts connections on the endpoint.
    async fn wait_for_socket(path: &Path) {
        while !ipc::is_served(path).await {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Connects a gRPC client to the server on the endpoint.
    async fn client(path: PathBuf) -> SandboxManagementServiceClient<Channel> {
        // The HTTP endpoint isn't used; it only shows up in the authority header.
        let channel = Endpoint::try_from("http://localhost")
            .unwrap()
            .connect_with_connector(tower::service_fn(move |_: Uri| connect(path.clone())))
            .await
            .unwrap();

        SandboxManagementServiceClient::new(channel)
    }

    /// Connects to the server's endpoint for a gRPC channel.
    async fn connect(path: PathBuf) -> std::io::Result<TokioIo<ipc::Stream>> {
        Ok(TokioIo::new(ipc::connect(&path).await?))
    }

    #[tokio::test]
    async fn run_fails_when_socket_already_exists() {
        let path = test_endpoint("existing");
        let other = ipc::Listener::bind(&path).unwrap();

        let result = run(&path, unused_store()).await;

        drop(other);
        ipc::remove(&path).await.unwrap();
        assert!(matches!(result, Err(ServerError::SocketAlreadyInUse())));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_fails_when_a_stale_socket_file_exists() {
        let path = temp_path("stale.sock");
        std::fs::write(&path, b"").unwrap();

        let result = run(&path, unused_store()).await;

        std::fs::remove_file(&path).unwrap();
        assert!(matches!(result, Err(ServerError::SocketAlreadyInUse())));
    }

    #[tokio::test]
    async fn serve_removes_socket_on_shutdown() {
        let path = test_endpoint("shutdown");
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();

        let handle = tokio::spawn(serve_until(path.clone(), rx));
        wait_for_socket(&path).await;

        tx.send(()).unwrap();
        handle.await.unwrap().unwrap();
        assert!(disappears(&path).await);
    }

    /// Waits up to a second for the endpoint to disappear. On Windows a connection that's still
    /// closing, such as the probe of [`wait_for_socket`], keeps a pipe instance alive for a few
    /// milliseconds after `serve` returns.
    async fn disappears(path: &Path) -> bool {
        let mut tries = 0;
        while ipc::exists(path).await && tries < 100 {
            tries += 1;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        !ipc::exists(path).await
    }

    #[tokio::test]
    async fn get_version_returns_the_package_version() {
        let response = name_server()
            .get_version(Request::new(GetVersionRequest {}))
            .await
            .unwrap();

        assert_eq!(response.into_inner().version, env!("CARGO_PKG_VERSION"));
    }

    /// Serves a server on the endpoint until a client asks it to shut down, returning a weak
    /// reference to its sessions so a test can wait for one to open without opening one.
    fn serve_until_requested(
        path: PathBuf,
    ) -> (
        std::sync::Weak<()>,
        tokio::task::JoinHandle<Result<(), ServerError>>,
    ) {
        let server = name_server();
        let sessions = Arc::downgrade(&server.sessions.0);
        let requested = server.shutdown_requested();
        let handle = tokio::spawn(async move { serve(&path, server, requested).await });

        (sessions, handle)
    }

    #[tokio::test]
    async fn shutdown_stops_an_idle_daemon_and_removes_its_endpoint() {
        let path = test_endpoint("shutdown-request");
        let (_, handle) = serve_until_requested(path.clone());
        wait_for_socket(&path).await;

        let result = client(path.clone())
            .await
            .shutdown(ShutdownRequest {})
            .await;

        assert!(result.is_ok(), "shutdown failed: {result:?}");
        handle.await.unwrap().unwrap();
        assert!(disappears(&path).await);
    }

    /// Attaches with the requests from `rx`.
    async fn attach_from(
        mut daemon: SandboxManagementServiceClient<Channel>,
        rx: mpsc::Receiver<AttachRequest>,
    ) -> Result<Response<Streaming<AttachResponse>>, Status> {
        daemon.attach(ReceiverStream::new(rx)).await
    }

    /// Returns how many sessions are open, from a weak reference to the sessions.
    fn open_sessions(sessions: &std::sync::Weak<()>) -> usize {
        sessions.strong_count().saturating_sub(1)
    }

    /// Waits up to 5 seconds until `count` sessions are open.
    async fn wait_for_sessions(sessions: &std::sync::Weak<()>, count: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

        while open_sessions(sessions) != count {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{} sessions are open, not {count}",
                open_sessions(sessions)
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn shutdown_or_completes_when_a_shutdown_is_requested() {
        let requested = std::future::ready(());

        let stopped = tokio::time::timeout(Duration::from_secs(5), shutdown_or(requested)).await;

        assert!(stopped.is_ok(), "a requested shutdown didn't complete");
    }

    #[tokio::test]
    async fn shutdown_is_refused_while_a_session_is_open() {
        let path = test_endpoint("shutdown-busy");
        let (sessions, handle) = serve_until_requested(path.clone());
        wait_for_socket(&path).await;
        let mut daemon = client(path.clone()).await;

        // An attach that never sends its start message keeps its session open.
        let (input, rx) = mpsc::channel::<AttachRequest>(1);
        let attach = tokio::spawn(attach_from(daemon.clone(), rx));
        wait_for_sessions(&sessions, 1).await;
        let refused = daemon.shutdown(ShutdownRequest {}).await.unwrap_err();

        drop(input);
        let _ = attach.await.unwrap();
        wait_for_sessions(&sessions, 0).await;
        let accepted = daemon.shutdown(ShutdownRequest {}).await;

        assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
        assert_eq!(refused.message(), "1 session is open");
        assert!(accepted.is_ok(), "shutdown failed: {accepted:?}");
        handle.await.unwrap().unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn serve_restricts_socket_to_owner() {
        use std::os::unix::fs::PermissionsExt;
        let path = test_endpoint("mode");
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();

        let handle = tokio::spawn(serve_until(path.clone(), rx));
        wait_for_socket(&path).await;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;

        tx.send(()).unwrap();
        handle.await.unwrap().unwrap();
        assert_eq!(mode, 0o600);
    }

    #[tokio::test]
    async fn serve_serves_connections_from_the_same_user() {
        let path = test_endpoint("same-user");
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();

        let handle = tokio::spawn(serve_until(path.clone(), rx));
        wait_for_socket(&path).await;
        let result = client(path.clone())
            .await
            .list_secrets(ListSecretsRequest {})
            .await;

        tx.send(()).unwrap();
        handle.await.unwrap().unwrap();
        assert!(result.is_ok(), "request failed: {result:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_fails_when_socket_directory_does_not_exist() {
        let path = temp_path("missing-dir").join("firebrick.sock");

        let result = run(&path, unused_store()).await;

        assert!(matches!(result, Err(ServerError::InvalidSocketPath(_))));
        assert!(!path.exists());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn run_fails_for_a_pipe_name_that_is_too_long() {
        let path = test_endpoint(&"a".repeat(300));

        let result = run(&path, unused_store()).await;

        assert!(matches!(result, Err(ServerError::InvalidSocketPath(_))));
    }

    #[tokio::test]
    async fn set_secret_rejects_invalid_secret_without_storing_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.yml");
        let server = FirebrickServer::new(
            SecretStore::new(&path),
            Arc::new(HostOpener),
            Arc::new(HostTools::new(None, None)),
        );

        let status = server
            .set_secret(Request::new(SetSecretRequest {
                name: "NOT-A-NAME".to_string(),
                value: "value".to_string(),
                allowed_hosts: vec![],
                sandbox: None,
            }))
            .await
            .expect_err("an invalid name should be rejected");

        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn list_secrets_returns_names_and_hosts_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_secrets(&dir.path().join("secrets.yml"), &["B_TOKEN", "A_TOKEN"]);
        let server = FirebrickServer::new(
            store,
            Arc::new(HostOpener),
            Arc::new(HostTools::new(None, None)),
        );

        let secrets = server
            .list_secrets(Request::new(ListSecretsRequest {}))
            .await
            .unwrap()
            .into_inner()
            .secrets;

        let expected: Vec<SecretSummary> = ["A_TOKEN", "B_TOKEN"]
            .map(|name| SecretSummary {
                name: name.to_string(),
                allowed_hosts: vec!["example.com".to_string()],
                sandbox: None,
            })
            .into();
        assert_eq!(secrets, expected);
    }

    #[tokio::test]
    async fn list_secrets_returns_the_scope() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_secrets(&dir.path().join("secrets.yml"), &["A"]);
        let secret = Secret::new("A".into(), "value".into(), vec!["example.com".into()]).unwrap();
        store.set(secret.in_scope(Some("dev".into()))).unwrap();
        let server = FirebrickServer::new(
            store,
            Arc::new(HostOpener),
            Arc::new(HostTools::new(None, None)),
        );

        let scopes: Vec<Option<String>> = server
            .list_secrets(Request::new(ListSecretsRequest {}))
            .await
            .unwrap()
            .into_inner()
            .secrets
            .into_iter()
            .map(|secret| secret.sandbox)
            .collect();

        assert_eq!(scopes, [None, Some("dev".to_string())]);
    }

    #[tokio::test]
    async fn remove_secret_rejects_invalid_name() {
        let server = FirebrickServer::new(
            unused_store(),
            Arc::new(HostOpener),
            Arc::new(HostTools::new(None, None)),
        );

        let status = server
            .remove_secret(Request::new(RemoveSecretRequest {
                name: "NOT-A-NAME".to_string(),
                sandbox: None,
            }))
            .await
            .expect_err("an invalid name should be rejected");

        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn remove_secret_returns_not_found_for_unknown_secret() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.yml");
        let store = SecretStore::new(&path);
        let secret = Secret::new("A".into(), "value".into(), vec!["example.com".into()]).unwrap();
        store.set(secret).unwrap();
        let server = FirebrickServer::new(
            store,
            Arc::new(HostOpener),
            Arc::new(HostTools::new(None, None)),
        );

        let status = server
            .remove_secret(Request::new(RemoveSecretRequest {
                name: "B".to_string(),
                sandbox: None,
            }))
            .await
            .expect_err("removing an unknown secret should fail");

        assert_eq!(status.code(), tonic::Code::NotFound);
        assert_eq!(SecretStore::new(&path).load().unwrap().len(), 1);
    }

    #[test]
    fn sandbox_name_rule_matches_microsandbox() {
        let long = "a".repeat(129);
        let longest = "a".repeat(128);
        let names = [
            "dev",
            "My.App_2",
            "firebrick-8a5eda",
            "0",
            "a..",
            &longest,
            "",
            "..",
            ".dev",
            "-dev",
            "_dev",
            "a/b",
            "../../x",
            "dév",
            "a b",
            "x\u{1b}]0;x\u{7}",
            &long,
        ];

        for name in names {
            assert_eq!(
                firebrick_spec::validate_sandbox_name(name).is_ok(),
                microsandbox::sandbox::validate_sandbox_name(name).is_ok(),
                "{name:?}"
            );
        }
    }

    /// Names that would escape fbkd's socket and record directories.
    const ESCAPING_NAMES: [&str; 3] = ["../../x", "a/b", ".."];

    fn name_server() -> FirebrickServer {
        FirebrickServer::new(
            unused_store(),
            Arc::new(HostOpener),
            Arc::new(HostTools::new(None, None)),
        )
    }

    fn assert_invalid_names(name: &str, results: Vec<Result<(), Status>>) {
        for result in results {
            let status = result.expect_err(name);

            assert_eq!(status.code(), tonic::Code::InvalidArgument, "{name:?}");
            assert!(
                status.message().contains("invalid sandbox name"),
                "{status}"
            );
        }
    }

    /// Starts a sandbox that shares the clipboard, so a start would listen on both sockets.
    async fn start_named(server: &FirebrickServer, name: &str) -> Result<(), Status> {
        let request = StartSandboxRequest {
            name: name.to_string(),
            clipboard: Some(true),
            ..Default::default()
        };

        server.start_sandbox(Request::new(request)).await.map(drop)
    }

    /// Calls the RPCs that change a sandbox with the name.
    async fn change_named(server: &FirebrickServer, name: &str) -> Vec<Result<(), Status>> {
        let name = name.to_string();
        let port = Some(PortForward {
            host: 8080,
            guest: 80,
        });
        let network = UpdateNetworkRequest {
            name: name.clone(),
            network: None,
        };
        let upgrade = UpgradeSandboxRequest {
            name: name.clone(),
            ..Default::default()
        };
        let forward = ForwardPortRequest {
            name: name.clone(),
            port,
        };
        let unforward = RemovePortRequest { name, host: 8080 };

        vec![
            server.update_network(Request::new(network)).await.map(drop),
            server
                .upgrade_sandbox(Request::new(upgrade))
                .await
                .map(drop),
            server.forward_port(Request::new(forward)).await.map(drop),
            server.remove_port(Request::new(unforward)).await.map(drop),
        ]
    }

    /// Calls the RPCs that look up, stop or remove a sandbox with the name.
    async fn manage_named(server: &FirebrickServer, name: &str) -> Vec<Result<(), Status>> {
        let name = name.to_string();
        let activity = ListNetworkActivityRequest { name: name.clone() };
        let stop = StopSandboxRequest { name: name.clone() };
        let get = GetSandboxRequest { name: name.clone() };
        let remove = RemoveSandboxRequest { name, force: true };

        vec![
            server
                .list_network_activity(Request::new(activity))
                .await
                .map(drop),
            server.stop_sandbox(Request::new(stop)).await.map(drop),
            server.get_sandbox(Request::new(get)).await.map(drop),
            server.remove_sandbox(Request::new(remove)).await.map(drop),
        ]
    }

    /// Calls the secret RPCs with the name as the secret's scope.
    async fn scope_named(server: &FirebrickServer, name: &str) -> Vec<Result<(), Status>> {
        let sandbox = Some(name.to_string());
        let set = SetSecretRequest {
            name: "TOKEN".to_string(),
            value: "value".to_string(),
            allowed_hosts: vec![],
            sandbox: sandbox.clone(),
        };
        let remove = RemoveSecretRequest {
            name: "TOKEN".to_string(),
            sandbox,
        };

        vec![
            server.set_secret(Request::new(set)).await.map(drop),
            server.remove_secret(Request::new(remove)).await.map(drop),
        ]
    }

    /// Calls every unary and server-streaming RPC that takes a sandbox name with each of the
    /// escaping names. `Attach` reads the name from its client stream, so it isn't called here.
    async fn call_with_escaping_names(
        server: &FirebrickServer,
    ) -> Vec<(&'static str, Vec<Result<(), Status>>)> {
        let mut calls = Vec::new();

        for name in ESCAPING_NAMES {
            let mut results = vec![start_named(server, name).await];
            results.extend(change_named(server, name).await);
            results.extend(manage_named(server, name).await);
            results.extend(scope_named(server, name).await);
            calls.push((name, results));
        }

        calls
    }

    #[tokio::test]
    async fn rpcs_reject_an_invalid_sandbox_name_before_using_it() {
        // An error from `StartSandbox` itself means fbkd didn't spawn the start, so it didn't
        // listen for the sandbox's network log or clipboard.
        let calls = call_with_escaping_names(&name_server()).await;

        calls
            .into_iter()
            .for_each(|(name, results)| assert_invalid_names(name, results));
    }

    #[tokio::test]
    async fn start_and_upgrade_reject_an_invalid_image_before_using_it() {
        let server = name_server();
        let image = "alpine\u{1b}]0;pwned\u{7}".to_string();
        let start = StartSandboxRequest {
            name: "dev".to_string(),
            image: image.clone(),
            ..Default::default()
        };
        let upgrade = UpgradeSandboxRequest {
            name: "dev".to_string(),
            image,
        };

        let results = [
            server.start_sandbox(Request::new(start)).await.map(drop),
            server
                .upgrade_sandbox(Request::new(upgrade))
                .await
                .map(drop),
        ];

        results.into_iter().for_each(assert_invalid_image);
    }

    #[tokio::test]
    async fn upgrade_accepts_an_empty_image_as_the_default() {
        let request = UpgradeSandboxRequest {
            name: format!("fbk-unit-{}-empty-image", std::process::id()),
            image: String::new(),
        };

        // The checks passed when the upgrade starts streaming; the stream itself would report
        // that the sandbox doesn't exist.
        let result = name_server().upgrade_sandbox(Request::new(request)).await;

        assert!(result.is_ok(), "{:?}", result.err());
    }

    fn assert_invalid_image(result: Result<(), Status>) {
        let status = result.unwrap_err();

        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("invalid image"), "{status}");
        assert!(!status.message().contains(char::is_control), "{status}");
    }

    #[tokio::test]
    async fn update_network_rejects_invalid_rule() {
        let server = FirebrickServer::new(
            unused_store(),
            Arc::new(HostOpener),
            Arc::new(HostTools::new(None, None)),
        );

        let status = server
            .update_network(Request::new(UpdateNetworkRequest {
                name: "fbk-unit-update-network".to_string(),
                network: Some(NetworkPolicy {
                    enforce: true,
                    allow: vec!["https://example.org".to_string()],
                    ..Default::default()
                }),
            }))
            .await
            .expect_err("an invalid rule should be rejected");

        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(
            status.message().contains("https://example.org"),
            "{status:?}"
        );
    }
}
