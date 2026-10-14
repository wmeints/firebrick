//! The gRPC server: adapts `SandboxManagementService` requests to the sandbox management in
//! [`crate::sandboxes`], [`crate::session`] and [`crate::tunnel`].

use crate::api::sandbox_management_service_server::{
    SandboxManagementService, SandboxManagementServiceServer,
};
use crate::api::{
    AttachRequest, AttachResponse, AttachStart, ForwardPortRequest, ForwardPortResponse,
    GetSandboxRequest, GetSandboxResponse, ImagePullProgress, ListNetworkActivityRequest,
    ListNetworkActivityResponse, ListSandboxesRequest, ListSandboxesResponse, ListSecretsRequest,
    ListSecretsResponse, MiseInstallProgress, Mount, NetworkActivity, NetworkActivityKind,
    NetworkPolicy, PortForward, PortForwardFailure, PortForwards, RemovePortRequest,
    RemovePortResponse, RemoveSandboxRequest, RemoveSandboxResponse, RemoveSecretRequest,
    RemoveSecretResponse, SandboxResources, SandboxStarted, SandboxStatus, SandboxSummary,
    SandboxUpgraded, SecretSummary, SetSecretRequest, SetSecretResponse, SshTunnelRequest,
    SshTunnelResponse, StartSandboxRequest, StartSandboxResponse, StopSandboxRequest,
    StopSandboxResponse, UpdateNetworkRequest, UpdateNetworkResponse, UpgradeSandboxRequest,
    UpgradeSandboxResponse, attach_request, ssh_tunnel_request, start_sandbox_response,
    upgrade_sandbox_response,
};
use crate::clipboard::{HostClipboard, HostTools};
use crate::forward::{ForwardFailure, ForwardReport};
use crate::mise::MiseUpdate;
use crate::netlog::{ConnectionEntry, DnsEntry, Protocol, Record};
use crate::network;
use crate::open::{HostOpener, Opener};
use crate::pull::PullUpdate;
use crate::sandboxes::{
    NewSecret, Resources, SandboxError, SandboxInfo, SandboxManager, StartProgress, StartSandbox,
    Upgrade,
};
use crate::secrets::{Secret, SecretStore};
use crate::session::{self, SessionCommand};
use crate::tunnel;
use anyhow::Result;
use async_trait::async_trait;
use firebrick_spec::{
    MountSpec, NetworkRule, NetworkSpec, PortMapping, SandboxResourcesSpec, VolumesSpec,
};
use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tonic::codegen::tokio_stream::wrappers::{ReceiverStream, UnixListenerStream};
use tonic::codegen::tokio_stream::{Stream, StreamExt};
use tonic::transport::Server;
use tonic::{Request, Response, Status, Streaming};
use url::Host;

/// Errors that can occur while running the gRPC server.
#[derive(Error, Debug)]
pub enum ServerError {
    #[error("socket is already in use")]
    SocketAlreadyInUse(),
    #[error("server can't listen: {0}")]
    FailedToListen(#[from] tonic::transport::Error),
    #[error("invalid socket path")]
    InvalidSocketPath(#[from] std::io::Error),
    #[error("can't restrict the socket to its owner")]
    InsecureSocket(#[source] std::io::Error),
}

/// gRPC service implementation that manages sandboxes.
pub struct FirebrickServer {
    // Shared with the tasks that start sandboxes, so a start finishes when its client is gone.
    sandboxes: Arc<SandboxManager>,
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
        }
    }

    /// Listens for the network logs and clipboards of the existing sandboxes that have their
    /// routes. Run when the daemon starts, so it serves the sandboxes that kept running while
    /// it was down.
    pub async fn restore_listeners(&self) {
        self.sandboxes.restore_listeners().await;
    }
}

/// Reads the sandbox to start from a request with its parsed egress rules and ports. A request
/// without `init` runs the image's init, and one without `mise` installs the workspace's mise
/// tools. A request without `clipboard` doesn't share the host clipboard.
fn start_sandbox_from<'a>(
    request: &'a StartSandboxRequest,
    network: &'a NetworkSpec,
    ports: Option<&'a [PortMapping]>,
    mounts: &'a [MountSpec],
) -> StartSandbox<'a> {
    StartSandbox {
        name: &request.name,
        workspace: &request.workspace,
        image: &request.image,
        init: request.init.unwrap_or(true),
        mise: request.mise.unwrap_or(true),
        network,
        ports,
        mounts,
        clipboard: request.clipboard.unwrap_or(false),
    }
}

/// Parses the ports of a request, failing with `InvalidArgument` on a port outside 1 to 65535
/// or a host port that is listed more than once.
fn port_mappings(ports: &PortForwards) -> Result<Vec<PortMapping>, Status> {
    let mut host_ports = HashSet::new();

    ports
        .ports
        .iter()
        .map(|port| {
            let mapping = PortMapping {
                host: port_number(port.host)?,
                guest: port_number(port.guest)?,
            };

            if !host_ports.insert(mapping.host) {
                return Err(Status::invalid_argument(format!(
                    "host port {} is listed more than once",
                    mapping.host
                )));
            }

            Ok(mapping)
        })
        .collect()
}

/// Converts a port from the request, failing with `InvalidArgument` outside 1 to 65535.
fn port_number(port: u32) -> Result<u16, Status> {
    u16::try_from(port)
        .ok()
        .filter(|port| *port > 0)
        .ok_or_else(|| Status::invalid_argument(format!("invalid port {port}: use 1 to 65535")))
}

/// Converts the forwards of a sandbox to the message that ends the start stream.
fn sandbox_started(report: ForwardReport) -> SandboxStarted {
    SandboxStarted {
        forwards: report.open.into_iter().map(port_forward).collect(),
        failed_forwards: report
            .failed
            .into_iter()
            .map(|ForwardFailure { port, reason }| PortForwardFailure {
                port: Some(port_forward(port)),
                reason,
            })
            .collect(),
    }
}

/// Wraps a message in the response of the start stream.
fn start_sandbox_response(message: start_sandbox_response::Message) -> StartSandboxResponse {
    StartSandboxResponse {
        message: Some(message),
    }
}

/// Converts the progress of an image pull to its protobuf message.
fn image_pull_progress(update: PullUpdate) -> ImagePullProgress {
    ImagePullProgress {
        image: update.image,
        downloaded_bytes: update.downloaded_bytes,
        total_bytes: update.total_bytes,
        complete: update.complete,
    }
}

/// Converts the progress of a start to a response of the start stream.
fn start_progress_response(progress: StartProgress) -> StartSandboxResponse {
    match progress {
        StartProgress::Pull(update) => pull_progress_response(update),
        StartProgress::Mise(update) => mise_progress_response(update),
    }
}

/// Converts the progress of installing the mise tools to a response of the start stream.
fn mise_progress_response(update: MiseUpdate) -> StartSandboxResponse {
    start_sandbox_response(start_sandbox_response::Message::MiseProgress(
        MiseInstallProgress {
            line: update.line,
            complete: update.complete,
        },
    ))
}

/// Converts the progress of an image pull to a response of the start stream.
fn pull_progress_response(update: PullUpdate) -> StartSandboxResponse {
    start_sandbox_response(start_sandbox_response::Message::PullProgress(
        image_pull_progress(update),
    ))
}

/// Wraps a message in the response of the upgrade stream.
fn upgrade_sandbox_response(message: upgrade_sandbox_response::Message) -> UpgradeSandboxResponse {
    UpgradeSandboxResponse {
        message: Some(message),
    }
}

/// Converts the result of an upgrade to the message that ends the upgrade stream.
fn sandbox_upgraded(upgrade: Upgrade) -> UpgradeSandboxResponse {
    upgrade_sandbox_response(upgrade_sandbox_response::Message::Upgraded(
        SandboxUpgraded {
            upgraded: upgrade.upgraded,
            old_image: upgrade.old_image,
            new_image: upgrade.new_image,
        },
    ))
}

/// Converts a mapping to its protobuf message.
fn port_forward(port: PortMapping) -> PortForward {
    PortForward {
        host: port.host.into(),
        guest: port.guest.into(),
    }
}

/// Reads the extra mounts of a request. The sandbox manager checks their paths.
fn mount_specs(mounts: &[Mount]) -> Vec<MountSpec> {
    mounts.iter().map(MountSpec::from).collect()
}

/// Parses the egress rules of a request. A request without them gets microsandbox's default
/// policy. Fails with `InvalidArgument` on the first invalid rule.
fn network_spec(network: Option<&NetworkPolicy>) -> Result<NetworkSpec, Status> {
    let Some(network) = network else {
        return Ok(NetworkSpec::default());
    };

    Ok(NetworkSpec {
        enabled: network.enabled,
        enforce: network.enforce,
        allow: parse_rules(&network.allow)?,
        deny: parse_rules(&network.deny)?,
    })
}

/// Parses network rules, failing with `InvalidArgument` on the first invalid one.
fn parse_rules(rules: &[String]) -> Result<Vec<NetworkRule>, Status> {
    rules
        .iter()
        .map(|rule| {
            rule.parse::<NetworkRule>()
                .map_err(|err| Status::invalid_argument(err.to_string()))
        })
        .collect()
}

/// Converts the record of a sandbox to its entries, sorted by last seen, newest first, each
/// with whether the rules allow its destination: domain rules for a DNS lookup, IP and CIDR
/// rules for a connection.
fn network_activity(record: Record, rules: &NetworkSpec) -> Vec<NetworkActivity> {
    let dns = record
        .dns
        .into_iter()
        .map(|entry| dns_activity(entry, rules));
    let connections = record
        .connections
        .into_iter()
        .map(|entry| connection_activity(entry, rules));
    let mut entries: Vec<NetworkActivity> = dns.chain(connections).collect();

    entries.sort_by_key(|entry| std::cmp::Reverse(entry.last_seen));

    entries
}

/// Converts a DNS lookup of the record to its protobuf entry.
fn dns_activity(entry: DnsEntry, rules: &NetworkSpec) -> NetworkActivity {
    NetworkActivity {
        kind: NetworkActivityKind::Dns.into(),
        allowed: network::allows_host(rules, &Host::Domain(entry.name.as_str())),
        destination: entry.name,
        addresses: entry.addresses.iter().map(ToString::to_string).collect(),
        ports: Vec::new(),
        count: entry.count,
        first_seen: entry.first_seen.timestamp(),
        last_seen: entry.last_seen.timestamp(),
    }
}

/// Converts the connection attempts of the record to their protobuf entry.
fn connection_activity(entry: ConnectionEntry, rules: &NetworkSpec) -> NetworkActivity {
    let kind = match entry.protocol {
        Protocol::Tcp => NetworkActivityKind::Tcp,
        Protocol::Udp => NetworkActivityKind::Udp,
    };

    NetworkActivity {
        kind: kind.into(),
        destination: entry.ip.to_string(),
        addresses: Vec::new(),
        ports: entry.ports.into_iter().map(u32::from).collect(),
        count: entry.count,
        first_seen: entry.first_seen.timestamp(),
        last_seen: entry.last_seen.timestamp(),
        allowed: network::allows_host(rules, &Host::Ipv4(entry.ip)),
    }
}

impl From<SandboxError> for Status {
    fn from(err: SandboxError) -> Self {
        match err {
            SandboxError::NotFound(message) => Status::not_found(message),
            SandboxError::InvalidArgument(message) => Status::invalid_argument(message),
            SandboxError::FailedPrecondition(message) => Status::failed_precondition(message),
            SandboxError::Unavailable(message) => Status::unavailable(message),
            SandboxError::Internal(message) => Status::internal(message),
        }
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
        check_name(&request_data.name)?;
        let network = network_spec(request_data.network.as_ref())?;
        let ports = request_data.ports.as_ref().map(port_mappings).transpose()?;
        let mounts = mount_specs(&request_data.mounts);
        let sandboxes = Arc::clone(&self.sandboxes);
        let (tx, rx) = mpsc::channel(16);

        tokio::spawn(async move {
            let progress_tx = tx.clone();
            let result = sandboxes
                .start(
                    start_sandbox_from(&request_data, &network, ports.as_deref(), &mounts),
                    || sandbox_resources(&request_data),
                    // Progress is best-effort: drop it when the client is slow or gone.
                    |progress| {
                        let _ = progress_tx.try_send(Ok(start_progress_response(progress)));
                    },
                )
                .await;
            let last = result
                .map(|report| {
                    start_sandbox_response(start_sandbox_response::Message::Started(
                        sandbox_started(report),
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

        let updated =
            tokio::spawn(
                async move { sandboxes.update_network(&request_data.name, &network).await },
            )
            .await
            .map_err(|err| {
                Status::internal(format!("failed to update the network rules: {err}"))
            })??;

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
        let sandboxes = Arc::clone(&self.sandboxes);
        let (tx, rx) = mpsc::channel(16);

        tokio::spawn(async move {
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
        let mut inbound = request.into_inner();
        let start = attach_start(&mut inbound).await?;
        check_name(&start.name)?;
        let size = session::window_size(start.size)?;
        let sb = self.sandboxes.connect(&start.name).await?;

        let command = SessionCommand {
            command: start.command,
            args: start.args,
            size,
        };
        let outbound = session::attach(sb, inbound, command).await?;

        Ok(Response::new(Box::pin(outbound)))
    }

    /// Serves an SSH connection to a sandbox over the stream, starting the sandbox when needed.
    async fn ssh_tunnel(
        &self,
        request: Request<Streaming<SshTunnelRequest>>,
    ) -> Result<Response<Self::SshTunnelStream>, Status> {
        let mut inbound = request.into_inner();
        let hostname = tunnel_hostname(&mut inbound).await?;
        let sb = self.sandboxes.connect_by_hostname(&hostname).await?;
        let outbound = tunnel::open(sb, inbound, hostname).await?;

        Ok(Response::new(Box::pin(outbound)))
    }
}

/// Fails with `InvalidArgument` when the sandbox name isn't one microsandbox accepts. fbkd names
/// files after sandboxes, so every request checks the name before it touches the file system or
/// microsandbox.
fn check_name(name: &str) -> Result<(), Status> {
    firebrick_spec::validate_sandbox_name(name)
        .map_err(|err| Status::invalid_argument(err.to_string()))
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

/// Converts a sandbox to its protobuf summary.
fn sandbox_summary(sandbox: SandboxInfo) -> SandboxSummary {
    SandboxSummary {
        name: sandbox.name,
        status: map_sandbox_status(sandbox.status),
        hostname: sandbox.hostname.unwrap_or_default(),
    }
}

/// Converts a secret to its protobuf summary, without its value.
fn secret_summary(secret: &Secret) -> SecretSummary {
    SecretSummary {
        name: secret.name().to_string(),
        allowed_hosts: secret.allowed_hosts().to_vec(),
        sandbox: secret.sandbox().map(str::to_string),
    }
}

/// Converts the requested resources and volumes to vCPUs and MiB, using the defaults for
/// missing resources, missing volumes and an empty volume size.
fn sandbox_resources(request: &StartSandboxRequest) -> Result<Resources, SandboxError> {
    let resources = request.resources.clone().unwrap_or_else(|| {
        let defaults = SandboxResourcesSpec::default();

        SandboxResources {
            cpu: defaults.cpu.into(),
            memory: defaults.memory,
        }
    });
    let docker_volume = request
        .volumes
        .as_ref()
        .map(|volumes| volumes.docker.clone())
        .filter(|size| !size.is_empty())
        .unwrap_or_else(|| VolumesSpec::default().docker);

    let cpus = u8::try_from(resources.cpu)
        .ok()
        .filter(|cpus| *cpus > 0)
        .ok_or_else(|| SandboxError::InvalidArgument("cpu must be between 1 and 255".into()))?;

    Ok(Resources {
        cpus,
        memory_mib: parse_size_mib(&resources.memory)?,
        docker_volume_mib: parse_size_mib(&docker_volume)?,
    })
}

/// Parses a requested size into MiB, reporting a bad size as an invalid argument.
fn parse_size_mib(size: &str) -> Result<u32, SandboxError> {
    firebrick_spec::parse_size_mib(size)
        .map_err(|err| SandboxError::InvalidArgument(err.to_string()))
}

/// How long the daemon waits for the updates and upgrades of sandboxes when it shuts down.
const RECREATION_TIMEOUT: Duration = Duration::from_secs(60);

/// Reopens the port forwards of the sandboxes that are running, listens for the network logs
/// and clipboards of the sandboxes and removes the upgrade probes an earlier daemon left behind,
/// then serves the gRPC API on the socket until SIGINT or SIGTERM is received, opening the URLs
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
    sandboxes.restore_forwards().await;
    sandboxes.restore_listeners().await;
    sandboxes.remove_stale_probes().await;

    let served = serve(socket_path, service, shutdown_signal()).await;

    if !sandboxes.finish_recreations(RECREATION_TIMEOUT).await {
        tracing::warn!("stopped while a sandbox was still being updated or upgraded");
    }

    sandboxes.close_listeners().await;

    served
}

/// Serves the gRPC API on the socket with `service` until `shutdown` completes, then removes
/// the socket.
pub async fn serve(
    socket_path: &Path,
    service: FirebrickServer,
    shutdown: impl Future<Output = ()>,
) -> Result<(), ServerError> {
    if socket_path.exists() {
        return Err(ServerError::SocketAlreadyInUse());
    }

    let listener = UnixListener::bind(socket_path).map_err(ServerError::InvalidSocketPath)?;
    let daemon_uid = restrict_to_owner(socket_path).inspect_err(|_| {
        let _ = fs::remove_file(socket_path);
    })?;
    let incoming = UnixListenerStream::new(listener)
        .filter(move |conn| conn.as_ref().map_or(true, |s| accept(s, daemon_uid)));

    // Dropping the service when the server stops closes the forwards.
    Server::builder()
        .trace_fn(|req| tracing::info_span!("grpc", path = %req.uri().path()))
        .add_service(SandboxManagementServiceServer::new(service))
        .serve_with_incoming_shutdown(incoming, shutdown)
        .await
        .map_err(ServerError::FailedToListen)?;

    fs::remove_file(socket_path)?;

    Ok(())
}

/// Gives the socket mode `0600`, whatever the umask, and returns the UID that owns it, which
/// is the daemon's effective UID.
fn restrict_to_owner(socket_path: &Path) -> Result<u32, ServerError> {
    fs::set_permissions(socket_path, fs::Permissions::from_mode(0o600))
        .and_then(|()| fs::metadata(socket_path))
        .map(|metadata| metadata.uid())
        .map_err(ServerError::InsecureSocket)
}

/// Returns whether a connection may reach the gRPC service, based on its peer credentials.
/// A rejected connection is closed when the caller drops it.
fn accept(conn: &UnixStream, daemon_uid: u32) -> bool {
    match conn.peer_cred() {
        Ok(peer) if firebrick_utils::is_trusted_peer(peer.uid(), daemon_uid) => true,
        Ok(peer) => {
            tracing::warn!(
                peer_uid = peer.uid(),
                peer_pid = ?peer.pid(),
                "rejected connection from another user"
            );
            false
        }
        Err(err) => {
            tracing::warn!(error = ?err, "rejected connection with unknown peer credentials");
            false
        }
    }
}

/// Completes when SIGINT or SIGTERM is received.
async fn shutdown_signal() {
    let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");

    tokio::select! {
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
        _ = sigterm.recv() => tracing::info!("shutting down"),
    }
}

/// Converts a microsandbox status to its protobuf status value.
fn map_sandbox_status(s: microsandbox::sandbox::SandboxStatus) -> i32 {
    let status = match s {
        microsandbox::sandbox::SandboxStatus::Running => SandboxStatus::Running,
        microsandbox::sandbox::SandboxStatus::Stopped => SandboxStatus::Stopped,
        microsandbox::sandbox::SandboxStatus::Created => SandboxStatus::Stopped,
        microsandbox::sandbox::SandboxStatus::Starting => SandboxStatus::Starting,
        microsandbox::sandbox::SandboxStatus::Paused => SandboxStatus::Paused,
        microsandbox::sandbox::SandboxStatus::Draining => SandboxStatus::Stopping,
        microsandbox::sandbox::SandboxStatus::Crashed => SandboxStatus::Crashed,
    };

    status.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::SandboxVolumes;
    use crate::api::sandbox_management_service_client::SandboxManagementServiceClient;
    use hyper_util::rt::TokioIo;
    use microsandbox::sandbox::SandboxStatus as MsbStatus;
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

    /// Waits until the server accepts connections on the socket.
    async fn wait_for_socket(path: &Path) {
        while UnixStream::connect(path).await.is_err() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Connects a gRPC client to the server on the socket.
    async fn client(path: PathBuf) -> SandboxManagementServiceClient<Channel> {
        // The HTTP endpoint isn't used; it only shows up in the authority header.
        let channel = Endpoint::try_from("http://localhost")
            .unwrap()
            .connect_with_connector(tower::service_fn(move |_: Uri| connect_unix(path.clone())))
            .await
            .unwrap();

        SandboxManagementServiceClient::new(channel)
    }

    /// Connects to the server socket for a gRPC channel.
    async fn connect_unix(path: PathBuf) -> std::io::Result<TokioIo<UnixStream>> {
        Ok(TokioIo::new(UnixStream::connect(path).await?))
    }

    #[test]
    fn map_sandbox_status_maps_every_variant() {
        let cases = [
            (MsbStatus::Running, SandboxStatus::Running),
            (MsbStatus::Stopped, SandboxStatus::Stopped),
            (MsbStatus::Created, SandboxStatus::Stopped),
            (MsbStatus::Starting, SandboxStatus::Starting),
            (MsbStatus::Paused, SandboxStatus::Paused),
            (MsbStatus::Draining, SandboxStatus::Stopping),
            (MsbStatus::Crashed, SandboxStatus::Crashed),
        ];

        for (input, expected) in cases {
            assert_eq!(map_sandbox_status(input), expected as i32);
        }
    }

    #[test]
    fn map_sandbox_status_produces_valid_proto_values() {
        let value = map_sandbox_status(MsbStatus::Draining);
        assert_eq!(SandboxStatus::try_from(value), Ok(SandboxStatus::Stopping));
    }

    #[test]
    fn start_sandbox_from_request_defaults_init_to_true() {
        let request = StartSandboxRequest::default();

        assert!(start_sandbox_from(&request, &NetworkSpec::default(), None, &[]).init);
    }

    #[test]
    fn start_sandbox_from_request_defaults_mise_to_true() {
        let request = StartSandboxRequest::default();

        assert!(start_sandbox_from(&request, &NetworkSpec::default(), None, &[]).mise);
    }

    #[test]
    fn start_sandbox_from_request_keeps_mise() {
        let request = StartSandboxRequest {
            mise: Some(false),
            ..Default::default()
        };

        assert!(!start_sandbox_from(&request, &NetworkSpec::default(), None, &[]).mise);
    }

    #[test]
    fn start_sandbox_from_request_keeps_init() {
        let request = StartSandboxRequest {
            name: "dev".to_string(),
            image: "alpine:3.22".to_string(),
            workspace: "/home/user/project".to_string(),
            init: Some(false),
            ..Default::default()
        };

        let network = NetworkSpec::default();
        let start = start_sandbox_from(&request, &network, None, &[]);

        assert_eq!(
            (start.name, start.image, start.workspace, start.init),
            ("dev", "alpine:3.22", "/home/user/project", false)
        );
    }

    #[test]
    fn port_mappings_converts_the_ports() {
        let ports = PortForwards {
            ports: vec![
                PortForward {
                    host: 3000,
                    guest: 3000,
                },
                PortForward {
                    host: 8080,
                    guest: 5173,
                },
            ],
        };

        assert_eq!(
            port_mappings(&ports).unwrap(),
            [
                PortMapping {
                    host: 3000,
                    guest: 3000
                },
                PortMapping {
                    host: 8080,
                    guest: 5173
                },
            ]
        );
    }

    #[test]
    fn port_mappings_rejects_invalid_and_duplicate_ports() {
        let port = |host, guest| PortForward { host, guest };

        for ports in [
            vec![port(0, 80)],
            vec![port(80, 0)],
            vec![port(65536, 80)],
            vec![port(80, 65536)],
            vec![port(80, 80), port(80, 81)],
        ] {
            let status = port_mappings(&PortForwards {
                ports: ports.clone(),
            })
            .unwrap_err();

            assert_eq!(status.code(), tonic::Code::InvalidArgument, "{ports:?}");
        }
    }

    fn mise_update(line: &str, complete: bool) -> StartProgress {
        StartProgress::Mise(MiseUpdate {
            line: line.to_string(),
            complete,
        })
    }

    fn mise_response(line: &str, complete: bool) -> StartSandboxResponse {
        start_sandbox_response(start_sandbox_response::Message::MiseProgress(
            MiseInstallProgress {
                line: line.to_string(),
                complete,
            },
        ))
    }

    fn pull_update() -> StartProgress {
        StartProgress::Pull(PullUpdate {
            image: "alpine:3.22".to_string(),
            downloaded_bytes: 100,
            total_bytes: Some(400),
            complete: false,
        })
    }

    fn pull_response() -> StartSandboxResponse {
        StartSandboxResponse {
            message: Some(start_sandbox_response::Message::PullProgress(
                ImagePullProgress {
                    image: "alpine:3.22".to_string(),
                    downloaded_bytes: 100,
                    total_bytes: Some(400),
                    complete: false,
                },
            )),
        }
    }

    #[test]
    fn start_progress_response_converts_each_kind_of_progress() {
        let cases = [
            (
                mise_update("mise node@22 download", false),
                mise_response("mise node@22 download", false),
            ),
            (mise_update("", true), mise_response("", true)),
            (pull_update(), pull_response()),
        ];

        for (progress, expected) in cases {
            assert_eq!(
                start_progress_response(progress.clone()),
                expected,
                "{progress:?}"
            );
        }
    }

    #[test]
    fn pull_progress_response_keeps_the_progress() {
        let update = PullUpdate {
            image: "alpine:3.22".to_string(),
            downloaded_bytes: 100,
            total_bytes: None,
            complete: true,
        };

        assert_eq!(
            pull_progress_response(update),
            StartSandboxResponse {
                message: Some(start_sandbox_response::Message::PullProgress(
                    ImagePullProgress {
                        image: "alpine:3.22".to_string(),
                        downloaded_bytes: 100,
                        total_bytes: None,
                        complete: true,
                    }
                )),
            }
        );
    }

    #[test]
    fn sandbox_started_lists_open_and_failed_forwards() {
        let mapping = |host, guest| PortMapping { host, guest };
        let report = ForwardReport {
            open: vec![mapping(8080, 5173)],
            failed: vec![ForwardFailure {
                port: mapping(3000, 3000),
                reason: "Address in use".to_string(),
            }],
        };

        let response = sandbox_started(report);

        assert_eq!(response.forwards, [port_forward(mapping(8080, 5173))]);
        assert_eq!(
            response.failed_forwards,
            [PortForwardFailure {
                port: Some(port_forward(mapping(3000, 3000))),
                reason: "Address in use".to_string(),
            }]
        );
    }

    #[test]
    fn mount_specs_keep_the_paths_and_readonly() {
        let mounts = [Mount {
            host: "/home/user/lib".to_string(),
            guest: "/workspaces/lib".to_string(),
            readonly: true,
        }];

        assert_eq!(
            mount_specs(&mounts),
            [MountSpec {
                host: "/home/user/lib".to_string(),
                guest: "/workspaces/lib".to_string(),
                readonly: true,
            }]
        );
    }

    #[test]
    fn network_spec_without_network_does_not_enforce() {
        assert_eq!(network_spec(None).unwrap(), NetworkSpec::default());
    }

    #[test]
    fn network_spec_parses_the_rules() {
        let network = NetworkPolicy {
            enforce: true,
            allow: vec!["*.github.com".to_string(), "10.0.0.0/8".to_string()],
            deny: vec!["gist.github.com".to_string()],
            ..Default::default()
        };

        let spec = network_spec(Some(&network)).unwrap();

        assert!(spec.enforce);
        assert!(spec.is_enabled());
        assert_eq!(
            spec.allow,
            [
                NetworkRule::DomainSuffix("github.com".to_string()),
                NetworkRule::Cidr {
                    address: "10.0.0.0".parse().unwrap(),
                    prefix: 8
                },
            ]
        );
        assert_eq!(
            spec.deny,
            [NetworkRule::Domain("gist.github.com".to_string())]
        );
    }

    #[test]
    fn network_spec_passes_on_the_network_switch() {
        for enabled in [None, Some(true), Some(false)] {
            let network = NetworkPolicy {
                enabled,
                ..Default::default()
            };

            assert_eq!(network_spec(Some(&network)).unwrap().enabled, enabled);
        }
    }

    #[test]
    fn network_spec_rejects_invalid_rules() {
        for network in [
            NetworkPolicy {
                allow: vec!["https://github.com".to_string()],
                ..Default::default()
            },
            NetworkPolicy {
                deny: vec!["https://github.com".to_string()],
                ..Default::default()
            },
        ] {
            let status = network_spec(Some(&network)).unwrap_err();

            assert_eq!(status.code(), tonic::Code::InvalidArgument);
            assert_eq!(
                status.message(),
                "invalid network rule \"https://github.com\": use a host name, *.domain, an IP \
                 address or a CIDR range"
            );
        }
    }

    #[test]
    fn sandbox_errors_map_to_status_codes_and_messages() {
        let cases = [
            (SandboxError::NotFound("a".into()), tonic::Code::NotFound),
            (
                SandboxError::InvalidArgument("b".into()),
                tonic::Code::InvalidArgument,
            ),
            (
                SandboxError::FailedPrecondition("c".into()),
                tonic::Code::FailedPrecondition,
            ),
            (
                SandboxError::Unavailable("e".into()),
                tonic::Code::Unavailable,
            ),
            (SandboxError::Internal("d".into()), tonic::Code::Internal),
        ];

        for (err, code) in cases {
            let message = err.to_string();
            let status = Status::from(err);

            assert_eq!(status.code(), code);
            assert_eq!(status.message(), message);
        }
    }

    /// Returns a start request with the resources and Docker volume size.
    fn resources_request(cpu: u32, memory: &str, docker: Option<&str>) -> StartSandboxRequest {
        StartSandboxRequest {
            resources: Some(SandboxResources {
                cpu,
                memory: memory.to_string(),
            }),
            volumes: docker.map(|docker| SandboxVolumes {
                docker: docker.to_string(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn sandbox_resources_converts_request() {
        let request = resources_request(4, "8 GiB", Some("40 GiB"));

        assert_eq!(
            sandbox_resources(&request).unwrap(),
            Resources {
                cpus: 4,
                memory_mib: 8192,
                docker_volume_mib: 40960,
            }
        );
    }

    #[test]
    fn sandbox_resources_uses_default_docker_volume_when_missing_or_empty() {
        for docker in [None, Some("")] {
            let request = resources_request(4, "8 GiB", docker);

            assert_eq!(
                sandbox_resources(&request).unwrap().docker_volume_mib,
                20480,
                "{docker:?}"
            );
        }
    }

    #[test]
    fn sandbox_resources_falls_back_to_defaults() {
        assert_eq!(
            sandbox_resources(&StartSandboxRequest::default()).unwrap(),
            Resources {
                cpus: 2,
                memory_mib: 4096,
                docker_volume_mib: 20480,
            }
        );
    }

    #[test]
    fn sandbox_resources_rejects_invalid_values() {
        let cases = [
            (0, "1 GiB", None),
            (256, "1 GiB", None),
            (2, "lots", None),
            (2, "", None),
            (2, "1 GiB", Some("20 GB")),
            (2, "1 GiB", Some("0 GiB")),
        ];

        for (cpu, memory, docker) in cases {
            let request = resources_request(cpu, memory, docker);

            assert_eq!(
                Status::from(sandbox_resources(&request).unwrap_err()).code(),
                tonic::Code::InvalidArgument,
                "cpu {cpu}, memory {memory:?}, docker {docker:?} should be rejected"
            );
        }
    }

    #[tokio::test]
    async fn run_fails_when_socket_already_exists() {
        let path = temp_path("existing.sock");
        fs::write(&path, b"").unwrap();

        let result = run(&path, unused_store()).await;

        fs::remove_file(&path).unwrap();
        assert!(matches!(result, Err(ServerError::SocketAlreadyInUse())));
    }

    #[tokio::test]
    async fn serve_removes_socket_on_shutdown() {
        let path = temp_path("shutdown.sock");
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();

        let handle = tokio::spawn(serve_until(path.clone(), rx));
        wait_for_socket(&path).await;

        tx.send(()).unwrap();
        handle.await.unwrap().unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn serve_restricts_socket_to_owner() {
        let path = temp_path("mode.sock");
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();

        let handle = tokio::spawn(serve_until(path.clone(), rx));
        wait_for_socket(&path).await;
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;

        tx.send(()).unwrap();
        handle.await.unwrap().unwrap();
        assert_eq!(mode, 0o600);
    }

    #[tokio::test]
    async fn serve_serves_connections_from_the_same_user() {
        let path = temp_path("same-user.sock");
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

    #[tokio::test]
    async fn run_fails_when_socket_directory_does_not_exist() {
        let path = temp_path("missing-dir").join("firebrick.sock");

        let result = run(&path, unused_store()).await;

        assert!(matches!(result, Err(ServerError::InvalidSocketPath(_))));
        assert!(!path.exists());
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

    /// Returns the time `seconds` after 2026-10-11 14:00:00 UTC.
    fn at(seconds: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(1_791_727_200 + seconds, 0).unwrap()
    }

    fn dns_entry(name: &str, last_seen: i64) -> DnsEntry {
        DnsEntry {
            name: name.to_string(),
            addresses: ["140.82.112.6".parse().unwrap()].into(),
            count: 12,
            first_seen: at(0),
            last_seen: at(last_seen),
        }
    }

    fn connection_entry(ip: &str, protocol: Protocol, last_seen: i64) -> ConnectionEntry {
        ConnectionEntry {
            ip: ip.parse().unwrap(),
            protocol,
            ports: [443, 80].into(),
            count: 3,
            first_seen: at(0),
            last_seen: at(last_seen),
        }
    }

    fn record() -> Record {
        Record {
            dns: vec![
                dns_entry("api.github.com", 131),
                dns_entry("evil.example.com", 220),
            ],
            connections: vec![
                connection_entry("140.82.112.6", Protocol::Tcp, 130),
                connection_entry("10.0.0.1", Protocol::Udp, 5),
            ],
        }
    }

    fn enforced(allow: &[&str]) -> NetworkSpec {
        NetworkSpec {
            enforce: true,
            allow: allow.iter().map(|rule| rule.parse().unwrap()).collect(),
            ..NetworkSpec::default()
        }
    }

    /// Describes each entry as `<destination> <allowed>`.
    fn allowed(entries: &[NetworkActivity]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| format!("{} {}", entry.destination, entry.allowed))
            .collect()
    }

    #[test]
    fn network_activity_sorts_entries_newest_first() {
        let entries = network_activity(record(), &NetworkSpec::default());
        let destinations: Vec<&str> = entries.iter().map(|e| e.destination.as_str()).collect();

        assert_eq!(
            destinations,
            [
                "evil.example.com",
                "api.github.com",
                "140.82.112.6",
                "10.0.0.1"
            ]
        );
    }

    #[test]
    fn network_activity_converts_the_entries() {
        let entries = network_activity(record(), &NetworkSpec::default());

        assert_eq!(
            entries[1],
            NetworkActivity {
                kind: NetworkActivityKind::Dns.into(),
                destination: "api.github.com".to_string(),
                addresses: vec!["140.82.112.6".to_string()],
                ports: vec![],
                count: 12,
                first_seen: 1_791_727_200,
                last_seen: 1_791_727_331,
                allowed: true,
            }
        );
        assert_eq!(
            entries[3],
            NetworkActivity {
                kind: NetworkActivityKind::Udp.into(),
                destination: "10.0.0.1".to_string(),
                addresses: vec![],
                ports: vec![80, 443],
                count: 3,
                first_seen: 1_791_727_200,
                last_seen: 1_791_727_205,
                allowed: true,
            }
        );
        assert_eq!(entries[2].kind(), NetworkActivityKind::Tcp);
    }

    #[test]
    fn network_activity_evaluates_domain_rules_for_dns_and_ip_rules_for_connections() {
        let rules = enforced(&["api.github.com", "140.82.112.0/20"]);

        assert_eq!(
            allowed(&network_activity(record(), &rules)),
            [
                "evil.example.com false",
                "api.github.com true",
                "140.82.112.6 true",
                "10.0.0.1 false"
            ]
        );
    }

    #[test]
    fn network_activity_allows_every_entry_when_the_rules_are_not_enforced() {
        let rules = NetworkSpec {
            enforce: false,
            ..enforced(&["api.github.com"])
        };

        let entries = network_activity(record(), &rules);

        assert!(entries.iter().all(|entry| entry.allowed), "{entries:?}");
    }

    #[test]
    fn network_activity_of_an_empty_record_is_empty() {
        assert_eq!(
            network_activity(Record::default(), &NetworkSpec::default()),
            []
        );
    }

    #[test]
    fn sandbox_upgraded_keeps_the_result() {
        let response = sandbox_upgraded(Upgrade {
            upgraded: true,
            old_image: "alpine:3.21".to_string(),
            new_image: "alpine:3.22".to_string(),
        });

        assert_eq!(
            response.message,
            Some(upgrade_sandbox_response::Message::Upgraded(
                SandboxUpgraded {
                    upgraded: true,
                    old_image: "alpine:3.21".to_string(),
                    new_image: "alpine:3.22".to_string(),
                }
            ))
        );
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
